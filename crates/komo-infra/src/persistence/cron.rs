//! Scheduled jobs: `cron_job_records` in `komo.db`, and the
//! [`CronJobRepository`] over them.
//!
//! Jobs are **durable** — a job silently vanishing means its work silently
//! stops happening — so schema changes here are additive and the table is never
//! dropped to reset. That was a separate file (`cron.db`) until docs/adr/0004
//! made durability a table-level rule; this module keeps the model, the
//! queries, the in-place schema upkeep and the one-time import from the old
//! file, and `Db` owns the connection.

use std::path::Path;

use anyhow::Context;
use async_trait::async_trait;

use super::db::Db;
use crate::persistence::with_write_retry;
use komo_core::domain::cron::{
    CronAction, CronJob, CronJobRepository, RoutineRun, Trigger, once_moment_local, parse_catch_up,
    parse_cron_job_status, parse_notify_policy, parse_routine_run_status, schedule_is_once,
};
use komo_core::domain::policy::RuleSpec;

// Optional i64 fields use 0 as the "unset" sentinel; `args` is a JSON array
// string; `status` is "active"/"paused"/"done" (same conventions as the other
// stores).
#[derive(Debug, toasty::Model)]
pub(crate) struct CronJobRecord {
    #[key]
    id: String,
    #[index]
    name: String,
    /// JSON `Trigger` — what makes the routine fire.
    trigger: String,
    /// "command" | "agent" — discriminates the columns below.
    kind: String,
    // Command-mode columns (empty/0 for agent jobs).
    command: String,
    args: String,
    workdir: String,
    timeout_secs: i64,
    // Agent-mode columns (empty for command jobs).
    prompt: String,
    skills: String,
    status: String,
    /// "late" | "skip" — what to do with a missed slot. Additive column.
    catch_up: String,
    /// "always" | "on_error" | "never" — where a run's outcome is delivered.
    notify: String,
    next_run_at: i64,
    last_error: String,
    /// JSON array of `RoutineRun`, newest last, capped at
    /// `ROUTINE_RUN_HISTORY`. Empty string = never ran.
    runs: String,
    /// JSON array of `RuleSpec` — the actions this job may take unattended.
    /// Empty string = no grants (every job written before the column existed).
    grants: String,
    created_at: i64,
    // Retired columns, still written because the table is durable: dropping a
    // column is a non-additive change, and one declared NOT NULL without a
    // default fails every insert that stops mentioning it. `trigger` replaced
    // `schedule`, `runs` replaced the four `last_*` run fields; both were
    // read once by `backfill_triggers` and are dead from then on.
    schedule: String,
    last_run_at: i64,
    last_status: String,
    last_output: String,
    last_run_session: String,
}

/// Columns added to `cron_job_records` after a `komo.db` was created. Extend
/// this for every new [`CronJobRecord`] column: the table is durable, so it is
/// migrated in place and never dropped to be rebuilt.
const EXPECTED: &[(&str, &str)] = &[
    ("kind", "\"kind\" text NOT NULL DEFAULT 'command'"),
    ("prompt", "\"prompt\" text NOT NULL DEFAULT ''"),
    ("skills", "\"skills\" text NOT NULL DEFAULT ''"),
    ("grants", "\"grants\" text NOT NULL DEFAULT ''"),
    ("status", "\"status\" text NOT NULL DEFAULT 'active'"),
    ("catch_up", "\"catch_up\" text NOT NULL DEFAULT 'late'"),
    ("last_output", "\"last_output\" text NOT NULL DEFAULT ''"),
    (
        "last_run_session",
        "\"last_run_session\" text NOT NULL DEFAULT ''",
    ),
    ("trigger", "\"trigger\" text NOT NULL DEFAULT ''"),
    ("runs", "\"runs\" text NOT NULL DEFAULT ''"),
    ("notify", "\"notify\" text NOT NULL DEFAULT 'always'"),
];

/// Bring an existing file's `cron_job_records` up to the current column set,
/// before toasty opens it.
pub(crate) async fn ensure_schema(path: &Path) -> anyhow::Result<()> {
    crate::persistence::ensure_columns(path, "cron_job_records", EXPECTED).await?;
    migrate_enabled_to_status(path).await?;
    backfill_triggers(path).await
}

