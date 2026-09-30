use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, deferred, div,
    prelude::FluentBuilder, px,
};

use crate::ui::radius;
use crate::{
    server::{
        AddServerSubmission, Protocol,
        feature::form::{
            ServerFormController, ServerFormInput, ServerFormIntent, ServerFormProps,
            ServerFormUpdate,
        },
    },
    theme,
};

use super::{
    editor::{Editor, EditorEvent},
    notification::{NotificationQueue, error_notification, notification_layer},
};

const SERVER_DIALOG_ERROR_NOTIFICATION_KEY: &str = "server-dialog:error";
const SERVER_DIALOG_NOTIFICATION_TOP_PX: f32 = 51.0;

pub struct AddServerDialogState {
    controller: ServerFormController,
    address: Entity<Editor>,
    port: Entity<Editor>,
    path: Entity<Editor>,
    username: Entity<Editor>,
    password: Entity<Editor>,
    notifications: NotificationQueue<&'static str>,
}

impl AddServerDialogState {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::new_add(cx)
    }

    pub fn new_add(cx: &mut Context<Self>) -> Self {
        Self::from_props(ServerFormProps::default(), cx)
    }

    pub(crate) fn from_props(props: ServerFormProps, cx: &mut Context<Self>) -> Self {
        let controller = ServerFormController::new(&props);
        let address_input = cx.new(|cx| {
            Editor::new("服务器地址", cx)
                .default_value(props.address)
                .borderless()
                .height(px(30.0))
        });
        let port_input = cx.new(|cx| {
            Editor::new("端口", cx)
                .default_value(props.port)
                .digits_only()
                .max_chars(5)
        });
        let path_input = cx.new(|cx| Editor::new("可空", cx).default_value(props.path));
        let username_input = cx.new(|cx| Editor::new("用户名", cx).default_value(props.username));
        let password_input = cx.new(|cx| {
            Editor::new("密码", cx)
                .default_value(props.password)
                .masked(true)
                .mask_toggle()
        });

        cx.subscribe(
            &address_input,
            |dialog: &mut AddServerDialogState, _, event, cx| match event {
                EditorEvent::Changed => dialog.auto_format_full_url(cx),
                EditorEvent::Submitted => {}
            },
        )
        .detach();

        Self {
            controller,
            address: address_input,
            port: port_input,
            path: path_input,
            username: username_input,
            password: password_input,
            notifications: NotificationQueue::default(),
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) -> Option<AddServerSubmission> {
        let address = self.address.read(cx).value();
        let port = self.port.read(cx).value();
        let path = self.path.read(cx).value();
        let username = self.username.read(cx).value();
        let password = self.password.read(cx).value();

        self.dispatch_form(
            ServerFormIntent::Submit(ServerFormInput {
                address: &address,
                port: &port,
                path: &path,
                username: &username,
                password: &password,
            }),
            cx,
        )
    }

    pub fn edit_server_id(&self) -> Option<String> {
        self.controller.view().edit_server_id.map(str::to_owned)
    }

    pub fn set_submitting(&mut self, is_submitting: bool, cx: &mut Context<Self>) {
        self.dispatch_form(ServerFormIntent::SetSubmitting(is_submitting), cx);
    }

    pub fn push_error_notification(
        &mut self,
        error: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.notifications.push_autohide(
            SERVER_DIALOG_ERROR_NOTIFICATION_KEY,
            error.into(),
            cx,
            |dialog| &mut dialog.notifications,
        );
    }

    fn auto_format_full_url(&mut self, cx: &mut Context<Self>) {
        let address = self.address.read(cx).value();
        self.dispatch_form(ServerFormIntent::AddressChanged(&address), cx);
    }

    fn dispatch_form(
        &mut self,
        intent: ServerFormIntent<'_>,
        cx: &mut Context<Self>,
    ) -> Option<AddServerSubmission> {
        match self.controller.dispatch(intent) {
            ServerFormUpdate::Ignored => return None,
            ServerFormUpdate::Changed => {}
            ServerFormUpdate::PortChanged(value) => {
                self.port.update(cx, |port, cx| port.set_value(value, cx));
            }
            ServerFormUpdate::EndpointChanged(endpoint) => {
                let formatted_address = endpoint.address_input_value();
                self.address.update(cx, |address, cx| {
                    if address.value().as_ref() != formatted_address {
                        address.set_value(formatted_address, cx);
                    }
                });
                self.port.update(cx, |port, cx| {
                    let formatted_port = endpoint.port.to_string();
                    if port.value().as_ref() != formatted_port {
                        port.set_value(formatted_port, cx);
                    }
                });
                self.path.update(cx, |path, cx| {
                    if path.value().as_ref() != endpoint.path {
                        path.set_value(endpoint.path, cx);
                    }
                });
            }
            ServerFormUpdate::Submission(result) => {
                self.notifications.clear();
                return match result {
                    Ok(submission) => Some(submission),
                    Err(error) => {
                        self.push_error_notification(error, cx);
                        None
                    }
                };
            }
        }
        cx.notify();
        None
    }

    pub fn render_layer(
        &self,
        dialog: Entity<Self>,
        corners: gpui::Corners<gpui::Pixels>,
        on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_submit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let view = self.controller.view();

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.overlay)
            // Keep the modal layer from forwarding clicks or wheel events to
            // the page underneath its transparent backdrop.
            .occlude()
            .cursor_default()
            .rounded_tl(corners.top_left)
            .rounded_tr(corners.top_right)
            .rounded_bl(corners.bottom_left)
            .rounded_br(corners.bottom_right)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(560.0))
                    .gap_5()
                    .rounded(radius::SURFACE)
                    .border_1()
                    .border_color(theme.input_border)
                    .bg(theme.dialog_background)
                    .p_5()
                    .shadow_lg()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(view.title),
                    )
                    .child(self.render_form(dialog.clone(), cx))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                dialog_button("cancel-add-server", "取消", false, false, cx)
                                    .on_click(on_cancel),
                            )
                            .child(
                                dialog_button(
                                    "submit-add-server",
                                    view.submit_label,
                                    true,
                                    view.is_submitting,
                                    cx,
                                )
                                .on_click(on_submit),
                            ),
                    ),
            )
            .when(!self.notifications.is_empty(), |this| {
                this.child(deferred(self.render_notification_layer(dialog, cx)).with_priority(3))
            })
    }

    fn render_form(&self, dialog: Entity<Self>, cx: &App) -> impl IntoElement {
        let port = self.port.read(cx).value();
        let view = self.controller.view();
        let on_protocol_select = move |protocol, cx: &mut App| {
            dialog.update(cx, |dialog, cx| dialog.select_protocol(protocol, cx));
        };

        div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .child(field(
                "服务器地址",
                address_input(
                    self.address.clone(),
                    format!("{}://", view.protocol.scheme()),
                    format!(":{}", port),
                    cx,
                ),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().w(px(148.0)).child(field(
                        "协议",
                        protocol_selector(view.protocol, on_protocol_select, cx),
                        cx,
                    )))
                    .child(div().flex_1().child(field("端口", self.port.clone(), cx))),
            )
            .child(field("路径", self.path.clone(), cx))
            .child(field("用户名", self.username.clone(), cx))
            .child(field("密码", self.password.clone(), cx))
    }

    fn select_protocol(&mut self, protocol: Protocol, cx: &mut Context<Self>) {
        let port = self.port.read(cx).value();
        self.dispatch_form(
            ServerFormIntent::SelectProtocol {
                protocol,
                port: &port,
            },
            cx,
        );
    }

    fn dismiss_notification(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.notifications.remove(id) {
            cx.notify();
        }
    }

    fn render_notification_layer(&self, dialog: Entity<Self>, cx: &App) -> impl IntoElement {
        notification_layer()
            .top(px(SERVER_DIALOG_NOTIFICATION_TOP_PX))
            .children(self.notifications.iter().map(|entry| {
                let id = entry.notification.id;
                let dismiss_dialog = dialog.clone();
                error_notification(
                    id,
                    entry.notification.message.clone(),
                    move |_, _, cx| {
                        dismiss_dialog.update(cx, |dialog, cx| {
                            dialog.dismiss_notification(id, cx);
                        });
                    },
                    cx,
                )
            }))
    }
}

