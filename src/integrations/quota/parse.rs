//! Parsing of the /usage page and RFC3339 timestamps for the quota service.
//! Moved out of quota.rs verbatim (std-split-quota); logic unchanged.

use super::*;

/// The two Gemini buckets parsed from one `/usage` page. Each is
/// `(remaining_fraction, refresh_in_minutes)`; the minutes are `None` when
/// the page shows no countdown for that bucket (a full bucket reads
/// "Quota available" instead).
#[derive(Debug, Default, Clone, Copy)]
pub struct UsageBuckets {
    pub weekly: Option<(f64, Option<i64>)>,
    pub five_hour: Option<(f64, Option<i64>)>,
}

/// Parse both Gemini buckets out of a rendered `/usage` page (ANSI already
/// stripped). The GEMINI MODELS section is cut out first so the Claude/GPT
/// group's identically-labelled buckets can never be picked up by mistake.
pub fn parse_usage(text: &str) -> UsageBuckets {
    let Some(start) = text.rfind("GEMINI MODELS") else {
        return UsageBuckets::default();
    };
    let rest = &text[start..];
    let end = rest
        .find("CLAUDE AND GPT MODELS")
        .unwrap_or_else(|| floor_boundary(rest, 4000));
    let section = &rest[..end];
    UsageBuckets {
        weekly: parse_bucket_at(section, "Weekly Limit"),
        five_hour: parse_bucket_at(section, "Five Hour Limit"),
    }
}

/// Parse the single bucket introduced by `anchor` inside the Gemini
/// section: the first percentage after the anchor is the remaining
/// fraction, and the first "Refreshes in" after it is the countdown.
pub(super) fn parse_bucket_at(section: &str, anchor: &str) -> Option<(f64, Option<i64>)> {
    let at = section.find(anchor)?;
    let window = &section[at..floor_boundary(section, at + 300)];
    let pct_at = window.find('%')?;
    let before = &window[..pct_at];
    let mut num_start = before.len();
    for (idx, c) in before.char_indices().rev() {
        if c.is_ascii_digit() || c == '.' {
            num_start = idx;
        } else {
            break;
        }
    }
    let percent: f64 = before[num_start..].parse().ok()?;
    let minutes = parse_refresh_minutes(window);
    Some((percent / 100.0, minutes))
}

/// The weekly bucket only; kept for the parser's original callers/tests.
pub fn parse_usage_weekly(text: &str) -> Option<(f64, Option<i64>)> {
    parse_usage(text).weekly
}

/// Parse a "Refreshes in 109h 11m"-style countdown into whole minutes.
/// Supports `Nd`, `Nh`, `Nm` components in any combination.
pub(super) fn parse_refresh_minutes(window: &str) -> Option<i64> {
    let at = window.find("Refreshes in")?;
    let start = at + "Refreshes in".len();
    let tail = &window[start..floor_boundary(window, start + 72)];
    let mut total: i64 = 0;
    let mut found = false;
    let mut num = String::new();
    for c in tail.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        if !num.is_empty() {
            let Ok(value) = num.parse::<i64>() else {
                return None;
            };
            match c {
                'd' => {
                    total += value * 1440;
                    found = true;
                }
                'h' => {
                    total += value * 60;
                    found = true;
                }
                'm' => {
                    total += value;
                    found = true;
                }
                _ => {}
            }
            num.clear();
        }
    }
    found.then_some(total)
}

/// Strip ANSI escape sequences (CSI, OSC, charset selects, stray escapes)
/// and normalise carriage returns, so TUI output can be parsed as text.
pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    i = (j + 1).min(bytes.len());
                }
                b']' => {
                    let mut j = i + 2;
                    while j < bytes.len() {
                        if bytes[j] == 0x07 {
                            j += 1;
                            break;
                        }
                        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                }
                b'(' | b')' | b'#' => {
                    i = (i + 3).min(bytes.len());
                }
                _ => {
                    i += 2;
                }
            }
        } else if b == b'\r' {
            out.push(b'\n');
            i += 1;
        } else if b == 0x08 {
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Largest char boundary `<= i` in `s` (defensive slicing for capped windows
/// over text containing multi-byte bar glyphs).
pub(super) fn floor_boundary(s: &str, i: usize) -> usize {
    let mut end = i.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

// ---------------------------------------------------------------------------
// Time helpers (no chrono in this crate; timestamps are unix seconds)

/// Format unix seconds as RFC 3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn format_rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Parse an RFC 3339 timestamp into unix seconds. Accepts `Z` or a numeric
/// offset, with optional fractional seconds.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let (date, time) = text.split_once('T')?;
    let mut dp = date.split('-');
    let year: i64 = dp.next()?.parse().ok()?;
    let month: i64 = dp.next()?.parse().ok()?;
    let day: i64 = dp.next()?.parse().ok()?;
    let (time, offset_secs) = if let Some(t) = time.strip_suffix('Z') {
        (t, 0)
    } else if let Some(idx) = time.rfind(['+', '-']) {
        if idx == 0 {
            return None;
        }
        let (t, off) = time.split_at(idx);
        let sign: i64 = if off.starts_with('+') { 1 } else { -1 };
        let mut op = off[1..].split(':');
        let oh: i64 = op.next()?.parse().ok()?;
        let om: i64 = op.next().and_then(|m| m.parse().ok()).unwrap_or(0);
        (t, sign * (oh * 3600 + om * 60))
    } else {
        (time, 0)
    };
    let mut tp = time.split(':');
    let hour: i64 = tp.next()?.parse().ok()?;
    let minute: i64 = tp.next()?.parse().ok()?;
    let second: i64 = tp
        .next()
        .map(|s| s.split('.').next().unwrap_or("0").parse().unwrap_or(0))
        .unwrap_or(0);
    Some(
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
            - offset_secs,
    )
}

/// Howard Hinnant's `civil_from_days`: days since the unix epoch to a
/// proleptic Gregorian `(year, month, day)`.
pub(super) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Inverse of [`civil_from_days`].
pub(super) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