/// Every job in a legacy `cron.db`, for the one-time merge into `komo.db`.
///
/// The old file gets its own schema upkeep first: a `cron.db` written before
/// `status` existed still has `enabled`, and opening it with the current model
/// would fail on the columns it lacks.
pub(crate) async fn import_from(path: &Path) -> anyhow::Result<Vec<CronJob>> {
    ensure_schema(path).await?;
    let db = toasty::Db::builder()
        .models(toasty::models!(CronJobRecord))
        .connect(&format!("turso:{}", path.display()))
        .await
        .with_context(|| format!("opening {} to merge it in", path.display()))?;
    let mut conn = db.connection().await?;
    let rows = toasty::query!(CronJobRecord).exec(&mut conn).await?;
    rows.into_iter()
        .filter_map(|record| job_from_record(record).transpose())
        .collect()
}

/// One-time migration from the pre-status schema: `enabled` (0/1) becomes the
/// stored `status` ('active'/'paused'), and the old column is dropped so it
/// cannot fork from the new authority (and so inserts, which no longer supply
/// it, don't trip its NOT NULL). Idempotent: a db without `enabled` is a no-op.
/// Runs on a direct turso handle before toasty's pool connects, like
/// `ensure_columns`.
async fn migrate_enabled_to_status(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;

    let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
        .build()
        .await
        .with_context(|| format!("opening {} for status migration", path.display()))?;
    let conn = db.connect()?;
    conn.pragma_update("journal_mode", "'mvcc'").await.ok();

    let mut has_enabled = false;
    let mut rows = conn
        .query("PRAGMA table_info(\"cron_job_records\")", ())
        .await
        .context("reading cron_job_records columns")?;
    while let Some(row) = rows.next().await? {
        if let turso::Value::Text(name) = row.get_value(1)?
            && name == "enabled"
        {
            has_enabled = true;
        }
    }
    if !has_enabled {
        return Ok(());
    }
    conn.execute(
        "UPDATE \"cron_job_records\" SET \"status\" = \
         CASE WHEN \"enabled\" = 0 THEN 'paused' ELSE 'active' END",
        (),
    )
    .await
    .context("backfilling status from enabled")?;
    conn.execute(
        "ALTER TABLE \"cron_job_records\" DROP COLUMN \"enabled\"",
        (),
    )
    .await
    .context("dropping the legacy enabled column")?;
    tracing::info!("migrated cron.db: enabled column replaced by status");
    Ok(())
}

/// One-time repair of rows written before `Trigger` and `runs` existed: the old
/// `schedule` string becomes a stored trigger, and the four `last_*` fields
/// become the single run they described.
///
/// A **repair**, not a read-path fallback: the read path knows only the new
/// columns, so nothing downstream branches on which shape a row was written in.
/// Idempotent by construction — only rows whose `trigger` is still empty are
/// touched, and every row this writes gets a non-empty one. The retired columns
/// are left where they are: `cron_job_records` is durable, and dropping a column
/// is not an additive change.
async fn backfill_triggers(path: &Path) -> anyhow::Result<()> {
    let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
        .build()
        .await
        .with_context(|| format!("opening {} for the trigger backfill", path.display()))?;
    let conn = db.connect()?;
    conn.pragma_update("journal_mode", "'mvcc'").await.ok();

    let mut pending = Vec::new();
    let mut rows = match conn
        .query(
            "SELECT \"id\", \"schedule\", \"last_run_at\", \"last_status\", \"last_output\", \
             \"last_run_session\" FROM \"cron_job_records\" WHERE \"trigger\" = ''",
            (),
        )
        .await
    {
        Ok(rows) => rows,
        // No table yet: a brand-new file, which push_schema builds with the
        // current columns and nothing to repair.
        Err(_) => return Ok(()),
    };
    while let Some(row) = rows.next().await? {
        let text = |i: usize| -> anyhow::Result<String> {
            Ok(match row.get_value(i)? {
                turso::Value::Text(s) => s,
                _ => String::new(),
            })
        };
        let number = |i: usize| -> anyhow::Result<i64> {
            Ok(match row.get_value(i)? {
                turso::Value::Integer(n) => n,
                _ => 0,
            })
        };
        pending.push((text(0)?, text(1)?, number(2)?, text(3)?, text(4)?, text(5)?));
    }
    if pending.is_empty() {
        return Ok(());
    }

    for (id, schedule, last_run_at, last_status, last_output, last_run_session) in &pending {
        let trigger = trigger_from_schedule(schedule);
        let runs = match (*last_run_at != 0) || !last_status.is_empty() {
            true => vec![RoutineRun {
                id: uuid::Uuid::now_v7().to_string(),
                status: parse_routine_run_status(last_status),
                started_at: *last_run_at,
                session_id: (!last_run_session.is_empty()).then(|| last_run_session.clone()),
                output: last_output.clone(),
            }],
            false => Vec::new(),
        };
        conn.execute(
            "UPDATE \"cron_job_records\" SET \"trigger\" = ?, \"runs\" = ? WHERE \"id\" = ?",
            turso::params![
                serde_json::to_string(&trigger)?,
                encode_runs(&runs)?,
                id.clone()
            ],
        )
        .await
        .with_context(|| format!("backfilling trigger for cron job {id}"))?;
    }
    tracing::info!(
        jobs = pending.len(),
        "backfilled cron triggers and run history from the schedule/last_* columns"
    );
    Ok(())
}

