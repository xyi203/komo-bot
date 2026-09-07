use super::*;
use chrono::{Datelike, TimeZone, Timelike};

#[test]
fn next_occurrence_in_rejects_invalid_expr() {
    let result = next_occurrence_in("not a cron", chrono::Utc::now());
    assert!(result.is_err());
}

#[test]
fn next_occurrence_in_computes_strictly_future_fire() {
    let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    let expr = "0 9 * * *"; // 9 AM daily

    // 8 AM local → next occurrence is 9 AM the same day
    let at_8am = tz.with_ymd_and_hms(2024, 1, 1, 8, 0, 0).unwrap();
    let next = next_occurrence_in(expr, at_8am).unwrap();
    assert_eq!(next.hour(), 9);
    assert_eq!(next.day(), 1);

    // exactly 9 AM local → next is 9 AM the following day (strictly future)
    let at_9am = tz.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap();
    let next = next_occurrence_in(expr, at_9am).unwrap();
    assert_eq!(next.hour(), 9);
    assert_eq!(next.day(), 2);
}

#[test]
fn at_schedule_fires_at_the_named_local_moment() {
    let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    let now = tz.with_ymd_and_hms(2024, 1, 1, 8, 0, 0).unwrap();
    let next = next_occurrence_in("@at 2024-01-02 09:30", now).unwrap();
    assert_eq!((next.day(), next.hour(), next.minute()), (2, 9, 30));
    assert_eq!(next.offset().local_minus_utc(), 8 * 3600, "local, not UTC");
}

#[test]
fn at_schedule_rejects_past_and_present_moments() {
    let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    let now = tz.with_ymd_and_hms(2024, 1, 2, 9, 30, 0).unwrap();
    // Exactly now counts as past — "strictly after", same as cron.
    let err = next_occurrence_in("@at 2024-01-02 09:30", now).unwrap_err();
    assert!(err.to_string().contains("already past"), "{err}");
    assert!(next_occurrence_in("@at 2023-12-31 09:30", now).is_err());
}

#[test]
fn at_schedule_rejects_malformed_times() {
    let now = chrono::Utc::now();
    for bad in ["@at tomorrow", "@at 2024-1-2", "@at 2024-01-02", "@at "] {
        assert!(
            next_occurrence_in(bad, now).is_err(),
            "{bad} must not parse"
        );
    }
}
