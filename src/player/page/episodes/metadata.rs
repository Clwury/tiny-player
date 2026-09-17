use crate::player::{PlaybackQueueItem, format_video_size};

use super::super::{format_playback_time, request::EMBY_TICKS_PER_SECOND};

pub(super) fn episode_metadata_label(
    item: &PlaybackQueueItem,
    size: Option<u64>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(day) = item.premiere_date.as_deref().and_then(premiere_day) {
        parts.push(day.to_string());
    }
    if let Some(ticks) = item.run_time_ticks.filter(|ticks| *ticks > 0) {
        parts.push(format_playback_time(
            ticks as f64 / EMBY_TICKS_PER_SECOND as f64,
        ));
    }
    if let Some(size) = size.filter(|size| *size > 0) {
        parts.push(format_video_size(size));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn premiere_day(value: &str) -> Option<&str> {
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
