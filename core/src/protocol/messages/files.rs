//! Continuity SDK FILE packets (`ChannelFrame`).
use super::media::MediaPacket;
use crate::Result;
use anyhow::{Context, ensure};
use std::collections::BTreeMap;

pub(crate) const SEND: i32 = 1;
pub(crate) const ACCEPTED: i32 = 2;
pub(crate) const SUCCEEDED: i32 = 3;
pub(crate) const FAILED: i32 = 4;
pub(crate) const CANCEL: i32 = 5;

#[derive(Clone, Debug)]
pub struct FileOffer {
    pub task_id: i32,
    /// -1 denotes a stream whose content length comes from its EOF packet.
    pub content_length: i64,
    pub content_type: String,
    pub filename: String,
    pub tag: String,
}

#[derive(Clone, Debug)]
pub enum FilePacket {
    Offer(FileOffer),
    Event {
        task_id: i32,
        event: i32,
    },
    Data {
        task_id: i32,
        offset: i64,
        data: Vec<u8>,
    },
}

impl FilePacket {
    pub(crate) fn task_id(&self) -> i32 {
        match self {
            Self::Offer(offer) => offer.task_id,
            Self::Event { task_id, .. } | Self::Data { task_id, .. } => *task_id,
        }
    }

    /// Returns None for application BYTES or non-FILE SDK frames.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Self>> {
        if !bytes.starts_with(&crate::protocol::messages::media::MAGIC) {
            return Ok(None);
        }
        let packet = MediaPacket::decode(bytes)?;
        if packet.kind != 2 && packet.kind != 3 {
            return Ok(None);
        }
        let task_id = i32::from_be_bytes(
            packet
                .value(0)
                .context("SDK FILE missing task id")?
                .try_into()?,
        );
        if packet.kind == 3 {
            let offset = i64::from_be_bytes(
                packet
                    .value(1)
                    .context("SDK FILE missing offset")?
                    .try_into()?,
            );
            let size = i32::from_be_bytes(
                packet
                    .value(2)
                    .context("SDK FILE missing size")?
                    .try_into()?,
            );
            let data = packet.value(3).context("SDK FILE missing data")?.to_vec();
            ensure!(
                size as usize == data.len(),
                "SDK FILE data size differs from payload"
            );
            return Ok(Some(Self::Data {
                task_id,
                offset,
                data,
            }));
        }
        let event = i32::from_be_bytes(
            packet
                .value(1)
                .context("SDK FILE missing event")?
                .try_into()?,
        );
        if event != SEND {
            return Ok(Some(Self::Event { task_id, event }));
        }
        let detail = packet.value(2).context("SDK FILE offer has no detail")?;
        ensure!(
            detail.starts_with(&[0, 0]),
            "Unknown SDK FILE metadata alternative"
        );
        let groups = items(&detail[2..])?;
        let metadata = items(groups.get(&0).context("SDK FILE offer has no metadata")?)?;
        let number = metadata.get(&0).context("SDK FILE offer has no length")?;
        let text = |tag| -> Result<String> {
            Ok(String::from_utf8(
                metadata
                    .get(&tag)
                    .context("SDK FILE metadata text missing")?
                    .to_vec(),
            )?)
        };
        Ok(Some(Self::Offer(FileOffer {
            task_id,
            content_length: i64::from_be_bytes((*number).try_into()?),
            content_type: text(1)?,
            filename: text(2)?,
            tag: text(3)?,
        })))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResourceEntry {
    pub url: String,
    pub name: String,
    pub size: u64,
}

pub(crate) fn resource_list(entries: &[ResourceEntry]) -> Vec<u8> {
    let mut writer = crate::protocol::pb::Writer::new();
    for entry in entries {
        let mut item = crate::protocol::pb::Writer::new();
        item.bytes(1, &entry.url);
        item.bytes(2, &entry.name);
        item.varint(3, entry.size);
        writer.bytes(2, item.finish());
    }
    writer.finish()
}

pub(crate) fn event(task: i32, event: i32) -> Vec<u8> {
    frame(
        2,
        &[
            tlv(3, 0, &task.to_be_bytes()),
            tlv(3, 1, &event.to_be_bytes()),
            tlv(0x101, 2, &[0xff, 0xff]),
        ]
        .concat(),
    )
}

pub(crate) fn offer(task: i32, filename: &str, content_type: &str, tag: &str) -> Vec<u8> {
    let metadata = [
        tlv(4, 0, &(-1i64).to_be_bytes()),
        tlv(5, 1, content_type.as_bytes()),
        tlv(5, 2, filename.as_bytes()),
        tlv(5, 3, tag.as_bytes()),
    ]
    .concat();
    frame(
        2,
        &[
            tlv(3, 0, &task.to_be_bytes()),
            tlv(3, 1, &SEND.to_be_bytes()),
            tlv(0x101, 2, &[vec![0, 0], tlv(0x100, 0, &metadata)].concat()),
        ]
        .concat(),
    )
}

pub(crate) fn data(task: i32, offset: i64, bytes: &[u8]) -> Vec<u8> {
    frame(
        3,
        &[
            tlv(3, 0, &task.to_be_bytes()),
            tlv(4, 1, &offset.to_be_bytes()),
            tlv(3, 2, &(bytes.len() as i32).to_be_bytes()),
            tlv(5, 3, bytes),
        ]
        .concat(),
    )
}

fn tlv(kind: u16, tag: u16, value: &[u8]) -> Vec<u8> {
    [
        kind.to_be_bytes().as_slice(),
        tag.to_be_bytes().as_slice(),
        (value.len() as u32).to_be_bytes().as_slice(),
        value,
    ]
    .concat()
}

fn frame(kind: u16, values: &[u8]) -> Vec<u8> {
    tlv(
        0x101,
        0xffff,
        &[kind.to_be_bytes().to_vec(), tlv(0x100, kind, values)].concat(),
    )
}

fn items(mut bytes: &[u8]) -> Result<BTreeMap<u16, &[u8]>> {
    let mut out = BTreeMap::new();
    while !bytes.is_empty() {
        ensure!(bytes.len() >= 8, "Truncated SDK FILE TLV");
        let tag = u16::from_be_bytes(bytes[2..4].try_into()?);
        let size = u32::from_be_bytes(bytes[4..8].try_into()?) as usize;
        let value = bytes.get(8..8 + size).context("Truncated SDK FILE value")?;
        out.insert(tag, value);
        bytes = &bytes[8 + size..];
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_protobuf_is_not_an_sdk_file() -> Result<()> {
        // RequestThumbnailInfo from the universal clipboard service starts
        // with messageType=6 and must not be parsed as an SDK FILE frame.
        let payload = [
            0x08, 0x06, 0x42, 0x07, 0x08, 0xbb, 0x80, 0xd9, 0xbc, 0x99, 0x34,
        ];
        assert!(FilePacket::decode(&payload)?.is_none());
        Ok(())
    }
}
