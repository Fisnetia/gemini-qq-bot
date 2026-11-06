//! Tests moved out verbatim (std-split); assertions unchanged.

use super::*;

/// A `/usage` page as rendered by the 1.4.7 TUI (ANSI already stripped),
/// captured from a real account. The Gemini weekly bucket — 90.85%,
/// "Refreshes in 109h 11m" — is the value under test; the Claude section
/// exists to prove the parser does not read past its own group.
const USAGE_PAGE: &str = "\
└ Models & Quota

  Account: someone@example.com

GEMINI MODELS
  Models within this group: Gemini Flash, Gemini Pro

  Weekly Limit Remaining
[█████████████████████████████████████████████░░░░░] 90.85%
Refreshes in 109h 11m

  Five Hour Limit Remaining
[██████████████████████████████████████████████░░░░] 92.66%
Refreshes in 2h 43m


CLAUDE AND GPT MODELS
  Models within this group: Claude Opus, Claude Sonnet, GPT-OSS

  Weekly Limit Remaining
[██████████████████████████████████████████████████] 99.42%
Refreshes in 49h 12m

  Five Hour Limit Remaining
[██████████████████████████████████████████████████] 100.00%
Quota available
";

#[test]
fn usage_page_parses_both_buckets_from_one_page() {
    let buckets = parse_usage(USAGE_PAGE);
    let (weekly_fraction, weekly_minutes) = buckets.weekly.expect("weekly bucket");
    assert!((weekly_fraction - 0.9085).abs() < 1e-9);
    assert_eq!(weekly_minutes, Some(109 * 60 + 11));
    let (five_fraction, five_minutes) = buckets.five_hour.expect("five hour bucket");
    assert!((five_fraction - 0.9266).abs() < 1e-9);
    assert_eq!(five_minutes, Some(2 * 60 + 43));
}

#[test]
fn usage_page_never_reads_the_claude_group() {
    // The Claude/GPT section carries the same labels; with no Gemini
    // section at all, both buckets must stay empty rather than borrow
    // the other group's 99.42% / 100%.
    let buckets = parse_usage(
        "CLAUDE AND GPT MODELS\n  Weekly Limit Remaining\n[██] 99.42%\n  Five Hour Limit Remaining\n[██] 100.00%",
    );
    assert!(buckets.weekly.is_none());
    assert!(buckets.five_hour.is_none());
}

#[test]
fn usage_page_parses_sub_hour_refresh() {
    let page = USAGE_PAGE
        .replace("90.85%", "99.10%")
        .replace("Refreshes in 109h 11m", "Refreshes in 38m");
    let (fraction, minutes) = parse_usage_weekly(&page).expect("weekly bucket");
    assert!((fraction - 0.9910).abs() < 1e-9);
    assert_eq!(minutes, Some(38));
}

#[test]
fn usage_page_full_bucket_has_no_countdown() {
    let page = "\
GEMINI MODELS
  Models within this group: Gemini Flash, Gemini Pro

  Weekly Limit Remaining
[██████████████████████████████████████████████████] 100.00%
Quota available
";
    let (fraction, minutes) = parse_usage_weekly(page).expect("weekly bucket");
    assert!((fraction - 1.0).abs() < 1e-9);
    assert_eq!(minutes, None);
}

#[test]
fn usage_page_without_gemini_section_is_none() {
    assert!(parse_usage_weekly("no quota here").is_none());
    assert!(
        parse_usage_weekly("CLAUDE AND GPT MODELS\n  Weekly Limit Remaining\n[██] 99.42%")
            .is_none()
    );
}

#[test]
fn strip_ansi_removes_csi_osc_and_queries() {
    let raw = "\u{1b}[?2026h\u{1b}[1;31mRed\u{1b}[0m plain \u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{1b}\\ \u{1b}[>qdone\r\n";
    let stripped = strip_ansi(raw);
    assert!(stripped.contains("Red plain link"));
    assert!(stripped.contains("done\n"));
    assert!(!stripped.contains('\u{1b}'));
}

#[test]
fn rfc3339_format_known_values() {
    assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_rfc3339(1_793_032_596), "2026-10-26T16:36:36Z");
    assert_eq!(format_rfc3339(1_767_225_600), "2026-01-01T00:00:00Z");
}

#[test]
fn rfc3339_parse_known_values_and_offsets() {
    assert_eq!(parse_rfc3339("2026-10-26T16:36:36Z"), Some(1_793_032_596));
    assert_eq!(
        parse_rfc3339("2026-10-27T00:36:36+08:00"),
        Some(1_793_032_596)
    );
    assert_eq!(
        parse_rfc3339("2026-10-26T16:36:36.123Z"),
        Some(1_793_032_596)
    );
    assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339("not a date"), None);
    // Round trip across a leap day.
    let leap = parse_rfc3339("2028-02-29T12:00:00Z").expect("leap day");
    assert_eq!(format_rfc3339(leap), "2028-02-29T12:00:00Z");
}

