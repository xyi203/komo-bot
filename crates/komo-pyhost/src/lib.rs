//! A Python plugin host: run `~/.komo/plugins/*.py` in a child process and
//! call the tools they register.
//!
//! Its own crate for the reason `komo-provider` and `komo-mcp` are: it
//! references nothing else in komo, so it compiles in parallel and an edit here
//! never rebuilds the agent. Nothing here knows about `Tool`, `ToolContext`, or
//! the catalog — the adapter that turns a [`PluginToolDef`] into a komo tool
//! lives in `komo-tools`, and deciding when to mount it lives in wiring.
//!
//! **Out of process, on purpose.** Embedding a Python interpreter would put a
//! GIL next to the async runtime, weld komo to one Python version, and give a
//! plugin the address space of the agent that runs it. A child process costs a
//! pipe and buys isolation: a plugin that segfaults, hangs, or leaks costs the
//! plugins and nothing else, and the supervisor starts a new one.
//!
//! The protocol is newline-delimited JSON over stdin/stdout — one object per
//! line. Requests travel both ways: komo asks for the manifest, a plugin call,
//! or a program run ([`PyHost::run_code`]); the host asks komo to run one of
//! *its* tools on behalf of a running program. The two id spaces cannot collide
//! because komo's requests carry positive ids and the host's carry negative
//! ones. The host also pushes `manifest/changed` unasked when a plugin file is
//! edited, which is what makes "write a `.py` file and the tool appears" work
//! without a restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, warn};

/// The SDK and host, embedded so a `cargo install`ed komo carries its own
/// Python side — there is no second package to install and no version of it
/// that can drift from the binary talking to it. Written out on spawn.
const SDK_SOURCE: &str = include_str!("../python/komo_plugin.py");
const HOST_SOURCE: &str = include_str!("../python/host.py");

/// Bumped when the wire contract changes. The host reports the version it
/// speaks in its manifest; a mismatch is refused rather than half-understood.
pub const PROTOCOL_VERSION: u32 = 4;

/// How a sub-call says the *turn* stopped to wait under it.
///
/// The one broker answer a program may not treat as a failure it can work
/// around: komo is already unwinding the turn, and a program that caught this
/// and kept going would go on making effects nobody is waiting for. `host.py`
/// matches this prefix and raises `ToolSuspended`, which derives
/// `BaseException` so neither `except Exception` nor `except ToolError` sees
/// it.
///
/// It lives here rather than beside the tool that produces it because the other
/// half of the agreement is this crate's embedded `host.py` — two spellings of
/// one marker is a program that keeps running, so a test holds them together.
pub const SUSPENDED_MARKER: &str = "komo: the turn stopped to wait";

/// Ceiling on one request to the host. A plugin doing real work can be slow, so
/// this is generous — it exists to catch a host that stopped answering, not to
/// bound legitimate work. The tool layer applies its own per-call timeout too.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum PyHostError {
    /// The host could not be started or has gone away. Retryable in the sense
    /// that a restarted host may answer — but the call never ran, so no side
    /// effect can have landed.
    #[error("plugin host unavailable: {0}")]
    Unavailable(String),
    /// The host answered, and the answer was an error — a plugin raising, an
    /// unknown tool name. Retrying re-sends the same request to code that
    /// already rejected it.
    #[error("{0}")]
    Plugin(String),
}

impl PyHostError {
    /// Whether a retry could plausibly succeed *and* is safe. Only for a host
    /// that never received the request.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// One tool a plugin registered, as the host describes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments, derived from the Python signature.
    pub parameters: serde_json::Value,
}

/// What the host reports when asked for its manifest.
#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    #[serde(default)]
    protocol: u32,
    #[serde(default)]
    tools: Vec<PluginToolDef>,
}

/// What a host has loaded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostManifest {
    pub tools: Vec<PluginToolDef>,
}

