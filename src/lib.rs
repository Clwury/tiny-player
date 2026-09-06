mod app;
mod app_metadata;
mod assets;
pub mod emby;
mod home;
mod images;
pub mod player;
pub mod server;
mod storage;
mod theme;
mod ui;

use std::rc::Rc;

use app::TinyApp;
use app_metadata::{APP_ID, APP_NAME};
use assets::ProjectAssets;
use gpui::{
    App, AppContext, Application, Bounds, Global, Platform, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions, px, size,
};
use storage::ServerCache;
use ui::editor::Editor;

const DEFAULT_WINDOW_WIDTH: u32 = 1100;
const DEFAULT_WINDOW_HEIGHT: u32 = 720;
const MIN_WINDOW_WIDTH: u32 = 900;
const MIN_WINDOW_HEIGHT: u32 = 600;

struct TinyPlatform(Rc<dyn Platform>);

impl Global for TinyPlatform {}

pub(crate) fn hide_cursor_until_mouse_moves(cx: &App) {
    if cx.has_global::<TinyPlatform>() {
        cx.global::<TinyPlatform>()
            .0
            .hide_cursor_until_mouse_moves();
    }
}

pub fn run() {
    let platform = gpui_platform::current_platform(false);
    Application::with_platform(platform.clone())
        .with_assets(ProjectAssets::new())
        .run(move |cx| {
            cx.set_global(TinyPlatform(platform));
            theme::init(cx);
            Editor::bind_keys(cx);

            let (cache, startup_error) = match storage::load_or_init() {
                Ok(cache) => (cache, None),
                Err(error) => (
                    ServerCache::empty(),
                    Some(format!("加载服务器缓存失败：{error}").into()),
                ),
            };
            theme::set(cache.color_theme, cx);
            let window_size = restored_window_size(&cache);
            let bounds = Bounds::centered(
                None,
                size(px(window_size.0 as f32), px(window_size.1 as f32)),
                cx,
            );

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(
                        px(MIN_WINDOW_WIDTH as f32),
                        px(MIN_WINDOW_HEIGHT as f32),
                    )),
                    window_decorations: Some(WindowDecorations::Client),
                    window_background: WindowBackgroundAppearance::Transparent,
                    titlebar: Some(TitlebarOptions {
                        title: Some(APP_NAME.into()),
                        appears_transparent: true,
                        traffic_light_position: None,
                    }),
                    app_id: Some(APP_ID.to_string()),
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| TinyApp::new(cache, startup_error, cx)),
            )
            .unwrap();

            cx.activate(true);
        });
}

fn restored_window_size(cache: &ServerCache) -> (u32, u32) {
    cache
        .window_size()
        .map(|window| {
            (
                window.width.max(MIN_WINDOW_WIDTH),
                window.height.max(MIN_WINDOW_HEIGHT),
            )
        })
        .unwrap_or((DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT))
}
