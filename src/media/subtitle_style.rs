//! Pure subtitle presentation preferences shared by settings and playback.
use serde::{Deserialize, Serialize};

/// Vertical position adjustment step, as a fraction of video height.
pub(crate) const SUBTITLE_VERTICAL_ADJUST_STEP_FRACTION: f32 = 0.01;
/// Clamp for persisted vertical position, expressed as a fraction of video height.
const MAX_VERTICAL_OFFSET_FRACTION: f32 = 1.0;
// Half of the adjustment step. Snapping this band to the default prevents
// non-grid values loaded from disk from oscillating around zero without ever
// restoring the default alignment.
const ZERO_OFFSET_SNAP_FRACTION: f32 = SUBTITLE_VERTICAL_ADJUST_STEP_FRACTION / 2.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleTextColor {
    Yellow,
    LightGray,
    #[default]
    #[serde(other)]
    White,
}

impl SubtitleTextColor {
    pub(crate) const ALL: [Self; 3] = [Self::White, Self::Yellow, Self::LightGray];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::White => "subtitle-color-white",
            Self::Yellow => "subtitle-color-yellow",
            Self::LightGray => "subtitle-color-light-gray",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::White => "白色",
            Self::Yellow => "黄色",
            Self::LightGray => "浅灰色",
        }
    }

    pub(crate) fn rgb_hex(self) -> u32 {
        match self {
            Self::White => 0xffffff,
            Self::Yellow => 0xffe066,
            Self::LightGray => 0xbdbdbd,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleOutline {
    #[default]
    Off,
    #[serde(other)]
    On,
}

impl SubtitleOutline {
    pub(crate) const ALL: [Self; 2] = [Self::Off, Self::On];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Off => "subtitle-outline-off",
            Self::On => "subtitle-outline-on",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Off => "关闭",
            Self::On => "开启",
        }
    }

    /// Outline width in logical pixels for the given subtitle scale.
    ///
    /// The width grows sublinearly so the stroke stays at roughly 5–7% of the
    /// em size across the 0.8×–2.0× presets: a constant width looks heavy on
    /// small subtitles and thin on large ones. Half-pixel steps keep the jump
    /// between presets smooth while staying cheap to blit.
    pub(crate) fn width_px(self, scale: f32) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::On => {
                let scale = if scale.is_finite() && scale > 0.0 {
                    scale
                } else {
                    1.0
                };
                let width = (4.0 * scale.sqrt()).round() * 0.5;
                width.clamp(1.5, 3.0)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleBackground {
    Subtle,
    Strong,
    #[default]
    #[serde(other)]
    Off,
}

impl SubtitleBackground {
    pub(crate) const ALL: [Self; 3] = [Self::Off, Self::Subtle, Self::Strong];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Off => "subtitle-background-off",
            Self::Subtle => "subtitle-background-subtle",
            Self::Strong => "subtitle-background-strong",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Off => "关闭",
            Self::Subtle => "半透明",
            Self::Strong => "深色",
        }
    }

    /// RGBA color for the text background box, if enabled.
    pub(crate) fn rgba_hex(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Subtle => Some(0x00000073),
            Self::Strong => Some(0x000000bf),
        }
    }
}

/// Subtitle appearance owned by the settings windows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleAppearance {
    #[serde(default)]
    pub(crate) text_color: SubtitleTextColor,
    #[serde(default)]
    pub(crate) outline: SubtitleOutline,
    #[serde(default)]
    pub(crate) background: SubtitleBackground,
}

/// Appearance preferences plus the playback-owned vertical position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubtitleStyle {
    #[serde(flatten)]
    pub(crate) appearance: SubtitleAppearance,
    /// Upward offset as a fraction of the video height. `None` keeps
    /// the player's default bottom alignment, including bitmap content lifting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) vertical_offset_fraction: Option<f32>,
}

impl SubtitleStyle {
    /// Clamp persisted values. A position that returns to zero is stored as
    /// `None` so the default alignment (not an explicit zero offset) is used.
    pub(crate) fn normalized(mut self) -> Self {
        self.vertical_offset_fraction =
            Self::normalize_vertical_offset(self.vertical_offset_fraction);
        self
    }

    /// Normalize a single position without touching appearance preferences.
    pub(crate) fn normalize_vertical_offset(value: Option<f32>) -> Option<f32> {
        value.and_then(|value| {
            if !value.is_finite() {
                return None;
            }
            let value = value.min(MAX_VERTICAL_OFFSET_FRACTION);
            (value > ZERO_OFFSET_SNAP_FRACTION).then_some(value)
        })
    }

    pub(crate) fn with_appearance(mut self, appearance: SubtitleAppearance) -> Self {
        self.appearance = appearance;
        self
    }

