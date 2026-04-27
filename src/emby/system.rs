use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tracing::{debug, instrument};

use crate::server::AddServerSubmission;

use super::{CLIENT_NAME, EmbyClient, VERSION, log_secrets};

impl EmbyClient {
    #[instrument(skip(self, submission), fields(server = %submission.endpoint.display_url()))]
    pub fn public_system_info(&self, submission: &AddServerSubmission) -> Result<PublicSystemInfo> {
        let mut url = submission.endpoint.base_url()?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("服务器地址不能作为 API 基础地址"))?
            .pop_if_empty()
            .extend(["System", "Info", "Public"]);

        debug!(method = "GET", url = %url, "sending Emby public system info request");
        let response = self
            .http
            .get(url)
            .header("User-Agent", format!("{CLIENT_NAME}/{VERSION}"))
            .send()
            .context("连接 Emby 服务器失败")?;

        let status = response.status();
        let response_headers = format!("{:?}", response.headers());
        let response_body = response.text().context("读取 Emby 服务器信息响应失败")?;
        debug!(status = %status, "received Emby public system info response");
        if log_secrets() {
            debug!(
                status = %status,
                headers = %response_headers,
                body = %response_body,
                "full Emby public system info response"
            );
        }

        if !status.is_success() {
            bail!("获取 Emby 服务器信息失败：HTTP {status} {response_body}");
        }

        serde_json::from_str::<PublicSystemInfo>(&response_body)
            .context("解析 Emby 服务器信息响应失败")
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PublicSystemInfo {
    pub server_name: Option<String>,
    pub id: Option<String>,
}
