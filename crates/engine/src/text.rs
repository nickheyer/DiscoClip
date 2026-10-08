//! Words for counts and spans in the notes a job writes

use jiff::Timestamp;

/// A count with its noun, 1 file or 3 files
pub fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The whole seconds, minutes, hours or days between two moments
pub fn elapsed(from: Timestamp, to: Timestamp) -> String {
    let secs = (to.as_second() - from.as_second()).unsigned_abs() as usize;
    match secs {
        0..60 => count(secs, "second", "seconds"),
        60..3600 => count(secs / 60, "minute", "minutes"),
        3600..86_400 => count(secs / 3600, "hour", "hours"),
        _ => count(secs / 86_400, "day", "days"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_take_the_noun_of_their_number() {
        assert_eq!(count(0, "file", "files"), "0 files");
        assert_eq!(count(1, "file", "files"), "1 file");
        assert_eq!(count(2, "entry", "entries"), "2 entries");
    }

    #[test]
    fn spans_round_down_to_their_largest_unit() {
        let at = |s: i64| Timestamp::from_second(s).unwrap();
        assert_eq!(elapsed(at(0), at(45)), "45 seconds");
        assert_eq!(elapsed(at(0), at(179)), "2 minutes");
        assert_eq!(elapsed(at(0), at(3600 * 3 + 100)), "3 hours");
        assert_eq!(elapsed(at(0), at(86_400 * 2 + 5)), "2 days");
        assert_eq!(elapsed(at(60), at(0)), "1 minute");
    }
}
