use gpui::{App, Global, Hsla, Pixels, hsla, px};

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
    pub error: Hsla,
    pub radius_lg: Pixels,
}

impl Global for TinyTheme {}

pub fn init(cx: &mut App) {
    cx.set_global(TinyTheme::catppuccin_mocha());
}

pub fn get(cx: &App) -> &TinyTheme {
    cx.global::<TinyTheme>()
}

impl TinyTheme {
    fn catppuccin_mocha() -> Self {
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
            error: hex(0xf38ba8),
            radius_lg: px(16.0),
        }
    }
}

fn hex(value: u32) -> Hsla {
    let [_, r, g, b] = value.to_be_bytes();
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;

    if (max - min).abs() < f32::EPSILON {
        return hsla(0.0, 0.0, l, 1.0);
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

    hsla(h, s, l, 1.0)
}
