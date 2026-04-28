use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, Context, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, div, ease_in_out, prelude::FluentBuilder, px,
};

use crate::{
    emby::{ResumeItems, UserViews},
    theme,
};

use super::{
    HomePage, HomeSection,
    carousel::{carousel_content_width, home_main_content_width, max_carousel_scroll_offset},
    components::{
        carousel_button, home_section_title, resume_item_card, section_placeholder, user_view_card,
    },
};

impl HomePage {
    fn render_main_content(
        &self,
        window: &Window,
        cx: &Context<Self>,
        rounded_window: bool,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let main_content_width = home_main_content_width(window);

        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .id("home-main-content")
            .overflow_y_scroll()
            .bg(theme.background)
            .p_6()
            .when(rounded_window, |this| {
                this.rounded_br(theme.radius_lg).overflow_hidden()
            })
            .when(self.active_section == HomeSection::Home, |this| {
                this.child(
                    div()
                        .mb_3()
                        .text_lg()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child("我的媒体"),
                )
                .when_some(self.user_views_failed.clone(), |this, error| {
                    this.child(div().text_sm().text_color(theme.error).child(error))
                })
                .when_some(self.user_views.as_ref(), |this, views| {
                    this.child(self.render_user_views_row(views, main_content_width, cx))
                })
                .when(
                    !self.user_views_loading
                        && self.user_views_failed.is_none()
                        && self
                            .user_views
                            .as_ref()
                            .is_none_or(|views| views.items.is_empty()),
                    |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("暂无媒体库"),
                        )
                    },
                )
                .child(home_section_title("继续观看", cx).mt_8().mb_3())
                .when_some(self.resume_items_failed.clone(), |this, error| {
                    this.child(div().text_sm().text_color(theme.error).child(error))
                })
                .when_some(self.resume_items.as_ref(), |this, items| {
                    this.child(self.render_resume_items_row(items, main_content_width, cx))
                })
                .when(
                    !self.resume_items_loading
                        && self.resume_items_failed.is_none()
                        && self
                            .resume_items
                            .as_ref()
                            .is_none_or(|items| items.items.is_empty()),
                    |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("暂无继续观看内容"),
                        )
                    },
                )
            })
            .when(self.active_section == HomeSection::Favorites, |this| {
                this.child(section_placeholder("收藏", "暂无收藏内容", cx))
            })
            .when(self.active_section == HomeSection::Search, |this| {
                this.child(section_placeholder("搜索", "搜索功能暂未实现", cx))
            })
    }

    fn render_user_views_row(
        &self,
        views: &UserViews,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let viewport_width = viewport_width.min(carousel_content_width(views.items.len()));
        let max_offset = max_carousel_scroll_offset(views.items.len(), viewport_width);
        let offset = self.user_views_scroll_offset.min(max_offset);
        let previous_offset = self.user_views_previous_scroll_offset.min(max_offset);
        let has_controls = max_offset > 0.0;
        let controls_visible =
            has_controls && (self.user_views_hovered || self.user_views_controls_hovered);
        let on_hover = cx.listener(|page: &mut HomePage, hovered: &bool, _, cx| {
            page.set_user_views_hovered(*hovered, cx);
        });
        let left_controls_hover = cx.listener(|page: &mut HomePage, hovered: &bool, _, cx| {
            page.set_user_views_controls_hovered(*hovered, cx);
        });
        let right_controls_hover = cx.listener(|page: &mut HomePage, hovered: &bool, _, cx| {
            page.set_user_views_controls_hovered(*hovered, cx);
        });
        let scroll_left = cx.listener(Self::scroll_user_views_left);
        let scroll_right = cx.listener(Self::scroll_user_views_right);

        div()
            .id("user-views-row")
            .relative()
            .group("user-views-row")
            .w(px(viewport_width))
            .max_w_full()
            .overflow_hidden()
            .on_hover(on_hover)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .children(views.items.iter().map(|view| {
                        let image_path = self.image_path_for_primary_image(
                            &view.id,
                            view.image_tags
                                .as_ref()
                                .and_then(|tags| tags.primary.as_deref()),
                        );
                        user_view_card(view.name.clone(), image_path, cx)
                    }))
                    .with_animation(
                        ("user-views-scroll", self.user_views_animation_id),
                        Animation::new(Duration::from_millis(220)).with_easing(ease_in_out),
                        move |track, delta| {
                            track.ml(px(-(previous_offset + (offset - previous_offset) * delta)))
                        },
                    ),
            )
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "user-views-scroll-left",
                    "icons/chevron-left.svg",
                    false,
                    controls_visible,
                    theme,
                    left_controls_hover,
                    scroll_left,
                ))
            })
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "user-views-scroll-right",
                    "icons/chevron-right.svg",
                    true,
                    controls_visible,
                    theme,
                    right_controls_hover,
                    scroll_right,
                ))
            })
    }

    fn render_resume_items_row(
        &self,
        items: &ResumeItems,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let viewport_width = viewport_width.min(carousel_content_width(items.items.len()));
        let max_offset = max_carousel_scroll_offset(items.items.len(), viewport_width);
        let offset = self.resume_items_scroll_offset.min(max_offset);
        let previous_offset = self.resume_items_previous_scroll_offset.min(max_offset);
        let has_controls = max_offset > 0.0;
        let controls_visible =
            has_controls && (self.resume_items_hovered || self.resume_items_controls_hovered);
        let on_hover = cx.listener(|page: &mut HomePage, hovered: &bool, _, cx| {
            page.set_resume_items_hovered(*hovered, cx);
        });
        let left_controls_hover = cx.listener(|page: &mut HomePage, hovered: &bool, _, cx| {
            page.set_resume_items_controls_hovered(*hovered, cx);
        });
        let right_controls_hover = cx.listener(|page: &mut HomePage, hovered: &bool, _, cx| {
            page.set_resume_items_controls_hovered(*hovered, cx);
        });
        let scroll_left = cx.listener(Self::scroll_resume_items_left);
        let scroll_right = cx.listener(Self::scroll_resume_items_right);

        div()
            .id("resume-items-row")
            .relative()
            .group("resume-items-row")
            .w(px(viewport_width))
            .max_w_full()
            .overflow_hidden()
            .on_hover(on_hover)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .children(items.items.iter().map(|item| {
                        let image_path = item
                            .image_source()
                            .and_then(|source| self.image_path_for_resume_image(source));
                        resume_item_card(item, image_path, cx)
                    }))
                    .with_animation(
                        ("resume-items-scroll", self.resume_items_animation_id),
                        Animation::new(Duration::from_millis(220)).with_easing(ease_in_out),
                        move |track, delta| {
                            track.ml(px(-(previous_offset + (offset - previous_offset) * delta)))
                        },
                    ),
            )
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "resume-items-scroll-left",
                    "icons/chevron-left.svg",
                    false,
                    controls_visible,
                    theme,
                    left_controls_hover,
                    scroll_left,
                ))
            })
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "resume-items-scroll-right",
                    "icons/chevron-right.svg",
                    true,
                    controls_visible,
                    theme,
                    right_controls_hover,
                    scroll_right,
                ))
            })
    }
}

impl Render for HomePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.load_user_views_if_needed(cx);
        self.load_resume_items_if_needed(cx);
        let theme = theme::get(cx);
        let rounded_window = !window.is_maximized();
        let on_back = cx.listener(Self::back_to_servers);
        let on_home = cx.listener(Self::select_home_section);
        let on_favorites = cx.listener(Self::select_favorites_section);
        let on_search = cx.listener(Self::select_search_section);

        div()
            .flex()
            .flex_1()
            .min_h_0()
            .size_full()
            .bg(theme.background)
            .when(rounded_window, |this| {
                this.rounded_b(theme.radius_lg).overflow_hidden()
            })
            .child(self.render_sidebar(
                cx,
                rounded_window,
                on_back,
                on_home,
                on_favorites,
                on_search,
            ))
            .child(self.render_main_content(window, cx, rounded_window))
    }
}
