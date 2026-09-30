//! Dialog business state and input rules. Editors own transient text; this
//! controller owns mode, protocol and submission status for one open dialog.
//! Replacing/closing the dialog drops it; all updates enter through intents.
use crate::server::{AddServerSubmission, CachedServer, Protocol, ServerEndpoint};

/// Initial editor values only: no session tokens, counts, icons or other catalog
/// metadata crosses into the dialog. Do not log these credential-bearing props.
pub(crate) struct ServerFormProps {
    pub(crate) edit_server_id: Option<String>,
    pub(crate) protocol: Protocol,
    pub(crate) address: String,
    pub(crate) port: String,
    pub(crate) path: String,
    pub(crate) username: String,
    pub(crate) password: String,
}

impl Default for ServerFormProps {
    fn default() -> Self {
        Self {
            edit_server_id: None,
            protocol: Protocol::Https,
            address: String::new(),
            port: Protocol::Https.default_port().into(),
            path: String::new(),
            username: String::new(),
            password: String::new(),
        }
    }
}

impl From<&CachedServer> for ServerFormProps {
    fn from(server: &CachedServer) -> Self {
        Self {
            edit_server_id: Some(server.id.clone()),
            protocol: server.endpoint.protocol,
            address: server.endpoint.address_input_value(),
            port: server.endpoint.port.to_string(),
            path: server.endpoint.path.clone(),
            username: server.username.clone(),
            password: server.password.clone(),
        }
    }
}

pub(crate) struct ServerFormInput<'a> {
    pub(crate) address: &'a str,
    pub(crate) port: &'a str,
    pub(crate) path: &'a str,
    pub(crate) username: &'a str,
    pub(crate) password: &'a str,
}

pub(crate) enum ServerFormIntent<'a> {
    SelectProtocol { protocol: Protocol, port: &'a str },
    AddressChanged(&'a str),
    Submit(ServerFormInput<'a>),
    SetSubmitting(bool),
}

pub(crate) enum ServerFormUpdate {
    Ignored,
    Changed,
    PortChanged(&'static str),
    EndpointChanged(ServerEndpoint),
    Submission(Result<AddServerSubmission, String>),
}

pub(crate) struct ServerFormView<'a> {
    pub(crate) edit_server_id: Option<&'a str>,
    pub(crate) protocol: Protocol,
    pub(crate) is_submitting: bool,
    pub(crate) title: &'static str,
    pub(crate) submit_label: &'static str,
}

pub(crate) struct ServerFormController {
    edit_server_id: Option<String>,
    protocol: Protocol,
    is_submitting: bool,
}

impl ServerFormController {
    pub(crate) fn new(props: &ServerFormProps) -> Self {
        Self {
            edit_server_id: props.edit_server_id.clone(),
            protocol: props.protocol,
            is_submitting: false,
        }
    }

    pub(crate) fn view(&self) -> ServerFormView<'_> {
        let (title, submit_label) = if self.edit_server_id.is_some() {
            ("编辑服务器", "保存")
        } else {
            ("添加服务器", "添加")
        };
        ServerFormView {
            edit_server_id: self.edit_server_id.as_deref(),
            protocol: self.protocol,
            is_submitting: self.is_submitting,
            title,
            submit_label,
        }
    }

    pub(crate) fn dispatch(&mut self, intent: ServerFormIntent<'_>) -> ServerFormUpdate {
        match intent {
            ServerFormIntent::SelectProtocol { protocol, port } => {
                if self.protocol == protocol {
                    return ServerFormUpdate::Ignored;
                }
                let replace_port = port.is_empty() || port == self.protocol.default_port();
                self.protocol = protocol;
                if replace_port {
                    ServerFormUpdate::PortChanged(protocol.default_port())
                } else {
                    ServerFormUpdate::Changed
                }
            }
            ServerFormIntent::AddressChanged(address) => {
                let Some(endpoint) = parsed_full_url_endpoint(address) else {
                    return ServerFormUpdate::Ignored;
                };
                self.protocol = endpoint.protocol;
                ServerFormUpdate::EndpointChanged(endpoint)
            }
            ServerFormIntent::Submit(input) => {
                if self.is_submitting {
                    return ServerFormUpdate::Ignored;
                }
                ServerFormUpdate::Submission(validate_server_submission(
                    self.protocol,
                    input.address,
                    input.port,
                    input.path,
                    input.username,
                    input.password,
                ))
            }
            ServerFormIntent::SetSubmitting(submitting) => {
                self.is_submitting = submitting;
                ServerFormUpdate::Changed
            }
        }
    }
}

fn validate_server_submission(
    protocol: Protocol,
    address: &str,
    port: &str,
    path: &str,
    username: &str,
    password: &str,
) -> Result<AddServerSubmission, String> {
    let endpoint = ServerEndpoint::parse_user_input(protocol, address, port, path)
        .map_err(|error| error.to_string())?;
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

#[cfg(test)]
mod tests;
