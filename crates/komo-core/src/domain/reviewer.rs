use async_trait::async_trait;

use super::{episode::AssessedEpisode, session::Session};

pub const SELF_REVIEW_PROMPT: &str = r#"Review the completed session for durable self-improvement.

Classify insights by ownership:
- memory: user-disclosed facts, persona, identity, project state, or stable references.
  Write each as a declarative fact, not an instruction ("User prefers concise replies" ✓,
  "Always reply concisely" ✗). Prioritize what reduces future steering — a fact that keeps
  the user from having to correct or remind you again. If a fact will be stale within a
  week it does not belong in memory: never store task progress, session outcomes,
  completed-work logs, PR/issue numbers, or commit SHAs.
- commitment: an open loop the user took on or is waiting on — something they said they
  would do, need to follow up on, or are waiting for someone else to deliver. Record the
  obligation as a short actionable title, who it involves (waiting_on), and any deadline.
  Only durable obligations, never idle chatter or work already finished in this session.

Never write:
- environment dependency failures such as command not found, missing credentials, or
  missing packages;
- negative durable claims that a tool is broken;
- session-specific transient errors that a retry can fix;
- one-off task narratives rather than reusable behavior.
"#;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewOutcome {
    pub memories_written: Vec<String>,
    /// Ids of commitments captured into the task inbox this review.
    pub tasks_captured: Vec<String>,
}

#[async_trait]
pub trait Reviewer: Send + Sync {
    /// Extract durable learning from one session's finished, not-yet-learned
    /// turns.
    ///
    /// `episodes` — not the transcript — is the input, because a transcript
    /// holds only user and assistant *text*: it cannot say whether a command
    /// actually ran, what it returned, or whether the turn ended by delivering
    /// or by failing. An extractor reading text alone infers all of that from
    /// the agent's own claims about itself.
    ///
    /// `session` supplies identity and workspace for the aux call; its
    /// transcript is not read, so callers need not load one.
    async fn review(
        &self,
        session: &Session,
        episodes: &[AssessedEpisode],
    ) -> anyhow::Result<ReviewOutcome>;
}
