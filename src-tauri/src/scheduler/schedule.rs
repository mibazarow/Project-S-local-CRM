// Time and recurrence rules (SPEC 5.3): the schedule's IANA timezone, the
// `run_at`/`notify` instant, the classic 5-field cron grid evaluated in that
// zone, and the short title derivation. Timestamps are RFC3339 UTC so SQLite
// string comparison orders them.
use crate::error::{AppError, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use std::str::FromStr;

pub const KINDS: [&str; 3] = ["run_at", "cron", "notify"];

// Short human title for a schedule: the first non-empty line of the prompt
// cut to `max_chars`, falling back to the spec when there is no prompt text.
// Shared by create() and the v23 backfill.
pub(crate) fn derive_title(prompt: Option<&str>, spec: &str, max_chars: usize) -> String {
    let line = prompt
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.is_empty() {
        return spec.trim().to_string();
    }
    line.chars().take(max_chars).collect::<String>().trim().to_string()
}

pub fn system_timezone_name() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into())
}

// NULL timezone = the machine's IANA zone, resolved once per call.
pub(super) fn resolve_timezone(tz: Option<&str>) -> Result<Tz> {
    let name = match tz.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => name.to_string(),
        None => system_timezone_name(),
    };
    Tz::from_str(&name).map_err(|_| AppError::Invalid(format!("unknown IANA timezone: {name}")))
}

pub(super) fn fmt_utc(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
pub fn parse_utc(text: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| AppError::Invalid(format!("invalid UTC timestamp {text:?}: {e}")))
}

// `run_at`/`notify` spec: RFC3339 (with offset or Z) or a naive
// "YYYY-MM-DDTHH:MM[:SS]" / "YYYY-MM-DD" interpreted in the schedule's zone.
fn parse_run_at(spec: &str, tz: Tz) -> Result<DateTime<Utc>> {
    let text = spec.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Ok(dt.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| NaiveDate::parse_from_str(text, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).unwrap()))
        .map_err(|_| {
            AppError::Invalid(format!(
                "invalid run_at {spec:?}: expected ISO datetime like 2026-10-20T10:00:00"
            ))
        })?;
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            Ok(dt.with_timezone(&Utc))
        }
        chrono::LocalResult::None => Err(AppError::Invalid(format!(
            "{spec:?} does not exist in timezone {tz} (DST gap)"
        ))),
    }
}

// The cron crate numbers days of week 1=Sunday..7=Saturday; classic 5-field
// cron is 0=Sunday,1=Monday..6=Saturday (7 also Sunday). Remap the numeric
// tokens of the DOW field; names, steps and lists pass through.
fn dow_to_crate_ordinal(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut digits = String::new();
    let flush = |out: &mut String, digits: &mut String| {
        if digits.is_empty() {
            return;
        }
        match digits.parse::<u32>() {
            // 0 and 7 are Sunday -> crate 1; 1..6 shift by one.
            Ok(n) if n <= 7 => out.push_str(&(n % 7 + 1).to_string()),
            _ => out.push_str(digits),
        }
        digits.clear();
    };
    for c in field.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            flush(&mut out, &mut digits);
            out.push(c);
        }
    }
    flush(&mut out, &mut digits);
    out
}

// The cron crate parses 6/7 fields (seconds first); SPEC 5.3 schedules use
// the classic 5-field form, so a leading "0 " seconds field is prepended.
// Shorthand (@daily, @hourly, …) passes through untouched.
fn parse_cron(expr: &str) -> Result<cron::Schedule> {
    let expr = expr.trim();
    if expr.starts_with('@') {
        return cron::Schedule::from_str(expr)
            .map_err(|e| AppError::Invalid(format!("invalid cron {expr:?}: {e}")));
    }
    let mut parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(AppError::Invalid(format!(
            "invalid 5-field cron {expr:?}: expected e.g. \"0 10 * * *\""
        )));
    }
    let dow = dow_to_crate_ordinal(parts[4]);
    parts[4] = &dow;
    let normalized = format!("0 {}", parts.join(" "));
    cron::Schedule::from_str(&normalized)
        .map_err(|e| AppError::Invalid(format!("invalid 5-field cron {expr:?}: {e}")))
}

// Next cron occurrence strictly after `base`, evaluated in the schedule's
// zone so DST transitions land on the right wall-clock instants.
pub(super) fn next_cron_after(expr: &str, tz: Tz, base: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let schedule = parse_cron(expr)?;
    let base_local = base.with_timezone(&tz);
    let next = schedule
        .after(&base_local)
        .next()
        .ok_or_else(|| AppError::Invalid(format!("cron {expr:?} has no occurrence after {base_local}")))?;
    Ok(next.with_timezone(&Utc))
}

// The next fire instant for an active schedule of the given kind, or None
// when it never fires again (done). Errors on an invalid spec.
pub(super) fn compute_next(kind: &str, spec: &str, tz: Tz, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    match kind {
        "run_at" | "notify" => Ok(Some(parse_run_at(spec, tz)?)),
        "cron" => Ok(Some(next_cron_after(spec, tz, now)?)),
        _ => Ok(None),
    }
}

pub(super) fn validate_kind(kind: &str) -> Result<()> {
    if KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "unknown schedule kind: {kind} (expected run_at|cron|notify)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::testutil::{moscow, utc};

    #[test]
    fn cron_next_occurrence_uses_the_fixed_timezone() {
        // 10:00 MSK = 07:00 UTC (Moscow has no DST).
        let next = next_cron_after("0 10 * * *", moscow(), utc("2026-10-16T08:00:00Z")).unwrap();
        assert_eq!(fmt_utc(next), "2026-10-17T07:00:00Z");
        // Strictly after the base instant, not the same minute.
        let next = next_cron_after("0 10 * * *", moscow(), utc("2026-10-16T07:00:00Z")).unwrap();
        assert_eq!(fmt_utc(next), "2026-10-17T07:00:00Z");
        // Weekday grid: Fri 16 Oct 2026 18:30 MSK = 15:30 UTC.
        let next = next_cron_after("30 18 * * 1-5", moscow(), utc("2026-10-16T00:00:00Z")).unwrap();
        assert_eq!(fmt_utc(next), "2026-10-16T15:30:00Z");
        // DST: 09:00 America/New_York; EDT (UTC-4) before the 1 Nov 2026
        // fall-back, EST (UTC-5) after it.
        let ny = Tz::from_str("America/New_York").unwrap();
        let next = next_cron_after("0 9 * * *", ny, utc("2026-10-30T12:00:00Z")).unwrap();
        assert_eq!(fmt_utc(next), "2026-10-30T13:00:00Z");
        let next = next_cron_after("0 9 * * *", ny, utc("2026-11-02T12:00:00Z")).unwrap();
        assert_eq!(fmt_utc(next), "2026-11-02T14:00:00Z");
        assert!(next_cron_after("bogus", moscow(), utc("2026-10-16T00:00:00Z")).is_err());
        // Exactly 5 fields (or @shorthand): the seconds-first form is rejected.
        assert!(next_cron_after("0 0 10 * * *", moscow(), utc("2026-10-16T00:00:00Z")).is_err());
        let next = next_cron_after("@daily", moscow(), utc("2026-10-16T00:00:00Z")).unwrap();
        // midnight MSK = 21:00 UTC the previous day
        assert_eq!(fmt_utc(next), "2026-10-16T21:00:00Z");
    }
}