    /// Replace the vertical position, normalizing it like persisted values.
    pub(crate) fn with_vertical_offset(mut self, value: Option<f32>) -> Self {
        self.vertical_offset_fraction = Self::normalize_vertical_offset(value);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_preserve_the_existing_subtitle_appearance() {
        let style = SubtitleStyle::default();
        assert_eq!(style.appearance.text_color, SubtitleTextColor::White);
        assert_eq!(style.appearance.outline, SubtitleOutline::Off);
        assert_eq!(style.appearance.background, SubtitleBackground::Off);
        assert_eq!(style.vertical_offset_fraction, None);
        assert_eq!(style.appearance.text_color.rgb_hex(), 0xffffff);
        assert_eq!(style.appearance.outline.width_px(1.0), 0.0);
        assert_eq!(style.appearance.background.rgba_hex(), None);
    }

    #[test]
    fn subtitle_style_keeps_flat_json_fields() {
        let json = serde_json::json!({
            "text_color": "yellow",
            "outline": "on",
            "background": "strong",
            "vertical_offset_fraction": 0.25,
        });
        let style: SubtitleStyle = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(style).unwrap(), json);
        let without_position = serde_json::to_value(style.with_vertical_offset(None)).unwrap();
        assert!(without_position.get("vertical_offset_fraction").is_none());
    }

    #[test]
    fn appearance_helpers_keep_the_playback_position() {
        let style = SubtitleStyle {
            vertical_offset_fraction: Some(0.02),
            ..Default::default()
        };
        let appearance = SubtitleAppearance {
            text_color: SubtitleTextColor::Yellow,
            ..Default::default()
        };

        let updated = style.with_appearance(appearance);

        assert_eq!(updated.appearance, appearance);
        assert_eq!(updated.vertical_offset_fraction, Some(0.02));
    }

    #[test]
    fn normalization_discards_non_finite_and_zero_offsets() {
        for value in [
            Some(f32::NAN),
            Some(f32::INFINITY),
            Some(-9.0),
            Some(-0.29),
            Some(0.0),
            Some(0.0001),
            Some(0.004),
            Some(0.005),
        ] {
            let style = SubtitleStyle {
                vertical_offset_fraction: value,
                ..Default::default()
            }
            .normalized();
            assert_eq!(style.vertical_offset_fraction, None, "{value:?}");
        }
        let above_snap = SubtitleStyle {
            vertical_offset_fraction: Some(0.006),
            ..Default::default()
        }
        .normalized();
        assert_eq!(above_snap.vertical_offset_fraction, Some(0.006));
        let high = SubtitleStyle {
            vertical_offset_fraction: Some(9.0),
            ..Default::default()
        }
        .normalized();
        assert_eq!(
            high.vertical_offset_fraction,
            Some(MAX_VERTICAL_OFFSET_FRACTION)
        );
    }

    #[test]
    fn older_settings_without_style_deserialize_to_defaults() {
        let style: SubtitleStyle = serde_json::from_str("{}").unwrap();
        assert_eq!(style, SubtitleStyle::default());
    }

    #[test]
    fn removed_warm_white_color_deserializes_to_white() {
        for value in ["warm_white", "magenta"] {
            let style: SubtitleStyle =
                serde_json::from_str(&format!(r#"{{"text_color":"{value}"}}"#)).unwrap();
            assert_eq!(
                style.appearance.text_color,
                SubtitleTextColor::White,
                "{value}"
            );
        }
    }

    #[test]
    fn unknown_background_deserializes_to_off() {
        for value in ["dim", "translucent"] {
            let style: SubtitleStyle =
                serde_json::from_str(&format!(r#"{{"background":"{value}"}}"#)).unwrap();
            assert_eq!(
                style.appearance.background,
                SubtitleBackground::Off,
                "{value}"
            );
        }
    }

    #[test]
    fn legacy_outline_presets_deserialize_to_on() {
        for value in ["thin", "medium", "thick"] {
            let style: SubtitleStyle =
                serde_json::from_str(&format!(r#"{{"outline":"{value}"}}"#)).unwrap();
            assert_eq!(style.appearance.outline, SubtitleOutline::On, "{value}");
        }
        let style: SubtitleStyle = serde_json::from_str(r#"{"outline":"on"}"#).unwrap();
        assert_eq!(style.appearance.outline, SubtitleOutline::On);
        let style: SubtitleStyle = serde_json::from_str(r#"{"outline":"off"}"#).unwrap();
        assert_eq!(style.appearance.outline, SubtitleOutline::Off);
    }

    #[test]
    fn presets_map_to_expected_colors() {
        assert_eq!(SubtitleTextColor::Yellow.rgb_hex(), 0xffe066);
        assert_eq!(SubtitleTextColor::LightGray.rgb_hex(), 0xbdbdbd);
        assert_eq!(SubtitleBackground::Subtle.rgba_hex(), Some(0x00000073));
        assert_eq!(SubtitleBackground::Strong.rgba_hex(), Some(0x000000bf));
        assert_eq!(SubtitleOutline::Off.width_px(1.0), 0.0);
        assert_eq!(SubtitleOutline::On.width_px(1.0), 2.0);
    }

    #[test]
    fn outline_width_grows_sublinearly_with_subtitle_scale() {
        let widths = [0.8, 1.0, 1.2, 1.5, 2.0].map(|scale| SubtitleOutline::On.width_px(scale));
        assert_eq!(widths, [2.0, 2.0, 2.0, 2.5, 3.0]);
        // The player clamps scale to 0.5×–3.0×.
        assert_eq!(SubtitleOutline::On.width_px(0.5), 1.5);
        assert_eq!(SubtitleOutline::On.width_px(0.25), 1.5);
        assert_eq!(SubtitleOutline::On.width_px(3.0), 3.0);
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0] {
            assert_eq!(SubtitleOutline::On.width_px(invalid), 2.0, "{invalid}");
        }
    }
}
