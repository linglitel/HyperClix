//! MiPush Thrift and RC4 framing codecs.
use crate::Result;
use anyhow::{Context, bail};
use std::collections::BTreeMap;

pub(crate) type Struct = BTreeMap<i16, Thrift>;

#[derive(Clone, Debug, PartialEq)]
pub enum Thrift {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Bytes(Vec<u8>),
    Struct(Struct),
    Map(u8, u8, Vec<(Thrift, Thrift)>),
    List(u8, Vec<Thrift>),
    Set(u8, Vec<Thrift>),
}
impl Thrift {
    fn kind(&self) -> u8 {
        match self {
            Self::Bool(_) => 2,
            Self::Byte(_) => 3,
            Self::Short(_) => 6,
            Self::Int(_) => 8,
            Self::Long(_) => 10,
            Self::Bytes(_) => 11,
            Self::Struct(_) => 12,
            Self::Map(..) => 13,
            Self::Set(..) => 14,
            Self::List(..) => 15,
        }
    }
    pub fn bytes(&self) -> Result<&[u8]> {
        match self {
            Self::Bytes(value) => Ok(value),
            _ => bail!("Expected Thrift byte string"),
        }
    }
    pub fn int(&self) -> Result<i64> {
        match self {
            Self::Bool(v) => Ok(i64::from(*v)),
            Self::Byte(v) => Ok(*v as i64),
            Self::Short(v) => Ok(*v as i64),
            Self::Int(v) => Ok(*v as i64),
            Self::Long(v) => Ok(*v),
            _ => bail!("Expected Thrift integer"),
        }
    }
    pub fn structure(&self) -> Result<&Struct> {
        match self {
            Self::Struct(v) => Ok(v),
            _ => bail!("Expected Thrift structure"),
        }
    }
    pub fn map(&self) -> Result<&[(Thrift, Thrift)]> {
        match self {
            Self::Map(_, _, v) => Ok(v),
            _ => bail!("Expected Thrift map"),
        }
    }
}
pub(crate) fn text(value: impl AsRef<[u8]>) -> Thrift {
    Thrift::Bytes(value.as_ref().to_vec())
}
fn encode_value(value: &Thrift, out: &mut Vec<u8>) -> Result<()> {
    match value {
        Thrift::Bool(v) => out.push(u8::from(*v)),
        Thrift::Byte(v) => out.push(*v as u8),
        Thrift::Short(v) => out.extend(v.to_be_bytes()),
        Thrift::Int(v) => out.extend(v.to_be_bytes()),
        Thrift::Long(v) => out.extend(v.to_be_bytes()),
        Thrift::Bytes(v) => {
            out.extend(i32::try_from(v.len())?.to_be_bytes());
            out.extend(v);
        }
        Thrift::Struct(v) => out.extend(encode_thrift(v)?),
        Thrift::Map(kt, vt, values) => {
            out.extend([*kt, *vt]);
            out.extend(i32::try_from(values.len())?.to_be_bytes());
            for (k, v) in values {
                encode_value(k, out)?;
                encode_value(v, out)?;
            }
        }
        Thrift::List(kind, values) | Thrift::Set(kind, values) => {
            out.push(*kind);
            out.extend(i32::try_from(values.len())?.to_be_bytes());
            for v in values {
                encode_value(v, out)?;
            }
        }
    }
    Ok(())
}
pub(crate) fn encode_thrift(values: &Struct) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (number, value) in values {
        out.push(value.kind());
        out.extend(number.to_be_bytes());
        encode_value(value, &mut out)?;
    }
    out.push(0);
    Ok(out)
}
struct Reader<'a> {
    data: &'a [u8],
    cursor: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .cursor
            .checked_add(n)
            .context("Thrift length overflow")?;
        let value = self
            .data
            .get(self.cursor..end)
            .context("Truncated Thrift value")?;
        self.cursor = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn size(&mut self) -> Result<usize> {
        let value = i32::from_be_bytes(self.take(4)?.try_into()?);
        Ok(usize::try_from(value)?)
    }
    fn structure(&mut self) -> Result<Struct> {
        let mut out = BTreeMap::new();
        loop {
            let kind = self.byte()?;
            if kind == 0 {
                return Ok(out);
            }
            let number = i16::from_be_bytes(self.take(2)?.try_into()?);
            out.insert(number, self.value(kind)?);
        }
    }
    fn value(&mut self, kind: u8) -> Result<Thrift> {
        Ok(match kind {
            2 => Thrift::Bool(self.byte()? != 0),
            3 => Thrift::Byte(self.byte()? as i8),
            6 => Thrift::Short(i16::from_be_bytes(self.take(2)?.try_into()?)),
            8 => Thrift::Int(i32::from_be_bytes(self.take(4)?.try_into()?)),
            10 => Thrift::Long(i64::from_be_bytes(self.take(8)?.try_into()?)),
            11 => {
                let n = self.size()?;
                Thrift::Bytes(self.take(n)?.to_vec())
            }
            12 => Thrift::Struct(self.structure()?),
            13 => {
                let kt = self.byte()?;
                let vt = self.byte()?;
                let n = self.size()?;
                let mut out = Vec::new();
                for _ in 0..n {
                    out.push((self.value(kt)?, self.value(vt)?));
                }
                Thrift::Map(kt, vt, out)
            }
            14 | 15 => {
                let et = self.byte()?;
                let n = self.size()?;
                let mut out = Vec::new();
                for _ in 0..n {
                    out.push(self.value(et)?);
                }
                if kind == 14 {
                    Thrift::Set(et, out)
                } else {
                    Thrift::List(et, out)
                }
            }
            _ => bail!("Unsupported Thrift type {kind}"),
        })
    }
}
pub(crate) fn decode_thrift(data: &[u8]) -> Result<Struct> {
    let mut reader = Reader { data, cursor: 0 };
    let result = reader.structure()?;
    anyhow::ensure!(
        reader.cursor == data.len(),
        "Trailing bytes in Thrift structure"
    );
    Ok(result)
}

pub(crate) fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut state = [0u8; 256];
    for (i, v) in state.iter_mut().enumerate() {
        *v = i as u8;
    }
    let mut j = 0;
    for i in 0..256 {
        j = (j + state[i] as usize + key[i % key.len()] as usize) & 255;
        state.swap(i, j);
    }
    let (mut i, mut j) = (0, 0);
    data.iter()
        .map(|value| {
            i = (i + 1) & 255;
            j = (j + state[i] as usize) & 255;
            state.swap(i, j);
            *value ^ state[((state[i] as usize) + (state[j] as usize)) & 255]
        })
        .collect()
}
pub(crate) fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for value in data {
        a = (a + *value as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
