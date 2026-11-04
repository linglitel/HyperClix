//! MessageCenter framing used by phone notification control and topics.
use crate::Result;
use crate::protocol::pb::Writer;
use anyhow::{Context, ensure};
use serde_json::Value as Json;

fn length(value: usize) -> Result<Vec<u8>> {
    ensure!(
        value < 16384,
        "MessageCenter payload exceeds its two-byte length"
    );
    Ok(if value < 127 {
        vec![value as u8]
    } else {
        vec![0x80 | ((value & 127) as u8), (value >> 7) as u8]
    })
}

fn text8(value: &str) -> Result<Vec<u8>> {
    let mut out = vec![u8::try_from(value.len())?];
    out.extend(value.as_bytes());
    Ok(out)
}

/// Native WLAN dynamic topic format; sequence occupies the low four bits.
pub fn encode_message(entity: &Json, source: &str, target: &str, sequence: u8) -> Result<Vec<u8>> {
    let config = [
        vec![0, 2],
        text8("notifi_trans")?,
        text8(source)?,
        vec![1],
        text8(target)?,
    ]
    .concat();
    let body = serde_json::to_vec(entity)?;
    Ok([
        vec![0, 255, sequence & 15],
        length(config.len())?,
        config,
        vec![254, sequence & 15],
        length(body.len())?,
        body,
    ]
    .concat())
}

/// Wrap a MessageCenter payload in the Networking module-1 envelope.
pub fn networking_frame(body: &[u8]) -> Vec<u8> {
    let mut inner = Writer::new();
    inner.bytes(1, body);
    let mut writer = Writer::new();
    writer.varint(1, 4);
    writer.bytes(8, inner.finish());
    writer.finish()
}

pub fn decode_message(raw: &[u8]) -> Result<Vec<Json>> {
    ensure!(
        raw.first() == Some(&0),
        "Unsupported MessageCenter transport"
    );
    let mut cursor = 1;
    let mut active = false;
    let mut out = Vec::new();
    while cursor < raw.len() {
        ensure!(cursor + 3 <= raw.len(), "Truncated MessageCenter header");
        let topic = raw[cursor];
        let flags = raw[cursor + 1];
        cursor += 2 + (flags >> 4) as usize;
        let first = *raw.get(cursor).context("Missing MessageCenter length")?;
        cursor += 1;
        let size = if first & 128 != 0 {
            let n = *raw.get(cursor).context("Missing second length byte")?;
            cursor += 1;
            ((first & 127) as usize) | ((n as usize) << 7)
        } else {
            first as usize
        };
        let body = raw
            .get(cursor..cursor + size)
            .context("Truncated MessageCenter body")?;
        cursor += size;
        if topic == 255 {
            ensure!(
                body.len() >= 3 && body[..2] == [0, 2],
                "Unknown MessageCenter topic header"
            );
            let n = body[2] as usize;
            active = body.get(3..3 + n) == Some(b"notifi_trans");
        } else if topic == 254 && active {
            out.push(serde_json::from_slice(body)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_messagecenter_vector() -> Result<()> {
        let entity = json!({"className":"noti_trans","methodName":"exc_key"});
        let expected=b"\x00\xff\x01\x14\x00\x02\x0cnotifi_trans\x01A\x01\x01B\xfe\x01\x31{\"className\":\"noti_trans\",\"methodName\":\"exc_key\"}";
        assert_eq!(encode_message(&entity, "A", "B", 1)?, expected);
        assert_eq!(decode_message(expected)?, vec![entity]);
        Ok(())
    }
}
