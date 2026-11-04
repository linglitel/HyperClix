//! Typed Lyra physical/logical envelopes.
mod logical;
mod physical;

pub(crate) use logical::{
    AuthExchange, LogicalBody, LogicalClose, LogicalEnvelope, LogicalFrame, LogicalSync,
    ServiceRequest, ServiceResponse, UserInfo,
};
pub(crate) use physical::{
    PhysicalBody, PhysicalFrame, PhysicalHeartbeatAck, PhysicalSync, PhysicalSyncReply,
};

use crate::protocol::pb::Writer;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub(crate) fn uid_feature(uid: &str) -> Vec<u8> {
    let mut nonce = vec![0; 8];
    rand::thread_rng().fill_bytes(&mut nonce);
    let digest = Sha256::digest([nonce.clone(), Sha256::digest(uid.as_bytes()).to_vec()].concat());
    let mut writer = Writer::new();
    writer.bytes(1, &nonce);
    writer.bytes(2, &digest);
    writer.finish()
}
