use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, deferred, div,
    prelude::FluentBuilder, px,
};

use crate::{
    server::{AddServerSubmission, CachedServer, Protocol, ServerEndpoint},
    theme,
};

use super::{
    editor::{Editor, EditorEvent},
    notification::{
        NOTIFICATION_AUTOHIDE, NotificationQueue, error_notification, notification_layer,
    },
};

const SERVER_DIALOG_ERROR_NOTIFICATION_KEY: &str = "server-dialog:error";
const SERVER_DIALOG_NOTIFICATION_TOP_PX: f32 = 51.0;

#[derive(Clone, Debug)]
pub enum ServerDialogMode {
    Add,
    Edit { server_id: String },
}

pub struct AddServerDialogState {
    mode: ServerDialogMode,
    protocol: Protocol,
    address: Entity<Editor>,
    port: Entity<Editor>,
    path: Entity<Editor>,
    username: Entity<Editor>,
    password: Entity<Editor>,
    is_submitting: bool,
    notifications: NotificationQueue<&'static str>,
}

impl AddServerDialogState {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::new_add(cx)
    }

    pub fn new_add(cx: &mut Context<Self>) -> Self {
        Self::new_with_values(
            ServerDialogMode::Add,
            Protocol::Https,
            String::new(),
            Protocol::Https.default_port().to_string(),
            String::new(),
            String::new(),
            String::new(),
            cx,
        )
    }

    pub fn new_edit(server: &CachedServer, cx: &mut Context<Self>) -> Self {
        Self::new_with_values(
            ServerDialogMode::Edit {
                server_id: server.id.clone(),
            },
            server.endpoint.protocol,
            server.endpoint.address_input_value(),
            server.endpoint.port.to_string(),
            server.endpoint.path.clone(),
            server.username.clone(),
            server.password.clone(),
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_values(
        mode: ServerDialogMode,
        protocol: Protocol,
        address: String,
        port: String,
        path: String,
        username: String,
        password: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let address_input = cx.new(|cx| {
            Editor::new("服务器地址", cx)
                .default_value(address)
                .borderless()
        });
        let port_input = cx.new(|cx| {
            Editor::new("端口", cx)
                .default_value(port)
                .digits_only()
                .max_chars(5)
        });
        let path_input = cx.new(|cx| Editor::new("可空", cx).default_value(path));
        let username_input = cx.new(|cx| Editor::new("用户名", cx).default_value(username));
        let password_input = cx.new(|cx| {
            Editor::new("密码", cx)
                .default_value(password)
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
            mode,
            protocol,
            address: address_input,
            port: port_input,
            path: path_input,
            username: username_input,
            password: password_input,
            is_submitting: false,
            notifications: NotificationQueue::default(),
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) -> Option<AddServerSubmission> {
        if self.is_submitting {
            return None;
        }

        self.notifications.clear();

        let protocol = self.protocol;
        let address = self.address.read(cx).value();
        let port = self.port.read(cx).value();
        let path = self.path.read(cx).value();
        let username = self.username.read(cx).value();
        let password = self.password.read(cx).value();

        match validate_server_submission(protocol, &address, &port, &path, &username, &password) {
            Ok(submission) => Some(submission),
            Err(error) => {
                self.push_error_notification(error, cx);
                None
            }
        }
    }

    pub fn edit_server_id(&self) -> Option<String> {
        match &self.mode {
            ServerDialogMode::Add => None,
            ServerDialogMode::Edit { server_id } => Some(server_id.clone()),
        }
    }

    pub fn set_submitting(&mut self, is_submitting: bool, cx: &mut Context<Self>) {
        self.is_submitting = is_submitting;
        cx.notify();
    }

    pub fn push_error_notification(
        &mut self,
        error: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let id = self
            .notifications
            .push(SERVER_DIALOG_ERROR_NOTIFICATION_KEY, error.into());
        cx.notify();

        cx.spawn(async move |dialog, cx| {
            cx.background_executor().timer(NOTIFICATION_AUTOHIDE).await;
            dialog
                .update(cx, |dialog, cx| {
                    if dialog.notifications.remove(id) {
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    fn auto_format_full_url(&mut self, cx: &mut Context<Self>) {
        let address = self.address.read(cx).value();
        let Some(endpoint) = parsed_full_url_endpoint(&address) else {
            return;
        };

        self.protocol = endpoint.protocol;

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

        cx.notify();
    }

    pub fn render_layer(
        &self,
        dialog: Entity<Self>,
        rounded_window: bool,
        on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_submit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let (title, submit_label) = match &self.mode {
            ServerDialogMode::Add => ("添加服务器", "添加"),
            ServerDialogMode::Edit { .. } => ("编辑服务器", "保存"),
        };

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
            .when(rounded_window, |this| {
                this.rounded(theme.radius_lg).overflow_hidden()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(560.0))
                    .gap_5()
                    .rounded(theme.radius_lg)
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
                            .child(title),
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
                                    submit_label,
                                    true,
                                    self.is_submitting,
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

        div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .child(field(
                "服务器地址",
                address_input(
                    self.address.clone(),
                    format!("{}://", self.protocol.scheme()),
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
                        protocol_selector(dialog.clone(), self.protocol, cx),
                        cx,
                    )))
                    .child(div().flex_1().child(field("端口", self.port.clone(), cx))),
            )
            .child(field("路径", self.path.clone(), cx))
            .child(field("用户名", self.username.clone(), cx))
            .child(field("密码", self.password.clone(), cx))
    }

    fn select_protocol(&mut self, protocol: Protocol, cx: &mut Context<Self>) {
        if self.protocol == protocol {
            return;
        }

        let previous_default = self.protocol.default_port();
        let next_default = protocol.default_port();
        self.protocol = protocol;

        self.port.update(cx, |port, cx| {
            let value = port.value();
            if value.is_empty() || value.as_ref() == previous_default {
                port.set_value(next_default, cx);
            }
        });
        cx.notify();
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

fn validate_server_submission(
    protocol: Protocol,
    address: &str,
    port: &str,
    path: &str,
    username: &str,
    password: &str,
) -> Result<AddServerSubmission, SharedString> {
    let endpoint = ServerEndpoint::parse_user_input(protocol, address, port, path)
        .map_err(|error| -> SharedString { error.to_string().into() })?;
    let username = username.trim();
    let password = password.trim();

    if username.is_empty() {
        return Err("请输入用户名".into());
    }
    if password.is_empty() {
        return Err("请输入密码".into());
    }

    Ok(AddServerSubmission {
        endpoint,
        username: username.to_string(),
        password: password.to_string(),
    })
}

fn parsed_full_url_endpoint(address: &str) -> Option<ServerEndpoint> {
    let address = address.trim();
    let lower_address = address.to_ascii_lowercase();
    if !lower_address.starts_with("http://") && !lower_address.starts_with("https://") {
        return None;
    }

    ServerEndpoint::parse_user_input(Protocol::Https, address, "", "").ok()
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
        .h(px(34.0))
        .w_full()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.input_background)
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
    dialog: Entity<AddServerDialogState>,
    selected: Protocol,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(34.0))
        .w_full()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.input_background)
        .p(px(2.0))
        .gap_0p5()
        .child(protocol_button(
            dialog.clone(),
            Protocol::Http,
            selected == Protocol::Http,
            cx,
        ))
        .child(protocol_button(
            dialog,
            Protocol::Https,
            selected == Protocol::Https,
            cx,
        ))
}

fn protocol_button(
    dialog: Entity<AddServerDialogState>,
    protocol: Protocol,
    selected: bool,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .id(protocol.label())
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
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
        .on_click(move |_, _, cx| {
            dialog.update(cx, |dialog, cx| dialog.select_protocol(protocol, cx));
        })
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
        .rounded(px(8.0))
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
mod tests {
    use super::*;

    #[test]
    fn parses_full_url_for_auto_formatting() {
        let endpoint = parsed_full_url_endpoint(" http://example.com:8096/custom ").unwrap();

        assert_eq!(endpoint.protocol, Protocol::Http);
        assert_eq!(endpoint.address_input_value(), "example.com");
        assert_eq!(endpoint.port, 8096);
        assert_eq!(endpoint.path, "/custom");
    }

    #[test]
    fn parses_full_url_without_port_using_scheme_default() {
        let endpoint = parsed_full_url_endpoint("https://example.com").unwrap();

        assert_eq!(endpoint.protocol, Protocol::Https);
        assert_eq!(endpoint.port, 443);
        assert_eq!(endpoint.path, "");
    }

    #[test]
    fn ignores_non_full_url_for_auto_formatting() {
        assert!(parsed_full_url_endpoint("example.com:8096").is_none());
        assert!(parsed_full_url_endpoint("ftp://example.com").is_none());
    }

    #[test]
    fn validates_address_before_credentials() {
        let error = validate_server_submission(Protocol::Https, "", "443", "", "", "")
            .expect_err("blank address should fail validation first");

        assert_eq!(error.as_ref(), "请输入服务器地址");
    }

    #[test]
    fn validates_username_before_password() {
        let error = validate_server_submission(Protocol::Https, "example.com", "443", "", "", "")
            .expect_err("blank username should fail validation before password");

        assert_eq!(error.as_ref(), "请输入用户名");

        let error =
            validate_server_submission(Protocol::Https, "example.com", "443", "", "user", "")
                .expect_err("blank password should fail after username is valid");
        assert_eq!(error.as_ref(), "请输入密码");
    }

    #[test]
    fn valid_submission_trims_credentials() {
        let submission = validate_server_submission(
            Protocol::Https,
            "example.com",
            "443",
            "",
            " user ",
            " password ",
        )
        .expect("valid fields should produce a submission");

        assert_eq!(submission.endpoint.address, "example.com");
        assert_eq!(submission.username, "user");
        assert_eq!(submission.password, "password");
    }
}
