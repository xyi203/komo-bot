//! `python`: let the model write a Python program that calls komo's tools.
//!
//! ## Why a program beats another tool
//!
//! The model's usual way to combine tools is to call one, read the result, and
//! call the next — a model round-trip per step, with every intermediate result
//! spent as context. A program does the same work in one call: the loop, the
//! filtering and the glue run in the host, and only what the program returns
//! becomes context.
//!
//! It is also the answer to "how does the agent extend itself without paying
//! for it". Mounting a new tool changes the schema block, which invalidates the
//! provider's cached prompt prefix once (see `domain::catalog`). Writing a
//! function inside a program changes nothing at all: the catalog is untouched,
//! the prefix survives, and the composition costs a single call. Durable
//! composition still belongs in a plugin file; a one-off belongs here.
//!
//! ## The sub-call is a real call
//!
//! `tools.shell(command="…")` inside a program goes back through the *same*
//! [`ToolExecutor`] the model's own calls do — approval, policy, redaction,
//! the run ledger, the result cap. A program is a way to sequence komo's tools,
//! never a way around what gating them means. That is also why the executor
//! handle here is weak: `python` sits in the catalog it dispatches through.
//!
//! What such a call does *not* get from the round it arrives in is a name, so
//! [`sub_turn`] hands it one: the enclosing call's identity plus an ordinal in
//! program order (`code-1-read`, `code-2-cron`). The gate and the scratch store
//! both key on the call id, and two calls sharing one id share an approval.

use std::sync::Arc;

use async_trait::async_trait;
use komo_core::domain::catalog::CatalogSnapshot;
use komo_core::domain::context::ToolContext;
use komo_core::domain::llm::ToolCallReq;
use komo_core::domain::tool::{APPROVAL_BOUND, Tool, ToolError, ToolOutput, parse_args};
use komo_pyhost::{PyHostError, SharedHost, ToolAnswer};
use komo_services::tool_execution::{
    NestedCalls, SpinDetector, ToolTurnContext, TurnResultBudget, WeakToolExecutor,
};
use serde::Deserialize;
use serde_json::Value;

/// Tools python may not call, whatever the catalog says — from a program or
/// from a plugin's own `@tool` function, which composes them the same way.
///
/// `python` itself, because a program spawning a program buys nothing and
/// costs an unbounded recursion; `ask_user`, because it suspends the *turn* on
/// a human answer and a program is not a turn — the sentinel would resolve into
/// a mid-program value nobody is waiting for; `tool`, because it is an
/// indirection for a schema a *prompt* does not carry, and a program has the
/// whole catalog by name already (`sdk_note` lists every tool, held-back ones
/// included) — going through it would only add a hop.
pub(crate) const NOT_CALLABLE: &[&str] = &["python", "ask_user", "tool"];

/// What a sub-call answers with once the turn has stopped to wait.
///
/// Defined by the host crate, because the Python side has to recognize it:
/// `host.py` raises `ToolSuspended` — a `BaseException` — on this prefix, so a
/// program cannot catch its way past the stop. See [`komo_pyhost::SUSPENDED_MARKER`].
pub(crate) const SUSPENDED_MARKER: &str = komo_pyhost::SUSPENDED_MARKER;

/// What the model is told a suspended program did: nothing yet.
///
/// The executor drops this — the enclosing call is the suspended one, so it
/// records no step — but a tool still has to answer something.
pub(crate) const WAITING_NOTE: &str =
    "waiting — this program stopped part-way and runs again once the answer arrives";

/// Whether the turn stopped on **this** call: the enclosing `python` / `py__…`
/// call, which is where the executor lifts a sub-call's suspension to
/// (`RunContext::lift_suspension`).
pub(crate) fn stopped_to_wait(ctx: &ToolContext) -> bool {
    match (&ctx.run, ctx.call_id()) {
        (Some(run), Some(call_id)) => run.suspended_call(call_id),
        _ => false,
    }
}

