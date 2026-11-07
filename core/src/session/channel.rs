//! SDK byte channels and the shared CAST lease pool.
use super::{CastLease, CastShared, Channel, ChannelEvent, Command, MediaCommand};
use crate::{Result, protocol::messages::media};
use anyhow::{Context, ensure};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::{broadcast, oneshot};

async fn release_cast(shared: &Arc<CastShared>) -> Result<()> {
    if shared.users.fetch_sub(1, Ordering::AcqRel) == 1 {
        // The official ChannelsManager keeps one CAST channel per device
        // session and replays cached screen messages. Closing it after the
        // last screen makes the next create wait for a response that OS4
        // routes to the old logical channel.
        tracing::info!(
            "[cast={}] last screen released; retaining CAST for device session",
            shared.channel.id()
        );
    }
    Ok(())
}
impl Drop for CastLease {
    fn drop(&mut self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            let shared = self.shared.clone();
            self.runtime.spawn(async move {
                let _ = release_cast(&shared).await;
            });
        }
    }
}

impl Channel {
    pub fn id(&self) -> u32 {
        self.inner.id
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
            || self
                .lease
                .as_ref()
                .is_some_and(|lease| lease.released.load(Ordering::Acquire))
    }
    pub fn service(&self) -> &str {
        &self.inner.service
    }
    pub fn subscribe(&self) -> broadcast::Receiver<ChannelEvent> {
        if let Some(lease) = &self.lease {
            return lease
                .initial
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| self.inner.events.subscribe());
        }
        self.inner
            .initial
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| self.inner.events.subscribe())
    }
    pub async fn send(&self, data: &[u8]) -> Result<()> {
        self.send_raw(&media::application_data(data)).await
    }
    pub async fn send_raw(&self, data: &[u8]) -> Result<()> {
        ensure!(
            !self.inner.closed.load(Ordering::Acquire)
                && !self
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.released.load(Ordering::Acquire)),
            "SDK通道已关闭"
        );
        let (reply, wait) = oneshot::channel();
        self.inner
            .commands
            .send(MediaCommand::Send(data.to_vec(), reply))
            .await
            .context("SDK通道已关闭")?;
        wait.await?
    }
    pub async fn close(&self) -> Result<()> {
        if let Some(lease) = &self.lease {
            if lease.released.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            return release_cast(&lease.shared).await;
        }
        self.close_transport().await
    }
    pub(super) fn notify_closed(&self, reason: String) {
        self.inner.closed.store(true, Ordering::Release);
        let _ = self.inner.events.send(ChannelEvent::Closed(reason));
    }
    async fn close_transport(&self) -> Result<()> {
        self.inner.closed.store(true, Ordering::Release);
        let mut events = self.inner.events.subscribe();
        let (reply, wait) = oneshot::channel();
        if self
            .inner
            .control
            .send(Command::CloseChannel(self.id(), false, reply))
            .await
            .is_err()
        {
            return Ok(());
        }
        wait.await??;
        let closed = tokio::time::timeout(Duration::from_millis(1500), async {
            loop {
                match events.recv().await {
                    Ok(ChannelEvent::Closed(_)) | Err(_) => return true,
                    Ok(_) => {}
                }
            }
        })
        .await
        .unwrap_or(false);
        if !closed {
            let (reply, wait) = oneshot::channel();
            self.inner
                .control
                .send(Command::CloseChannel(self.id(), true, reply))
                .await
                .context("SDK通道已关闭")?;
            wait.await??;
        }
        Ok(())
    }
}
