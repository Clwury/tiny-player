use std::{collections::HashMap, sync::LazyLock};

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

    // The bundled themes use different colors for `primary` and `ring`.
    // Keep the application's accent consistently Catppuccin Mauve in every flavor.
    fn mauve(self) -> Hsla {
        hex(match self {
            Self::Latte => 0x8839ef,
            Self::Frappe => 0xca9ee6,
            Self::Macchiato => 0xc6a0f6,
            Self::Mocha => 0xcba6f7,
        })
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

#[derive(Debug, PartialEq)]
pub struct ContextMenuColors {
    pub background: Hsla,
    pub border: Hsla,
    pub foreground: Hsla,
    pub hover_background: Hsla,
    pub disabled_foreground: Hsla,
    pub destructive_foreground: Hsla,
    pub destructive_hover_background: Hsla,
}

#[derive(Debug, PartialEq)]
pub struct TinyTheme {
    pub selection: ColorTheme,
    pub background: Hsla,
    pub foreground: Hsla,
    pub title_bar: Hsla,
    pub title_bar_border: Hsla,
    pub window_border: Hsla,
    pub panel_background: Hsla,
    pub secondary_hover: Hsla,
    pub element_selected: Hsla,
    pub element_selected_hover: Hsla,
    pub selection_background: Hsla,
    pub accent: Hsla,
    pub accent_text: Hsla,
    pub accent_hover: Hsla,
    pub accent_foreground: Hsla,
    pub input_background: Hsla,
    pub editor_background: Hsla,
    pub input_border: Hsla,
    pub input_border_focused: Hsla,
    pub muted_foreground: Hsla,
    pub placeholder_foreground: Hsla,
    pub dialog_background: Hsla,
    pub context_menu: ContextMenuColors,
    pub overlay: Hsla,
    pub warning: Hsla,
    pub error: Hsla,
    pub scrollbar_track: Hsla,
    pub scrollbar_thumb: Hsla,
    pub scrollbar_thumb_hover: Hsla,
    /// Fallback window decoration radius; component corners live in `ui::radius`.
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

/// Image and video overlays use light text on dark scrims in every application theme.
pub fn media_overlay(cx: &App) -> &TinyTheme {
    static DARK_THEME: LazyLock<TinyTheme> = LazyLock::new(|| {
        TinyTheme::from_theme_set_json(DEFAULT_THEME_JSON, ColorTheme::Mocha)
            .unwrap_or_else(|_| TinyTheme::catppuccin_mocha_fallback())
    });
    let theme = get(cx);
    if theme.selection == ColorTheme::Latte {
        &DARK_THEME
    } else {
        theme
    }
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
    #[serde(default)]
    highlight: EditorThemeConfig,
}

#[derive(Debug, Default, Deserialize)]
struct EditorThemeConfig {
    #[serde(rename = "editor.background")]
    background: Option<String>,
}

// Source colors stay in the bundled JSON. UI roles are derived separately so
// editor-oriented panel, selection and disabled-text colors do not leak into widgets.
struct ThemePalette {
    background: Hsla,
    editor_background: Hsla,
    foreground: Hsla,
    title_bar: Hsla,
    title_bar_border: Hsla,
    window_border: Hsla,
    input_border: Hsla,
    warning: Hsla,
    error: Hsla,
    scrollbar_thumb: Hsla,
}

impl ThemePalette {
    fn mocha_fallback() -> Self {
        Self {
            background: hex(0x181825),
            editor_background: hex(0x181825),
            foreground: hex(0xcdd6f4),
            title_bar: hex(0x11111b),
            title_bar_border: hex(0x313244),
            window_border: hex(0x313244),
            input_border: hex(0x6c7086),
            warning: hex(0xf9e2af),
            error: hex(0xf38ba8),
            scrollbar_thumb: hex(0x4e4e5e),
        }
    }
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
        let fallback = ThemePalette::mocha_fallback();
        let background = color_or_any(config, &["background"], fallback.background)?;
        let palette = ThemePalette {
            background,
            editor_background: config
                .highlight
                .background
                .as_deref()
                .map(parse_hex_color)
                .transpose()
                .context("解析输入框背景颜色失败")?
                .unwrap_or(background),
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
            input_border: color_or_any(config, &["input.border", "border"], fallback.input_border)?,
            warning: color_or_any(
                config,
                &["warning.foreground", "warning.background", "base.yellow"],
                fallback.warning,
            )?,
            error: color_or_any(config, &["danger.background", "base.red"], fallback.error)?,
            scrollbar_thumb: color_or_any(
                config,
                &["scrollbar.thumb.background"],
                fallback.scrollbar_thumb,
            )?,
        };
        Ok(Self::from_palette(
            selection,
            palette,
            px(config.radius_lg.unwrap_or(16.0)),
        ))
    }

    fn from_palette(selection: ColorTheme, palette: ThemePalette, radius_lg: Pixels) -> Self {
        let light = selection == ColorTheme::Latte;
        let background = palette.background;
        let foreground = palette.foreground;
        let accent = selection.mauve();
        let white = hex(0xffffff);
        let black = hex(0x000000);
        let input_background = if light {
            background.blend(white.opacity(0.45))
        } else {
            palette.title_bar
        };
        let error = if light {
            palette.error.blend(black.opacity(0.25))
        } else {
            palette.error
        };
        let menu_background = background.blend(if light {
            white.opacity(0.90)
        } else {
            foreground.opacity(0.08)
        });
        let menu_foreground = foreground.blend(if light {
            black.opacity(0.08)
        } else {
            white.opacity(0.08)
        });

        Self {
            selection,
            background,
            foreground,
            title_bar: palette.title_bar,
            title_bar_border: palette.title_bar_border,
            window_border: palette.window_border,
            // Sidebars share the window chrome; popups sit above the content surface.
            panel_background: palette.title_bar,
            dialog_background: background.blend(if light {
                white.opacity(0.75)
            } else {
                foreground.opacity(0.04)
            }),
            // Menus overlay artwork as well as panels. Composite every fill on
            // their own opaque surface so hover and text contrast stay stable.
            context_menu: ContextMenuColors {
                background: menu_background,
                border: menu_background.blend(foreground.opacity(0.26)),
                foreground: menu_foreground,
                hover_background: menu_background.blend(accent.opacity(if light {
                    0.16
                } else {
                    0.22
                })),
                disabled_foreground: menu_background.blend(menu_foreground.opacity(if light {
                    0.78
                } else {
                    0.68
                })),
                destructive_foreground: error.blend(if light {
                    black.opacity(0.06)
                } else {
                    white.opacity(0.30)
                }),
                destructive_hover_background: menu_background.blend(error.opacity(if light {
                    0.14
                } else {
                    0.18
                })),
            },
            // Opaque, composited fills keep hover/selection consistent on every surface.
            secondary_hover: background.blend(foreground.opacity(if light { 0.16 } else { 0.08 })),
            element_selected: background.blend(accent.opacity(if light { 0.10 } else { 0.16 })),
            element_selected_hover: background.blend(accent.opacity(if light {
                0.18
            } else {
                0.20
            })),
            selection_background: accent.opacity(0.24),
            accent,
            accent_text: if light {
                accent.blend(black.opacity(0.22))
            } else {
                accent
            },
            accent_hover: accent.blend(if light {
                black.opacity(0.12)
            } else {
                white.opacity(0.18)
            }),
            accent_foreground: if light { white } else { background },
            input_background,
            editor_background: palette.editor_background,
            input_border: background.blend(palette.input_border.opacity(0.7)),
            input_border_focused: accent,
            // Metadata must remain readable: the source `muted.foreground` is also
            // used for disabled editor text and is too faint for regular UI labels.
            muted_foreground: background.blend(foreground.opacity(if light { 0.96 } else { 0.80 })),
            placeholder_foreground: input_background.blend(foreground.opacity(if light {
                0.83
            } else {
                0.60
            })),
            overlay: black.opacity(if light { 0.25 } else { 0.55 }),
            warning: if light {
                palette.warning.blend(black.opacity(0.4))
            } else {
                palette.warning
            },
            error,
            scrollbar_track: background.opacity(0.0),
            scrollbar_thumb: palette.scrollbar_thumb.blend(foreground.opacity(0.18)),
            scrollbar_thumb_hover: palette.scrollbar_thumb.blend(foreground.opacity(0.46)),
            radius_lg,
        }
    }

    fn catppuccin_mocha_fallback() -> Self {
        Self::from_palette(ColorTheme::Mocha, ThemePalette::mocha_fallback(), px(16.0))
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
        for (selection, background, border, accent) in [
            (ColorTheme::Latte, "#e5e9ef", "#ccd0da", "#8839ef"),
            (ColorTheme::Frappe, "#232634", "#3e4255", "#ca9ee6"),
            (ColorTheme::Macchiato, "#1e2030", "#494d64", "#c6a0f6"),
            (ColorTheme::Mocha, "#181825", "#313244", "#cba6f7"),
        ] {
            let theme = TinyTheme::from_theme_set_json(DEFAULT_THEME_JSON, selection).unwrap();
            assert_eq!(theme.selection, selection);
            assert_eq!(theme.background, parse_hex_color(background).unwrap());
            assert_eq!(theme.window_border, parse_hex_color(border).unwrap());
            assert_eq!(theme.accent, parse_hex_color(accent).unwrap());
            assert_eq!(theme.input_border_focused, theme.accent);
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
        assert_eq!(theme.title_bar, parse_hex_color("#11111b").unwrap());
        assert_eq!(theme.input_background, parse_hex_color("#11111b").unwrap());
        assert_eq!(theme.editor_background, parse_hex_color("#181825").unwrap());
        assert_eq!(
            theme.input_border_focused,
            parse_hex_color("#cba6f7").unwrap()
        );
        assert_eq!(theme.warning, parse_hex_color("#f9e2af").unwrap());
        assert_eq!(theme.error, parse_hex_color("#f38ba8").unwrap());
    }

    #[test]
    fn editor_surface_reads_highlight_colors_without_parsing_syntax_as_colors() {
        let mut config: ThemeConfig = serde_json::from_value(serde_json::json!({
            "name": "Catppuccin Latte",
            "colors": {"background": "#e5e9ef"},
            "highlight": {
                "editor.background": "#eff1f5",
                "syntax": {"comment": {"color": "#9ca0b0", "font_style": "italic"}}
            }
        }))
        .unwrap();
        let theme = TinyTheme::from_theme_config(&config, ColorTheme::Latte).unwrap();
        assert_eq!(theme.editor_background, parse_hex_color("#eff1f5").unwrap());

        config.highlight.background = None;
        let theme = TinyTheme::from_theme_config(&config, ColorTheme::Latte).unwrap();
        assert_eq!(theme.editor_background, theme.background);

        config.highlight.background = Some("invalid".into());
        assert!(TinyTheme::from_theme_config(&config, ColorTheme::Latte).is_err());
    }

    #[test]
    fn fallback_uses_the_same_ui_colors_as_bundled_mocha() {
        assert_eq!(
            TinyTheme::catppuccin_mocha_fallback(),
            TinyTheme::from_theme_set_json(DEFAULT_THEME_JSON, ColorTheme::Mocha).unwrap(),
        );
    }

    fn contrast_ratio(foreground: Hsla, background: Hsla) -> f32 {
        fn luminance(color: Hsla) -> f32 {
            let color = color.to_rgb();
            [color.r, color.g, color.b]
                .into_iter()
                .zip([0.2126, 0.7152, 0.0722])
                .map(|(channel, weight)| {
                    weight
                        * if channel <= 0.04045 {
                            channel / 12.92
                        } else {
                            ((channel + 0.055) / 1.055).powf(2.4)
                        }
                })
                .sum()
        }
        let foreground = luminance(background.blend(foreground));
        let background = luminance(background);
        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
    }

    #[test]
    fn bundled_text_remains_readable_on_surfaces_and_controls() {
        for selection in ColorTheme::ALL {
            let theme = TinyTheme::from_theme_set_json(DEFAULT_THEME_JSON, selection).unwrap();
            for surface in [
                theme.background,
                theme.panel_background,
                theme.dialog_background,
                theme.secondary_hover,
                theme.element_selected,
                theme.element_selected_hover,
            ] {
                assert_eq!(surface.a, 1.0, "{selection:?} surface must be opaque");
                for text in [theme.foreground, theme.muted_foreground] {
                    assert!(contrast_ratio(text, surface) >= 4.5, "{selection:?}");
                }
            }
            for (text, surface) in [
                (theme.accent_text, theme.element_selected),
                (theme.accent_text, theme.element_selected_hover),
                (theme.accent_foreground, theme.accent),
                (theme.accent_foreground, theme.accent_hover),
                (theme.placeholder_foreground, theme.input_background),
                (theme.warning, theme.dialog_background),
                (theme.error, theme.dialog_background),
                (
                    theme.foreground,
                    theme.input_background.blend(theme.selection_background),
                ),
            ] {
                assert!(contrast_ratio(text, surface) >= 4.5, "{selection:?}");
            }
            let menu = &theme.context_menu;
            for (text, surface) in [
                (menu.foreground, menu.background),
                (menu.foreground, menu.hover_background),
                (menu.disabled_foreground, menu.background),
                (menu.destructive_foreground, menu.background),
                (
                    menu.destructive_foreground,
                    menu.destructive_hover_background,
                ),
            ] {
                assert_eq!(surface.a, 1.0, "{selection:?} menu surface must be opaque");
                assert_eq!(text.a, 1.0, "{selection:?} menu text must be opaque");
                assert!(
                    contrast_ratio(text, surface) >= 4.5,
                    "{selection:?} menu text"
                );
            }
            for hover in [menu.hover_background, menu.destructive_hover_background] {
                assert!(
                    contrast_ratio(hover, menu.background) >= 1.2,
                    "{selection:?} menu hover"
                );
            }
            assert!(
                contrast_ratio(menu.border, menu.background) >= 1.4,
                "{selection:?} menu border"
            );
            assert_ne!(theme.background, theme.panel_background);
            assert_ne!(theme.background, theme.dialog_background);
            assert_ne!(theme.secondary_hover, theme.element_selected);
            assert_ne!(theme.scrollbar_thumb, theme.scrollbar_thumb_hover);
        }
    }

    #[test]
    fn latte_hover_is_visible_on_content_and_sidebar_surfaces() {
        let theme = TinyTheme::from_theme_set_json(DEFAULT_THEME_JSON, ColorTheme::Latte).unwrap();
        assert!(contrast_ratio(theme.secondary_hover, theme.background) >= 1.25);
        assert!(contrast_ratio(theme.secondary_hover, theme.panel_background) >= 1.15);
        assert!(contrast_ratio(theme.element_selected_hover, theme.element_selected) >= 1.1);
    }

    #[gpui::test]
    fn video_controls_remain_readable_when_switching_to_a_light_theme(cx: &mut TestAppContext) {
        for selection in ColorTheme::ALL.into_iter().chain([ColorTheme::Latte]) {
            cx.update(|cx| {
                set(selection, cx);
                // The application keeps its selected theme; only the video overlay is dark.
                assert_eq!(get(cx).selection, selection);
                let video = media_overlay(cx);
                assert!(contrast_ratio(video.foreground, hex(0x373737)) >= 4.5);
                assert!(contrast_ratio(video.muted_foreground, hex(0x373737)) >= 4.5);
                if selection != ColorTheme::Latte {
                    assert!(std::ptr::eq(video, get(cx)));
                }
            });
        }
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
