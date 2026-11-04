//! Phone recent apps, full app search and icons through Mirror's SDK service.
//!
//! The official desktop adapter keeps one app-search channel per device
//! session. Reopening it for every query leaves the phone's app-search handler
//! with repeated channel setup and was the remaining lifecycle divergence from
//! the official client.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, bail, ensure};

use crate::{
    Result,
    protocol::messages::mirror::{
        FrameDecoder, RecentApp, RecentAppsRequest, SearchAppRequest, TYPE_RECENT_APPS,
        TYPE_SEARCH_APP_RESPONSE, decode_search_response,
    },
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

#[derive(Clone, Copy)]
enum AppChannel {
    Recent,
    Cast,
}

#[derive(Default)]
struct AppRuntime {
    recent: tokio::sync::Mutex<Option<Channel>>,
    cast: tokio::sync::Mutex<Option<Channel>>,
    session: AtomicU64,
}

impl AppRuntime {
    fn next_session(&self) -> u64 {
        self.session.fetch_add(1, Ordering::SeqCst) + 1
    }

    async fn channel(&self, session: &Session, kind: AppChannel) -> Result<Channel> {
        let slot = match kind {
            AppChannel::Recent => &self.recent,
            AppChannel::Cast => &self.cast,
        };
        let mut slot = slot.lock().await;
        if let Some(channel) = slot.as_ref().filter(|channel| !channel.is_closed()) {
            return Ok(channel.clone());
        }
        *slot = None;
        let channel = match kind {
            AppChannel::Recent => {
                session
                    .open_channel(
                        RECENT_APPS_SERVICE,
                        crate::session::TRUST_PAIR,
                        SdkPlatform::Linux,
                    )
                    .await?
            }
            AppChannel::Cast => session.open_cast().await?,
        };
        tracing::info!(
            "[apps] opened shared {} channel id={}",
            match kind {
                AppChannel::Recent => "recent-apps",
                AppChannel::Cast => "mirror",
            },
            channel.id()
        );
        *slot = Some(channel.clone());
        Ok(channel)
    }

    async fn invalidate(&self, channel: &Channel, kind: AppChannel) {
        let slot = match kind {
            AppChannel::Recent => &self.recent,
            AppChannel::Cast => &self.cast,
        };
        let mut slot = slot.lock().await;
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
        kind: AppChannel,
    ) -> Result<Vec<u8>> {
        for attempt in 0..2 {
            let channel = self.channel(session, kind).await?;
            let mut incoming = channel.subscribe();
            if let Err(error) = channel.send(&packet).await {
                self.invalidate(&channel, kind).await;
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
                    self.invalidate(&channel, kind).await;
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
        let payload = runtime
            .request(self, packet, TYPE_RECENT_APPS, AppChannel::Recent)
            .await?;
        let apps = RecentAppsRequest::decode_response(&payload)?;
        tracing::info!("[apps] recent count={}", apps.len());
        Ok(phone_apps(apps))
    }

    /// Search the phone's installed apps. The special keyword `*` returns the
    /// complete list, matching the phone's `AppSearchHelper` wildcard path.
    pub async fn search_apps(&self, keyword: &str) -> Result<Vec<PhoneApp>> {
        ensure!(!keyword.trim().is_empty(), "应用搜索关键字不能为空");
        let runtime = self.extension::<AppRuntime>();
        let session_id = runtime.next_session();
        let packet = SearchAppRequest {
            session_id,
            keyword: keyword.to_owned(),
        }
        .encode();
        let payload = match runtime
            .request(
                self,
                packet.clone(),
                TYPE_SEARCH_APP_RESPONSE,
                AppChannel::Cast,
            )
            .await
        {
            Ok(payload) => payload,
            Err(cast_error) => {
                tracing::info!(
                    "[apps] mirror channel search failed, retrying recent-apps: {cast_error:#}"
                );
                runtime
                    .request(self, packet, TYPE_SEARCH_APP_RESPONSE, AppChannel::Recent)
                    .await?
            }
        };
        let apps = decode_search_response(&payload, session_id)?;
        tracing::info!("[apps] search keyword={keyword:?} count={}", apps.len());
        Ok(phone_apps(apps))
    }

    /// Complete installed-app list via the phone's wildcard app search.
    pub async fn all_apps(&self) -> Result<Vec<PhoneApp>> {
        self.search_apps("*").await
    }
}
