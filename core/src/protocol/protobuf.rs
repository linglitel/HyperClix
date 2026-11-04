//! Protobuf wire primitives shared by Lyra and Mirror codecs.
use crate::Result;
use anyhow::{bail, ensure};

pub(crate) enum RawValue {
    Varint(u64),
    Fixed32([u8; 4]),
    Fixed64([u8; 8]),
    Bytes(Vec<u8>),
}

pub(crate) fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
    out
}

pub(crate) fn key(number: u32, wire: u8) -> Vec<u8> {
    varint(((number as u64) << 3) | u64::from(wire))
}

fn read_varint(data: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut out = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *data
            .get(*cursor)
            .ok_or_else(|| anyhow::anyhow!("incomplete protobuf varint"))?;
        *cursor += 1;
        ensure!(shift < 63 || byte <= 1, "protobuf varint overflow");
        out |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(out);
        }
    }
    bail!("protobuf varint overflow")
}

pub(crate) fn decode(data: &[u8]) -> Result<Vec<(u32, RawValue)>> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor < data.len() {
        let tag = read_varint(data, &mut cursor)?;
        let number = (tag >> 3) as u32;
        let value = match tag & 7 {
            0 => RawValue::Varint(read_varint(data, &mut cursor)?),
            1 => {
                let end = cursor
                    .checked_add(8)
                    .filter(|end| *end <= data.len())
                    .ok_or_else(|| anyhow::anyhow!("incomplete fixed64"))?;
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&data[cursor..end]);
                cursor = end;
                RawValue::Fixed64(bytes)
            }
            2 => {
                let length = read_varint(data, &mut cursor)? as usize;
                let end = cursor
                    .checked_add(length)
                    .filter(|end| *end <= data.len())
                    .ok_or_else(|| anyhow::anyhow!("incomplete protobuf bytes"))?;
                let bytes = data[cursor..end].to_vec();
                cursor = end;
                RawValue::Bytes(bytes)
            }
            5 => {
                let end = cursor
                    .checked_add(4)
                    .filter(|end| *end <= data.len())
                    .ok_or_else(|| anyhow::anyhow!("incomplete fixed32"))?;
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(&data[cursor..end]);
                cursor = end;
                RawValue::Fixed32(bytes)
            }
            wire => bail!("unsupported protobuf wire type {wire}"),
        };
        out.push((number, value));
    }
    Ok(out)
}
