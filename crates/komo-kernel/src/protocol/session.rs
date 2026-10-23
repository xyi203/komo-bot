//! Pure wire types for session tree operations; no routes or execution state.

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
pub struct NavigateSessionRequest {
    pub target_id: String,
    #[serde(default)]
    pub summarize: bool,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateSessionResponse {
    pub leaf_id: Option<String>,
    pub editor_text: Option<String>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkSessionRequest {
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub clone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkSessionResponse {
    pub session: SessionId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditSessionContextRequest {
    pub target_id: String,
    #[serde(default)]
    pub replacement: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelSessionRequest {
    pub target_id: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameSessionRequest {
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_requests_use_camel_case_and_default_optional_operations() {
        let navigate: NavigateSessionRequest =
            serde_json::from_value(json!({"targetId": "a"})).unwrap();
        assert!(!navigate.summarize);
        assert_eq!(navigate.instructions, None);
        assert_eq!(navigate.label, None);
        assert_eq!(serde_json::to_value(navigate).unwrap()["targetId"], "a");
        let fork: ForkSessionRequest = serde_json::from_value(json!({})).unwrap();
        assert!(!fork.clone);
        assert_eq!(fork.target_id, None);
        let edit: EditSessionContextRequest =
            serde_json::from_value(json!({"targetId": "a"})).unwrap();
        assert_eq!(edit.replacement, None);
        let label: LabelSessionRequest = serde_json::from_value(json!({"targetId": "a"})).unwrap();
        assert_eq!(label.label, None);
    }

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
