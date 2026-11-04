//! Session-side P2P upgrade: negotiate the phone GO, authenticate the new
//! physical link and retain the temporary network for the session lifetime.
use crate::{
    Result,
    protocol::messages::upgrade::UpgradeFrame,
    session::{Session, SessionEvent},
    transport::{
        p2p::wifi::P2pLink,
        upgrade::{P2pUpgradeLink, UpgradeClient},
    },
};
use anyhow::{Context, bail};
use std::time::Duration;

impl UpgradeClient {
    pub(crate) async fn join(
        &self,
        session: &Session,
        deadline: tokio::time::Instant,
    ) -> Result<P2pUpgradeLink> {
        let mut events = session.events();
        let logical_id = session
            .begin_link_upgrade(
                &self.query(),
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            )
            .await?;
        tracing::info!("[nearby] P2P server query sent: role=gc:go");
        let result = async {
            let group = tokio::time::timeout_at(deadline, async {
                loop {
                    match events.recv().await? {
                        SessionEvent::LinkUpgrade { full, frame, .. } if full == logical_id => {
                            if let Some(group) = Self::server_available(&frame)? {
                                return Ok(group);
                            }
                            tracing::info!(
                                "[nearby] P2P negotiation frame received: type={}",
                                UpgradeFrame::decode(&frame)?.kind
                            );
                        }
                        SessionEvent::LinkUpgradeFailed { full, reason } if full == logical_id => {
                            bail!("P2P 协商失败：{reason}");
                        }
                        SessionEvent::Disconnected(reason) => bail!("BLE 连接已断开：{reason}"),
                        _ => {}
                    }
                }
            })
            .await
            .context("等待手机 P2P 服务配置超时")??;
            let introduction = self.introduction(session.account().short_id().as_bytes(), &group);
            let endpoint = group.endpoint;
            let (lease, interface, address) = tokio::time::timeout_at(deadline, async {
                let joining = self.oob().join(
                    &group,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                );
                tokio::pin!(joining);
                loop {
                    tokio::select! {
                        result = &mut joining => return result,
                        event = events.recv() => match event? {
                            SessionEvent::LinkUpgradeFailed { full, reason } if full == logical_id => {
                                bail!("P2P 入组期间手机结束升级：{reason}");
                            }
                            SessionEvent::Disconnected(reason) => {
                                bail!("P2P 入组期间 BLE 连接已结束：{reason}");
                            }
                            _ => {},
                        },
                    }
                }
            })
            .await
            .context("等待 P2P 入组超时")??;
            let link = P2pLink::oob(lease, interface, address);
            Ok(P2pUpgradeLink {
                link,
                endpoint,
                logical_id,
                introduction,
            })
        }
        .await;
        if result.is_err() {
            let _ = session.cancel_link_upgrade(logical_id).await;
        }
        result
    }

    /// Join the phone GO, authenticate the upgraded physical link and retain
    /// the temporary network for the lifetime of the session.
    pub async fn upgrade(&self, session: &Session, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let prepared = self.join(session, deadline).await?;
        let result = tokio::time::timeout_at(
            deadline,
            session.upgrade_physical(
                prepared.link.local_ip,
                *prepared.endpoint.ip(),
                prepared.endpoint.port(),
                prepared.logical_id,
                &prepared.introduction,
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            ),
        )
        .await
        .context("等待 P2P 链路认证及 SDK 就绪超时")
        .and_then(|result| result);
        if let Err(error) = result {
            let _ = session.cancel_link_upgrade(prepared.logical_id).await;
            let _ = prepared.link.close().await;
            return Err(error);
        }
        session.retain_p2p_link(prepared.link);
        tracing::info!("[nearby] P2P upgrade complete: authenticated CAST data channel ready");
        Ok(())
    }
}
