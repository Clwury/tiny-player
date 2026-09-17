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
}