/// What one `run_code` program produced.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CodeResult {
    /// Everything the program printed, in order. Its own output channel: a
    /// program reports intermediate work by printing rather than by stuffing it
    /// all into the return value.
    #[serde(default)]
    pub logs: String,
    /// What the program returned, or `None` when it returned nothing.
    #[serde(default)]
    pub result: Option<serde_json::Value>,
}

/// What one of a program's tool calls answers with.
///
/// Two channels, because a tool's text is laid out for a reader and a program is
/// not one: `content` is the string the model would have been shown, and
/// `structured` is the same result as data — `Null` for a tool that reports no
/// structured view. A program that has to *compute* on a result reads the second
/// rather than re-parsing the first.
pub struct ToolAnswer {
    pub content: String,
    pub structured: serde_json::Value,
}

impl ToolAnswer {
    /// An answer carrying text alone — what a tool with no structured view
    /// gives.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            structured: serde_json::Value::Null,
        }
    }
}

/// One komo tool call a running program made.
struct ToolRequest {
    /// The host's own (negative) request id, echoed back with the answer.
    id: i64,
    name: String,
    args: serde_json::Value,
}

/// A message from the host that komo did not ask for.
#[derive(Debug, Clone)]
pub enum HostEvent {
    /// A plugin file changed and the host reloaded: this is the new set.
    ManifestChanged(HostManifest),
    /// The host process exited. Whoever owns it decides whether to restart —
    /// this crate reports, it does not resurrect.
    Exited { status: String },
}

/// A running plugin host. Cheap to clone; every clone talks to the same child.
#[derive(Clone)]
pub struct PyHost {
    inner: Arc<Inner>,
}

struct Inner {
    /// Request id → where its response goes. The reader task drains this.
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<serde_json::Value, PyHostError>>>>,
    /// Request id → where the callbacks of the handler running it go. Every
    /// call the host originates is tagged with the request it belongs to, which
    /// is what lets two programs (or two plugin tools) run over one host
    /// without their calls crossing.
    code_runs: Mutex<HashMap<i64, mpsc::UnboundedSender<ToolRequest>>>,
    next_id: AtomicI64,
    outbound: mpsc::UnboundedSender<String>,
    /// The child, kept so dropping the host kills it rather than orphaning a
    /// Python process for the rest of the session.
    child: Mutex<Option<Child>>,
}

/// The current plugin host, shared with whoever needs to reach it later.
///
/// A [`PyHost`] handle is one child process: when the supervisor restarts a
/// dead host, the old handle is not the new one. Anything long-lived that calls
/// into the host — a tool registered at wiring, for instance — therefore holds
/// this slot rather than a handle, and reads whichever host is current at call
/// time. Empty means no host is running, which is a real answer: the caller
/// says so rather than failing obscurely.
#[derive(Clone, Default)]
pub struct SharedHost(Arc<std::sync::RwLock<Option<PyHost>>>);

impl SharedHost {
    /// The host that is running right now, if any.
    pub fn get(&self) -> Option<PyHost> {
        match self.0.read() {
            Ok(host) => host.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Publish a newly started host, or `None` when one has gone away.
    pub fn set(&self, host: Option<PyHost>) {
        match self.0.write() {
            Ok(mut slot) => *slot = host,
            Err(poisoned) => *poisoned.into_inner() = host,
        }
    }
}

/// Where the plugin host's own files live, written fresh on every spawn.
///
/// Not the plugin directory: these are komo's, and a user editing them would be
/// editing something the next launch overwrites. Keeping them apart also means
/// `~/.komo/plugins` contains only what the operator (or the agent) authored.
pub fn runtime_dir(home: &Path) -> PathBuf {
    home.join("pyhost")
}

impl PyHost {
    /// Start a host for the plugins in `plugins_dir`.
    ///
    /// `python` is the interpreter to run (`python3` unless configured
    /// otherwise) and `home` is where the embedded SDK is materialized. Returns
    /// the handle plus the stream of unsolicited host events; dropping the
    /// receiver is fine — events are then discarded.
    pub async fn spawn(
        python: &str,
        home: &Path,
        plugins_dir: &Path,
    ) -> Result<(Self, mpsc::UnboundedReceiver<HostEvent>), PyHostError> {
        let runtime = runtime_dir(home);
        write_runtime(&runtime)?;

        let mut child = Command::new(python)
            // Unbuffered: the protocol is a conversation, and a buffered child
            // would answer only once its pipe filled.
            .arg("-u")
            .arg(runtime.join("host.py"))
            .arg(plugins_dir)
            .env("PYTHONPATH", &runtime)
            // A plugin importing something that prompts, or a stray `input()`,
            // must fail rather than park the host forever.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is the host's log (and where plugin `print`s land); it is
            // inherited so it reaches komo's own stderr and daily log file.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                PyHostError::Unavailable(format!("could not start `{python}`: {error}"))
            })?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");

        let (outbound, outbound_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            pending: Mutex::new(HashMap::new()),
            code_runs: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            outbound,
            child: Mutex::new(Some(child)),
        });

