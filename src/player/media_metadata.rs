pub(crate) fn format_video_size(bytes: u64) -> String {
    let mut size = bytes as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if size < 1024.0 || unit == "TiB" {
            return format!("{size:.2} {unit}");
        }
        size /= 1024.0;
    }
    unreachable!()
}

pub(crate) fn premiere_day(value: &str) -> Option<&str> {
    // Preserve the calendar day supplied by Emby without shifting it by timezone.
    let day = value.trim().get(..10)?;
    let bytes = day.as_bytes();
    if !bytes.iter().enumerate().all(|(index, byte)| {
        if matches!(index, 4 | 7) {
            *byte == b'-'
        } else {
            byte.is_ascii_digit()
        }
    }) {
        return None;
    }
    let year = day[..4].parse::<u32>().ok()?;
    let month = day[5..7].parse::<u32>().ok()?;
    let date = day[8..].parse::<u32>().ok()?;
    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month = match month {
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    (year > 0 && (1..=days_in_month).contains(&date)).then_some(day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_size_uses_binary_units_and_two_decimal_places() {
        for (bytes, expected) in [
            (1_024, "1.00 KiB"),
            (1_310_720, "1.25 MiB"),
            (1_073_741_824, "1.00 GiB"),
            (13_249_974_108, "12.34 GiB"),
            (1_099_511_627_776, "1.00 TiB"),
        ] {
            assert_eq!(format_video_size(bytes), expected);
        }
    }

    #[test]
    fn premiere_dates_keep_the_calendar_day_and_skip_invalid_values() {
        for (value, expected) in [
            ("1998-04-03T00:00:00.0000000Z", Some("1998-04-03")),
            ("2024-02-29T23:30:00-08:00", Some("2024-02-29")),
            (" 2026-09-17 ", Some("2026-09-17")),
            ("", None),
            ("上映日期未知", None),
            ("2023-02-29T00:00:00Z", None),
            ("2026-13-01", None),
            ("2026-04-31", None),
        ] {
            assert_eq!(premiere_day(value), expected);
        }
    }
}
