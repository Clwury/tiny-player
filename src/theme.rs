use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{App, Global, Hsla, Pixels, hsla, px};
use serde::{Deserialize, Serialize};
use tracing::debug;

const DEFAULT_THEME_JSON: &str = include_str!("../themes/catppuccin.json");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorTheme {
    Latte,
    Frappe,
    Macchiato,
    #[default]
    #[serde(other)]
    Mocha,
}

impl ColorTheme {
    pub const ALL: [Self; 4] = [Self::Latte, Self::Frappe, Self::Macchiato, Self::Mocha];

    pub fn name(self) -> &'static str {
        match self {
            Self::Latte => "Catppuccin Latte",
            Self::Frappe => "Catppuccin Frappe",
            Self::Macchiato => "Catppuccin Macchiato",
            Self::Mocha => "Catppuccin Mocha",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Latte => "theme-latte",
            Self::Frappe => "theme-frappe",
            Self::Macchiato => "theme-macchiato",
            Self::Mocha => "theme-mocha",
        }
    }
}

pub struct TinyTheme {
    pub selection: ColorTheme,
    pub background: Hsla,
    pub foreground: Hsla,
    pub title_bar: Hsla,
    pub title_bar_border: Hsla,
    pub window_border: Hsla,
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
    set(ColorTheme::default(), cx);
}

pub fn set(selection: ColorTheme, cx: &mut App) {
    if cx
        .try_global::<TinyTheme>()
        .is_some_and(|theme| theme.selection == selection)
    {
        return;
    }
    let theme =
        TinyTheme::from_theme_set_json(DEFAULT_THEME_JSON, selection).unwrap_or_else(|error| {
            debug!(%error, "failed to load bundled theme, falling back to built-in Mocha colors");
            TinyTheme::catppuccin_mocha_fallback()
        });
    cx.set_global(theme);
    // Refresh every window, including cached child views, rather than only
    // invalidating the settings dialog that initiated the change.
    cx.refresh_windows();
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
    fn from_theme_set_json(json: &str, selection: ColorTheme) -> Result<Self> {
        let theme_set: ThemeSet = serde_json::from_str(json).context("解析主题 JSON 失败")?;
        let config = theme_set
            .themes
            .iter()
            .find(|theme| theme.name == selection.name())
            .ok_or_else(|| anyhow!("主题 JSON 中没有 {}", selection.name()))?;

        Self::from_theme_config(config, selection)
    }

    fn from_theme_config(config: &ThemeConfig, selection: ColorTheme) -> Result<Self> {
        let fallback = Self::catppuccin_mocha_fallback();

        Ok(Self {
            selection,
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
            window_border: color_or_any(config, &["border"], fallback.window_border)?,
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
            selection: ColorTheme::Mocha,
            background: hex(0x1e1e2e),
            foreground: hex(0xcdd6f4),
            title_bar: hex(0x181825),
            title_bar_border: hex(0x313244),
            window_border: hex(0x313244),
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
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, Render, StyleRefinement, Styled,
        TestAppContext, Window, div,
    };

    #[test]
    fn all_bundled_color_themes_load_their_own_palette() {
        for (selection, background, border) in [
            (ColorTheme::Latte, "#E5E9EF", "#CCD0DA"),
            (ColorTheme::Frappe, "#232634", "#3e4255"),
            (ColorTheme::Macchiato, "#1E2030", "#494d64"),
            (ColorTheme::Mocha, "#181825", "#313244"),
        ] {
            let theme = TinyTheme::from_theme_set_json(DEFAULT_THEME_JSON, selection).unwrap();
            assert_eq!(theme.selection, selection);
            assert_eq!(theme.background, parse_hex_color(background).unwrap());
            assert_eq!(theme.window_border, parse_hex_color(border).unwrap());
            assert_ne!(theme.foreground, theme.background);
        }
    }

    struct ThemeSurface {
        rendered_background: Option<Hsla>,
    }

    impl Render for ThemeSurface {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let background = get(cx).background;
            self.rendered_background = Some(background);
            div().size_full().bg(background)
        }
    }

    struct ThemeWindow(Entity<ThemeSurface>);

    impl Render for ThemeWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.0
                .clone()
                .cached(StyleRefinement::default().size_full())
        }
    }

    #[gpui::test]
    fn theme_changes_refresh_cached_views_in_every_open_window(cx: &mut TestAppContext) {
        cx.update(init);
        let windows: Vec<_> = (0..2)
            .map(|_| {
                cx.add_window(|_, cx| {
                    ThemeWindow(cx.new(|_| ThemeSurface {
                        rendered_background: None,
                    }))
                })
            })
            .collect();
        cx.run_until_parked();
        for selection in ColorTheme::ALL {
            cx.update(|cx| set(selection, cx));
            cx.run_until_parked();
            let expected = cx.update(|cx| get(cx).background);
            for window in &windows {
                assert_eq!(
                    window
                        .read_with(cx, |root, cx| root.0.read(cx).rendered_background)
                        .unwrap(),
                    Some(expected)
                );
            }
        }
    }

    #[test]
    fn loads_mocha_from_theme_json() {
        let theme = TinyTheme::from_theme_set_json(DEFAULT_THEME_JSON, ColorTheme::Mocha).unwrap();

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
