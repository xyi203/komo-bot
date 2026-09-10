use komo_bot::daemon::{DreamSweep, ReviewSweep, RoutineEventSource, Schedule, WakeupWiring};
use komo_bot::gateway::{Channel, Gateway, MaintenanceService};
use komo_bot::interaction::{GatewayDispatcher, TurnWaker, WaitParts};
use komo_infra::persistence::db::Db;
use std::collections::HashMap;
use std::sync::Arc;

use komo_bot::notify::Notifier;
use komo_core::domain::{
    context::SessionOrigin,
    cron::CronJobRepository,
    gateway::MessageHandler,
    home::HomeRepository,
    pairing::PairingRepository,
    repository::SessionEventRepository,
    repository::SessionRepository,
    run::RunRepository,
    todo::SessionTodoRepository,
    wakeup::{WakeupDispatch, WakeupRepository},
};

use crate::{
    cli::wiring,
    infra::messaging::{
        api::ApiChannel,
        feishu::{FeishuChannel, FeishuSender},
        home_notifier::{HomeNotifier, TextSender},
        telegram::{TelegramChannel, TelegramSender},
        wechat::{WeChatChannel, WeChatQrLogin, WeChatSender, build_bot},
    },
    services::operator_control::actions::OperatorActions,
};
use komo_config::{ConfigSnapshot, IssueSeverity};

