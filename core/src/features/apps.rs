//! Phone recent apps, full app search and icons through Mirror's SDK service.
//!
//! The official desktop adapter keeps one app-search channel per device
//! session. Reopening it for every query leaves the phone's app-search handler
//! with repeated channel setup and was the remaining lifecycle divergence from
//! the official client.

use std::time::Duration;

use anyhow::{Context, bail};

use crate::{
    Result,
    protocol::messages::mirror::{FrameDecoder, RecentApp, RecentAppsRequest, TYPE_RECENT_APPS},
    session::{Channel, ChannelEvent, SdkPlatform, Session},
};

pub const RECENT_APPS_SERVICE: &str = crate::protocol::service::RECENT_APPS;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct PhoneApp {
    pub package: String,
    pub name: String,
    /// Encoded image bytes supplied by the phone.
    pub icon: Vec<u8>,
}

#[derive(Default)]
struct AppRuntime {
    channel: tokio::sync::Mutex<Option<Channel>>,
}

impl AppRuntime {
    async fn channel(&self, session: &Session) -> Result<Channel> {
        let mut slot = self.channel.lock().await;
        if let Some(channel) = slot.as_ref().filter(|channel| !channel.is_closed()) {
            return Ok(channel.clone());
        }
        *slot = None;
        let channel = session
            .open_channel(
                RECENT_APPS_SERVICE,
                crate::session::TRUST_PAIR,
                SdkPlatform::Linux,
            )
            .await?;
        tracing::info!("[apps] opened app channel id={}", channel.id());
        *slot = Some(channel.clone());
        Ok(channel)
    }

    async fn invalidate(&self, channel: &Channel) {
        let mut slot = self.channel.lock().await;
        if slot
            .as_ref()
            .is_some_and(|value| value.id() == channel.id())
        {
            *slot = None;
            let _ = channel.close().await;
        }
    }

    async fn request(
        &self,
        session: &Session,
        packet: Vec<u8>,
        response_type: u8,
    ) -> Result<Vec<u8>> {
        for attempt in 0..2 {
            let channel = tokio::time::timeout(RESPONSE_TIMEOUT, self.channel(session))
                .await
                .context("打开应用通道超时")??;
            let mut incoming = channel.subscribe();
            tracing::info!(
                "[apps] send type={} bytes={} channel={}",
                packet.first().copied().unwrap_or(0),
                packet.len(),
                channel.id()
            );
            if let Err(error) = channel.send(&packet).await {
                self.invalidate(&channel).await;
                if attempt == 0 {
                    continue;
                }
                return Err(error);
            }
            let result = tokio::time::timeout(RESPONSE_TIMEOUT, async {
                let mut decoder = FrameDecoder::default();
                loop {
                    let bytes = match incoming.recv().await.context("等待手机应用列表失败")?
                    {
                        ChannelEvent::Data(bytes) => bytes,
                        ChannelEvent::Closed(reason) => bail!("应用列表通道已关闭：{reason}"),
                    };
                    for frame in decoder.feed(&bytes)? {
                        tracing::info!(
                            "[apps] recv type={} bytes={} channel={}",
                            frame.message_type,
                            frame.payload.len(),
                            channel.id()
                        );
                        if frame.message_type == response_type {
                            return Ok(frame.payload);
                        }
                    }
                }
            })
            .await
            .context("等待手机应用列表超时")
            .and_then(|result| result);
            match result {
                Ok(payload) => return Ok(payload),
                Err(error) => {
                    let closed = channel.is_closed();
                    self.invalidate(&channel).await;
                    if attempt == 0 && closed {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        unreachable!("app request retry loop always returns")
    }
}

fn phone_apps(apps: Vec<RecentApp>) -> Vec<PhoneApp> {
    apps.into_iter()
        .map(|app| PhoneApp {
            package: app.package,
            name: app.name,
            icon: app.icon,
        })
        .collect()
}

impl Session {
    /// Returns the phone's recent apps, currently up to four in the official
    /// service. Pass an entry's package to open_app.
    pub async fn recent_apps(&self) -> Result<Vec<PhoneApp>> {
        let runtime = self.extension::<AppRuntime>();
        let packet = RecentAppsRequest.encode();
        let payload = runtime.request(self, packet, TYPE_RECENT_APPS).await?;
        let apps = RecentAppsRequest::decode_response(&payload)?;
        tracing::info!("[apps] recent count={}", apps.len());
        Ok(phone_apps(apps))
    }
}