/// The pre-`Trigger` schedule string as a trigger. `@at` resolves to the moment
/// it named — past included, since a spent one-shot still has to say what it
/// was — and anything else is the cron expression it always was; an expression
/// that no longer parses stays stored, and the sweep pauses the job with the
/// reason, exactly as it did before.
fn trigger_from_schedule(schedule: &str) -> Trigger {
    if schedule_is_once(schedule)
        && let Ok(at) = once_moment_local(schedule)
    {
        return Trigger::At { at };
    }
    Trigger::cron(schedule)
}

#[async_trait]
impl CronJobRepository for Db {
    async fn save(&self, job: &CronJob) -> anyhow::Result<()> {
        let cols = ActionColumns::from_action(&job.action)?;
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            toasty::create!(CronJobRecord {
                id: job.id.clone(),
                name: job.name.clone(),
                trigger: serde_json::to_string(&job.trigger)?,
                kind: job.action.kind().to_string(),
                command: cols.command.clone(),
                args: cols.args.clone(),
                workdir: cols.workdir.clone(),
                timeout_secs: cols.timeout_secs,
                prompt: cols.prompt.clone(),
                skills: cols.skills.clone(),
                status: job.status.as_str().to_string(),
                catch_up: job.catch_up.as_str().to_string(),
                notify: job.notify.as_str().to_string(),
                next_run_at: job.next_run_at,
                last_error: job.last_error.clone(),
                runs: encode_runs(&job.runs)?,
                created_at: job.created_at,
                grants: encode_grants(&job.grants)?,
                schedule: String::new(),
                last_run_at: 0,
                last_status: String::new(),
                last_output: String::new(),
                last_run_session: String::new(),
            })
            .exec(&mut conn)
            .await?;
            Ok(())
        })
        .await
    }

    async fn list(&self) -> anyhow::Result<Vec<CronJob>> {
        let mut conn = self.inner.connection().await?;
        let rows = toasty::query!(CronJobRecord).exec(&mut conn).await?;
        let mut jobs = rows
            .into_iter()
            .filter_map(|record| job_from_record(record).transpose())
            .collect::<anyhow::Result<Vec<_>>>()?;
        jobs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(jobs)
    }

    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<CronJob>> {
        let mut conn = self.inner.connection().await?;
        let rows = toasty::query!(CronJobRecord).exec(&mut conn).await?;
        for record in rows {
            if record.name == name {
                return job_from_record(record);
            }
        }
        Ok(None)
    }

    async fn update(&self, job: &CronJob) -> anyhow::Result<()> {
        let cols = ActionColumns::from_action(&job.action)?;
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let mut record = CronJobRecord::get_by_id(&mut conn, &job.id).await?;
            record
                .update()
                .name(job.name.clone())
                .trigger(serde_json::to_string(&job.trigger)?)
                .kind(job.action.kind().to_string())
                .command(cols.command.clone())
                .args(cols.args.clone())
                .workdir(cols.workdir.clone())
                .timeout_secs(cols.timeout_secs)
                .prompt(cols.prompt.clone())
                .skills(cols.skills.clone())
                .status(job.status.as_str().to_string())
                .catch_up(job.catch_up.as_str().to_string())
                .notify(job.notify.as_str().to_string())
                .next_run_at(job.next_run_at)
                .last_error(job.last_error.clone())
                .runs(encode_runs(&job.runs)?)
                .grants(encode_grants(&job.grants)?)
                .exec(&mut conn)
                .await?;
            Ok(())
        })
        .await
    }

    /// Deletes by row, not by job: a routine whose stored trigger no longer
    /// reads is exactly the one an operator wants gone, and it never becomes a
    /// `CronJob` to look an id up on.
    async fn delete(&self, name: &str) -> anyhow::Result<bool> {
        let mut conn = self.inner.connection().await?;
        let rows = toasty::query!(CronJobRecord).exec(&mut conn).await?;
        let Some(id) = rows
            .into_iter()
            .find(|record| record.name == name)
            .map(|record| record.id)
        else {
            return Ok(false);
        };
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let record = CronJobRecord::get_by_id(&mut conn, &id).await?;
            record.delete().exec(&mut conn).await?;
            Ok(())
        })
        .await?;
        Ok(true)
    }
}