#[test]
fn picks_tiered_flash_bucket() {
    let value = serde_json::json!({
        "models": {
            "gemini-3.8-flash-tiered": {"quotaInfo": {"remainingFraction": 0.9266, "resetTime": "2026-10-26T16:36:36Z"}},
            "claude-sonnet-4-6": {"quotaInfo": {"remainingFraction": 1.0}}
        },
        "tieredModelIds": {"flash": ["gemini-3.8-flash-tiered"]}
    });
    let bucket = parked_api::pick_gemini_bucket(&value).expect("bucket");
    assert!((bucket.remaining_fraction - 0.9266).abs() < 1e-9);
    assert_eq!(bucket.reset_time.as_deref(), Some("2026-10-26T16:36:36Z"));
}

#[test]
fn picks_bucket_via_tiered_pointer_then_any_gemini() {
    let via_pointer = serde_json::json!({
        "models": {
            "gemini-9-flash-x": {"quotaInfo": {"remainingFraction": 0.5, "resetTime": "2026-10-26T16:36:36Z"}}
        },
        "tieredModelIds": {"flash": ["gemini-9-flash-x"]}
    });
    assert!(
        (parked_api::pick_gemini_bucket(&via_pointer)
            .unwrap()
            .remaining_fraction
            - 0.5)
            .abs()
            < 1e-9
    );
    let bare = serde_json::json!({
        "models": {
            "gemini-2.5-pro": {"quotaInfo": {"remainingFraction": 0.25, "resetTime": "2026-10-26T16:36:36Z"}},
            "chat_1": {"quotaInfo": {"remainingFraction": 1.0}}
        }
    });
    assert!(
        (parked_api::pick_gemini_bucket(&bare)
            .unwrap()
            .remaining_fraction
            - 0.25)
            .abs()
            < 1e-9
    );
}

#[test]
fn scans_oauth_candidates_from_binary_bytes() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x7fELF junk ");
    bytes.extend_from_slice(b"1071006060591-abc123def456.apps.googleusercontent.com");
    bytes.extend_from_slice(b"\x00\x01noise");
    bytes.extend_from_slice(format!("GOCSPX-{}", "a".repeat(28)).as_bytes());
    bytes.extend_from_slice(b" separate ");
    // A candidate polluted by a '.' inside the 35-byte window is rejected.
    bytes.extend_from_slice(format!("GOCSPX-bad.{}", "b".repeat(24)).as_bytes());
    let (id, secrets) = parked_api::scan_oauth_candidates(&bytes).expect("candidates");
    assert_eq!(id, "1071006060591-abc123def456.apps.googleusercontent.com");
    assert_eq!(secrets, vec![format!("GOCSPX-{}", "a".repeat(28))]);
}

#[test]
fn scan_tolerates_in_charset_bytes_trailing_the_id() {
    // The real binary does not NUL-terminate the id: the in-charset run
    // continues well past the suffix (measured: 160 bytes), so requiring
    // the run to END with the suffix never matches in production.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"1071006060591-abc123def456.apps.googleusercontent.com");
    bytes.extend_from_slice(b".trailing-junk.more-junk:8080");
    bytes.extend_from_slice(b"\x00");
    bytes.extend_from_slice(format!("GOCSPX-{}", "a".repeat(28)).as_bytes());
    let (id, _) = parked_api::scan_oauth_candidates(&bytes).expect("candidates");
    assert_eq!(id, "1071006060591-abc123def456.apps.googleusercontent.com");
}

#[test]
fn scan_requires_the_consumer_id() {
    let bytes = format!("GOCSPX-{}", "a".repeat(28)).into_bytes();
    assert!(parked_api::scan_oauth_candidates(&bytes).is_none());
}

#[test]
fn refresh_due_truth_table() {
    use std::time::Duration;
    // Never attempted: always due.
    assert!(super::refresh_due(None, false));
    assert!(super::refresh_due(None, true));
    // After a failure the retry fuse is short.
    assert!(!super::refresh_due(Some(Duration::from_secs(119)), true));
    assert!(super::refresh_due(Some(Duration::from_secs(121)), true));
    // After a success the full TTL applies.
    assert!(!super::refresh_due(Some(Duration::from_secs(599)), false));
    assert!(super::refresh_due(Some(Duration::from_secs(601)), false));
}
