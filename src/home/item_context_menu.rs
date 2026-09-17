use gpui::{
    App, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Point, Styled, Window, anchored, div, point, prelude::FluentBuilder, px,
};

use crate::{emby::UserItemData, theme};

use super::{HomeContent, resume_actions::ResumeItemAction};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ItemContextMenuSource {
    Resume,
    UserItem,
}

#[derive(Clone, Debug)]
pub(super) struct ItemContextMenu {
    pub(super) item_id: String,
    position: Point<Pixels>,
    pub(super) source: ItemContextMenuSource,
}

#[derive(Clone, Copy)]
enum ItemContextMenuAction {
    ToggleFavorite,
    MarkPlayed,
    HideFromResume,
}

impl HomeContent {
    pub(super) fn open_item_context_menu(
        &mut self,
        item_id: String,
        source: ItemContextMenuSource,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let menu = ItemContextMenu {
            item_id,
            position,
            source,
        };
        if self.context_menu_user_data(&menu).is_none() {
            return;
        }
        self.item_context_menu = Some(menu);
        cx.notify();
    }

    fn close_item_context_menu(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.item_context_menu.take().is_some() {
            cx.notify();
        }
    }

    fn context_menu_user_data(&self, menu: &ItemContextMenu) -> Option<UserItemData> {
        if menu.item_id.trim().is_empty() {
            return None;
        }
        match menu.source {
            ItemContextMenuSource::Resume => self
                .resume_item_by_id(&menu.item_id)
                .map(|item| item.user_data.unwrap_or_default()),
            ItemContextMenuSource::UserItem => self
                .user_item_by_id(&menu.item_id)
                .filter(|item| {
                    matches!(
                        item.item_type.as_deref(),
                        Some("Movie" | "Series" | "Episode")
                    )
                })
                .map(|item| item.user_data.unwrap_or_default()),
        }
    }

    fn activate_item_context_menu_action(
        &mut self,
        action: ItemContextMenuAction,
        cx: &mut Context<Self>,
    ) {
        if self.detail_user_data_pending() {
            return;
        }
        let Some(menu) = self.item_context_menu.clone() else {
            return;
        };
        let Some(data) = self.context_menu_user_data(&menu) else {
            self.item_context_menu = None;
            cx.notify();
            return;
        };
        self.item_context_menu = None;
        match action {
            ItemContextMenuAction::ToggleFavorite => {
                self.toggle_item_favorite(menu.item_id, Some(data), cx);
            }
            ItemContextMenuAction::MarkPlayed => match menu.source {
                ItemContextMenuSource::Resume => {
                    self.start_resume_item_action(menu.item_id, ResumeItemAction::MarkPlayed, cx);
                }
                ItemContextMenuSource::UserItem => {
                    self.mark_user_item_played(&menu.item_id, cx);
                }
            },
            ItemContextMenuAction::HideFromResume => {
                if menu.source == ItemContextMenuSource::Resume {
                    self.start_resume_item_action(
                        menu.item_id,
                        ResumeItemAction::HideFromResume,
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    pub(super) fn render_item_context_menu(
        &self,
        menu: ItemContextMenu,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let data = self.context_menu_user_data(&menu);
        let pending = self.detail_user_data_pending() || data.is_none();
        let favorite = data.is_some_and(|data| data.is_favorite);
        let resume = menu.source == ItemContextMenuSource::Resume;
        let menu_id = if resume {
            "resume-item-context-menu"
        } else {
            "user-view-item-context-menu"
        };
        let options = [
            (
                if resume {
                    "resume-item-favorite"
                } else {
                    "user-view-item-favorite"
                },
                if favorite { "取消收藏" } else { "收藏" },
                ItemContextMenuAction::ToggleFavorite,
            ),
            (
                if resume {
                    "resume-item-mark-played"
                } else {
                    "user-view-item-mark-played"
                },
                ResumeItemAction::MarkPlayed.label(),
                ItemContextMenuAction::MarkPlayed,
            ),
            (
                "resume-item-hide-from-resume",
                ResumeItemAction::HideFromResume.label(),
                ItemContextMenuAction::HideFromResume,
            ),
        ];

        anchored()
            .position(menu.position)
            .offset(point(px(4.0), px(4.0)))
            .snap_to_window_with_margin(px(8.0))
            .child(
                div()
                    .id(menu_id)
                    .debug_selector(move || menu_id.into())
                    .occlude()
                    .flex()
                    .flex_col()
                    .min_w(px(176.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.context_menu.border)
                    .bg(theme.context_menu.background)
                    .shadow_lg()
                    .p(px(4.0))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down_out(cx.listener(Self::close_item_context_menu))
                    .children(options.into_iter().take(if resume { 3 } else { 2 }).map(
                        |(id, label, action)| {
                            item_context_menu_option(
                                id,
                                label,
                                matches!(action, ItemContextMenuAction::HideFromResume),
                                pending,
                                cx.listener(move |page, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    page.activate_item_context_menu_action(action, cx);
                                }),
                                cx,
                            )
                        },
                    )),
            )
    }
}

fn item_context_menu_option(
    id: &'static str,
    label: &'static str,
    destructive: bool,
    disabled: bool,
    on_mouse_down: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    cx: &Context<HomeContent>,
) -> impl IntoElement {
    let colors = &theme::get(cx).context_menu;
    let hover_background = if destructive {
        colors.destructive_hover_background
    } else {
        colors.hover_background
    };
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex()
        .h(px(32.0))
        .items_center()
        .rounded(px(6.0))
        .px_2()
        .text_sm()
        .text_color(if disabled {
            colors.disabled_foreground
        } else if destructive {
            colors.destructive_foreground
        } else {
            colors.foreground
        })
        .when(disabled, |this| this.cursor_default())
        .when(!disabled, |this| {
            this.cursor_pointer()
                .hover(move |style| style.bg(hover_background))
                .on_mouse_down(MouseButton::Left, on_mouse_down)
        })
        .child(
            div()
                .debug_selector(move || format!("{id}-{label}"))
                .child(label),
        )
}
