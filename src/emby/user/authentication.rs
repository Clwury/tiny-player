use super::*;

impl EmbyClient {
    #[instrument(skip(self, submission), fields(server = %submission.endpoint.display_url(), username = %submission.username))]
    pub fn authenticate_by_name(&self, submission: &AddServerSubmission) -> Result<AuthSession> {
        let mut url = submission.endpoint.base_url()?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("服务器地址不能作为 API 基础地址"))?
            .pop_if_empty()
            .extend(["Users", "AuthenticateByName"]);

        let authorization = self.authorization_header();
        let request_body = AuthenticateUserByName {
            username: &submission.username,
            password: &submission.password,
            pw: &submission.password,
        };
        let request_body_json =
            serde_json::to_string(&request_body).context("序列化 Emby 认证请求失败")?;
        let include_secrets = log_secrets();

        debug!(method = "POST", url = %url_for_log(&url, include_secrets), "sending Emby authentication request");
        if include_secrets {
            debug!(
                method = "POST",
                url = %url,
                x_emby_authorization = %authorization,
                content_type = "application/json",
                body = %request_body_json,
                "full Emby authentication request"
            );
        }

        let response = self
            .http
            .post(url)
            .header("X-Emby-Authorization", authorization)
            .header("Content-Type", "application/json")
            .header("User-Agent", format!("{CLIENT_NAME}/{VERSION}"))
            .body(request_body_json)
            .send()
            .map_err(|error| anyhow!("连接 Emby 服务器失败：{}", error.without_url()))?;

        let status = response.status();
        let response_headers = include_secrets.then(|| format!("{:?}", response.headers()));
        let response_body = response.text().context("读取 Emby 认证响应失败")?;
        debug!(status = %status, "received Emby authentication response");
        if let Some(response_headers) = response_headers {
            debug!(
                status = %status,
                headers = %response_headers,
                body = %response_body,
                "full Emby authentication response"
            );
        }

        if !status.is_success() {
            bail!("Emby 认证失败：HTTP {status} {response_body}");
        }

        let session = serde_json::from_str::<AuthSession>(&response_body)
            .context("解析 Emby 认证响应失败")?;
        if include_secrets {
            debug!(access_token = %session.access_token, "received Emby access token");
        }

        Ok(session)
    }
}
