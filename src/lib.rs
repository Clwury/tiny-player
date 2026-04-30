mod app;
mod assets;
pub mod emby;
mod home;
mod images;
pub mod server;
mod storage;
mod theme;
mod ui;

use app::TinyApp;
use assets::ProjectAssets;
use gpui::{
    AppContext, Application, Bounds, TitlebarOptions, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowOptions, px, size,
};
use storage::ServerCache;
use ui::text_input::TextInput;

const DEFAULT_WINDOW_WIDTH: u32 = 1100;
const DEFAULT_WINDOW_HEIGHT: u32 = 720;
const MIN_WINDOW_WIDTH: u32 = 900;
const MIN_WINDOW_HEIGHT: u32 = 600;

pub fn run() {
    Application::new()
        .with_assets(ProjectAssets::new())
        .run(|cx| {
            theme::init(cx);
            TextInput::bind_keys(cx);

            let (cache, cache_error) = match storage::load_or_init() {
                Ok(cache) => (cache, None),
                Err(error) => (
                    ServerCache::empty(),
                    Some(format!("加载服务器缓存失败：{error}").into()),
                ),
            };
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
                        title: Some("Tiny".into()),
                        appears_transparent: true,
                        traffic_light_position: None,
                    }),
                    app_id: Some("tiny".to_string()),
                    ..Default::default()
                },
                |_, cx| cx.new(|_| TinyApp::new(cache, cache_error)),
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
