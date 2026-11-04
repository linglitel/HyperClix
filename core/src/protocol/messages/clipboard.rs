//! universalClipboard control messages.
use crate::Result;
use crate::protocol::{
    message_center::networking_frame,
    pb::{Fields, Writer},
};
use anyhow::{Context, bail, ensure};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextRequest {
    pub sequence: u8,
    pub item: usize,
    pub code: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextResponse {
    pub sequence: u8,
    pub item: usize,
    pub code: u8,
    pub status: Option<u64>,
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileInfoRequest {
    pub sequence: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileInfoItem {
    pub index: usize,
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileInfoResponse {
    pub sequence: u8,
    pub status: Option<u64>,
    pub items: Vec<FileInfoItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileContentRequest {
    pub sequence: u8,
    pub item: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileFailure {
    pub sequence: u8,
    pub item: usize,
    pub status: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRequest {
    pub timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Thumbnail {
    pub code: u8,
    pub name: Option<String>,
    pub size: Option<u64>,
    pub text: Option<String>,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryItem {
    pub sequence: u8,
    pub timestamp: u64,
    pub thumbnails: Vec<Thumbnail>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryResponse {
    pub last_sequence: Option<u8>,
    pub items: Vec<HistoryItem>,
    pub status: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClipboardMessage {
    TextRequest(TextRequest),
    FileInfoRequest(FileInfoRequest),
    TextResponse(TextResponse),
    FileInfoResponse(FileInfoResponse),
    FileContentRequest(FileContentRequest),
    FileFailure(FileFailure),
    HistoryRequest(HistoryRequest),
    HistoryResponse(HistoryResponse),
}

impl ClipboardMessage {
    pub(crate) fn kind(&self) -> u64 {
        match self {
            Self::TextRequest(_) => 0,
            Self::FileInfoRequest(_) => 1,
            Self::TextResponse(_) => 2,
            Self::FileInfoResponse(_) => 3,
            Self::FileContentRequest(_) => 4,
            Self::FileFailure(_) => 5,
            Self::HistoryRequest(_) => 6,
            Self::HistoryResponse(_) => 7,
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let (kind, field, body) = match self {
            Self::TextRequest(message) => (0, 2, message.encode()),
            Self::FileInfoRequest(message) => (1, 3, message.encode()),
            Self::TextResponse(message) => (2, 4, message.encode()),
            Self::FileInfoResponse(message) => (3, 5, message.encode()),
            Self::FileContentRequest(message) => (4, 6, message.encode()),
            Self::FileFailure(message) => (5, 7, message.encode()),
            Self::HistoryRequest(message) => (6, 8, message.encode()),
            Self::HistoryResponse(message) => (7, 9, message.encode()),
        };
        let mut writer = Writer::new();
        writer.varint(1, kind);
        writer.bytes(field, body);
        writer.finish()
    }

    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(match fields.uint(1) {
            0 => Self::TextRequest(TextRequest::decode(fields.bytes(2))?),
            1 => Self::FileInfoRequest(FileInfoRequest::decode(fields.bytes(3))?),
            2 => Self::TextResponse(TextResponse::decode(fields.bytes(4))?),
            3 => Self::FileInfoResponse(FileInfoResponse::decode(fields.bytes(5))?),
            4 => Self::FileContentRequest(FileContentRequest::decode(fields.bytes(6))?),
            5 => Self::FileFailure(FileFailure::decode(fields.bytes(7))?),
            6 => Self::HistoryRequest(HistoryRequest::decode(fields.bytes(8))?),
            7 => Self::HistoryResponse(HistoryResponse::decode(fields.bytes(9))?),
            kind => bail!("unknown clipboard message kind {kind}"),
        })
    }
}

impl TextRequest {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.sequence as u64);
        writer.varint(2, self.item as u64);
        writer.varint(3, self.code as u64);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            sequence: fields.uint(1) as u8,
            item: fields.uint(2) as usize,
            code: fields.uint(3) as u8,
        })
    }
}

impl TextResponse {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.sequence as u64);
        writer.varint(2, self.item as u64);
        writer.varint(3, self.code as u64);
        if let Some(status) = self.status {
            writer.varint(4, status);
        }
        if let Some(text) = &self.text {
            writer.bytes(5, text);
        }
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            sequence: fields.uint(1) as u8,
            item: fields.uint(2) as usize,
            code: fields.uint(3) as u8,
            status: fields.has(4).then(|| fields.uint(4)),
            text: fields.has(5).then(|| fields.text(5)),
        })
    }
}

impl FileInfoRequest {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.sequence as u64);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        Ok(Self {
            sequence: Fields::decode(data)?.uint(1) as u8,
        })
    }
}

impl FileInfoResponse {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.sequence as u64);
        for item in &self.items {
            let mut info = Writer::new();
            info.bytes(1, &item.name);
            info.varint(2, item.size);
            let mut row = Writer::new();
            row.varint(1, item.index as u64);
            row.bytes(2, info.finish());
            writer.bytes(3, row.finish());
        }
        if let Some(status) = self.status {
            writer.varint(2, status);
        }
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        let mut items = Vec::new();
        for row in fields.repeated_message::<Fields>(3)? {
            let info = row.message::<Fields>(2)?;
            items.push(FileInfoItem {
                index: row.uint(1) as usize,
                name: info.text(1),
                size: info.uint(2),
            });
        }
        Ok(Self {
            sequence: fields.uint(1) as u8,
            status: fields.has(2).then(|| fields.uint(2)),
            items,
        })
    }
}

impl FileContentRequest {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.sequence as u64);
        writer.varint(2, self.item as u64);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            sequence: fields.uint(1) as u8,
            item: fields.uint(2) as usize,
        })
    }
}

