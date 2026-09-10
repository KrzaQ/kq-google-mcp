//! Reading an IANA time zone name, in the one place every entrance reads it.
//!
//! `PATCH /api/me` and `gmcp user set-timezone` both come through here, so the
//! portal and the terminal refuse the same name in the same words. The parsed
//! `Tz` is what reaches the database: a name the tz database does not know
//! never becomes a column value, and so never renders somebody's mail on the
//! wrong clock.

use chrono_tz::Tz;

/// A few names that work, for the refusal. Any IANA name is accepted; these
/// are only there so nobody has to guess at the spelling of the format.
const EXAMPLES: &str = "Europe/Warsaw, Europe/London, America/New_York or UTC";

pub fn parse(name: &str) -> Result<Tz, String> {
    let name = name.trim();
    name.parse()
        .map_err(|_| format!("{name:?} is not an IANA time zone. Write it as one of {EXAMPLES}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_read_or_refused_by_example() {
        assert_eq!(parse("Europe/Warsaw").unwrap(), chrono_tz::Europe::Warsaw);
        assert_eq!(parse("  UTC  ").unwrap(), chrono_tz::UTC);
        let error = parse("Europe/Warszawa").unwrap_err();
        assert!(error.contains("Europe/Warszawa"), "{error}");
        assert!(error.contains("Europe/Warsaw"), "{error}");
        // A zone abbreviation is not a zone: it says nothing about which rules
        // are in force in the winter.
        assert!(parse("CEST").is_err());
    }
}
