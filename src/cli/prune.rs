//! `gmcp prune`: the only thing that deletes anything on a schedule.
//!
//! Two kinds of row go: download links that can no longer be used, and audit
//! rows older than the retention. Connections, tokens and users are never
//! touched — the log is the answer to "why did the model draft that", so how
//! far back it reaches is a decision, spelled `--audit-older-than`.

use anyhow::Result;
use chrono::{Duration, Utc};

use crate::cli;
use crate::db::Db;

/// How the retention is written: a whole number of days, weeks or months.
/// A month is 30 days; nothing here needs a calendar, and a retention that
/// drifts with the length of February would be worse than one that does not.
const DAY: i64 = 1;
const WEEK: i64 = 7;
const MONTH: i64 = 30;

/// `30d`, `12w`, `6m`, or a bare number of days. Anything else is refused
/// with the forms that work, because a silently misread retention deletes the
/// wrong rows and there is no putting them back. Zero is one of the things
/// that does not parse: `0` and `0d` would empty the whole log, and nobody
/// types that meaning to.
pub fn parse_retention(s: &str) -> Result<Duration, String> {
    let text = s.trim().to_ascii_lowercase();
    let refuse = || {
        format!(
            "{s:?} is not a retention; write a number of days, weeks or months, \
             like 180d, 12w or 6m"
        )
    };
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let per_day = match &text[digits.len()..] {
        "" | "d" => DAY,
        "w" => WEEK,
        "m" => MONTH,
        _ => return Err(refuse()),
    };
    // Unsigned, so a negative retention — which would delete the future — is
    // one of the things that does not parse.
    let count: u64 = digits.parse().map_err(|_| refuse())?;
    if count == 0 {
        return Err(refuse());
    }
    i64::try_from(count)
        .ok()
        .and_then(|c| c.checked_mul(per_day))
        .and_then(Duration::try_days)
        .ok_or_else(|| format!("{s:?} is longer than anything this log will ever hold"))
}

pub async fn run(db: &Db, audit_retention: Duration) -> Result<()> {
    let now = Utc::now();
    let cutoff = now - audit_retention;
    let pruned = db.prune(now, audit_retention).await?;
    println!("{} expired download links deleted", pruned.links);
    println!(
        "{} audit rows deleted, everything before {}",
        pruned.audit,
        cli::day(cutoff)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days(s: &str) -> i64 {
        parse_retention(s).unwrap().num_days()
    }

    #[test]
    fn a_retention_is_days_weeks_or_months() {
        assert_eq!(days("30d"), 30);
        assert_eq!(days("12w"), 84);
        // Months are 30 days, so half a year is 180 whichever way it is
        // written; the default retention is exactly that.
        assert_eq!(days("6m"), 180);
        assert_eq!(days("180d"), 180);
        // A bare number is days, and spelling is not something to be strict
        // about at a terminal.
        assert_eq!(days("90"), 90);
        assert_eq!(days(" 4W "), 28);
    }

    #[test]
    fn anything_else_is_refused_with_the_forms_that_work() {
        // A retention of nothing keeps nothing: it would delete the whole
        // log, which is the one outcome no one asks for by typing a number.
        for bad in [
            "", "d", "-1d", "1.5d", "6 months", "6mo", "1y", "3h", "z", "0", "0d", "0w", "0m",
        ] {
            let error = parse_retention(bad).unwrap_err();
            assert!(error.contains("180d, 12w or 6m"), "{bad}: {error}");
        }
        // Long enough to overflow the multiplication, not long enough to be
        // silently wrong.
        let error = parse_retention(&format!("{}m", i64::MAX)).unwrap_err();
        assert!(error.contains("longer than"), "{error}");
    }
}
