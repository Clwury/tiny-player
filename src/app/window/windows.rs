use gpui::{App, Window, WindowAppearance};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::HWND,
    Graphics::Dwm::{DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
};

use crate::theme::{self, ColorTheme};

/// Keep DWM in sync with the app's theme, which can differ from Windows settings.
pub(in crate::app) fn sync_window_theme(window: &mut Window, cx: &mut App) {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut _);
    let current = (theme::get(cx).selection, window.appearance());
    let applied = window.use_keyed_state("native-window-theme", cx, |_, _| {
        None::<(ColorTheme, WindowAppearance)>
    });
    if *applied.read(cx) == Some(current) {
        return;
    }

    let dark_mode = i32::from(current.0 != ColorTheme::Latte);
    let border = gpui::Rgba::from(theme::get(cx).window_border);
    let border_color = ((border.r * 255.0).round() as u32)
        | (((border.g * 255.0).round() as u32) << 8)
        | (((border.b * 255.0).round() as u32) << 16);

    // SAFETY: GPUI owns the live HWND; each attribute receives the required
    // four-byte value, which remains valid for the duration of the call.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark_mode as *const i32).cast(),
            std::mem::size_of_val(&dark_mode) as u32,
        );
        // Custom border colors require Windows 11. Earlier versions retain
        // their native light/dark border when this attribute is unsupported.
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&border_color as *const u32).cast(),
            std::mem::size_of_val(&border_color) as u32,
        );
    }
    applied.update(cx, |applied, _| *applied = Some(current));
}