fn field(label: &'static str, input: impl IntoElement, cx: &App) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.foreground)
                .child(label),
        )
        .child(input)
}

fn address_input(
    input: Entity<Editor>,
    prefix: String,
    suffix: String,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .items_center()
        .h_8()
        .w_full()
        .rounded(radius::INPUT)
        .border_1()
        .border_color(theme.window_border)
        .bg(theme.editor_background)
        .cursor_default()
        .text_sm()
        .line_height(gpui::relative(1.3))
        .px_2()
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(prefix),
        )
        .child(div().flex_1().child(input))
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(suffix),
        )
}

fn protocol_selector(
    selected: Protocol,
    on_select: impl Fn(Protocol, &mut App) + Clone + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .h_8()
        .w_full()
        .rounded(radius::INPUT)
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.input_background)
        .p(px(2.0))
        .gap_0p5()
        .child(protocol_button(
            Protocol::Http,
            selected == Protocol::Http,
            on_select.clone(),
            cx,
        ))
        .child(protocol_button(
            Protocol::Https,
            selected == Protocol::Https,
            on_select,
            cx,
        ))
}

fn protocol_button(
    protocol: Protocol,
    selected: bool,
    on_select: impl Fn(Protocol, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .id(protocol.label())
        .cursor_pointer()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .rounded(radius::CONTROL)
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.foreground)
        .when(selected, |this| {
            this.bg(theme.element_selected)
                .text_color(theme.accent_text)
        })
        .hover(move |style| {
            style.bg(if selected {
                theme.element_selected_hover
            } else {
                theme.secondary_hover
            })
        })
        .child(protocol.label())
        .on_click(move |_, _, cx| on_select(protocol, cx))
}

fn dialog_button(
    id: &'static str,
    label: &'static str,
    primary: bool,
    disabled: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);

    div()
        .id(id)
        .flex()
        .h(px(34.0))
        .items_center()
        .justify_center()
        .rounded(radius::CONTROL)
        .px_4()
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if primary {
            theme.accent_foreground
        } else {
            theme.foreground
        })
        .border_1()
        .border_color(if primary {
            theme.accent
        } else {
            theme.input_border
        })
        .bg(if primary {
            theme.accent
        } else {
            theme.input_background
        })
        .cursor_default()
        .when(!disabled, |this| this.cursor_pointer())
        .when(disabled, |this| this.opacity(0.65))
        .hover(move |style| {
            if disabled {
                style
            } else if primary {
                style
                    .bg(theme.accent_hover)
                    .text_color(theme.accent_foreground)
            } else {
                style.bg(theme.secondary_hover).text_color(theme.foreground)
            }
        })
        .child(label)
}

#[cfg(test)]
mod tests;
