//! Phone camera TS/PES protection used by the WFD preview receiver.
use crate::Result;
use anyhow::Context;
use openssl::symm::{Cipher, Crypter, Mode};
use std::collections::HashMap;

pub(crate) const WFD_PROTECTION: &str = "4 1 0 1 1 1 0 0";

/// Native camera PES protection: private IV16, CBC full blocks, clear short tail.
pub fn decrypt_pes(pes: &[u8], key: &[u8; 16], encrypted_bytes: usize) -> Result<Vec<u8>> {
    anyhow::ensure!(
        pes.len() >= 9 && pes[..3] == [0, 0, 1] && (0xe0..=0xef).contains(&pes[3]),
        "Expected camera video PES"
    );
    let end = 9 + pes[8] as usize;
    anyhow::ensure!(pes.len() >= end, "Truncated PES header");
    let flags = pes[7];
    let mut offset = 9 + match flags >> 6 {
        3 => 10,
        2 => 5,
        _ => 0,
    };
    for (bit, size) in [(5, 6), (4, 3), (3, 1), (2, 1), (1, 2)] {
        if flags & (1 << bit) != 0 {
            offset += size;
        }
    }
    anyhow::ensure!(
        flags & 1 != 0 && offset + 17 <= end && pes[offset] & 0x80 != 0,
        "Camera PES has no private AES IV"
    );
    anyhow::ensure!(
        encrypted_bytes % 16 == 0,
        "Camera encrypted data length is not block aligned"
    );
    let payload = &pes[end..];
    let mut length = payload.len() & !15;
    if encrypted_bytes > 0 {
        length = length.min(encrypted_bytes);
    }
    let mut decoder = Crypter::new(
        Cipher::aes_128_cbc(),
        Mode::Decrypt,
        key,
        Some(&pes[offset + 1..offset + 17]),
    )?;
    decoder.pad(false);
    let mut plain = vec![0; length + 16];
    let used = decoder.update(&payload[..length], &mut plain)?;
    let final_used = decoder.finalize(&mut plain[used..])?;
    plain.truncate(used + final_used);
    plain.extend_from_slice(&payload[length..]);
    Ok(plain)
}

/// Reassembles complete video PES from TS, dropping a damaged ciphertext PES.
pub struct PreviewTsDecoder {
    key: [u8; 16],
    encrypted_bytes: usize,
    pending: Vec<u8>,
    pes: HashMap<u16, Vec<u8>>,
    continuity: HashMap<u16, u8>,
    pub complete_pes: u64,
    pub continuity_gaps: u64,
    pub discarded_pes: u64,
}
impl PreviewTsDecoder {
    pub fn new(key: [u8; 16], encrypted_bytes: usize) -> Self {
        Self {
            key,
            encrypted_bytes,
            pending: Vec::new(),
            pes: HashMap::new(),
            continuity: HashMap::new(),
            complete_pes: 0,
            continuity_gaps: 0,
            discarded_pes: 0,
        }
    }
    fn finish_pid(&mut self, pid: u16) -> Result<Vec<u8>> {
        let mut data = self.pes.remove(&pid).context("Missing camera PES")?;
        let size = u16::from_be_bytes(data[4..6].try_into()?) as usize;
        if size != 0 {
            anyhow::ensure!(data.len() >= size + 6, "Truncated camera PES");
            data.truncate(size + 6);
        }
        self.complete_pes += 1;
        decrypt_pes(&data, &self.key, self.encrypted_bytes)
    }
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.pending.extend_from_slice(data);
        let count = self.pending.len() / 188;
        let packets: Vec<u8> = self.pending.drain(..count * 188).collect();
        let mut result = Vec::new();
        for packet in packets.chunks_exact(188) {
            anyhow::ensure!(
                packet[0] == 0x47 && packet[1] & 0x80 == 0,
                "Invalid camera TS sync/transport error"
            );
            let pid = (((packet[1] & 31) as u16) << 8) | packet[2] as u16;
            if packet[3] & 0x10 == 0 {
                continue;
            }
            let offset = if packet[3] & 0x20 != 0 {
                5 + packet[4] as usize
            } else {
                4
            };
            anyhow::ensure!(offset <= 188, "Invalid TS adaptation field");
            let payload = &packet[offset..];
            let counter = packet[3] & 15;
            if let Some(previous) = self.continuity.get_mut(&pid) {
                if counter != (*previous + 1) & 15 {
                    self.continuity_gaps += 1;
                    if self.pes.remove(&pid).is_some() {
                        self.discarded_pes += 1;
                    }
                }
                *previous = counter;
            }
            if packet[1] & 0x40 != 0 {
                if self.pes.contains_key(&pid) {
                    result.push(self.finish_pid(pid)?);
                }
                if payload.len() >= 6
                    && payload[..3] == [0, 0, 1]
                    && (0xe0..=0xef).contains(&payload[3])
                {
                    self.pes.insert(pid, Vec::new());
                } else {
                    self.continuity.remove(&pid);
                    continue;
                }
            }
            if let Some(pes) = self.pes.get_mut(&pid) {
                self.continuity.insert(pid, counter);
                pes.extend_from_slice(payload);
                if pes.len() >= 6 {
                    let size = u16::from_be_bytes(pes[4..6].try_into()?) as usize;
                    if size != 0 && pes.len() >= size + 6 {
                        result.push(self.finish_pid(pid)?);
                    }
                }
            }
        }
        Ok(result)
    }
}