        tokio::spawn(write_loop(stdin, outbound_rx));
        tokio::spawn(read_loop(inner.clone(), stdout, events));

        Ok((Self { inner }, events_rx))
    }

    /// Ask the host what it has loaded. Also the liveness check — a host that
    /// cannot answer this is not usable.
    pub async fn manifest(&self) -> Result<HostManifest, PyHostError> {
        let value = self.request("manifest", serde_json::json!({})).await?;
        let manifest: Manifest = serde_json::from_value(value)
            .map_err(|error| PyHostError::Plugin(format!("malformed manifest: {error}")))?;
        if manifest.protocol != PROTOCOL_VERSION {
            return Err(PyHostError::Unavailable(format!(
                "plugin host speaks protocol {}, this komo speaks {PROTOCOL_VERSION}",
                manifest.protocol
            )));
        }
        Ok(HostManifest {
            tools: manifest.tools,
        })
    }

    /// Run one plugin tool. The returned string is what the model sees.
    ///
    /// `dispatch` is the same broker a program gets (see [`run_code`]): a
    /// plugin function's `tools.read(path=...)` is one invocation of it. A
    /// plugin *is* a program somebody kept, so it reaches komo's own tools the
    /// same way and pays the same approval, ledger and cap.
    ///
    /// [`run_code`]: Self::run_code
    pub async fn call<F, Fut>(
        &self,
        name: &str,
        args: serde_json::Value,
        dispatch: F,
    ) -> Result<String, PyHostError>
    where
        F: Fn(String, serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<ToolAnswer, String>>,
    {
        let value = self
            .serving(
                "call",
                serde_json::json!({ "name": name, "args": args }),
                dispatch,
            )
            .await?;
        Ok(value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// Run one program on the host, servicing the komo tool calls it makes.
    ///
    /// `dispatch` is how a call gets back into komo's own executor — the
    /// program's `tools.read(path=...)` becomes one invocation of it, so a
    /// sub-call pays the same approval, ledger and cap the model's own call
    /// would. Sub-calls are serviced while the program runs; the future
    /// resolves when the program returns.
    pub async fn run_code<F, Fut>(
        &self,
        source: &str,
        dispatch: F,
    ) -> Result<CodeResult, PyHostError>
    where
        F: Fn(String, serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<ToolAnswer, String>>,
    {
        let value = self
            .serving(
                "run_code",
                serde_json::json!({ "source": source }),
                dispatch,
            )
            .await?;
        serde_json::from_value(value)
            .map_err(|error| PyHostError::Plugin(format!("malformed program result: {error}")))
    }

    /// One request whose handler may call back into komo, serviced until it
    /// answers.
    ///
    /// The request's own id is what the host tags each call with, which is what
    /// lets two of these run over one host without their calls crossing —
    /// whether they are two programs, two plugin tools, or one of each.
    async fn serving<F, Fut>(
        &self,
        method: &str,
        params: serde_json::Value,
        dispatch: F,
    ) -> Result<serde_json::Value, PyHostError>
    where
        F: Fn(String, serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<ToolAnswer, String>>,
    {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
        self.inner.pending.lock().await.insert(id, response_tx);
        self.inner.code_runs.lock().await.insert(id, calls_tx);

        let cleanup = || async {
            self.inner.pending.lock().await.remove(&id);
            self.inner.code_runs.lock().await.remove(&id);
        };

        if let Err(error) = self.send(serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        })) {
            cleanup().await;
            return Err(error);
        }

        // Service the callbacks until the handler answers. One at a time, which
        // is what the host does anyway — synchronous python blocks on each call
        // — and what keeps the side effects in the order they were written.
        tokio::pin!(response_rx);
        let outcome = loop {
            tokio::select! {
                answer = &mut response_rx => break match answer {
                    Ok(result) => result,
                    Err(_) => Err(PyHostError::Unavailable(
                        "the plugin host exited before finishing".into(),
                    )),
                },
                Some(request) = calls_rx.recv() => {
                    let (answer, is_error) = match dispatch(request.name, request.args).await {
                        Ok(answer) => (answer, false),
                        Err(message) => (
                            ToolAnswer { content: message, structured: serde_json::Value::Null },
                            true,
                        ),
                    };
                    // Answer even if the send fails: a host that went away is
                    // about to fail the response too.
                    let _ = self.send(serde_json::json!({
                        "id": request.id,
                        "result": {
                            "content": answer.content,
                            "structured": answer.structured,
                            "is_error": is_error,
                        },
                    }));
                }
            }
        };
        cleanup().await;
        outcome
    }

    /// Ask the host to exit, then wait briefly for it. Dropping the handle also
    /// kills the child (`kill_on_drop`); this is the polite path, which lets a
    /// plugin's own cleanup run.
    pub async fn shutdown(&self) {
        let _ = self.send(serde_json::json!({ "method": "shutdown" }));
        let mut child = self.inner.child.lock().await;
        if let Some(mut child) = child.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
            let _ = child.start_kill();
        }
    }

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, PyHostError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);

        if let Err(error) = self.send(serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        })) {
            self.inner.pending.lock().await.remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            // The reader task dropped the sender: the host died mid-request.
            Ok(Err(_)) => Err(PyHostError::Unavailable(
                "the plugin host exited before answering".into(),
            )),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(PyHostError::Unavailable(format!(
                    "the plugin host did not answer `{method}` within {}s",
                    REQUEST_TIMEOUT.as_secs()
                )))
            }
        }
    }

    fn send(&self, message: serde_json::Value) -> Result<(), PyHostError> {
        self.inner
            .outbound
            .send(message.to_string())
            .map_err(|_| PyHostError::Unavailable("the plugin host is not running".into()))
    }
}

