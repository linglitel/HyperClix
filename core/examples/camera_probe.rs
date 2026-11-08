use anyhow::{Context, Result};
use hyperclix_core::{
    Account, ConnectOptions, Session,
    camera::CameraOptions,
    default_account_dir,
    discovery::{self, DeviceEndpoint},
};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let log = hyperclix_core::logging::init(hyperclix_core::paths::logs_dir(), "camera probe")?;
    println!("log {}", log.display());
    let account = Account::load(default_account_dir()).await?;
    let args: Vec<String> = std::env::args().collect();
    let iface = args.get(1).map(String::as_str).unwrap_or("wlp1s0");
    let local = discovery::interfaces()?
        .into_iter()
        .find(|item| item.name == iface)
        .with_context(|| format!("interface {iface} has no IPv4"))?
        .address;
    let endpoint = if let Some(value) = args.get(2) {
        let (ip, port) = value.rsplit_once(':').context("endpoint must be IP:PORT")?;
        DeviceEndpoint::new(local, ip.parse()?, port.parse()?)
    } else {
        discovery::discover(local, Duration::from_secs(4))
            .await?
            .into_iter()
            .next()
            .context("no phone discovered")?
    };
    println!("endpoint {endpoint:?}");
    let session = Session::connect(&account, &endpoint, ConnectOptions::default()).await?;
    println!("connected");
    let preview = session.open_camera(CameraOptions::default()).await?;
    println!("camera opened: {:?}", preview.config());
    let feed = preview.feed();
    let player = hyperclix_core::media::MediaPlayer::play_camera(
        session.local_ip(),
        feed,
        hyperclix_core::media::PlaybackOptions { audio: false, ..Default::default() },
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let stats = player.stats();
    println!("rtp_packets={} bytes={} decoded={} size={}x{}", stats.rtp_packets, stats.received_bytes, stats.decoded_frames, stats.width, stats.height);
    let _ = player.close().await;
    let _ = preview.close().await;
    let _ = session.close().await;
    Ok(())
}
