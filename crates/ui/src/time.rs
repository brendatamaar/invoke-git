//! Dates as the history list shows them. Pure: the caller passes "now".

/// `YYYY-MM-DD` in UTC from Unix seconds (Howard Hinnant's civil-from-days).
pub fn format_date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `YYYY-MM-DD HH:MM +HH:MM`: the author's own local time and the offset it
/// was written with, as the commit detail shows it.
pub fn format_date_time(seconds: i64, offset: i32) -> String {
    let local = seconds.saturating_add(i64::from(offset));
    let day_seconds = local.rem_euclid(86_400);
    let (hours, minutes) = (day_seconds / 3_600, day_seconds % 3_600 / 60);
    let sign = if offset < 0 { '-' } else { '+' };
    let away = offset.unsigned_abs();
    format!(
        "{} {hours:02}:{minutes:02} {sign}{:02}:{:02}",
        format_date(local),
        away / 3_600,
        away % 3_600 / 60
    )
}

/// How long ago `then` was, at `now` (both Unix seconds): `just now`,
/// `12 min ago`, `3 h ago`, `Yesterday`, `4 days ago`, and a plain date from
/// a week on. A time in the future (clock skew) reads as `just now`. Numbers
/// over adjectives, as the design README asks.
pub fn relative(now: i64, then: i64) -> String {
    let age = now.saturating_sub(then);
    match age {
        i64::MIN..=59 => "just now".to_owned(),
        60..=3_599 => format!("{} min ago", age / 60),
        3_600..=86_399 => format!("{} h ago", age / 3_600),
        86_400..=172_799 => "Yesterday".to_owned(),
        172_800..=604_799 => format!("{} days ago", age / 86_400),
        _ => format_date(then),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn formats_known_dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(951_782_400), "2000-02-29");
        assert_eq!(format_date(1_795_123_524), "2026-11-19");
        assert_eq!(format_date(-86_400), "1969-12-31");
    }

    #[test]
    fn date_time_uses_the_authors_offset() {
        // 2024-03-05 03:20:30 UTC is 10:20 at +07:00, 22:50 the day before at -04:30.
        assert_eq!(
            format_date_time(1_709_608_830, 7 * 3_600),
            "2024-03-05 10:20 +07:00"
        );
        assert_eq!(
            format_date_time(1_709_608_830, 0),
            "2024-03-05 03:20 +00:00"
        );
        assert_eq!(
            format_date_time(1_709_608_830, -(4 * 3_600 + 1_800)),
            "2024-03-04 22:50 -04:30"
        );
    }
    #[test]
    fn reads_like_the_mockup() {
        let now = 1_000_000_000;
        assert_eq!(relative(now, now - 30), "just now");
        assert_eq!(relative(now, now - 12 * 60), "12 min ago");
        assert_eq!(relative(now, now - 3 * 3_600), "3 h ago");
        assert_eq!(relative(now, now - 30 * 3_600), "Yesterday");
        assert_eq!(relative(now, now - 4 * 86_400), "4 days ago");
        assert_eq!(
            relative(now, now - 30 * 86_400),
            format_date(now - 30 * 86_400)
        );
        assert_eq!(relative(now, now + 500), "just now");
    }

    proptest! {
        /// Whatever the times, the text is short and never panics.
        #[test]
        fn relative_never_panics(now in any::<i64>(), then in any::<i64>()) {
            let text = relative(now, then);
            prop_assert!(!text.is_empty());
        }

        /// A date text always has the shape `YYYY-MM-DD`.
        #[test]
        fn dates_have_a_fixed_shape(seconds in -4_000_000_000i64..4_000_000_000) {
            let text = format_date(seconds);
            prop_assert_eq!(text.len(), 10);
            prop_assert_eq!(&text[4..5], "-");
            prop_assert_eq!(&text[7..8], "-");
        }

        /// Later dates sort after earlier ones as text.
        #[test]
        fn dates_sort_like_time(a in -4_000_000_000i64..4_000_000_000, b in -4_000_000_000i64..4_000_000_000) {
            if a.div_euclid(86_400) < b.div_euclid(86_400) {
                prop_assert!(format_date(a) < format_date(b));
            }
        }
    }
}