/// Run the always-on gateway: a persistent process hosting the maintenance
/// scheduler and the config-declared ingress channels. Runs until Ctrl-C.
/// Everything is read from the caller's one resolved `config` snapshot.
///
/// A channel mounts iff its `[channels.<name>]` table is enabled and
/// credentialed; `validate_gateway` has already made an enabled-but-
/// misconfigured one fatal, so a `ready()` miss here means "not configured".
/// Declaration order is the `home_chat` fallback priority (feishu first).
pub async fn run(config: &ConfigSnapshot) -> anyhow::Result<()> {
    // The gateway hosts every surface, so any fatal config issue (unusable
    // model, enabled-but-credential-less channel) stops startup here, before
    // the db is opened. Warnings are logged and tolerated.
    config.validate_gateway()?;
    for issue in &config.report.issues {
        if issue.severity == IssueSeverity::Warning {
            tracing::warn!(path = issue.path, "{}", issue.message);
        }
    }
    let rt = &config.runtime;

    // A cron typo must not crash-loop the always-on gateway (same principle as
    // the missing-credential warnings above): the maintenance schedule degrades
    // to the built-in default cadence, an opt-in sweep (dream) is
    // disabled — each with a warning naming the bad expression. Parsed here,
    // once, so the startup banner and the sweeps can never disagree.
    let (review_schedule, schedule_expr) = schedule_or_default(&rt.maintenance_schedule);
    let (dream_schedule, dream_expr) =
        optional_schedule(rt.dream_schedule.as_deref(), "dream_schedule");

    let db = Arc::new(Db::connect(&rt.db_url).await?);
    // Reconcile runs left `Running` by a crashed earlier process (launchd
    // restarts the gateway): flip them to failed/"interrupted" so the ledger is
    // truthful. Best-effort — a reconciliation failure must not block startup.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    match RunRepository::reconcile_interrupted(&*db, now).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "reconciled interrupted runs on startup"),
        Err(error) => tracing::warn!(%error, "failed to reconcile interrupted runs"),
    }
    // The other half of the crash-residue check: a turn the log says is
    // suspended with nothing registered to wake it. Best-effort, like the
    // reconciliation above — a repair that fails must not block startup.
    let events: Arc<dyn SessionEventRepository> = db.clone();
    let wakeups: Arc<dyn WakeupRepository> = db.clone();
    match komo_bot::daemon::reregister_suspended_turns(
        &events,
        &wakeups,
        komo_bot::daemon::SUSPEND_RECHECK_SESSIONS,
        now,
    )
    .await
    {
        0 => {}
        n => tracing::info!(count = n, "re-registered suspended turns on startup"),
    }
    // Tasks and cron jobs are tables in the same database now (docs/adr/0004);
    // the sweeps still take them as their own repositories.
    let cron_jobs: Arc<dyn CronJobRepository> = db.clone();

    let mut wired = wiring::build(config, db.clone()).await?;
    // The pending-approval registry the wiring's `ChatApprover` writes into.
    // Shared with the dispatcher and the api channel so an answer — a chat
    // `/approve`, the desktop modal, the TUI's — resolves the parked wait.
    let approvals = wired.approvals.clone();

    // Expire stored tool outputs once, here. Not a `Maintenance` sweep on
    // purpose: the list of scheduled sweeps is long already, and a scratch file
    // living a few hours past its week costs nothing (the store also re-sweeps
    // at most hourly whenever it writes).
    match wired.output_store.sweep() {
        0 => {}
        n => tracing::info!(removed = n, "expired stored tool outputs"),
    }

    // ── Ingress channels ─────────────────────────────────────────────────────
    // Senders outside `allow_from` go through the pairing handshake; the
    // pairing store is shared with the `komo pair` CLI via the same db.
    let pairings: Arc<dyn PairingRepository> = db.clone();
    let mut chat_channels: Vec<Box<dyn Channel>> = Vec::new();
    let mut channel_names: Vec<String> = Vec::new();
    let mut senders: HashMap<String, Arc<dyn TextSender>> = HashMap::new();
    // `home_chat` candidates in declaration order — first wins.
    let mut home_candidates: Vec<String> = Vec::new();
    let mut wechat_login: Option<Arc<dyn komo_bot::interaction::WeChatLogin>> = None;

    if let Some(cfg) = rt.feishu.ready() {
        let sender = Arc::new(FeishuSender::new(
            cfg.app_id.clone(),
            cfg.app_secret.clone(),
        ));
        senders.insert("feishu".to_string(), sender.clone());
        if let Some(chat) = &cfg.home_chat {
            home_candidates.push(format!("feishu:{chat}"));
        }
        channel_names.push("feishu".to_string());
        chat_channels.push(Box::new(FeishuChannel::new(sender, cfg, pairings.clone())));
    }
    if let Some(cfg) = rt.telegram.ready() {
        let sender = Arc::new(TelegramSender::new(cfg.bot_token.clone()));
        senders.insert("telegram".to_string(), sender.clone());
        if let Some(chat) = &cfg.home_chat {
            home_candidates.push(format!("telegram:{chat}"));
        }
        channel_names.push("telegram".to_string());
        chat_channels.push(Box::new(TelegramChannel::new(
            sender,
            cfg,
            pairings.clone(),
        )));
    }
    if let Some(cfg) = rt.wechat.ready() {
        let cred_path = komo_config::wechat_cred_path();
        // One bot instance shared between the sender and the channel so the
        // channel's poll loop populates the context-token map the sender reads.
        let bot = build_bot(&cred_path);
        senders.insert(
            "wechat".to_string(),
            Arc::new(WeChatSender::new(bot.clone())),
        );
        if let Some(chat) = &cfg.home_chat {
            home_candidates.push(format!("wechat:{chat}"));
        }
        // Shared between the login coordinator (`/wechat login`) and the
        // channel: a successful login pulses this so the channel starts polling
        // without a restart.
        let ready = Arc::new(tokio::sync::Notify::new());
        let provisioning = Arc::new(std::sync::atomic::AtomicBool::new(false));
        wechat_login = Some(Arc::new(WeChatQrLogin::new(
            cred_path.clone(),
            ready.clone(),
            bot.clone(),
            provisioning.clone(),
        )));
        channel_names.push("wechat".to_string());
        chat_channels.push(Box::new(WeChatChannel::new(
            bot,
            cfg,
            cred_path,
            ready,
            provisioning,
            pairings.clone(),
        )));
    }

    // A single home notifier delivers all proactive output (routines, task
    // due notices, the shutdown notice). It resolves the home chat at
    // notify-time — a `/sethome` override (db) wins over the config `home_chat`
    // (plugin order preserves the feishu-first priority).
    let config_home = home_candidates.first().cloned();
    let home_repo: Arc<dyn HomeRepository> = db.clone();
    let notifier: Arc<dyn Notifier> = Arc::new(HomeNotifier::new(
        senders,
        home_repo.clone(),
        config_home.clone(),
    ));

    // Taken before the runtime moves into the handler: /health reports what the
    // main catalog has mounted, live.
    let tool_catalog = wired.runtime.tool_executor.catalog().clone();
    let handler: Arc<dyn MessageHandler> = Arc::new(wired.runtime);
    let sessions: Arc<dyn SessionRepository> = db.clone();
    let todos: Arc<dyn SessionTodoRepository> = db.clone();
    let dispatcher = Arc::new(
        GatewayDispatcher::new(
            handler.clone(),
            approvals.clone(),
            sessions,
            home_repo,
            todos,
            wechat_login,
            db.clone(),
            db.clone(),
        )
        // A woken turn comes back on the runtime it ran on. A routine that
        // stopped to ask is still a routine when it resumes: the conversation's
        // runtime would hand it a wider tool set, the user's memory library and
        // an approver that answers for a human who is not there — so its next
        // ungranted action would come back refused instead of stopping to ask.
        .with_runtime(SessionOrigin::Cron, wired.cron_runtime.clone())
        // Where a woken routine's *next* question goes — the sweep that
        // delivered the first one is long gone by then.
        .with_notifier(notifier.clone())
        // What lets it bring a suspended turn back — for a sweep's wake and for
        // an arriving `/approve` alike.
        .with_waits(WaitParts {
            runs: db.clone(),
            events: db.clone(),
            wakeups: db.clone(),
        }),
    );

    // Waking a suspended turn: the scheduler fires, this continues the turn.
    // Built here because it needs the dispatcher (for the session slot) and the
    // handler (for the continuation) — the two things only the gateway holds.
    let waker: Arc<dyn WakeupDispatch> = Arc::new(TurnWaker::new(dispatcher.clone()));
    // Everything a routine firing needs, built once and handed to the
    // every-minute sweep that is its only ingress.
    let routines = Arc::new(RoutineEventSource {
        jobs: cron_jobs.clone(),
        notifier: notifier.clone(),
        runtime: Some(wired.cron_runtime.clone()),
        // Standing waits ride the sweep's tick (docs/bot-runtime.md §3.3):
        // one scheduler for routines and for the turns waiting on an answer.
        wakeups: Some(WakeupWiring {
            registrations: db.clone(),
            events: db.clone(),
            dispatch: waker.clone(),
        }),
    });
    // The third crash-residue check, after the interrupted runs and the
    // suspended turns: a message claimed from a channel whose turn never started.
    // Before the channels serve, so a recovered turn holds its session slot
    // ahead of whatever arrives next.
    match dispatcher
        .recover_inbox(komo_bot::interaction::INBOX_RECOVERY_LIMIT)
        .await
    {
        0 => {}
        n => tracing::info!(count = n, "re-delivered inbound messages lost to a restart"),
    }

    // ── Scheduled sweeps ─────────────────────────────────────────────────────
    let mut gateway = Gateway::new(dispatcher.clone())
        .with_maintenance(MaintenanceService {
            name: "review".to_string(),
            schedule: review_schedule,
            maintenance: Arc::new(ReviewSweep {
                review: wired.review.clone(),
            }),
            alert: Some(notifier.clone()),
        })
        // Routines (`komo cron add`): one every-minute sweep reads the store
        // and executes the ones whose slot has come, so jobs added, removed or
        // toggled while the gateway runs take effect on the next tick — no
        // restart. The same tick fires the standing wakeups, through the
        // `RoutineEventSource` above.
        .with_maintenance(MaintenanceService {
            name: "cron-jobs".to_string(),
            schedule: Schedule::parse("* * * * *")?,
            maintenance: Arc::new(routines.sweep()),
            alert: Some(notifier.clone()),
        })
        // Config changes: the same every-minute tick, because an edit is worth
        // hearing about while the operator is still at the keyboard. It only
        // *reports* — the running process keeps the snapshot it booted with,
        // and `komo config reload` is what applies one.
        .with_maintenance(MaintenanceService {
            name: "config-watch".to_string(),
            schedule: Schedule::parse("* * * * *")?,
            maintenance: Arc::new(komo_bot::daemon::config_watch::ConfigWatchSweep::new(
                &config.runtime.home,
                notifier.clone(),
            )),
            alert: Some(notifier.clone()),
        });
    // Dreaming — mounted only when `dream_schedule` is in effect. Reads the
    // whole memory library, promotes well-supported candidates, and archives
    // cold and refuted ones.
    if let Some(schedule) = dream_schedule {
        gateway = gateway.with_maintenance(MaintenanceService {
            name: "dreaming".to_string(),
            schedule,
            maintenance: Arc::new(DreamSweep {
                memories: wired.memories.clone(),
            }),
            alert: Some(notifier.clone()),
        });
    }

    // Whether an interactive chat channel exists — gates the shutdown notice.
    // The api channel below is not one.
    let mut channels = channel_names;
    let has_chat_channel = !chat_channels.is_empty();
    for channel in chat_channels {
        gateway = gateway.add_channel(channel);
    }

    // For the startup banner.
    let cron_job_count = cron_jobs.list().await.map(|j| j.len()).unwrap_or(0);

    // HTTP API channel: serves the local dashboard UI and any OpenAI-compatible
    // client. It calls the handler directly (synchronous request/response), so
    // it needs the repositories rather than just the dispatcher. Always on: it
    // is how the local `komo` CLI reaches this gateway while we hold the
    // exclusive Turso db lock, so nothing may switch it off.
    // By default it is loopback-only on an ephemeral port (published in the
    // rendezvous file); `[channels.api] enabled = true` widens it to an external
    // bind/port for Open WebUI / the dashboard.
    let api = rt
        .api
        .ready()
        .ok_or_else(|| anyhow::anyhow!("api channel misconfigured"))?;
    {
        let enabled = {
            let mut names = channels.clone();
            names.push("api".to_string());
            names
        };
        // The operator use cases behind the /api/* routes — the same shared
        // definitions the CLI's direct adapter runs, here over the gateway's
        // repositories.
        let actions = Arc::new(OperatorActions {
            sessions: db.clone(),
            messages: db.clone(),
            events: db.clone(),
            todos: db.clone(),
            memories: wired.memories.clone(),
            runs: db.clone(),
            pairings: pairings.clone(),
            home: db.clone(),
            cron_jobs: cron_jobs.clone(),
            memory_query: Some(wired.memory_query.clone()),
            wiki: wired.wiki.take(),
        });
        gateway = gateway.add_channel(Box::new(ApiChannel::new(
            api,
            handler.clone(),
            dispatcher.clone(),
            actions,
            tool_catalog,
            enabled,
            config_home.clone(),
            crate::infra::messaging::api::ModelMenu::from_config(&rt.model),
            approvals.clone(),
            rt.home.join("workspaces"),
            std::env::current_dir().unwrap_or_else(|_| rt.home.clone()),
        )));
        channels.push("api".to_string());
    }

    // Send the offline notice on shutdown only when a chat channel exists; with
    // none, the home notifier would fall back to a macOS popup, which is noise
    // on a foreground Ctrl-C.
    if has_chat_channel {
        gateway = gateway.with_shutdown_notice(notifier);
    }

    let fmt_opt = |e: &Option<String>| {
        e.as_deref()
            .map(|e| format!("`{e}`"))
            .unwrap_or_else(|| "off".to_string())
    };
    println!(
        "Komo gateway — maintenance `{}`, dreaming {}, jobs: {}, channels: {}. Ctrl-C to stop.\n",
        schedule_expr,
        fmt_opt(&dream_expr),
        format!("{cron_job_count} in cron.db"),
        if channels.is_empty() {
            "none".to_string()
        } else {
            channels.join(", ")
        }
    );

    gateway.run(shutdown_signal()).await
}

