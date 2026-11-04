//! One protobuf model for every Lyra/Mirror message.
use super::protobuf::{self, RawValue};
use crate::Result;
use anyhow::{Context, bail, ensure};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Varint(u64),
    Fixed32(u32),
    Fixed64(u64),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Fields(pub(crate) Vec<(u32, Value)>);

impl Fields {
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let mut fields = Vec::new();
        for (number, value) in protobuf::decode(data)? {
            ensure!(number > 0, "invalid protobuf field zero");
            let value = match value {
                RawValue::Varint(value) => Value::Varint(value),
                RawValue::Fixed32(bytes) => Value::Fixed32(u32::from_le_bytes(bytes)),
                RawValue::Fixed64(bytes) => Value::Fixed64(u64::from_le_bytes(bytes)),
                RawValue::Bytes(bytes) => Value::Bytes(bytes),
            };
            fields.push((number, value));
        }
        Ok(Self(fields))
    }

    pub(crate) fn uint(&self, number: u32) -> u64 {
        self.opt_uint(number).unwrap_or(0)
    }
    pub(crate) fn opt_uint(&self, number: u32) -> Option<u64> {
        self.last(number).and_then(|value| match value {
            Value::Varint(value) => Some(*value),
            _ => None,
        })
    }
    pub(crate) fn bytes(&self, number: u32) -> &[u8] {
        self.opt_bytes(number).unwrap_or(&[])
    }
    pub(crate) fn opt_bytes(&self, number: u32) -> Option<&[u8]> {
        self.last(number).and_then(|value| match value {
            Value::Bytes(value) => Some(value.as_slice()),
            _ => None,
        })
    }
    pub(crate) fn repeated_bytes(&self, number: u32) -> Vec<&[u8]> {
        self.values(number)
            .into_iter()
            .filter_map(|value| match value {
                Value::Bytes(value) => Some(value.as_slice()),
                _ => None,
            })
            .collect()
    }
    pub(crate) fn text(&self, number: u32) -> String {
        String::from_utf8_lossy(self.bytes(number)).into_owned()
    }
    pub(crate) fn string(&self, number: u32) -> String {
        self.text(number)
    }
    pub(crate) fn has(&self, number: u32) -> bool {
        self.0.iter().any(|(field, _)| *field == number)
    }
    pub(crate) fn message<T: Message>(&self, number: u32) -> Result<T> {
        T::decode(self.bytes(number)).with_context(|| format!("decode protobuf field {number}"))
    }
    pub(crate) fn opt_message<T: Message>(&self, number: u32) -> Result<Option<T>> {
        if self.bytes(number).is_empty() {
            return Ok(None);
        }
        self.message(number).map(Some)
    }
    pub(crate) fn repeated_message<T: Message>(&self, number: u32) -> Result<Vec<T>> {
        self.0
            .iter()
            .filter(|(field, _)| *field == number)
            .map(|(_, value)| match value {
                Value::Bytes(bytes) => T::decode(bytes),
                _ => bail!("protobuf field {number} is not a message"),
            })
            .collect()
    }
    fn last(&self, number: u32) -> Option<&Value> {
        self.0
            .iter()
            .rev()
            .find(|(field, _)| *field == number)
            .map(|(_, value)| value)
    }
    fn values(&self, number: u32) -> Vec<&Value> {
        self.0
            .iter()
            .filter(|(field, _)| *field == number)
            .map(|(_, value)| value)
            .collect()
    }
}

#[derive(Default)]
pub(crate) struct Writer(Vec<u8>);

impl Writer {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn varint(&mut self, number: u32, value: u64) {
        self.0.extend(protobuf::key(number, 0));
        self.0.extend(protobuf::varint(value));
    }
    pub(crate) fn opt_varint(&mut self, number: u32, value: u64) {
        if value != 0 {
            self.varint(number, value);
        }
    }
    pub(crate) fn bytes(&mut self, number: u32, value: impl AsRef<[u8]>) {
        let value = value.as_ref();
        self.0.extend(protobuf::key(number, 2));
        self.0.extend(protobuf::varint(value.len() as u64));
        self.0.extend_from_slice(value);
    }
    pub(crate) fn opt_bytes(&mut self, number: u32, value: impl AsRef<[u8]>) {
        let value = value.as_ref();
        if !value.is_empty() {
            self.bytes(number, value);
        }
    }
    pub(crate) fn message(&mut self, number: u32, value: &impl Message) {
        self.bytes(number, value.encode());
    }
    pub(crate) fn fixed32(&mut self, number: u32, value: u32) {
        self.0.extend(protobuf::key(number, 5));
        self.0.extend(value.to_le_bytes());
    }
    pub(crate) fn fixed64(&mut self, number: u32, value: u64) {
        self.0.extend(protobuf::key(number, 1));
        self.0.extend(value.to_le_bytes());
    }
    pub(crate) fn finish(self) -> Vec<u8> {
        self.0
    }
}

pub(crate) trait Message: Sized {
    fn encode(&self) -> Vec<u8>;
    fn decode(data: &[u8]) -> Result<Self>;
}

impl Message for Fields {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        for (number, value) in &self.0 {
            match value {
                Value::Varint(value) => writer.varint(*number, *value),
                Value::Fixed32(value) => writer.fixed32(*number, *value),
                Value::Fixed64(value) => writer.fixed64(*number, *value),
                Value::Bytes(value) => writer.bytes(*number, value),
            }
        }
        writer.finish()
    }

    fn decode(data: &[u8]) -> Result<Self> {
        Fields::decode(data)
    }
}

pub(crate) fn fields(data: &[u8]) -> Result<Fields> {
    Fields::decode(data)
}

#[cfg(test)]
pub(crate) fn v(number: u32, value: u64) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(number, value);
    writer.finish()
}

#[cfg(test)]
pub(crate) fn b(number: u32, value: impl AsRef<[u8]>) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.bytes(number, value);
    writer.finish()
}

pub(crate) fn mesh(kind: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![(kind << 3) | 1, 4];
    out.extend(((data.len() + 4) as u16).to_be_bytes());
    out.extend_from_slice(data);
    out
}

pub(crate) fn unmesh(data: &[u8]) -> Result<(u8, &[u8])> {
    ensure!(data.len() >= 4, "Truncated mesh header");
    let length = u16::from_be_bytes([data[2], data[3]]) as usize;
    let header = (data[1] & 15) as usize;
    ensure!(
        data.len() == length && header >= 4 && header <= length,
        "Incomplete mesh packet"
    );
    Ok((data[0] >> 3, &data[header..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_varint_bytes_and_fixed_fields() -> Result<()> {
        let mut writer = Writer::new();
        writer.varint(1, 300);
        writer.bytes(2, b"hello");
        writer.varint(3, 0);
        let data = writer.finish();
        let fields = Fields::decode(&data)?;
        assert_eq!(fields.uint(1), 300);
        assert_eq!(fields.bytes(2), b"hello");
        assert!(fields.has(3));
        Ok(())
    }
}
