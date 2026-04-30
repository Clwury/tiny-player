use anyhow::Result;
use reqwest::Method;
use serde::Deserialize;
use tracing::instrument;

use crate::server::AddServerSubmission;

use super::EmbyClient;

impl EmbyClient {
    #[instrument(skip(self, submission), fields(server = %submission.endpoint.display_url()))]
    pub fn public_system_info(&self, submission: &AddServerSubmission) -> Result<PublicSystemInfo> {
        self.send_public_json(
            &submission.endpoint,
            Method::GET,
            &["System", "Info", "Public"],
            "获取 Emby 服务器信息失败",
            "解析 Emby 服务器信息响应失败",
        )
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PublicSystemInfo {
    pub server_name: Option<String>,
    pub id: Option<String>,
}