/// Parse the maintenance cron, degrading a typo to the built-in default
/// cadence: an always-on gateway must not crash-loop over a config typo.
/// Returns the schedule plus the expression actually in effect (for display).
fn schedule_or_default(expr: &str) -> (Schedule, String) {
    match Schedule::parse(expr) {
        Ok(schedule) => (schedule, expr.to_string()),
        Err(error) => {
            tracing::warn!(%error, default = komo_config::DEFAULT_MAINTENANCE_SCHEDULE,
                "invalid maintenance schedule; falling back to the default");
            let default = komo_config::DEFAULT_MAINTENANCE_SCHEDULE;
            (
                Schedule::parse(default).expect("built-in default cron is valid"),
                default.to_string(),
            )
        }
    }
}

/// Parse an opt-in sweep's cron; a typo disables that sweep with a warning
/// (never the whole gateway). Returns the schedule plus the effective
/// expression (`None` = the sweep is off, for the startup banner).
fn optional_schedule(expr: Option<&str>, what: &str) -> (Option<Schedule>, Option<String>) {
    match expr {
        None => (None, None),
        Some(expr) => match Schedule::parse(expr) {
            Ok(schedule) => (Some(schedule), Some(expr.to_string())),
            Err(error) => {
                tracing::warn!(%error, config = what, "invalid schedule; sweep disabled");
                (None, None)
            }
        },
    }
}

