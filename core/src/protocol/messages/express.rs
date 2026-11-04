//! MiExpress TLV frames and AES-GCM TCP packets.
use crate::Result;
use anyhow::{Context, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use openssl::symm::{Cipher, decrypt_aead, encrypt_aead};
use rand::RngCore;
use std::collections::BTreeMap;

pub(crate) fn handshake(port: u16, key: &[u8]) -> Vec<u8> {
    frame(
        1,
        &[
            int(0, 0),
            int(1, 1),
            int(2, 0),
            int(3, port as i32),
            tlv(5, 4, key),
            tlv(1, 5, &[8]),
        ]
        .concat(),
    )
}

pub(crate) fn control(serial: i32, data: &[u8]) -> Vec<u8> {
    frame(
        2,
        &[int(0, serial), tlv(5, 1, data), tlv(1, 2, &[0])].concat(),
    )
}

pub(crate) fn stream_send(
    serial: i32,
    stream_id: i32,
    length: i64,
    filename: &str,
    tag: &str,
) -> Vec<u8> {
    frame(
        3,
        &[
            int(0, serial),
            int(1, stream_id),
            long(2, length),
            string(3, ""),
            string(4, filename),
            string(5, tag),
        ]
        .concat(),
    )
}

pub(crate) fn begin(serial: i32, stream_id: i32) -> Vec<u8> {
    frame(4, &[int(0, serial), int(1, stream_id), int(2, 1)].concat())
}

pub(crate) fn end(serial: i32, stream_id: i32, result: i32) -> Vec<u8> {
    frame(
        5,
        &[int(0, serial), int(1, stream_id), int(2, result)].concat(),
    )
}

pub(crate) fn streamlet(stream_id: i32, offset: i64, size: i32) -> Vec<u8> {
    frame(
        0x100,
        &[
            tlv(5, 0, &[]),
            int(1, stream_id),
            long(2, offset),
            int(3, size),
        ]
        .concat(),
    )
}

pub(crate) fn user_data() -> String {
    STANDARD.encode(frame(1, &string(0, "AQAAAAAAAAA=")))
}

#[derive(Clone, Debug)]
pub(crate) enum ExpressPacket {
    Handshake {
        ab: i64,
        port: u16,
        key: Vec<u8>,
        lanes: i64,
    },
    Control {
        serial: i64,
        data: Vec<u8>,
        finished: bool,
    },
    StreamSend {
        stream_id: i32,
        length: i64,
        tag: String,
    },
    Begin {
        stream_id: i32,
        mode: i32,
    },
    End {
        stream_id: i32,
        result: i32,
    },
    Data {
        stream_id: i32,
        offset: i64,
        data_length: i32,
    },
}

impl ExpressFrame {
    pub(crate) fn packet(&self) -> Result<ExpressPacket> {
        Ok(match self.kind {
            1 => ExpressPacket::Handshake {
                ab: self.number(2)?,
                port: self.number(3)? as u16,
                key: self.value(4)?.to_vec(),
                lanes: self.number(5)?,
            },
            2 => ExpressPacket::Control {
                serial: self.number(0)?,
                data: self.value(1)?.to_vec(),
                finished: self.number(2)? == 0,
            },
            3 => ExpressPacket::StreamSend {
                stream_id: self.number(1)? as i32,
                length: self.number(2)?,
                tag: self.text(5)?,
            },
            4 => ExpressPacket::Begin {
                stream_id: self.number(1)? as i32,
                mode: self.number(2)? as i32,
            },
            5 => ExpressPacket::End {
                stream_id: self.number(1)? as i32,
                result: self.number(2)? as i32,
            },
            0x100 => ExpressPacket::Data {
                stream_id: self.number(1)? as i32,
                offset: self.number(2)?,
                data_length: self.number(3)? as i32,
            },
            kind => bail!("未知 MiExpress 消息类型 {kind}"),
        })
    }
}

#[derive(Debug)]
pub(crate) struct ExpressFrame {
    pub(crate) kind: u16,
    values: BTreeMap<u16, Vec<u8>>,
}

impl ExpressFrame {
    pub(crate) fn value(&self, tag: u16) -> Result<&[u8]> {
        self.values
            .get(&tag)
            .map(Vec::as_slice)
            .with_context(|| format!("MiExpress 缺少字段 {tag}"))
    }
    pub(crate) fn number(&self, tag: u16) -> Result<i64> {
        let bytes = self.value(tag)?;
        Ok(match bytes.len() {
            1 => bytes[0] as i8 as i64,
            4 => i32::from_be_bytes(bytes.try_into()?) as i64,
            8 => i64::from_be_bytes(bytes.try_into()?),
            length => bail!("MiExpress 整数字段长度 {length}"),
        })
    }
    pub(crate) fn text(&self, tag: u16) -> Result<String> {
        String::from_utf8(self.value(tag)?.to_vec()).context("MiExpress 文本不是 UTF-8")
    }
}

pub(crate) fn tlv(kind: u16, tag: u16, data: &[u8]) -> Vec<u8> {
    [
        kind.to_be_bytes().as_slice(),
        tag.to_be_bytes().as_slice(),
        (data.len() as u32).to_be_bytes().as_slice(),
        data,
    ]
    .concat()
}
pub(crate) fn int(tag: u16, value: i32) -> Vec<u8> {
    tlv(3, tag, &value.to_be_bytes())
}
pub(crate) fn long(tag: u16, value: i64) -> Vec<u8> {
    tlv(4, tag, &value.to_be_bytes())
}
pub(crate) fn string(tag: u16, value: &str) -> Vec<u8> {
    tlv(5, tag, value.as_bytes())
}
pub(crate) fn frame(kind: u16, body: &[u8]) -> Vec<u8> {
    tlv(
        0x101,
        0xffff,
        &[kind.to_be_bytes().to_vec(), tlv(0x100, kind, body)].concat(),
    )
}

fn items(data: &[u8]) -> Result<BTreeMap<u16, (u16, Vec<u8>)>> {
    let mut offset = 0;
    let mut values = BTreeMap::new();
    while offset < data.len() {
        ensure!(data.len() - offset >= 8, "MiExpress TLV 包头不完整");
        let kind = u16::from_be_bytes(data[offset..offset + 2].try_into()?);
        let tag = u16::from_be_bytes(data[offset + 2..offset + 4].try_into()?);
        let length = u32::from_be_bytes(data[offset + 4..offset + 8].try_into()?) as usize;
        offset += 8;
        ensure!(length <= data.len() - offset, "MiExpress TLV 正文不完整");
        values.insert(tag, (kind, data[offset..offset + length].to_vec()));
        offset += length;
    }
    Ok(values)
}

pub(crate) fn parse_frame(data: &[u8]) -> Result<ExpressFrame> {
    let outer = items(data)?;
    let (kind, payload) = outer.get(&0xffff).context("缺少 MiExpress 外层 TLV")?;
    ensure!(
        *kind == 0x101 && payload.len() >= 2,
        "无效 MiExpress 外层 TLV"
    );
    let kind = u16::from_be_bytes(payload[..2].try_into()?);
    let groups = items(&payload[2..])?;
    let (group_type, group) = groups.get(&kind).context("缺少 MiExpress 分组")?;
    ensure!(*group_type == 0x100, "无效 MiExpress 分组");
    Ok(ExpressFrame {
        kind,
        values: items(group)?
            .into_iter()
            .map(|(tag, (_, value))| (tag, value))
            .collect(),
    })
}

pub(crate) fn encode_tcp(
    key: &[u8],
    metadata: &[u8],
    data: &[u8],
    nonce: Option<[u8; 12]>,
) -> Result<Vec<u8>> {
    let nonce = nonce.unwrap_or_else(|| {
        let mut nonce = [0; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        nonce
    });
    let plaintext = [data, metadata, &(metadata.len() as u16).to_be_bytes()].concat();
    let mut tag = [0; 16];
    let cipher = encrypt_aead(
        Cipher::aes_128_gcm(),
        key,
        Some(&nonce),
        &[],
        &plaintext,
        &mut tag,
    )?;
    let body = [&nonce[..], &tag[..], &cipher].concat();
    Ok([&[0; 6][..], &(body.len() as u32).to_be_bytes(), &body].concat())
}

pub(crate) fn decode_tcp(key: &[u8], body: &[u8]) -> Result<(ExpressFrame, Vec<u8>)> {
    ensure!(body.len() >= 30, "MiExpress 加密包不完整");
    let plaintext = decrypt_aead(
        Cipher::aes_128_gcm(),
        key,
        Some(&body[..12]),
        &[],
        &body[28..],
        &body[12..28],
    )
    .context("MiExpress GCM 认证失败")?;
    ensure!(plaintext.len() >= 2, "MiExpress 明文缺少尾部长度");
    let tail = plaintext.len() - 2;
    let size = u16::from_be_bytes(plaintext[tail..].try_into()?) as usize;
    ensure!(size <= tail, "MiExpress 元数据超出包长");
    let metadata = parse_frame(&plaintext[tail - size..tail])?;
    let data = plaintext[..tail - size].to_vec();
    if metadata.kind == 0x100 && metadata.number(3)? >= 0 {
        ensure!(
            metadata.number(3)? as usize == data.len(),
            "MiExpress 文件数据长度不一致"
        );
    }
    Ok((metadata, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn executed_native_constructor_vectors() -> Result<()> {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/miexpress-native.json"
        ))?;
        let vectors = &fixtures["frames"];
        let metadata = frame(
            0x100,
            &[tlv(5, 0, &[]), int(1, 123), long(2, 0), int(3, 4)].concat(),
        );
        assert_eq!(
            hex::encode(metadata),
            vectors["streamlet"].as_str().unwrap()
        );
        assert_eq!(
            STANDARD.decode(user_data())?,
            hex::decode(vectors["link_initial"].as_str().unwrap())?
        );
        let handshake = parse_frame(&hex::decode(vectors["handshake"].as_str().unwrap())?)?;
        assert_eq!(handshake.number(3)?, 41471);
        assert_eq!(handshake.number(5)?, 8);
        assert_eq!(handshake.value(4)?, &(0..16).collect::<Vec<u8>>());
        let stream = parse_frame(&hex::decode(vectors["stream_send"].as_str().unwrap())?)?;
        assert_eq!(stream.number(1)?, 123);
        assert_eq!(stream.text(4)?, "name.txt");
        Ok(())
    }
}