/// Materialize the embedded SDK and host next to komo's home.
///
/// Rewritten on every spawn rather than only when missing: these files belong
/// to the binary, and a stale copy from an older komo talking a newer protocol
/// is exactly the failure this avoids.
fn write_runtime(runtime: &Path) -> Result<(), PyHostError> {
    let write = |name: &str, source: &str| -> Result<(), PyHostError> {
        std::fs::write(runtime.join(name), source).map_err(|error| {
            PyHostError::Unavailable(format!(
                "could not write the plugin host to {}: {error}",
                runtime.display()
            ))
        })
    };
    std::fs::create_dir_all(runtime).map_err(|error| {
        PyHostError::Unavailable(format!("could not create {}: {error}", runtime.display()))
    })?;
    write("komo_plugin.py", SDK_SOURCE)?;
    write("host.py", HOST_SOURCE)
}

/// Feed the child's stdin. Ends when the host handle is dropped.
async fn write_loop(mut stdin: ChildStdin, mut outbound: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = outbound.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err()
            || stdin.write_all(b"\n").await.is_err()
            || stdin.flush().await.is_err()
        {
            break;
        }
    }
}

/// Demultiplex the child's stdout: responses to their waiters, everything else
/// to the event stream. Ends when the child's stdout closes, which is also how
/// "the host died" is detected — every pending request is failed rather than
/// left to time out one by one.
async fn read_loop(
    inner: Arc<Inner>,
    stdout: tokio::process::ChildStdout,
    events: mpsc::UnboundedSender<HostEvent>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                warn!(%error, "plugin host stdout failed");
                break;
            }
        };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            warn!(line = %truncate(&line), "unparseable line from the plugin host");
            continue;
        };

        // A *negative* id is the host asking komo for something — the only
        // request it originates is a running program's tool call.
        if let Some(id) = message
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .filter(|id| *id < 0)
        {
            route_tool_call(&inner, id, &message).await;
            continue;
        }

        // A positive id is the response to something komo sent; anything else
        // is a notification.
        if let Some(id) = message.get("id").and_then(serde_json::Value::as_i64) {
            let Some(waiter) = inner.pending.lock().await.remove(&id) else {
                debug!(id, "response for an unknown request id");
                continue;
            };
            let answer = match message.get("error") {
                Some(error) => Err(PyHostError::Plugin(
                    error
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("the plugin failed")
                        .to_string(),
                )),
                None => Ok(message.get("result").cloned().unwrap_or_default()),
            };
            let _ = waiter.send(answer);
            continue;
        }

        match message.get("method").and_then(serde_json::Value::as_str) {
            Some("manifest/changed") => {
                let tools = message
                    .get("params")
                    .and_then(|p| p.get("tools"))
                    .cloned()
                    .unwrap_or_default();
                match serde_json::from_value::<Vec<PluginToolDef>>(tools) {
                    Ok(tools) => {
                        let _ = events.send(HostEvent::ManifestChanged(HostManifest { tools }));
                    }
                    Err(error) => warn!(%error, "malformed manifest from the plugin host"),
                }
            }
            Some("log") => {
                let params = message.get("params");
                let text = params
                    .and_then(|p| p.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let level = params
                    .and_then(|p| p.get("level"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("info");
                match level {
                    "warn" | "error" => warn!(target: "pyhost", "{text}"),
                    _ => debug!(target: "pyhost", "{text}"),
                }
            }
            other => debug!(?other, "ignoring an unknown notification"),
        }
    }

    // The host is gone. Fail everyone still waiting — a request that will never
    // be answered should say so now, not in two minutes.
    inner.code_runs.lock().await.clear();
    let waiting: Vec<_> = inner.pending.lock().await.drain().collect();
    for (_, waiter) in waiting {
        let _ = waiter.send(Err(PyHostError::Unavailable(
            "the plugin host exited".into(),
        )));
    }
    let status = match inner.child.lock().await.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(status)) => status.to_string(),
            _ => "stdout closed".to_string(),
        },
        None => "shut down".to_string(),
    };
    let _ = events.send(HostEvent::Exited { status });
}

/// Hand a tool call back to the request that owns it.
///
/// A call whose request is unknown is answered rather than dropped: the python
/// side is blocked on it, and a plugin hung forever is worse than one told its
/// call went nowhere.
async fn route_tool_call(inner: &Arc<Inner>, id: i64, message: &serde_json::Value) {
    let params = message.get("params");
    let run = params
        .and_then(|p| p.get("run"))
        .and_then(serde_json::Value::as_i64);
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = params
        .and_then(|p| p.get("args"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let sender = match run {
        Some(run) => inner.code_runs.lock().await.get(&run).cloned(),
        None => None,
    };
    match sender {
        Some(sender) => {
            let _ = sender.send(ToolRequest { id, name, args });
        }
        None => {
            warn!(id, ?run, tool = %name, "tool call from an unknown request");
            let _ = inner.outbound.send(
                serde_json::json!({
                    "id": id,
                    "error": { "message": "this request is no longer active" },
                })
                .to_string(),
            );
        }
    }
}

fn truncate(line: &str) -> String {
    line.chars().take(200).collect()
}

#[cfg(test)]
mod tests;
