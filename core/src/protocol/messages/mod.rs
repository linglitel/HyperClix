//! Typed protocol messages. Field numbers live only in this module.
//!
//! Encoding convention:
//! - `pb::Message` is for self-contained protobuf bodies that need symmetric
//!   `encode`/`decode`, especially nested/repeated payloads.
//! - Inherent `encode`/`decode` is for wire envelopes: Lyra physical/logical
//!   frames, Mirror type-framed messages, SDK control headers, discriminated
//!   unions and direction-specific one-way messages. These carry routing or
//!   message-type information that is not part of the protobuf body.
//! - Free functions construct fixed wire packets (heartbeats, frame wrappers,
//!   TLV/media packets). They stay here; feature code must not read field
//!   numbers or build these bytes itself.
pub(crate) mod auth;
pub(crate) mod camera;
pub(crate) mod clipboard;
pub(crate) mod data;
pub(crate) mod device;
pub(crate) mod express;
pub(crate) mod files;
pub(crate) mod lyra;
pub(crate) mod media;
pub(crate) mod mirror;
pub(crate) mod mishare;
pub(crate) mod sdk;
pub(crate) mod upgrade;
