//! HyperClix on Linux. Feature handles own their protocol lifecycle.
//!
//! Existing account state is loaded explicitly; importing this library never
//! logs in, announces a device or connects to a phone.
//!
//! Architecture: `protocol` (wire formats) <- `transport` (links/auth) <-
//! `session` (one phone connection) <- `features` (desktop, camera, files, ...).
//! `media` is a platform service composed by features and the UI.

mod common;
mod features;
mod identity;
mod session;
mod transport;

pub mod media;
pub mod prelude;
/// Recovered low-level wire formats. Most callers should use the feature modules.
pub mod protocol;
pub mod proximity;

pub const DEFAULT_DEVICE_NAME: &str = "Linux PC";

pub use crate::common::{Error, Result, logging, paths};
pub use crate::features::{apps, camera, clipboard, desktop, files, mishare, notifications, relay};
pub use crate::identity::{Account, Credentials, QrLogin, default_account_dir, device_subject};
pub use crate::protocol::capabilities::CastCapabilities;
pub use crate::session::server::{Server, ServerEvent, ServerOptions, ServerPeer};
pub use crate::session::{Channel, ChannelEvent, ConnectOptions, Session, SessionEvent};
pub use crate::transport::discovery;