/// Whether the turn stopped on the call **this nested round runs under** —
/// the same question as [`stopped_to_wait`], asked from inside the program.
///
/// A sub-call that stops is lifted onto the enclosing call by the executor
/// before `dispatch` reads this, so "our enclosing call is the suspended one"
/// is exactly "one of our sub-calls stopped". A wait held by a *sibling* of the
/// enclosing call — another tool in the model's own round — is not ours to act
/// on: the loop lets the rest of that round finish, this program included, so
/// stopping here would turn a neighbour's approval into this program's failure.
/// Without a nesting record (a detached context) any wait counts, since there
/// is no round to be a sibling in.
fn stopped_under(turn: &ToolTurnContext) -> bool {
    match (&turn.run, &turn.nested) {
        (Some(run), Some(nested)) => run.suspended_call(&nested.enclosing_call_id),
        (Some(run), None) => run.suspension().is_some(),
        _ => false,
    }
}

#[derive(Deserialize)]
struct Args {
    /// The program body. Runs as a function, so a top-level `return` answers.
    source: String,
}

pub struct PythonTool {
    /// The slot, not a handle: a restarted host is a different handle, and a
    /// tool registered at wiring outlives any one of them.
    host: SharedHost,
    /// Weak, because this tool is registered in the catalog the executor
    /// dispatches against — see the module docs.
    executor: WeakToolExecutor,
}

impl PythonTool {
    pub fn new(host: SharedHost, executor: WeakToolExecutor) -> Self {
        Self { host, executor }
    }
}

#[async_trait]
impl Tool for PythonTool {
    fn name(&self) -> &'static str {
        "python"
    }

    fn description(&self) -> &'static str {
        "Run a Python program that calls komo's other tools through a `tools` \
         object (`tools.read(path=\"a.txt\")`). `print(...)` reports progress, a \
         top-level `return` answers. Each call is gated exactly as a direct one."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "The program body, written as if inside a \
                                    function. A failed tool call raises \
                                    `ToolError` (`.tool`, `.message`).",
                }
            },
            "required": ["source"],
        })
    }

    /// The program is gated by what it calls, not by being a program: each
    /// `tools.x(...)` inside it goes through the executor and prompts exactly
    /// as a direct call to `x` would. Gating the program itself as well would
    /// ask the operator to approve something whose effects are not yet known.
    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: Args = parse_args(&input)?;
        let Some(host) = self.host.get() else {
            return Err(ToolError::Failed(anyhow::anyhow!(
                "the python plugin host is not running, so `python` cannot run \
                 a program; it restarts on its own — try again, or use the tools \
                 directly"
            )));
        };
        let Some(executor) = self.executor.upgrade() else {
            return Err(ToolError::Failed(anyhow::anyhow!(
                "the tool executor is gone; `python` cannot dispatch"
            )));
        };
        // The program's calls join *this* turn — see `sub_turn`.
        let turn = sub_turn(ctx);

        let callable = executor.snapshot();
        let outcome = host
            .run_code(&args.source, |name, args| {
                let executor = executor.clone();
                let turn = &turn;
                let callable = callable.clone();
                async move { dispatch(&executor, turn, &callable, name, args).await }
            })
            .await;

        // A sub-call that stopped to wait took the program with it, and the
        // wait is now *this* call's (`RunContext::lift_suspension`): the
        // executor records no step for it, keeps its scratch, and drops
        // whatever comes back. Answered before the outcome is read, because
        // how the program ended — an unwound `ToolSuspended`, a `return` — says
        // nothing about why, and "The program failed" is the wrong thing to
        // put in front of a model that is about to run it again.
        if stopped_to_wait(ctx) {
            return Ok(ToolOutput::text(WAITING_NOTE));
        }

        match outcome {
            Ok(result) => Ok(render(result)),
            Err(PyHostError::Unavailable(message)) => Err(ToolError::Failed(anyhow::anyhow!(
                "the plugin host is unavailable, so `python` did not run: {message}"
            ))),
            // A program that raised is a result the model rewrites from, not a
            // tool failure to retry — same call the executor makes for invalid
            // input.
            Err(PyHostError::Plugin(message)) => Ok(ToolOutput::text(format!(
                "The program failed:\n{}",
                message.trim_end()
            ))),
        }
    }

    /// A program can legitimately run for as long as the tools it calls do,
    /// approval prompts included.
    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(APPROVAL_BOUND)
    }

    /// A program is arbitrary composition; whether running it twice is safe is
    /// exactly what komo cannot know.
    fn idempotent(&self) -> bool {
        false
    }

    /// The program is the payload, and it can be long. The ledger keeps the
    /// first lines — enough to recognize what ran — rather than a whole file.
    fn redact_args(&self, args: &str) -> String {
        elide_source(args)
    }
}

