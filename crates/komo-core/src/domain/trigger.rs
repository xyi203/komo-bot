//! What an arrival matches (docs/bot-runtime.md §3.7).
//!
//! Pure functions over an [`EventFilter`] and the thing that arrived, separated
//! from everything that has to happen afterwards — reading standing
//! registrations, claiming one, waking a turn — because "is this about that?"
//! is a question with a definite answer and no I/O.
//!
//! Matching is on the **address**, never on a name someone typed. `waiting_on:
//! "张三"` is for a human to read; a [`ChannelPeer`] is what an inbound message
//! can actually be compared with, and a task whose `waiting_on` never resolved
//! to one is simply not wakeable.

use super::session::ChannelPeer;
use super::session_event::EventFilter;
use super::wakeup::WakeupRegistration;

/// One thing that arrived, in the terms a filter is written in.
pub enum InboundEvent<'a> {
    /// A chat message, on whichever channel carried it.
    Message {
        peer: &'a ChannelPeer,
        text: &'a str,
    },
    /// A named inbound webhook (`POST /api/hooks/{name}`).
    Webhook { name: &'a str },
}

/// Whether this filter is about the thing that just arrived.
pub fn matches(filter: &EventFilter, event: &InboundEvent<'_>) -> bool {
    match (filter, event) {
        (EventFilter::FromPeer { platform, peer_id }, InboundEvent::Message { peer, .. }) => {
            platform == &peer.platform && peer_id == &peer.peer_id
        }
        (EventFilter::Webhook { name }, InboundEvent::Webhook { name: arrived }) => name == arrived,
        _ => false,
    }
}

/// Every standing registration this message fires, oldest first.
///
/// All of them, not the first: two commitments waiting on the same person are
/// two standing instructions, and one arriving message answers both.
pub fn matching<'a>(
    registrations: &'a [WakeupRegistration],
    event: &InboundEvent<'_>,
) -> Vec<&'a WakeupRegistration> {
    registrations
        .iter()
        .filter(|r| match &r.wakeup {
            super::session_event::Wakeup::Event { filter } => matches(filter, event),
            _ => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::session_event::Wakeup;

    fn from_peer(platform: &str, peer_id: &str) -> Wakeup {
        Wakeup::Event {
            filter: EventFilter::FromPeer {
                platform: platform.into(),
                peer_id: peer_id.into(),
            },
        }
    }

    #[test]
    fn a_peer_filter_matches_that_peer_and_nobody_else() {
        let peer = ChannelPeer::new("feishu", "ou_x");
        let event = InboundEvent::Message {
            peer: &peer,
            text: "在了",
        };
        assert!(matches(
            &EventFilter::FromPeer {
                platform: "feishu".into(),
                peer_id: "ou_x".into()
            },
            &event
        ));
        // Same id on another platform is another person.
        assert!(!matches(
            &EventFilter::FromPeer {
                platform: "telegram".into(),
                peer_id: "ou_x".into()
            },
            &event
        ));
        assert!(!matches(
            &EventFilter::Webhook { name: "ci".into() },
            &event
        ));
    }

    #[test]
    fn every_registration_watching_that_peer_fires() {
        let now = 1_000;
        let rows = vec![
            WakeupRegistration::new("s1", from_peer("feishu", "ou_x"), now),
            WakeupRegistration::new("s2", from_peer("feishu", "ou_y"), now),
            WakeupRegistration::new("s3", from_peer("feishu", "ou_x"), now),
            WakeupRegistration::new("s4", Wakeup::UserReply, now),
        ];
        let peer = ChannelPeer::new("feishu", "ou_x");
        let hits = matching(
            &rows,
            &InboundEvent::Message {
                peer: &peer,
                text: "ok",
            },
        );
        assert_eq!(
            hits.iter().map(|r| &r.session_id).collect::<Vec<_>>(),
            vec!["s1", "s3"]
        );
    }

    /// A turn parked on `wait { for_event: { webhook } }` is woken by the hook
    /// it named and by no other — the ingress cannot tell them apart, so this
    /// is the only thing that does.
    #[test]
    fn a_webhook_filter_matches_its_own_name_only() {
        let arrived = InboundEvent::Webhook { name: "ci-done" };
        assert!(matches(
            &EventFilter::Webhook {
                name: "ci-done".into()
            },
            &arrived
        ));
        assert!(!matches(
            &EventFilter::Webhook {
                name: "deploy".into()
            },
            &arrived
        ));
        assert!(!matches(
            &EventFilter::FromPeer {
                platform: "feishu".into(),
                peer_id: "ci-done".into()
            },
            &arrived
        ));
    }
}
