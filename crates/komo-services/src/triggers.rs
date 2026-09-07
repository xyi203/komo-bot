//! An arrival → the standing wakes it fires (docs/bot-runtime.md §3.7, §5.12).
//!
//! The thin shell around [`komo_core::domain::trigger`]: that decides whether a
//! filter is about the thing that arrived, this reads the registrations, claims
//! each hit and wakes what it points at. Split that way because the deciding is
//! pure and the rest is the store — and because the routine side of an event
//! (`RoutineEventSource`) is the same shell over the same matcher, differing
//! only in what a hit turns into.
//!
//! Two ingresses reach it: every channel's inbound message, through
//! `GatewayDispatcher::handle`, and a named webhook, through the routine event
//! source. A feishu message deliberately arrives only by the first — routing it
//! by both would fire one wait twice.
//!
//! **The message keeps its own route.** A trigger never redirects it: whoever
//! wrote is talking to komo on their own conversation, and that turn happens as
//! it always did. What a hit adds is a *second* turn on the session that
//! registered the wait, saying what just arrived.
//!
//! **Claim before fire.** `take` answers `false` when the row is already gone,
//! so a message racing the expiry sweep wakes the turn once.

use std::sync::{Arc, RwLock};

use komo_core::domain::{
    session::ChannelPeer,
    session_event::WakeupCause,
    trigger::{InboundEvent, matching},
    wakeup::{WakeupDispatch, WakeupRegistration, WakeupRepository},
};
use tracing::{info, warn};

pub struct TriggerMatcher {
    wakeups: Arc<dyn WakeupRepository>,
    /// Whoever knows how to wake a turn. Attached after construction: it needs
    /// the dispatcher, and the dispatcher holds this. Absent ⇒ nothing fires.
    dispatch: RwLock<Option<Arc<dyn WakeupDispatch>>>,
}

impl TriggerMatcher {
    pub fn new(wakeups: Arc<dyn WakeupRepository>) -> Self {
        Self {
            wakeups,
            dispatch: RwLock::new(None),
        }
    }

    /// Install who wakes a turn. Called once, during gateway wiring.
    pub fn attach_dispatch(&self, dispatch: Arc<dyn WakeupDispatch>) {
        *self.dispatch.write().unwrap() = Some(dispatch);
    }

    /// One inbound message. Fires every standing wake it matches, and answers
    /// how many.
    ///
    /// Best-effort throughout: a trigger store that cannot be read must never
    /// keep the message itself from being answered.
    pub async fn on_inbound(&self, peer: &ChannelPeer, text: &str) -> usize {
        self.on_event(&InboundEvent::Message { peer, text }, text)
            .await
    }

    /// The same shell over anything a filter can be written about — a chat
    /// message, or (§5.12) a named webhook. `payload` is what a woken turn is
    /// handed: the message itself, or the event's account of what happened.
    ///
    /// Best-effort throughout: a trigger store that cannot be read must never
    /// keep the arrival itself from being answered.
    /// How many standing wakes this arrival matches, claiming and waking
    /// nothing. What an ingress that must answer before the work is done
    /// reports (docs/bot-runtime.md §5.12) — a read, so asking twice costs
    /// nothing and changes nothing.
    pub async fn count_matching(&self, event: &InboundEvent<'_>) -> usize {
        if self.dispatch.read().unwrap().is_none() {
            return 0;
        }
        match self.wakeups.list().await {
            Ok(rows) => matching(&rows, event).len(),
            Err(error) => {
                warn!(%error, "could not read standing wakes to count an event's matches");
                0
            }
        }
    }