impl FileFailure {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.sequence as u64);
        writer.varint(2, self.item as u64);
        writer.varint(3, self.status);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            sequence: fields.uint(1) as u8,
            item: fields.uint(2) as usize,
            status: fields.uint(3),
        })
    }
}

impl HistoryRequest {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.timestamp);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        Ok(Self {
            timestamp: Fields::decode(data)?.uint(1),
        })
    }
}

impl HistoryResponse {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        if let Some(sequence) = self.last_sequence {
            writer.varint(1, sequence as u64);
        }
        for item in &self.items {
            let mut detail = Writer::new();
            detail.varint(1, item.sequence as u64);
            detail.varint(2, item.timestamp);
            for thumbnail in &item.thumbnails {
                let mut row = Writer::new();
                row.varint(1, thumbnail.code as u64);
                if let Some(name) = &thumbnail.name {
                    row.bytes(2, name);
                }
                if let Some(size) = thumbnail.size {
                    row.varint(3, size);
                }
                if let Some(text) = &thumbnail.text {
                    row.bytes(4, text);
                }
                if thumbnail.complete {
                    row.varint(5, 1);
                }
                detail.bytes(3, row.finish());
            }
            writer.bytes(2, detail.finish());
        }
        if let Some(status) = self.status {
            writer.varint(3, status);
        }
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        let mut items = Vec::new();
        for detail in fields.repeated_message::<Fields>(2)? {
            let mut thumbnails = Vec::new();
            for row in detail.repeated_message::<Fields>(3)? {
                thumbnails.push(Thumbnail {
                    code: row.uint(1) as u8,
                    name: row.has(2).then(|| row.text(2)),
                    size: row.has(3).then(|| row.uint(3)),
                    text: row.has(4).then(|| row.text(4)),
                    complete: row.uint(5) != 0,
                });
            }
            items.push(HistoryItem {
                sequence: detail.uint(1) as u8,
                timestamp: detail.uint(2),
                thumbnails,
            });
        }
        Ok(Self {
            last_sequence: fields.has(1).then(|| fields.uint(1) as u8),
            items,
            status: fields.has(3).then(|| fields.uint(3)),
        })
    }
}

pub(crate) const TOPIC_NAME: &str = "topic.name:universalClipboard";

#[derive(Clone, Debug)]
pub(crate) struct TopicAdvertisement {
    pub sequence: u8,
    pub advanced: bool,
    pub mimes: Vec<u8>,
    pub extended: Vec<u8>,
}

impl TopicAdvertisement {
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        ensure!(
            self.mimes.len() <= 7,
            "UniversalClipboard supports up to seven items per publication"
        );
        let mut base = vec![(self.sequence << 1) | u8::from(self.advanced)];
        base.extend_from_slice(&self.mimes);
        let mut raw = vec![0, 2, ((base.len() as u8) << 4) | (self.sequence & 15)];
        raw.extend_from_slice(&base);
        raw.extend(length(self.extended.len())?);
        raw.extend_from_slice(&self.extended);
        Ok(networking_frame(&raw))
    }

    pub(crate) fn decode(networking: &[u8]) -> Result<Vec<Self>> {
        let frame = Fields::decode(networking)?;
        if frame.uint(1) != 4 {
            return Ok(Vec::new());
        }
        let message = frame.message::<Fields>(8)?;
        let raw = message.bytes(1);
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        ensure!(
            raw[0] == 0,
            "Unsupported compressed MessageCenter clipboard transport: {}",
            raw[0]
        );
        let mut cursor = 1;
        let mut active = false;
        let mut out = Vec::new();
        while cursor < raw.len() {
            let id = *raw
                .get(cursor)
                .context("Truncated MessageCenter topic id")?;
            let flags = *raw
                .get(cursor + 1)
                .context("Truncated MessageCenter topic flags")?;
            cursor += 2;
            let base_end = cursor + (flags >> 4) as usize;
            let base = raw
                .get(cursor..base_end)
                .context("Truncated MessageCenter base data")?;
            cursor = base_end;
            let first = *raw
                .get(cursor)
                .context("Truncated MessageCenter extended length")?;
            cursor += 1;
            let size = if first & 128 == 0 {
                first as usize
            } else {
                let second = *raw
                    .get(cursor)
                    .context("Truncated MessageCenter extended length")?;
                cursor += 1;
                (first as usize & 127) | ((second as usize) << 7)
            };
            let end = cursor + size;
            let extended = raw
                .get(cursor..end)
                .context("Truncated MessageCenter extended data")?;
            cursor = end;
            if id == 255 {
                active = extended.get(..2) == Some(&[0, 2]);
                if active {
                    let n = *extended
                        .get(2)
                        .context("Truncated dynamic clipboard topic")?
                        as usize;
                    active = extended.get(3..3 + n) == Some(TOPIC_NAME.as_bytes());
                }
                continue;
            }
            if id == 2 || (id == 254 && active) {
                let Some(header) = base.first() else {
                    continue;
                };
                if base.len() == 1 {
                    continue;
                }
                out.push(Self {
                    sequence: header >> 1,
                    advanced: header & 1 != 0,
                    mimes: base[1..].to_vec(),
                    extended: extended.to_vec(),
                });
            }
        }
        Ok(out)
    }
}

fn length(size: usize) -> Result<Vec<u8>> {
    ensure!(
        size < 16384,
        "Clipboard MessageCenter extended data exceeds wire length"
    );
    Ok(if size < 127 {
        vec![size as u8]
    } else {
        vec![0x80 | (size as u8 & 127), (size >> 7) as u8]
    })
}
