use crate::Result;
use anyhow::{Context, ensure};

// libmicontinuity BleDataFormatter::Pack / Encode / DecodeDataHeader.
pub(super) fn pack(payload: &[u8], index: u16, max_packet: usize) -> Result<Vec<Vec<u8>>> {
    ensure!(
        max_packet > 8 && max_packet <= 4095,
        "unsupported BLE packet size"
    );
    let size = max_packet - 8;
    let count = payload.len().div_ceil(size).max(1);
    let mut packets = Vec::with_capacity(count);
    for sequence in 0..count {
        let start = sequence * size;
        let end = (start + size).min(payload.len());
        let length = 8 + end - start;
        let mut packet = vec![
            0x55,
            0xaa,
            (length >> 4) as u8,
            ((length << 4) as u8) | 8,
            (index >> 8) as u8,
            index as u8,
            sequence as u8,
            if sequence == 0 { 0x80 } else { 0 } | if sequence + 1 == count { 0x40 } else { 0 },
        ];
        packet.extend_from_slice(&payload[start..end]);
        packets.push(packet);
    }
    Ok(packets)
}

#[derive(Default)]
pub(super) struct Assembler {
    pending: Option<(u16, u8, Vec<u8>)>,
}
impl Assembler {
    pub fn feed(&mut self, packet: &[u8]) -> Result<Option<Vec<u8>>> {
        ensure!(
            packet.len() >= 8 && packet[..2] == [0x55, 0xaa],
            "invalid Lyra BLE packet header"
        );
        let length = (usize::from(packet[2]) << 4) | usize::from(packet[3] >> 4);
        ensure!(length == packet.len(), "Lyra BLE packet length mismatch");
        let header_length = usize::from(packet[3] & 15);
        ensure!(
            header_length >= 8 && header_length <= length,
            "invalid Lyra BLE header length"
        );
        let flags = packet[7];
        let payload_end = if flags & 0x20 != 0 {
            ensure!(length >= 12, "Lyra BLE CRC packet is incomplete");
            let end = length - 4;
            let mut crc = flate2::Crc::new();
            crc.update(&packet[..end]);
            ensure!(
                crc.sum() == u32::from_be_bytes(packet[end..].try_into().unwrap()),
                "Lyra BLE CRC mismatch"
            );
            end
        } else {
            length
        };
        if flags & 3 == 1 {
            tracing::info!("[nearby] BLE transport control packet received");
            return Ok(None);
        }
        ensure!(flags & 3 == 0, "unsupported Lyra BLE packet kind");
        let index = u16::from_be_bytes([packet[4], packet[5]]);
        let sequence = packet[6];
        if flags & 0x80 != 0 {
            self.pending = Some((index, sequence, packet[header_length..payload_end].to_vec()));
        } else {
            let (previous_index, previous_sequence, data) = self
                .pending
                .as_mut()
                .context("Lyra BLE continuation without first fragment")?;
            ensure!(
                *previous_index == index && previous_sequence.wrapping_add(1) == sequence,
                "Lyra BLE fragment sequence mismatch"
            );
            data.extend_from_slice(&packet[header_length..payload_end]);
            *previous_sequence = sequence;
        }
        if flags & 0x40 != 0 {
            Ok(self.pending.take().map(|(_, _, data)| data))
        } else {
            Ok(None)
        }
    }
}