    pub async fn on_event(&self, event: &InboundEvent<'_>, payload: &str) -> usize {
        let Some(dispatch) = self.dispatch.read().unwrap().clone() else {
            return 0;
        };
        let rows = match self.wakeups.list().await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(%error, "could not read standing wakes for an inbound event");
                return 0;
            }
        };
        let hits: Vec<WakeupRegistration> = matching(&rows, event).into_iter().cloned().collect();

        let mut fired = 0;
        for registration in hits {
            match self.wakeups.take(&registration.id).await {
                Ok(true) => {}
                // Somebody else claimed it — a sweep expiring it at this
                // instant. Theirs to report.
                Ok(false) => continue,
                Err(error) => {
                    warn!(%error, wake = %registration.id, "could not claim a triggered wake");
                    continue;
                }
            }
            match dispatch
                .fire(&registration, WakeupCause::Event, payload)
                .await
            {
                Ok(()) => {
                    fired += 1;
                    info!(
                        wake = %registration.id,
                        session = %registration.session_id,
                        "an event woke a standing wait"
                    );
                }
                Err(error) => warn!(%error, wake = %registration.id, "failed to fire a wake"),
            }
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use komo_core::domain::session_event::{EventFilter, Wakeup};
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryWakeups {
        rows: Mutex<Vec<WakeupRegistration>>,
    }

    #[async_trait]
    impl WakeupRepository for MemoryWakeups {
        async fn save(&self, registration: &WakeupRegistration) -> anyhow::Result<()> {
            self.rows.lock().unwrap().push(registration.clone());
            Ok(())
        }
        async fn list(&self) -> anyhow::Result<Vec<WakeupRegistration>> {
            Ok(self.rows.lock().unwrap().clone())
        }
        async fn take(&self, id: &str) -> anyhow::Result<bool> {
            let mut rows = self.rows.lock().unwrap();
            let before = rows.len();
            rows.retain(|r| r.id != id);
            Ok(rows.len() != before)
        }
        async fn take_for_turn(&self, _session: &str, _turn: &str) -> anyhow::Result<usize> {
            Ok(0)
        }
    }

    #[derive(Default)]
    struct RecordingDispatch {
        fired: Mutex<Vec<(String, WakeupCause, String)>>,
    }

    #[async_trait]
    impl WakeupDispatch for RecordingDispatch {
        async fn fire(
            &self,
            registration: &WakeupRegistration,
            cause: WakeupCause,
            payload: &str,
        ) -> anyhow::Result<()> {
            self.fired.lock().unwrap().push((
                registration.session_id.clone(),
                cause,
                payload.to_string(),
            ));
            Ok(())
        }
    }

    fn from_peer(platform: &str, peer_id: &str) -> Wakeup {
        Wakeup::Event {
            filter: EventFilter::FromPeer {
                platform: platform.into(),
                peer_id: peer_id.into(),
            },
        }
    }

    fn matcher(wakeups: &Arc<MemoryWakeups>) -> (TriggerMatcher, Arc<RecordingDispatch>) {
        let dispatch = Arc::new(RecordingDispatch::default());
        let matcher = TriggerMatcher::new(wakeups.clone());
        matcher.attach_dispatch(dispatch.clone());
        (matcher, dispatch)
    }

    /// The registration is the whole gate: with the wait retired, the same
    /// person writing again is just a message.
    #[tokio::test]
    async fn a_peer_nobody_is_waiting_on_fires_nothing() {
        let wakeups = Arc::new(MemoryWakeups::default());
        let (matcher, dispatch) = matcher(&wakeups);
        assert_eq!(
            matcher
                .on_inbound(&ChannelPeer::new("feishu", "ou_x"), "在么")
                .await,
            0
        );
        assert!(dispatch.fired.lock().unwrap().is_empty());
    }

    /// A turn parked on `wait { for_event }` is handed the message itself.
    #[tokio::test]
    async fn a_suspended_turn_is_handed_the_message_itself() {
        let wakeups = Arc::new(MemoryWakeups::default());
        wakeups
            .save(
                &WakeupRegistration::new("s1", from_peer("feishu", "ou_x"), 1_000).continuing("t1"),
            )
            .await
            .unwrap();

        let (matcher, dispatch) = matcher(&wakeups);
        matcher
            .on_inbound(&ChannelPeer::new("feishu", "ou_x"), "好了")
            .await;
        assert_eq!(dispatch.fired.lock().unwrap()[0].2, "好了");
    }
}