/// The action fields flattened into record columns; the unused side stays
/// empty/zero. Keeps the enum → columns mapping in one place for save/update.
struct ActionColumns {
    command: String,
    args: String,
    workdir: String,
    timeout_secs: i64,
    prompt: String,
    skills: String,
}

impl ActionColumns {
    fn from_action(action: &CronAction) -> anyhow::Result<Self> {
        Ok(match action {
            CronAction::Command {
                command,
                args,
                workdir,
                timeout_secs,
            } => Self {
                command: command.clone(),
                args: serde_json::to_string(args)?,
                workdir: workdir.clone().unwrap_or_default(),
                timeout_secs: *timeout_secs as i64,
                prompt: String::new(),
                skills: String::new(),
            },
            // An agent job's workspace rides in the `workdir` column: the two
            // are the same question ("where does this job work?"), asked of a
            // process and of a turn, and cron.db is durable — a second column
            // for the same answer is a schema change nobody needs.
            CronAction::Agent {
                prompt,
                skills,
                workspace,
            } => Self {
                command: String::new(),
                args: String::new(),
                workdir: workspace.clone().unwrap_or_default(),
                timeout_secs: 0,
                prompt: prompt.clone(),
                skills: serde_json::to_string(skills)?,
            },
            // The delivered text rides in the `prompt` column for the same
            // reason a workspace rides in `workdir`: it is the job's one piece
            // of authored text, and the table is durable.
            CronAction::Message { text } => Self {
                command: String::new(),
                args: String::new(),
                workdir: String::new(),
                timeout_secs: 0,
                prompt: text.clone(),
                skills: String::new(),
            },
        })
    }
}

/// Grants as the column stores them. An empty list is written as `''` rather
/// than `'[]'` so a job without grants is byte-identical to a pre-column row.
fn encode_grants(grants: &[RuleSpec]) -> anyhow::Result<String> {
    if grants.is_empty() {
        return Ok(String::new());
    }
    Ok(serde_json::to_string(grants)?)
}

/// Same convention as grants: a routine that never ran writes `''`, not `'[]'`.
fn encode_runs(runs: &[RoutineRun]) -> anyhow::Result<String> {
    if runs.is_empty() {
        return Ok(String::new());
    }
    Ok(serde_json::to_string(runs)?)
}

/// One row as a job, or `None` when its stored trigger is a shape this build no
/// longer has — a routine that fired on an event rather than a clock. Skipped
/// and named, never guessed at: reading it as some clock trigger would make a
/// job run at a time nobody asked for, and failing the read would take every
/// other routine down with it.
fn job_from_record(record: CronJobRecord) -> anyhow::Result<Option<CronJob>> {
    let trigger: Trigger = match serde_json::from_str(&record.trigger) {
        Ok(trigger) => trigger,
        Err(error) => {
            tracing::warn!(
                job = %record.name,
                stored = %record.trigger,
                %error,
                "skipping a cron job whose trigger this build cannot read"
            );
            return Ok(None);
        }
    };
    // Default to command for legacy rows written before `kind` existed.
    let action = if record.kind == "agent" {
        CronAction::Agent {
            prompt: record.prompt,
            skills: serde_json::from_str(&record.skills).unwrap_or_default(),
            workspace: (!record.workdir.is_empty()).then_some(record.workdir),
        }
    } else if record.kind == "message" {
        CronAction::Message {
            text: record.prompt,
        }
    } else {
        CronAction::Command {
            command: record.command,
            args: serde_json::from_str(&record.args).unwrap_or_default(),
            workdir: (!record.workdir.is_empty()).then_some(record.workdir),
            timeout_secs: record.timeout_secs.max(0) as u64,
        }
    };
    Ok(Some(CronJob {
        id: record.id,
        name: record.name,
        trigger,
        action,
        status: parse_cron_job_status(&record.status),
        catch_up: parse_catch_up(&record.catch_up),
        notify: parse_notify_policy(&record.notify),
        next_run_at: record.next_run_at,
        last_error: record.last_error,
        runs: serde_json::from_str(&record.runs).unwrap_or_default(),
        created_at: record.created_at,
        // A row written before the column existed reads as empty, which is the
        // same thing as "no grants" — never an error.
        grants: serde_json::from_str(&record.grants).unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests;