/// Resolve when the process is asked to stop. Catches both Ctrl-C (SIGINT, the
/// foreground case) and SIGTERM — the signal `launchctl bootout` sends when
/// `komo gateway stop`/`restart` tears the job down. Without the SIGTERM arm
/// launchd would kill the process before the shutdown notice could be sent.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(error) => {
                tracing::warn!(%error, "failed to install SIGTERM handler; relying on Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_schedule_typo_degrades_to_default() {
        let (_, expr) = schedule_or_default("not a cron");
        assert_eq!(expr, komo_config::DEFAULT_MAINTENANCE_SCHEDULE);
        let (_, expr) = schedule_or_default("*/5 * * * *");
        assert_eq!(expr, "*/5 * * * *");
    }

    #[test]
    fn optional_schedule_typo_disables_the_sweep() {
        let (schedule, expr) = optional_schedule(Some("not a cron"), "dream_schedule");
        assert!(schedule.is_none());
        assert!(expr.is_none());
        let (schedule, expr) = optional_schedule(Some("0 3 * * *"), "dream_schedule");
        assert!(schedule.is_some());
        assert_eq!(expr.as_deref(), Some("0 3 * * *"));
        let (schedule, _) = optional_schedule(None, "dream_schedule");
        assert!(schedule.is_none());
    }
}