/// Keep the head of a long program for the ledger.
///
/// The audit record wants to say what ran, not keep a copy: the first lines are
/// enough to recognize a program, and a whole file per call would dominate the
/// ledger. Cut on a char boundary — a program is often not ASCII.
fn elide_source(args: &str) -> String {
    const KEEP: usize = 2_000;
    if args.len() <= KEEP {
        return args.to_string();
    }
    let head: String = args.chars().take(KEEP).collect();
    let elided = args.len() - head.len();
    format!("{head}…[{elided} bytes elided]")
}

/// The turn a nested call joins: the caller's own session, run and approver,
/// so every `tools.x(...)` is audited individually and the turn's per-call
/// budget keeps counting across them — which is what bounds a runaway program.
///
/// The *output* budget is not shared, and deliberately: it exists to bound what
/// enters the model's context, and a sub-call's result enters the program, not
/// the context. Only what the program returns is paid for. Each result is still
/// capped individually by the executor, exactly as a direct call would be.
///
/// It also carries this call's own identity as the enclosing one, which is what
/// [`dispatch`] numbers the program's calls against — see [`NestedCalls`]. A
/// context with no call in it (a detached one, a test) numbers nothing: there
/// is no identity to hang the ordinals off, and an unnumbered id is what those
/// callers had before.
pub(crate) fn sub_turn(ctx: &ToolContext) -> ToolTurnContext {
    ToolTurnContext {
        session: ctx.session.clone(),
        run: ctx.run.clone(),
        budget: TurnResultBudget::new(0),
        spin: SpinDetector::default(),
        nested: match (ctx.call_id(), ctx.call_index()) {
            (Some(call_id), Some(call_index)) => {
                Some(Arc::new(NestedCalls::new(call_id, call_index)))
            }
            _ => None,
        },
    }
}

