//! MiShare basic control messages and file metadata.
use crate::Result;
use crate::protocol::pb::{Fields, Message, Writer};
use anyhow::ensure;

pub(crate) const OFFER: u64 = 1;
pub(crate) const ACCEPT: u64 = 2;
pub(crate) const CANCEL: u64 = 3;
pub(crate) const PEER_CANCEL: u64 = 4;
pub(crate) const PEER_ERROR: u64 = 5;
pub(crate) const ERROR_ACK: u64 = 6;
pub(crate) const FINISH: u64 = 7;
pub(crate) const COMPLETE_ACK: u64 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Control {
    pub tag: u64,
    pub body: Vec<u8>,
}

impl Control {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.tag);
        writer.opt_bytes(2, &self.body);
        writer.finish()
    }

    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        ensure!(fields.uint(1) != 0, "无效 MiShare 消息类型");
        Ok(Self {
            tag: fields.uint(1),
            body: fields.bytes(2).to_vec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub name: String,
    pub size: u64,
    pub id: String,
    pub hash: String,
    pub mime_type: String,
}

impl Message for FileInfo {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.bytes(1, &self.name);
        writer.opt_varint(2, self.size);
        writer.bytes(3, &self.id);
        writer.bytes(4, &self.hash);
        writer.bytes(5, &self.mime_type);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            name: fields.text(1),
            size: fields.uint(2),
            id: fields.text(3),
            hash: fields.text(4),
            mime_type: fields.text(5),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Offer {
    pub request_id: u64,
    pub device_name: String,
    pub job_id: String,
    pub files: Vec<FileInfo>,
}

impl Message for Offer {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.request_id);
        writer.bytes(2, &self.device_name);
        writer.bytes(3, &self.job_id);
        for file in &self.files {
            writer.message(5, file);
        }
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            request_id: fields.uint(1),
            device_name: fields.text(2),
            job_id: fields.text(3),
            files: fields.repeated_message(5)?,
        })
    }
}

pub(crate) fn request_id_body(request_id: u64) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(1, request_id);
    writer.finish()
}

pub(crate) fn cancel_body(request_id: u64, code: u64) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(1, request_id);
    writer.varint(3, code);
    writer.finish()
}
