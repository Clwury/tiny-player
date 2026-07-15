use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px, svg,
};

use crate::{
    server::{AddServerSubmission, CachedServer, Protocol, ServerEndpoint},
    theme,
};

use super::text_input::{TextInput, TextInputEvent};

#[derive(Default)]
struct AddServerErrors {
    address: Option<SharedString>,
    port: Option<SharedString>,
    username: Option<SharedString>,
    password: Option<SharedString>,
}

#[derive(Clone, Debug)]
pub enum ServerDialogMode {
    Add,
    Edit { server_id: String },
}

pub struct AddServerDialogState {
    mode: ServerDialogMode,
    protocol: Protocol,
    address: Entity<TextInput>,
    port: Entity<TextInput>,
    path: Entity<TextInput>,
    username: Entity<TextInput>,
    password: Entity<TextInput>,
    show_password: bool,
    is_submitting: bool,
    form_error: Option<SharedString>,
    errors: AddServerErrors,
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
            TextInput::new("服务器地址", cx)
                .default_value(address)
                .borderless()
        });
        let port_input = cx.new(|cx| {
            TextInput::new("端口", cx)
                .default_value(port)
                .digits_only()
                .max_chars(5)
        });
        let path_input = cx.new(|cx| TextInput::new("可空", cx).default_value(path));
        let username_input = cx.new(|cx| TextInput::new("用户名", cx).default_value(username));
        let password_input = cx.new(|cx| {
            TextInput::new("密码", cx)
                .default_value(password)
                .masked(true)
        });

        cx.subscribe(
            &address_input,
            |dialog: &mut AddServerDialogState, _, event, cx| match event {
                TextInputEvent::Changed => dialog.auto_format_full_url(cx),
                TextInputEvent::Submitted => {}
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
            show_password: false,
            is_submitting: false,
            form_error: None,
            errors: AddServerErrors::default(),
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) -> Option<AddServerSubmission> {
        if self.is_submitting {
            return None;
        }

        self.errors = AddServerErrors::default();
        self.form_error = None;

        let protocol = self.protocol;
        let address = self.address.read(cx).value();
        let port = self.port.read(cx).value();
        let path = self.path.read(cx).value();
        let username = self.username.read(cx).value();
        let password = self.password.read(cx).value();

        let username_trimmed = username.trim();
        let password_trimmed = password.trim();

        let endpoint = match ServerEndpoint::parse_user_input(protocol, &address, &port, &path) {
            Ok(endpoint) => Some(endpoint),
            Err(error) => {
                let message = error.to_string();
                if message.contains("端口") {
                    self.errors.port = Some(message.into());
                } else {
                    self.errors.address = Some(message.into());
                }
                None
            }
        };

        if username_trimmed.is_empty() {
            self.errors.username = Some("请输入用户名".into());
        }

        if password_trimmed.is_empty() {
            self.errors.password = Some("请输入密码".into());
        }

        if self.has_errors() {
            cx.notify();
            return None;
        }

        Some(AddServerSubmission {
            endpoint: endpoint.expect("endpoint was validated"),
            username: username_trimmed.to_string(),
            password: password_trimmed.to_string(),
        })
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

    pub fn set_form_error(&mut self, error: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.form_error = Some(error.into());
        cx.notify();
    }

    pub fn clear_form_error(&mut self, cx: &mut Context<Self>) {
        self.form_error = None;
        cx.notify();
    }

    fn auto_format_full_url(&mut self, cx: &mut Context<Self>) {
        let address = self.address.read(cx).value();
        let Some(endpoint) = parsed_full_url_endpoint(&address) else {
            return;
        };

        self.protocol = endpoint.protocol;
        self.errors.address = None;
        self.errors.port = None;

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
            ServerDialogMode::Add => (
                "添加服务器",
                if self.is_submitting {
                    "添加中..."
                } else {
                    "添加"
                },
            ),
            ServerDialogMode::Edit { .. } => (
                "编辑服务器",
                if self.is_submitting {
                    "保存中..."
                } else {
                    "保存"
                },
            ),
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
                    .child(self.render_form(dialog, cx))
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
    }

    fn render_form(&self, dialog: Entity<Self>, cx: &App) -> impl IntoElement {
        let theme = theme::get(cx);
        let port = self.port.read(cx).value();
        let password_icon = if self.show_password {
            "icons/eye-off.svg"
        } else {
            "icons/eye.svg"
        };
        let toggle_dialog = dialog.clone();

        div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .when_some(self.form_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
            .child(field(
                "服务器地址",
                address_input(
                    self.address.clone(),
                    format!("{}://", self.protocol.scheme()),
                    format!(":{}", port),
                    cx,
                ),
                self.errors.address.clone(),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().w(px(148.0)).child(field(
                        "协议",
                        protocol_selector(dialog.clone(), self.protocol, cx),
                        None,
                        cx,
                    )))
                    .child(div().flex_1().child(field(
                        "端口",
                        self.port.clone(),
                        self.errors.port.clone(),
                        cx,
                    ))),
            )
            .child(field("路径", self.path.clone(), None, cx))
            .child(field(
                "用户名",
                self.username.clone(),
                self.errors.username.clone(),
                cx,
            ))
            .child(field(
                "密码",
                password_input(self.password.clone(), password_icon, toggle_dialog, cx),
                self.errors.password.clone(),
                cx,
            ))
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

    fn toggle_password(&mut self, cx: &mut Context<Self>) {
        self.show_password = !self.show_password;
        let masked = !self.show_password;
        self.password.update(cx, |password, cx| {
            password.set_masked(masked, cx);
        });
        cx.notify();
    }

    fn has_errors(&self) -> bool {
        self.errors.address.is_some()
            || self.errors.port.is_some()
            || self.errors.username.is_some()
            || self.errors.password.is_some()
    }
}

fn parsed_full_url_endpoint(address: &str) -> Option<ServerEndpoint> {
    let address = address.trim();
    let lower_address = address.to_ascii_lowercase();
    if !lower_address.starts_with("http://") && !lower_address.starts_with("https://") {
        return None;
    }

    ServerEndpoint::parse_user_input(Protocol::Https, address, "", "").ok()
}

fn field(
    label: &'static str,
    input: impl IntoElement,
    error: Option<SharedString>,
    cx: &App,
) -> impl IntoElement {
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
        .when_some(error, |this, error| {
            this.child(div().text_xs().text_color(theme.error).child(error))
        })
}

fn address_input(
    input: Entity<TextInput>,
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

fn password_input(
    input: Entity<TextInput>,
    icon_path: &'static str,
    dialog: Entity<AddServerDialogState>,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .items_center()
        .gap_1()
        .child(div().flex_1().child(input))
        .child(
            div()
                .id("toggle-password-visibility")
                .flex()
                .size(px(34.0))
                .items_center()
                .justify_center()
                .rounded_md()
                .text_color(theme.foreground)
                .hover(move |style| style.bg(theme.secondary_hover))
                .child(
                    svg()
                        .path(icon_path)
                        .size(px(16.0))
                        .text_color(theme.foreground),
                )
                .on_click(move |_, _, cx| {
                    dialog.update(cx, |dialog, cx| dialog.toggle_password(cx));
                }),
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
        .when(selected, |this| this.bg(theme.secondary_hover))
        .hover(move |style| style.bg(theme.secondary_hover))
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
            theme.background
        } else {
            theme.foreground
        })
        .border_1()
        .border_color(if primary {
            theme.input_border_focused
        } else {
            theme.input_border
        })
        .bg(if primary {
            theme.foreground
        } else {
            theme.input_background
        })
        .when(disabled, |this| this.opacity(0.65))
        .hover(move |style| {
            if disabled {
                style
            } else if primary {
                style.bg(theme.foreground).text_color(theme.background)
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
}
