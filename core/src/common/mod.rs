//! Infrastructure shared by every layer: errors, time, storage and crypto.
pub(crate) mod crypto;
pub mod logging;
pub mod paths;
pub(crate) mod storage;
pub(crate) mod time;

pub use crate::common::error::{Error, Result};
mod error;
