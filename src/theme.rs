use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{App, Global, Hsla, Pixels, hsla, px};
use serde::Deserialize;
use tracing::debug;

const DEFAULT_THEME_NAME: &str = "Catppuccin Mocha";
const DEFAULT_THEME_JSON: &str = include_str!("../themes/catppuccin.json");

pub struct TinyTheme {
    pub background: Hsla,
    pub foreground: Hsla,
    pub title_bar: Hsla,
    pub title_bar_border: Hsla,
    pub secondary_hover: Hsla,
    pub input_background: Hsla,
    pub input_border: Hsla,
    pub input_border_focused: Hsla,
    pub muted_foreground: Hsla,
    pub dialog_background: Hsla,
    pub overlay: Hsla,
    pub warning: Hsla,
    pub error: Hsla,
    pub radius_lg: Pixels,
}

impl Global for TinyTheme {}

pub fn init(cx: &mut App) {
    let theme = TinyTheme::from_theme_set_json(DEFAULT_THEME_JSON, DEFAULT_THEME_NAME)
        .unwrap_or_else(|error| {
            debug!(%error, "failed to load bundled theme, falling back to built-in Mocha colors");
            TinyTheme::catppuccin_mocha_fallback()
        });
    cx.set_global(theme);
}

pub fn get(cx: &App) -> &TinyTheme {
    cx.global::<TinyTheme>()
}

type ThemeColors = HashMap<String, Option<String>>;

#[derive(Debug, Deserialize)]
struct ThemeSet {
    themes: Vec<ThemeConfig>,
}

#[derive(Debug, Deserialize)]
struct ThemeConfig {
    name: String,
    #[serde(default, rename = "radius.lg")]
    radius_lg: Option<f32>,
    colors: ThemeColors,
}

impl TinyTheme {
    fn from_theme_set_json(json: &str, preferred_name: &str) -> Result<Self> {
        let theme_set: ThemeSet = serde_json::from_str(json).context("解析主题 JSON 失败")?;
        let config = theme_set
            .themes
            .iter()
            .find(|theme| theme.name == preferred_name)
            .or_else(|| theme_set.themes.first())
            .ok_or_else(|| anyhow!("主题 JSON 中没有可用主题"))?;

        Self::from_theme_config(config)
    }

    fn from_theme_config(config: &ThemeConfig) -> Result<Self> {
        let fallback = Self::catppuccin_mocha_fallback();

        Ok(Self {
            background: color_or_any(config, &["background"], fallback.background)?,
            foreground: color_or_any(config, &["foreground"], fallback.foreground)?,
            title_bar: color_or_any(
                config,
                &["title_bar.background", "tab_bar.background", "background"],
                fallback.title_bar,
            )?,
            title_bar_border: color_or_any(
                config,
                &["title_bar.border", "border"],
                fallback.title_bar_border,
            )?,
            secondary_hover: color_or_any(
                config,
                &["secondary.hover.background", "secondary.active.background"],
                fallback.secondary_hover,
            )?,
            input_background: color_or_any(
                config,
                &[
                    "input.background",
                    "title_bar.background",
                    "popover.background",
                    "background",
                ],
                fallback.input_background,
            )?,
            input_border: color_or_any(config, &["input.border", "border"], fallback.input_border)?,
            input_border_focused: color_or_any(
                config,
                &["ring", "primary.background", "link.foreground"],
                fallback.input_border_focused,
            )?,
            muted_foreground: color_or_any(
                config,
                &["muted.foreground"],
                fallback.muted_foreground,
            )?,
            dialog_background: color_or_any(
                config,
                &["popover.background", "panel.background", "background"],
                fallback.dialog_background,
            )?,
            overlay: color_or_any(config, &["overlay"], fallback.overlay)?,
            warning: color_or_any(
                config,
                &["warning.foreground", "warning.background", "base.yellow"],
                fallback.warning,
            )?,
            error: color_or_any(config, &["danger.background", "base.red"], fallback.error)?,
            radius_lg: px(config.radius_lg.unwrap_or(16.0)),
        })
    }

