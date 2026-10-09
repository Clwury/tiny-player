use gpui::{Context, IntoElement};

use crate::home::{
    HomeContent,
    library::{LibraryView, controller::LibrarySource},
    model::navigation::HomeRoute,
};

impl HomeContent {
    pub(in crate::home) fn render_genre_scrollable_content(
        &self,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let HomeRoute::Genre { genre_key, .. } = self.controller.route() else {
            unreachable!("genre renderer requires a Genre route");
        };
        self.render_items_feed_content(
            &LibrarySource::Genre(genre_key.clone()),
            LibraryView {
                model: self
                    .controller
                    .genre_view(genre_key)
                    .expect("Genre route has a controller"),
                presentation: &self
                    .genre_resources
                    .get(genre_key)
                    .expect("Genre route has presentation resources")
                    .presentation,
            },
            None,
            "该类型暂无相关电影或剧集",
            cx,
        )
    }
}