/// Run one tool call python made, through the executor.
///
/// Each call is named `code-<ordinal>-<tool>` off the turn's [`NestedCalls`]
/// counter, so a program calling one tool twice makes two calls the approval
/// gate and the scratch store can tell apart — and makes the *same* two on a
/// re-run, since the ordinals follow program order. A turn context with no
/// nesting (a detached caller) keeps the unnumbered `code-<tool>`.
///
/// Returns `Err(text)` for a call the caller should see as a failure — an
/// unknown or forbidden name, or a tool that errored. The host turns that into
/// a `ToolError` the python side may catch.
///
/// One `Err` is not a failure and must not be caught: a call that stopped the
/// turn answers with [`SUSPENDED_MARKER`], and no further call runs after it.
pub(crate) async fn dispatch(
    executor: &komo_services::tool_execution::ToolExecutor,
    turn: &ToolTurnContext,
    callable: &Arc<CatalogSnapshot>,
    name: String,
    args: Value,
) -> Result<ToolAnswer, String> {
    if NOT_CALLABLE.contains(&name.as_str()) {
        return Err(format!(
            "`{name}` cannot be called from a program; call it directly instead"
        ));
    }
    // Nothing more runs once *this program's* turn has stopped. A program that
    // caught the stop and carried on — it cannot, but the guard does not
    // depend on that — must not make effects the turn is already walking away
    // from. Scoped to the enclosing call on purpose: a sibling in the model's
    // round (a `shell` beside this `python`) may be the one waiting, and the
    // loop lets the rest of that round finish — this program included.
    if stopped_under(turn) {
        return Err(format!(
            "{SUSPENDED_MARKER}: the turn is waiting for an answer, so `{name}` was not run."
        ));
    }
    if callable.get(&name).is_none() {
        // Naming what *is* available beats a bare "unknown": the program was
        // written against a guess, and the fix is one edit away.
        return Err(format!(
            "no tool named `{name}`. Available: {}",
            callable
                .names()
                .filter(|n| !NOT_CALLABLE.contains(n))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // The call's index stays the executor's to derive — it is the position in
    // the round, and a nested round holds one call, so it is always 0. The
    // ordinal is carried in the id, which is what both the gate and the scratch
    // key on.
    let id = match &turn.nested {
        Some(nested) => format!("code-{}-{name}", nested.next_ordinal()),
        None => format!("code-{name}"),
    };
    let call = ToolCallReq {
        id,
        call_id: None,
        name,
        args: args.to_string(),
    };
    let mut outcomes = executor
        .execute_round(std::slice::from_ref(&call), turn)
        .await;
    // The round stopped the turn: an approval nobody has answered yet, or a
    // tool that asked to be woken. The executor has already lifted the wait
    // onto the enclosing call, so what is left is to stop the program — it is
    // re-run from its first line on the continuation, and everything it does
    // between here and the end of the turn is work nobody will read.
    if stopped_under(turn) {
        return Err(format!(
            "{SUSPENDED_MARKER}: `{}` is waiting for an answer. This program stops here and \
             runs again from its first line once the answer arrives.",
            call.name
        ));
    }
    let outcome = outcomes.pop();
    let content = outcome
        .as_ref()
        .map(|o| o.content.clone())
        .unwrap_or_default();
    // The executor answers a failure as content (the model is meant to recover
    // from it), so "did this work" has to be read off the text — the same
    // convention every other reader of an outcome uses.
    if content.starts_with("error:") || content.starts_with("tool `") {
        return Err(content);
    }
    Ok(ToolAnswer {
        content,
        structured: outcome.map(|o| o.structured).unwrap_or(Value::Null),
    })
}

/// Turn a finished program into the model's answer.
fn render(result: komo_pyhost::CodeResult) -> ToolOutput {
    let logs = result.logs.trim_end();
    let value = match &result.result {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => Some(other.to_string()),
    };
    let text = match (logs.is_empty(), value) {
        (true, None) => "(the program produced no output and returned nothing)".to_string(),
        (true, Some(value)) => value,
        (false, None) => logs.to_string(),
        (false, Some(value)) => format!("{logs}\n\n{value}"),
    };
    ToolOutput::text(text).with_structured(serde_json::json!({
        "returned": result.result,
        "printed_bytes": result.logs.len(),
    }))
}

/// The `tools` API text for the system prompt: one line per callable tool.
///
/// Rendered from the catalog, so it lists exactly what a program may call and
/// nothing else. Name-sorted like everything else the model is shown — an
/// unchanged tool set renders byte-identically, which is what keeps this text
/// from costing the provider's cached prefix on every turn.
pub fn sdk_note(catalog: &CatalogSnapshot) -> Option<String> {
    let mut lines: Vec<String> = catalog
        .tools()
        .filter(|tool| !NOT_CALLABLE.contains(&tool.name()))
        .map(|tool| {
            let args = argument_names(&tool.parameters_schema());
            format!("  tools.{}({})", tool.name(), args.join(", "))
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    lines.sort();
    Some(format!(
        "Inside `python`, these are callable as Python functions with keyword \
         arguments. Each is the same gated tool you can call directly, and each \
         returns the tool's output as a **str**; a failure raises \
         `ToolError`.\n{}\n\
         That str is the same text you would be shown — laid out for reading, \
         not for parsing. `read` returns a header line and then `N│text` \
         gutters; `grep` returns `Found N matches` and indented `Line N:` \
         entries. Do not compute on that layout. Every result also carries \
         `.structured`, the same result as data (`read` puts the page's own \
         lines in `.structured[\"text\"]`), which is `None` only for a tool that \
         reports no structured view — reach for a shell when that happens.",
        lines.join("\n")
    ))
}

/// Argument names from a tool's schema, required ones first and marked.
fn argument_names(schema: &Value) -> Vec<String> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut names: Vec<String> = properties
        .keys()
        .map(|name| {
            if required.contains(&name.as_str()) {
                name.clone()
            } else {
                // The model reads this as "optional", the same way a Python
                // signature with a default does.
                format!("{name}=None")
            }
        })
        .collect();
    // Required first, then alphabetical — a signature, not a set.
    names.sort_by_key(|name| (name.contains('='), name.clone()));
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::catalog::ToolCatalog;
    use komo_core::domain::tool::ToolOutput;

    struct Fake(&'static str, Value);

    #[async_trait]
    impl Tool for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "stand-in"
        }
        fn parameters_schema(&self) -> Value {
            self.1.clone()
        }
        async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn schema(required: &[&str], optional: &[&str]) -> Value {
        let mut properties = serde_json::Map::new();
        for name in required.iter().chain(optional) {
            properties.insert(name.to_string(), serde_json::json!({ "type": "string" }));
        }
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    }

    /// The note is a signature list, so the model can write a call without
    /// guessing: required arguments first, optional ones marked.
    #[test]
    fn the_sdk_note_renders_each_tool_as_a_python_signature() {
        let catalog = ToolCatalog::new();
        catalog.register(Arc::new(Fake("read", schema(&["path"], &["limit"]))));
        catalog.register(Arc::new(Fake("time", schema(&[], &[]))));

        let note = sdk_note(&catalog.snapshot()).expect("two tools is not empty");
        assert!(note.contains("tools.read(path, limit=None)"), "{note}");
        assert!(note.contains("tools.time()"), "{note}");
    }

    /// The return type is stated because a model that has to guess gets it
    /// wrong: the first real program written against this note called `.get()`
    /// on what is a `str`.
    #[test]
    fn the_note_states_what_a_tool_call_returns() {
        let catalog = ToolCatalog::new();
        catalog.register(Arc::new(Fake("read", schema(&["path"], &[]))));
        let note = sdk_note(&catalog.snapshot()).unwrap();
        assert!(note.contains("str"), "{note}");
        assert!(note.contains("ToolError"), "{note}");
    }

    /// Saying "str" was not enough: the first programs written against this note
    /// parsed `read`'s display page as data and computed line counts off the
    /// header and the gutter. The note has to say the text is laid out for
    /// reading, and name the channel that carries the result as data.
    #[test]
    fn the_note_warns_that_output_is_display_text() {
        let catalog = ToolCatalog::new();
        catalog.register(Arc::new(Fake("read", schema(&["path"], &[]))));
        let note = sdk_note(&catalog.snapshot()).unwrap();
        assert!(note.contains("N│text"), "{note}");
        assert!(note.contains(".structured"), "{note}");
    }

    /// Byte stability is the whole reason this is worth generating rather than
    /// hand-writing: the same tool set must render identically every turn, or
    /// the prompt prefix changes for nothing.
    #[test]
    fn the_same_tool_set_renders_byte_identically_whatever_the_order() {
        let first = ToolCatalog::new();
        first.register(Arc::new(Fake("read", schema(&["path"], &[]))));
        first.register(Arc::new(Fake("shell", schema(&["command"], &[]))));

        let second = ToolCatalog::new();
        second.register(Arc::new(Fake("shell", schema(&["command"], &[]))));
        second.register(Arc::new(Fake("read", schema(&["path"], &[]))));

        assert_eq!(sdk_note(&first.snapshot()), sdk_note(&second.snapshot()));
    }

    /// What a program may not call must not be advertised to it either.
    #[test]
    fn the_note_leaves_out_what_a_program_cannot_call() {
        let catalog = ToolCatalog::new();
        catalog.register(Arc::new(Fake("read", schema(&["path"], &[]))));
        catalog.register(Arc::new(Fake("python", schema(&["source"], &[]))));
        catalog.register(Arc::new(Fake("ask_user", schema(&["question"], &[]))));

        let note = sdk_note(&catalog.snapshot()).unwrap();
        // Only the listed signatures matter — the indented lines. The prose
        // around them names `python` itself (the thing being described) and
        // `tools.shell` (where to go for unformatted bytes).
        let listed: Vec<&str> = note.lines().filter(|l| l.starts_with("  tools.")).collect();
        assert_eq!(listed, vec!["  tools.read(path)"]);
    }

    /// An empty catalog has nothing to say — better no note than a heading over
    /// an empty list, which costs prompt bytes every turn to describe nothing.
    #[test]
    fn an_empty_catalog_produces_no_note() {
        let catalog = ToolCatalog::new();
        assert!(sdk_note(&catalog.snapshot()).is_none());
        catalog.register(Arc::new(Fake("python", schema(&["source"], &[]))));
        assert!(
            sdk_note(&catalog.snapshot()).is_none(),
            "a catalog with only uncallable tools is still nothing to say"
        );
    }

    fn code(logs: &str, result: Option<Value>) -> komo_pyhost::CodeResult {
        komo_pyhost::CodeResult {
            logs: logs.to_string(),
            result,
        }
    }

    /// Printed output and the returned value are different channels, and a
    /// program that used both must have both rendered.
    #[test]
    fn a_program_reports_what_it_printed_and_what_it_returned() {
        let out = render(code("step 1\nstep 2\n", Some(serde_json::json!("done"))));
        assert_eq!(out.text, "step 1\nstep 2\n\ndone");

        assert_eq!(render(code("", Some(serde_json::json!(42)))).text, "42");
        assert_eq!(render(code("just logs\n", None)).text, "just logs");
    }

    /// Silence is reported as silence: a program that printed nothing and
    /// returned nothing did run, and "nothing happened" is the honest answer.
    #[test]
    fn a_silent_program_says_so_rather_than_returning_an_empty_string() {
        let out = render(code("", None));
        assert!(out.text.contains("no output"), "{}", out.text);
    }

    /// A long program is recognizable in the ledger without storing the whole
    /// file — the audit record wants to say what ran, not keep a copy.
    #[test]
    fn a_long_program_is_elided_in_the_ledger() {
        let short = r#"{"source":"return 1"}"#;
        assert_eq!(elide_source(short), short);

        let long = format!(r#"{{"source":"{}"}}"#, "x".repeat(5_000));
        let redacted = elide_source(&long);
        assert!(redacted.len() < long.len());
        assert!(redacted.contains("bytes elided"), "{redacted}");
    }

    /// Cutting mid-character would make the ledger row unreadable — programs
    /// are routinely not ASCII.
    #[test]
    fn eliding_cuts_on_a_character_boundary() {
        let long = "读".repeat(3_000);
        let redacted = elide_source(&long);
        assert!(redacted.starts_with('读'));
        assert!(redacted.contains("bytes elided"));
    }

    /// A tool that keeps the call id the executor handed it: that id is what
    /// the approval gate and the scratch store key on, so it is the only thing
    /// that tells one of a program's calls from the next.
    struct Recording(&'static str, Arc<std::sync::Mutex<Vec<String>>>);

    #[async_trait]
    impl Tool for Recording {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "records the call id it ran under"
        }
        fn parameters_schema(&self) -> Value {
            schema(&[], &[])
        }
        async fn call(&self, _input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            self.1
                .lock()
                .unwrap()
                .push(ctx.call_id().unwrap_or_default().to_string());
            Ok(ToolOutput::text("ok"))
        }
    }

    /// An executor holding one [`Recording`] tool per name, and the list they
    /// all write their call ids to.
    fn recording_executor(
        names: &[&'static str],
    ) -> (
        komo_services::tool_execution::ToolExecutor,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut executor = komo_services::tool_execution::ToolExecutor::new(Default::default());
        for name in names {
            executor.register(Arc::new(Recording(name, seen.clone())));
        }
        (executor, seen)
    }

    /// The context a `python` call itself runs in — a real call identity, which
    /// is what `sub_turn` numbers its program's calls against.
    fn enclosing_ctx() -> ToolContext {
        ToolContext::new(
            komo_core::domain::context::SessionContext::detached("s"),
            Some(komo_core::domain::context::RunContext::new("run-1".into())),
            Arc::new(crate::test_support::AllowAll),
        )
        .with_call("call-0", 0)
    }

    async fn call_tool(
        executor: &komo_services::tool_execution::ToolExecutor,
        turn: &ToolTurnContext,
        name: &str,
    ) {
        dispatch(
            executor,
            turn,
            &executor.snapshot(),
            name.to_string(),
            serde_json::json!({}),
        )
        .await
        .expect("the fake tool succeeds");
    }

    /// Two calls from one program are two calls, and the ordinal is what says
    /// so: sharing an id would let the first one's approval answer the second.
    #[tokio::test]
    async fn nested_calls_are_numbered_in_program_order() {
        let (executor, seen) = recording_executor(&["read", "cron"]);
        let turn = sub_turn(&enclosing_ctx());

        call_tool(&executor, &turn, "read").await;
        call_tool(&executor, &turn, "cron").await;

        assert_eq!(*seen.lock().unwrap(), ["code-1-read", "code-2-cron"]);
    }

    /// The numbering has to be the *same* the second time round, because the
    /// second time round is what a suspended `python` call comes back as: the
    /// program reruns from its first line, and the answer recorded against
    /// `code-2-cron` has to still be that call's.
    #[tokio::test]
    async fn a_second_program_run_numbers_its_calls_the_same_way() {
        let (executor, seen) = recording_executor(&["read", "cron"]);
        let ctx = enclosing_ctx();

        let first = sub_turn(&ctx);
        call_tool(&executor, &first, "read").await;
        call_tool(&executor, &first, "cron").await;

        let second = sub_turn(&ctx);
        call_tool(&executor, &second, "read").await;
        call_tool(&executor, &second, "cron").await;

        assert_eq!(
            *seen.lock().unwrap(),
            ["code-1-read", "code-2-cron", "code-1-read", "code-2-cron"]
        );
    }

    /// A context nobody dispatched has no identity to number against, and keeps
    /// the unnumbered id it always had.
    #[tokio::test]
    async fn a_detached_context_keeps_the_unnumbered_id() {
        let (executor, seen) = recording_executor(&["read"]);
        let turn = sub_turn(&crate::test_support::approving_ctx("s"));
        assert!(turn.nested.is_none());

        call_tool(&executor, &turn, "read").await;

        assert_eq!(*seen.lock().unwrap(), ["code-read"]);
    }

    /// A tool that stops the turn instead of running — a sub-call whose
    /// approval nobody has answered yet.
    struct Waiting;

    #[async_trait]
    impl Tool for Waiting {
        fn name(&self) -> &'static str {
            "gated"
        }
        fn description(&self) -> &'static str {
            "stops the turn waiting for an approval"
        }
        fn parameters_schema(&self) -> Value {
            schema(&[], &[])
        }
        async fn call(&self, _input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            let _ = ctx.wait_for(
                komo_core::domain::session_event::Wakeup::Approval {
                    call_id: ctx.call_id().unwrap_or_default().to_string(),
                },
                "approve it",
                None,
            );
            Ok(ToolOutput::text("never read"))
        }
    }

    /// The turn stopped under the program, so the program stops too — and
    /// nothing it goes on to ask for runs. A second call landing after the stop
    /// would be an effect nobody is waiting for, made while komo is already
    /// ending the turn.
    #[tokio::test]
    async fn a_suspended_sub_call_stops_the_program() {
        let (executor, seen) = recording_executor(&["read"]);
        let mut executor = executor;
        executor.register(Arc::new(Waiting));
        let ctx = enclosing_ctx();
        let turn = sub_turn(&ctx);

        let Err(stopped) = dispatch(
            &executor,
            &turn,
            &executor.snapshot(),
            "gated".to_string(),
            serde_json::json!({}),
        )
        .await
        else {
            panic!("a call that stopped to wait is not an answer");
        };
        assert!(stopped.starts_with(SUSPENDED_MARKER), "{stopped}");

        // Lifted onto the `python` call the model actually asked for — the one
        // a continuation re-dispatches.
        let pending = ctx.run.as_ref().unwrap().suspension().unwrap();
        assert_eq!(pending.call_id, "call-0");

        let Err(refused) = dispatch(
            &executor,
            &turn,
            &executor.snapshot(),
            "read".to_string(),
            serde_json::json!({}),
        )
        .await
        else {
            panic!("nothing runs after the turn has stopped");
        };
        assert!(refused.starts_with(SUSPENDED_MARKER), "{refused}");
        assert!(
            seen.lock().unwrap().is_empty(),
            "and it was refused before it ran"
        );
    }

    #[test]
    fn the_model_facing_text_stays_short() {
        let executor = komo_services::tool_execution::ToolExecutor::new(Default::default());
        crate::test_support::assert_model_text_budget(&PythonTool::new(
            Default::default(),
            executor.downgrade(),
        ));
    }
}
