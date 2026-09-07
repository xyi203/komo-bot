use super::*;

fn at(unix: i64) -> Trigger {
    Trigger::At { at: unix }
}

#[test]
fn a_spent_one_shot_has_no_next_slot_but_a_cron_always_does() {
    let moment = 1_767_225_600;
    assert_eq!(at(moment).next_slot(moment - 1).unwrap(), Some(moment));
    assert!(at(moment).next_slot(moment).unwrap().is_none());
    assert!(
        Trigger::cron("0 8 * * *")
            .next_slot(moment)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_broken_expression_is_an_error_not_an_absent_slot() {
    assert!(Trigger::cron("not a cron").next_slot(0).is_err());
}

#[test]
fn triggers_roundtrip_through_json() {
    for trigger in [Trigger::cron("0 8 * * *"), Trigger::At { at: 42 }] {
        let json = serde_json::to_string(&trigger).unwrap();
        assert_eq!(serde_json::from_str::<Trigger>(&json).unwrap(), trigger);
    }
}

/// A stored trigger this build no longer understands is refused here, so
/// the load path can skip that job instead of guessing at what it meant.
#[test]
fn a_retired_trigger_shape_no_longer_deserializes() {
    for stored in [
        r#"{"kind":"webhook","name":"ci"}"#,
        r#"{"kind":"file_changed","root":"/srv/notes","glob":"**/*.md"}"#,
        r#"{"kind":"any","triggers":[{"kind":"cron","expr":"0 8 * * *"}]}"#,
    ] {
        assert!(serde_json::from_str::<Trigger>(stored).is_err(), "{stored}");
    }
}
