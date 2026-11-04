//! SDK media channel framing and association packets.
use crate::Result;
use anyhow::ensure;
use std::collections::BTreeMap;

pub(crate) const MAGIC: [u8; 4] = [1, 1, 255, 255];

/// Decoded `sdk_values` payload: a kind and its TLV values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MediaPacket {
    pub kind: u16,
    pub values: BTreeMap<u16, Vec<u8>>,
}

impl MediaPacket {
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        ensure!(data.len() >= 18 && data[..4] == MAGIC, "Invalid SDK TLV");
        let kind = u16::from_be_bytes([data[8], data[9]]);
        let mut values = BTreeMap::new();
        let mut offset = 18;
        while offset < data.len() {
            ensure!(offset + 8 <= data.len(), "Truncated SDK TLV");
            let tag = u16::from_be_bytes([data[offset + 2], data[offset + 3]]);
            let length = u32::from_be_bytes(data[offset + 4..offset + 8].try_into()?) as usize;
            offset += 8;
            ensure!(offset + length <= data.len(), "Truncated SDK value");
            values.insert(tag, data[offset..offset + length].to_vec());
            offset += length;
        }
        Ok(Self { kind, values })
    }

    pub(crate) fn value(&self, tag: u16) -> Option<&[u8]> {
        self.values.get(&tag).map(Vec::as_slice)
    }

    pub(crate) fn id(&self) -> u32 {
        self.value(0)
            .and_then(|value| value.try_into().ok())
            .map(u32::from_be_bytes)
            .unwrap_or(0)
    }
}

/// Native application-data wrapper for the SDK media channel.
pub(crate) fn application_data(data: &[u8]) -> Vec<u8> {
    let item = [
        vec![0, 5, 0, 0],
        (data.len() as u32).to_be_bytes().to_vec(),
        data.to_vec(),
    ]
    .concat();
    let body = [
        vec![0, 1, 1, 0, 0, 1],
        (item.len() as u32).to_be_bytes().to_vec(),
        item,
    ]
    .concat();
    [
        MAGIC.to_vec(),
        (body.len() as u32).to_be_bytes().to_vec(),
        body,
    ]
    .concat()
}

/// Initial client association frame; the SDK id occupies bytes 26..30.
pub(crate) fn association(sdk_id: u32) -> Result<Vec<u8>> {
    let mut frame = hex::decode(
        "0101ffff0000002e0000010000000000002400030000000000040000000200030001000000040000000100030002000000040000ff00",
    )?;
    ensure!(frame.len() >= 30, "invalid SDK association template");
    frame[26..30].copy_from_slice(&sdk_id.to_be_bytes());
    Ok(frame)
}

/// Server response carrying the negotiated SDK id.
pub(crate) fn server_info(sdk_id: u32) -> Vec<u8> {
    let values = [
        vec![0, 3, 0, 0, 0, 0, 0, 4],
        sdk_id.to_be_bytes().to_vec(),
        vec![0, 3, 0, 1, 0, 0, 0, 4, 0, 0, 255, 0],
    ]
    .concat();
    let body = [
        vec![0, 4, 1, 0, 0, 4],
        (values.len() as u32).to_be_bytes().to_vec(),
        values,
    ]
    .concat();
    [
        MAGIC.to_vec(),
        (body.len() as u32).to_be_bytes().to_vec(),
        body,
    ]
    .concat()
}

/// Encrypt one SDK media payload.
pub(crate) fn encrypt(key: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
    let encrypted = crate::common::crypto::aes_encrypt(key, plain)?;
    ensure!(encrypted.len() + 8 <= u16::MAX as usize, "SDK单帧过大");
    Ok([
        vec![0x82, 0x58],
        ((encrypted.len() + 8) as u16).to_be_bytes().to_vec(),
        vec![0, 0, 0, 1],
        encrypted,
    ]
    .concat())
}

/// One encrypted fragment from the SDK media channel.
pub(crate) struct Fragment<'a> {
    pub index: u16,
    pub count: u16,
    pub ciphertext: &'a [u8],
}

pub(crate) fn fragment(packet: &[u8]) -> Result<Option<Fragment<'_>>> {
    if packet.starts_with(&MAGIC) {
        return Ok(None);
    }
    ensure!(packet.len() >= 4, "截断SDK加密片段");
    let header = (packet[1] & 15) as usize;
    ensure!(packet.len() >= header, "截断SDK片段头");
    let (index, count) = if header == 8 {
        (
            u16::from_be_bytes(packet[4..6].try_into()?),
            u16::from_be_bytes(packet[6..8].try_into()?),
        )
    } else {
        (0, 1)
    };
    Ok(Some(Fragment {
        index,
        count,
        ciphertext: &packet[header..],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_packet_decodes_application_data() -> Result<()> {
        let packet = MediaPacket::decode(&application_data(b"hello"))?;
        assert_eq!(packet.kind, 1);
        assert_eq!(packet.value(0), Some(b"hello".as_slice()));
        Ok(())
    }
}
