//! Pure wire types for session tree reads, fork and rename; no routes or execution state.

use serde::{Deserialize, Serialize};

use crate::session::SessionEntry;
use crate::types::ids::SessionId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeResponse {
    pub entries: Vec<SessionEntry>,
    pub leaf_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateSessionResponse {
    pub leaf_id: Option<String>,
    pub editor_text: Option<String>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ForkSessionResponse {
    pub session: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct NameSessionRequest {
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_responses_round_trip_through_public_reexports() {
        let response = crate::protocol::SessionTreeResponse {
            entries: Vec::new(),
            leaf_id: None,
        };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value, json!({"entries": [], "leafId": null}));
        assert_eq!(
            serde_json::from_value::<SessionTreeResponse>(value).unwrap(),
            response
        );
        let response = NavigateSessionResponse {
            leaf_id: Some("a".into()),
            editor_text: Some("edit me".into()),
            cancelled: false,
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "leafId": "a", "editorText": "edit me", "cancelled": false
            })
        );
        let response = ForkSessionResponse {
            session: SessionId::from_raw("fork"),
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({"session": "fork"})
        );
    }
}
