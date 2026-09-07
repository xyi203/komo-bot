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

use async_trait::async_trait;

use super::db::Db;
use crate::persistence::with_write_retry;
use komo_core::domain::cron::{
    CronAction, CronJob, CronJobRepository, RoutineRun, Trigger, parse_catch_up,
    parse_cron_job_status, parse_notify_policy,
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
    // `schedule`, `runs` replaced the four `last_*` run fields; nothing reads
    // them.
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
    crate::persistence::ensure_columns(path, "cron_job_records", EXPECTED).await
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
