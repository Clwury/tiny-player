use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px, svg,
};

use crate::{
    home::{
        HomeContent,
        library::{LibraryView, controller::LibrarySource},
        model::navigation::HomeRoute,
    },
    theme,
    ui::{radius, tooltip::text_tooltip},
};

impl HomeContent {
    pub(in crate::home) fn render_person_scrollable_content(
        &self,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let HomeRoute::Person { person_id, .. } = self.controller.route() else {
            unreachable!("person renderer requires a Person route");
        };
        let person = self
            .controller
            .person_view(person_id)
            .expect("Person route has a controller");
        let resources = self
            .person_resources
            .get(person_id)
            .expect("Person route has presentation resources");
        let favorite = self
            .controller
            .effective_user_data(person_id, person.user_data)
            .is_some_and(|data| data.is_favorite);
        let enabled = person.loaded && !self.controller.user_data_pending();
        let theme = theme::get(cx);
        let label = if favorite {
            "取消人物收藏"
        } else {
            "收藏人物"
        };
        let button = div()
            .id("person-favorite-button")
            .debug_selector(|| "person-favorite-button".into())
            .role(gpui::Role::Button)
            .aria_label(label)
            .tooltip(move |_, cx| text_tooltip(label, cx))
            .flex()
            .flex_none()
            .size(px(32.0))
            .items_center()
            .justify_center()
            .rounded(radius::CONTROL)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                svg()
                    .path(if favorite {
                        "icons/heart-filled.svg"
                    } else {
                        "icons/heart.svg"
                    })
                    .size(px(18.0))
                    .text_color(if favorite {
                        theme.accent_text
                    } else {
                        theme.foreground
                    }),
            )
            .when(enabled, |this| {
                this.cursor_pointer()
                    .hover(move |style| style.bg(theme.secondary_hover))
                    .on_click(cx.listener(|page, _, _, cx| page.toggle_person_favorite(cx)))
            })
            .when(!enabled, |this| this.cursor_default().opacity(0.55));
        self.render_items_feed_content(
            &LibrarySource::Person(person_id.clone()),
            LibraryView {
                model: person.items,
                presentation: &resources.items.presentation,
            },
            Some(button.into_any_element()),
            "该人物暂无相关电影或剧集",
            cx,
        )
    }
}
