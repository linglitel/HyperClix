//! MiPush Slim framing, RC4 transport and Thrift application containers.
use super::PushRuntime;
use crate::{
    Result,
    protocol::pb::{Fields, Message, Value},
};
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use openssl::{
    sha::sha1,
    symm::{Cipher, decrypt},
};
use std::{collections::BTreeMap, io::Read};

mod thrift;

pub(crate) use thrift::{Struct, Thrift, decode_thrift};
use thrift::{adler32, encode_thrift, rc4, text};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

fn encode_proto(values: &Fields) -> Vec<u8> {
    values.encode()
}
#[derive(Clone)]
pub struct Blob {
    pub header: Fields,
    pub payload: Vec<u8>,
    pub kind: u16,
}
impl Blob {
    pub fn command(&self) -> String {
        self.header.string(5)
    }
    fn encode(&self) -> Result<Vec<u8>> {
        let header = encode_proto(&self.header);
        let mut out = Vec::with_capacity(8 + header.len() + self.payload.len());
        out.extend(self.kind.to_be_bytes());
        out.extend(u16::try_from(header.len())?.to_be_bytes());
        out.extend(u32::try_from(self.payload.len())?.to_be_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(&self.payload);
        Ok(out)
    }
    fn decode(data: &[u8]) -> Result<Self> {
        anyhow::ensure!(data.len() >= 8, "Truncated Slim blob");
        let kind = u16::from_be_bytes(data[..2].try_into()?);
        let hsize = u16::from_be_bytes(data[2..4].try_into()?) as usize;
        let psize = u32::from_be_bytes(data[4..8].try_into()?) as usize;
        anyhow::ensure!(data.len() == 8 + hsize + psize, "Slim blob length mismatch");
        Ok(Self {
            kind,
            header: Fields::decode(&data[8..8 + hsize])?,
            payload: data[8 + hsize..].to_vec(),
        })
    }
}
fn frame(blob: Option<&Blob>, key: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut body = match blob {
        Some(blob) => blob.encode()?,
        None => Vec::new(),
    };
    if !body.is_empty() {
        if let Some(key) = key {
            body = rc4(key, &body);
        }
    }
    let mut out = Vec::with_capacity(8 + body.len() + 4);
    out.extend([0xc2, 0xfe, 0, 5]);
    out.extend(u32::try_from(body.len())?.to_be_bytes());
    out.extend_from_slice(&body);
    let checksum = adler32(&out);
    out.extend(checksum.to_be_bytes());
    Ok(out)
}
fn bytes(n: u32, value: impl AsRef<[u8]>) -> (u32, Value) {
    (n, Value::Bytes(value.as_ref().to_vec()))
}
fn integer(n: u32, value: u64) -> (u32, Value) {
    (n, Value::Varint(value))
}
pub struct SlimClient {
    stream: TcpStream,
    buffer: Vec<u8>,
    runtime: PushRuntime,
    key: Option<Vec<u8>>,
    challenge: String,
}
impl SlimClient {
    pub async fn connect(runtime: PushRuntime, host: &str, port: u16) -> Result<Self> {
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            TcpStream::connect((host, port)),
        )
        .await
        .context("MiPush connection timed out")?
        .context("MiPush connection failed")?;
        let mut client = Self {
            stream,
            buffer: Vec::new(),
            runtime,
            key: None,
            challenge: String::new(),
        };
        let conn = Fields(vec![
            integer(1, 106),
            bytes(2, crate::DEFAULT_DEVICE_NAME),
            bytes(3, "Linux"),
            bytes(4, &client.runtime.device_id),
            integer(5, 48),
            bytes(6, "wifi"),
            bytes(7, host),
            bytes(8, "zh_CN"),
            integer(10, 36),
            integer(13, 1),
            integer(14, 1),
        ]);
        let blob = Blob {
            header: Fields(vec![
                integer(1, 0),
                integer(2, 0),
                bytes(3, "xiaomi.com"),
                bytes(5, "CONN"),
            ]),
            payload: encode_proto(&conn),
            kind: 2,
        };
        client.send(Some(&blob)).await?;
        let reply = tokio::time::timeout(std::time::Duration::from_secs(15), client.receive())
            .await??
            .context("MiPush returned empty CONN")?;
        anyhow::ensure!(reply.command() == "CONN", "MiPush did not return CONN");
        let message = Fields::decode(&reply.payload)?;
        let challenge = message.string(1);
        anyhow::ensure!(!challenge.is_empty(), "MiPush returned empty challenge");
        let device = &client.runtime.device_id;
        let material = format!(
            "{}{}",
            &challenge[challenge.len() / 2..],
            &device[device.len() / 2..]
        );
        client.key = Some(rc4(challenge.as_bytes(), material.as_bytes()));
        client.challenge = challenge;
        Ok(client)
    }
    async fn send(&mut self, blob: Option<&Blob>) -> Result<()> {
        self.stream
            .write_all(&frame(blob, self.key.as_deref())?)
            .await?;
        Ok(())
    }
    /// Cancellation-safe: partial socket reads remain in `buffer` across heartbeats.
    pub async fn receive(&mut self) -> Result<Option<Blob>> {
        loop {
            if self.buffer.len() >= 8 {
                anyhow::ensure!(
                    self.buffer[..4] == [0xc2, 0xfe, 0, 5],
                    "Invalid MiPush Slim header"
                );
                let length = u32::from_be_bytes(self.buffer[4..8].try_into()?) as usize;
                anyhow::ensure!(length <= 32768, "MiPush Slim frame too large");
                let total = 12 + length;
                if self.buffer.len() >= total {
                    let packet: Vec<u8> = self.buffer.drain(..total).collect();
                    anyhow::ensure!(
                        adler32(&packet[..total - 4])
                            == u32::from_be_bytes(packet[total - 4..].try_into()?),
                        "MiPush Slim Adler32 mismatch"
                    );
                    if length == 0 {
                        return Ok(None);
                    }
                    let data = if let Some(key) = &self.key {
                        rc4(key, &packet[8..8 + length])
                    } else {
                        packet[8..8 + length].to_vec()
                    };
                    return Ok(Some(Blob::decode(&data)?));
                }
            }
            anyhow::ensure!(
                self.stream.read_buf(&mut self.buffer).await? != 0,
                "MiPush connection closed"
            );
        }
    }
    pub async fn heartbeat(&mut self) -> Result<()> {
        self.send(None).await
    }
    fn blob(
        &self,
        command: &str,
        payload: Vec<u8>,
        packet_id: Option<&str>,
        encrypted: bool,
    ) -> Result<Blob> {
        let (user, resource) = self
            .runtime
            .uuid
            .split_once('/')
            .context("MiPush UUID has no resource")?;
        let (number, domain) = user.split_once('@').context("MiPush UUID has no domain")?;
        let id = packet_id
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let mut header = Fields(vec![
            integer(1, 5),
            integer(2, number.parse()?),
            bytes(3, domain),
            bytes(4, resource),
            bytes(5, command),
            bytes(7, &id),
        ]);
        let payload = if encrypted {
            header.0.push(integer(9, 1));
            let mut key = STANDARD.decode(&self.runtime.security)?;
            key.push(b'_');
            key.extend(id.as_bytes());
            rc4(&key, &payload)
        } else {
            payload
        };
        Ok(Blob {
            header,
            payload,
            kind: 2,
        })
    }
    pub async fn bind(&mut self) -> Result<()> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let client_attrs = "sdk_ver:48,cpvn:6_8_8-C,cpvc:60088,country_code:CN,region:China";
        let cloud_attrs = format!("appid:{},locale:zh_CN,sync:1", self.runtime.app_id);
        let values = BTreeMap::from([
            ("challenge", self.challenge.as_str()),
            ("token", self.runtime.token.as_str()),
            ("chid", "5"),
            ("from", self.runtime.uuid.as_str()),
            ("id", id.as_str()),
            ("to", "xiaomi.com"),
            ("kick", "0"),
            ("client_attrs", client_attrs),
            ("cloud_attrs", cloud_attrs.as_str()),
        ]);
        let mut parts = vec!["XMPUSH-PASS".to_owned()];
        parts.extend(values.iter().map(|(k, v)| format!("{k}={v}")));
        parts.push(self.runtime.security.clone());
        let signature = STANDARD.encode(sha1(parts.join("&").as_bytes()));
        let payload = encode_proto(&Fields(vec![
            bytes(1, &self.runtime.token),
            bytes(2, "0"),
            bytes(3, "XMPUSH-PASS"),
            bytes(4, client_attrs),
            bytes(5, &cloud_attrs),
            bytes(6, signature),
        ]));
        let blob = self.blob("BIND", payload, Some(&id), false)?;
        self.send(Some(&blob)).await?;
        let response = tokio::time::timeout(std::time::Duration::from_secs(15), self.receive())
            .await??
            .context("MiPush returned empty BIND")?;
        anyhow::ensure!(
            response.command() == "BIND" && Fields::decode(&response.payload)?.uint(1) == 1,
            "MiPush BIND rejected"
        );
        Ok(())
    }
    fn target(&self) -> Result<Struct> {
        let (user, resource) = self
            .runtime
            .uuid
            .split_once('/')
            .context("Invalid MiPush UUID")?;
        let (number, _) = user.split_once('@').context("Invalid MiPush UUID")?;
        Ok(BTreeMap::from([
            (1, Thrift::Long(5)),
            (2, text(number)),
            (3, text("xiaomi.com")),
            (4, text(resource)),
            (5, Thrift::Bool(false)),
        ]))
    }
    pub async fn send_registration(&mut self) -> Result<()> {
        let target = self.target()?;
        let r = &self.runtime;
        let request = BTreeMap::from([
            (2, Thrift::Struct(target.clone())),
            (3, text(uuid::Uuid::new_v4().simple().to_string())),
            (4, text(&r.app_id)),
            (5, text("1")),
            (6, text(&r.package)),
            (7, text(&r.app_token)),
            (8, text(&r.device_id)),
            (10, text("6_8_8-C")),
            (12, text("6_8_8-C")),
            (13, Thrift::Int(60088)),
            (14, Thrift::Int(1)),
            (20, Thrift::Int(0)),
            (21, Thrift::Bool(true)),
            (101, Thrift::Bool(false)),
        ]);
        let container = BTreeMap::from([
            (1, Thrift::Int(1)),
            (2, Thrift::Bool(false)),
            (3, Thrift::Bool(true)),
            (4, text(encode_thrift(&request)?)),
            (5, text(&r.app_id)),
            (6, text(&r.package)),
            (7, Thrift::Struct(target)),
        ]);
        let mut blob = self.blob("SECMSG", encode_thrift(&container)?, None, true)?;
        blob.kind = 1;
        blob.header.0.push(bytes(6, "message"));
        self.send(Some(&blob)).await
    }
    pub fn decode_container(&self, blob: &Blob) -> Result<Struct> {
        let mut data = blob.payload.clone();
        if blob.header.uint(9) == 1 {
            let mut key = STANDARD.decode(&self.runtime.security)?;
            key.push(b'_');
            key.extend(blob.header.bytes(7));
            data = rc4(&key, &data);
        }
        if data.starts_with(b"PUSH") {
            anyhow::ensure!(data.len() >= 15, "Truncated MiPush compressed envelope");
            let codec = data[6];
            let plain_size = u32::from_be_bytes(data[7..11].try_into()?) as usize;
            let packed_size = u32::from_be_bytes(data[11..15].try_into()?) as usize;
            let packed = data
                .get(15..15 + packed_size)
                .context("Truncated MiPush gzip payload")?;
            data = if codec == 2 {
                let mut output = Vec::new();
                flate2::read::GzDecoder::new(packed).read_to_end(&mut output)?;
                output
            } else {
                packed.to_vec()
            };
            anyhow::ensure!(
                data.len() == plain_size,
                "MiPush decompressed size mismatch"
            );
        }
        decode_thrift(&data)
    }
    pub fn decode_action(&self, container: &Struct) -> Result<Struct> {
        let mut raw = container
            .get(&4)
            .context("MiPush container has no action")?
            .bytes()?
            .to_vec();
        if container.get(&2).map(Thrift::int).transpose()?.unwrap_or(0) != 0 {
            let key = STANDARD.decode(&self.runtime.reg_secret)?;
            let iv = hex::decode("64175472480004614961023454661220")?;
            let cipher = match key.len() {
                16 => Cipher::aes_128_cbc(),
                24 => Cipher::aes_192_cbc(),
                32 => Cipher::aes_256_cbc(),
                _ => bail!("Invalid MiPush registration secret length"),
            };
            raw = decrypt(cipher, &key, Some(&iv), &raw)?;
        }
        decode_thrift(&raw)
    }
    pub async fn send_ack(&mut self, container: &Struct, message: &Struct) -> Result<()> {
        let empty = BTreeMap::new();
        let meta = container
            .get(&8)
            .map(Thrift::structure)
            .transpose()?
            .unwrap_or(&empty);
        let item = message
            .get(&8)
            .map(Thrift::structure)
            .transpose()?
            .unwrap_or(&empty);
        let id = meta.get(&1).or_else(|| message.get(&3));
        let timestamp = meta.get(&2).or_else(|| item.get(&5));
        let (Some(id), Some(timestamp)) = (id, timestamp) else {
            return Ok(());
        };
        let app = container
            .get(&5)
            .cloned()
            .unwrap_or_else(|| text(&self.runtime.app_id));
        let package = container
            .get(&6)
            .cloned()
            .unwrap_or_else(|| text(&self.runtime.package));
        let mut ack = BTreeMap::from([
            (3, id.clone()),
            (4, app.clone()),
            (5, Thrift::Long(timestamp.int()?)),
            (9, package.clone()),
        ]);
        if let Some(topic) = meta.get(&3) {
            ack.insert(6, topic.clone());
        }
        let wrapped = BTreeMap::from([
            (1, Thrift::Int(6)),
            (2, Thrift::Bool(false)),
            (3, Thrift::Bool(true)),
            (4, text(encode_thrift(&ack)?)),
            (5, app),
            (6, package),
            (7, Thrift::Struct(self.target()?)),
        ]);
        let mut blob = self.blob("SECMSG", encode_thrift(&wrapped)?, None, true)?;
        blob.kind = 1;
        blob.header.0.push(bytes(6, "message"));
        self.send(Some(&blob)).await
    }
    pub async fn close(&mut self) -> Result<()> {
        let blob = Blob {
            header: Fields(vec![integer(1, 0), bytes(5, "CLOSE")]),
            payload: Vec::new(),
            kind: 2,
        };
        self.send(Some(&blob)).await?;
        self.stream.shutdown().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    fn vfield(number: u32, value: u64) -> Vec<u8> {
        let mut writer = crate::protocol::pb::Writer::new();
        writer.varint(number, value);
        writer.finish()
    }

    use super::*;
    #[test]
    fn recovered_slim_and_thrift_vectors() -> Result<()> {
        assert_eq!(hex::encode(rc4(b"Key", b"Plaintext")), "bbf316e8d940af0ad3");
        let fixture = hex::decode("0a000600000000000000000b00080000000361626300")?;
        let values = BTreeMap::from([(6, Thrift::Long(0)), (8, text("abc"))]);
        assert_eq!(decode_thrift(&fixture)?, values);
        assert_eq!(encode_thrift(&values)?, fixture);
        let blob = Blob {
            header: Fields(vec![integer(1, 0), bytes(5, "CONN")]),
            payload: vfield(1, 106),
            kind: 2,
        };
        assert_eq!(
            &frame(Some(&blob), None)?[..8],
            &hex::decode("c2fe000500000012")?
        );
        Ok(())
    }
}
