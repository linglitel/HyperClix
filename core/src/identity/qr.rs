//! Explicit user-initiated QR login. Creating a QrLogin contacts Xiaomi.
use super::{
    account::Credentials,
    http::{http_error, json_response, required},
};
use crate::{Result, common::storage::save_private};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{
    Client,
    cookie::{CookieStore, Jar},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub struct QrLogin {
    pub image_path: PathBuf,
    pub expires_in: Duration,
    directory: PathBuf,
    sid: String,
    polling_url: String,
    client: Client,
    cookies: Arc<Jar>,
    deadline: tokio::time::Instant,
}
impl QrLogin {
    pub async fn begin(directory: impl AsRef<Path>, sid: &str) -> Result<Self> {
        ensure!(
            ["miconnect", "micloud", "deviceinfo"].contains(&sid),
            "未知小米服务SID"
        );
        let cookies = Arc::new(Jar::default());
        let client = Client::builder()
            .cookie_provider(cookies.clone())
            .user_agent(format!("HyperClix/{} (Linux)", env!("CARGO_PKG_VERSION")))
            .build()?;
        let initial = json_response(
            client
                .get("https://account.xiaomi.com/pass/serviceLogin")
                .query(&[("sid", sid), ("_json", "true")])
                .send()
                .await
                .map_err(http_error)?,
        )
        .await?;
        let mut params = Vec::new();
        for key in ["sid", "qs", "callback", "serviceParam", "_sign"] {
            params.push((key, required(&initial, key)?.to_string()));
        }
        params.extend([
            ("_qrsize", "400".into()),
            ("_locale", "zh_CN".into()),
            ("_hasLogo", "false".into()),
        ]);
        let qr = json_response(
            client
                .get("https://account.xiaomi.com/longPolling/loginUrl")
                .query(&params)
                .send()
                .await
                .map_err(http_error)?,
        )
        .await?;
        ensure!(
            qr["code"].as_i64() == Some(0),
            "二维码初始化失败，code={}",
            qr["code"]
        );
        let directory = directory.as_ref().to_path_buf();
        let image_path = directory.join("qr.png");
        let image = client
            .get(required(&qr, "qr")?)
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?
            .bytes()
            .await
            .map_err(http_error)?;
        save_private(&image_path, &image).await?;
        let expires_in = Duration::from_secs(qr["timeout"].as_u64().context("二维码缺少有效期")?);
        Ok(Self {
            image_path,
            expires_in,
            directory,
            sid: sid.into(),
            polling_url: required(&qr, "lp")?.into(),
            client,
            cookies,
            deadline: tokio::time::Instant::now() + expires_in,
        })
    }
    pub async fn wait(self) -> Result<Credentials> {
        let authorized = loop {
            let remaining = self
                .deadline
                .saturating_duration_since(tokio::time::Instant::now());
            ensure!(!remaining.is_zero(), "二维码已过期");
            match self
                .client
                .get(&self.polling_url)
                .timeout(remaining.min(Duration::from_secs(50)))
                .send()
                .await
            {
                Ok(response) => break json_response(response).await?,
                Err(e) if e.is_timeout() => continue,
                Err(e) => return Err(http_error(e)),
            }
        };
        ensure!(
            authorized["code"].as_i64() == Some(0),
            "扫码授权未完成，code={}",
            authorized["code"]
        );
        let mut credentials = Credentials {
            sid: self.sid.clone(),
            user_id: authorized["userId"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| authorized["userId"].to_string()),
            c_user_id: authorized["cUserId"].as_str().unwrap_or_default().into(),
            security: required(&authorized, "ssecurity")?.into(),
            service_token: String::new(),
        };
        let mut exchange = reqwest::Url::parse(required(&authorized, "location")?)?;
        if self.sid != "miconnect" {
            let nonce = authorized["nonce"]
                .as_i64()
                .or_else(|| authorized["nonce"].as_str().and_then(|v| v.parse().ok()))
                .context("授权响应缺少nonce")?;
            let signature = STANDARD.encode(openssl::sha::sha1(
                format!("nonce={nonce}&{}", credentials.security).as_bytes(),
            ));
            exchange
                .query_pairs_mut()
                .append_pair("clientSign", &signature)
                .append_pair("_userIdNeedEncrypt", "true");
        }
        let final_url = self
            .client
            .get(exchange.clone())
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?
            .url()
            .clone();
        let mut cookie_headers = Vec::new();
        for url in [
            exchange,
            final_url,
            reqwest::Url::parse("https://idm.api.io.mi.com/")?,
        ] {
            if let Some(header) = self.cookies.cookies(&url) {
                cookie_headers.push(header.to_str()?.to_owned());
            }
        }
        credentials.service_token = service_token(&self.sid, &cookie_headers).unwrap_or_default();
        ensure!(
            !credentials.service_token.is_empty(),
            "授权回调没有返回 serviceToken"
        );
        save_private(
            &self.directory.join("credentials.json"),
            &serde_json::to_vec_pretty(&credentials)?,
        )
        .await?;
        Ok(credentials)
    }
}

fn service_token(sid: &str, headers: &[String]) -> Option<String> {
    let specific_name = format!("{sid}_serviceToken");
    let (mut specific, mut generic) = (None, None);
    for header in headers {
        for cookie in header.split(';') {
            if let Some((name, value)) = cookie.trim().split_once('=') {
                if name == specific_name {
                    specific = Some(value.to_owned());
                } else if name == "serviceToken" {
                    generic = Some(value.to_owned());
                }
            }
        }
    }
    specific.or(generic)
}

#[cfg(test)]
mod cookie_tests {
    use super::service_token;
    #[test]
    fn sid_specific_sts_token_wins_over_generic_sso_cookie() {
        let cookies = vec![
            "micloud_serviceToken=cloud-token; serviceToken=sso-token".into(),
            "serviceToken=later-sso".into(),
        ];
        assert_eq!(
            service_token("micloud", &cookies).as_deref(),
            Some("cloud-token")
        );
        assert_eq!(
            service_token("miconnect", &cookies).as_deref(),
            Some("later-sso")
        );
    }
}