    fn catppuccin_mocha_fallback() -> Self {
        Self {
            background: hex(0x1e1e2e),
            foreground: hex(0xcdd6f4),
            title_bar: hex(0x181825),
            title_bar_border: hex(0x313244),
            secondary_hover: hsla(0.647, 0.20, 0.36, 0.55),
            input_background: hex(0x11111b),
            input_border: hex(0x45475a),
            input_border_focused: hex(0x89b4fa),
            muted_foreground: hex(0x6c7086),
            dialog_background: hex(0x1e1e2e),
            overlay: hsla(0.0, 0.0, 0.0, 0.55),
            warning: hex(0xf9e2af),
            error: hex(0xf38ba8),
            radius_lg: px(16.0),
        }
    }
}

fn color_or_any(config: &ThemeConfig, keys: &[&str], fallback: Hsla) -> Result<Hsla> {
    for key in keys {
        if let Some(value) = color_value(&config.colors, key) {
            return parse_hex_color(value).with_context(|| format!("解析主题颜色 `{key}` 失败"));
        }
    }

    Ok(fallback)
}

fn color_value<'a>(colors: &'a ThemeColors, key: &str) -> Option<&'a str> {
    colors
        .get(key)
        .and_then(|value| value.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn parse_hex_color(value: &str) -> Result<Hsla> {
    let hex = value
        .trim()
        .strip_prefix('#')
        .ok_or_else(|| anyhow!("颜色必须以 # 开头：{value}"))?;

    if !matches!(hex.len(), 6 | 8) {
        bail!("颜色必须为 #RRGGBB 或 #RRGGBBAA：{value}");
    }

    let rgb = u32::from_str_radix(&hex[..6], 16)
        .with_context(|| format!("颜色包含非法十六进制字符：{value}"))?;
    let alpha = if hex.len() == 8 {
        u8::from_str_radix(&hex[6..], 16)
            .with_context(|| format!("颜色 alpha 包含非法十六进制字符：{value}"))? as f32
            / 255.0
    } else {
        1.0
    };

    let [_, r, g, b] = rgb.to_be_bytes();
    Ok(rgb_to_hsla(r, g, b, alpha))
}

fn hex(value: u32) -> Hsla {
    let [_, r, g, b] = value.to_be_bytes();
    rgb_to_hsla(r, g, b, 1.0)
}

fn rgb_to_hsla(r: u8, g: u8, b: u8, alpha: f32) -> Hsla {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;

    if (max - min).abs() < f32::EPSILON {
        return hsla(0.0, 0.0, l, alpha);
    }

    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / d + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if (max - g).abs() < f32::EPSILON {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };

    hsla(h, s, l, alpha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_mocha_from_theme_json() {
        let theme = TinyTheme::from_theme_set_json(DEFAULT_THEME_JSON, DEFAULT_THEME_NAME).unwrap();

        assert_eq!(theme.background, parse_hex_color("#181825").unwrap());
        assert_eq!(theme.title_bar, parse_hex_color("#11111B").unwrap());
        assert_eq!(theme.input_background, parse_hex_color("#11111B").unwrap());
        assert_eq!(
            theme.input_border_focused,
            parse_hex_color("#cba6f7").unwrap()
        );
        assert_eq!(theme.warning, parse_hex_color("#f9e2af").unwrap());
        assert_eq!(theme.error, parse_hex_color("#f38ba8").unwrap());
    }

    #[test]
    fn parses_theme_hex_alpha() {
        assert_eq!(
            parse_hex_color("#00000080").unwrap(),
            hsla(0.0, 0.0, 0.0, 128.0 / 255.0)
        );
    }

    #[test]
    fn rejects_invalid_theme_hex() {
        assert!(parse_hex_color("575268").is_err());
        assert!(parse_hex_color("#12345").is_err());
        assert!(parse_hex_color("#zzzzzz").is_err());
    }
}
