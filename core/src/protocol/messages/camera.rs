//! Camera distributedHardware metadata envelopes.
use crate::Result;
use crate::common::time::epoch_ms;
use crate::protocol::pb::{Fields, Message, Writer};
use anyhow::Context;

pub(crate) const METADATA_CATEGORY: &str =
    "CATEG:com.xiaomi.dist.hardware.service:hardware_service:cmd";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HardwareInfo {
    pub id: String,
    pub owner: String,
    pub kind: u64,
    pub property: String,
    pub attributes: String,
    pub device_id: String,
    pub device_name: String,
    pub device_type: u64,
}

impl Message for HardwareInfo {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        for (number, value) in [
            (1, &self.id),
            (2, &self.owner),
            (5, &self.property),
            (6, &self.attributes),
            (7, &self.device_id),
            (8, &self.device_name),
        ] {
            writer.opt_bytes(number, value);
        }
        writer.varint(4, self.kind);
        writer.varint(9, self.device_type);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            id: fields.text(1),
            owner: fields.text(2),
            kind: fields.uint(4),
            property: fields.text(5),
            attributes: fields.text(6),
            device_id: fields.text(7),
            device_name: fields.text(8),
            device_type: fields.uint(9),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CameraEnvelope {
    pub request: bool,
    pub body: Vec<u8>,
}

impl CameraEnvelope {
    pub(crate) fn encode(&self, own_id: &str) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, 1);
        writer.bytes(2, own_id);
        writer.varint(3, epoch_ms());
        writer.bytes(4, METADATA_CATEGORY);
        if self.request {
            writer.varint(5, 1);
            writer.varint(6, 1);
        }
        writer.bytes(8, &self.body);
        writer.finish()
    }
}

pub(crate) fn metadata_request(own_id: &str, device_type: u64) -> Vec<u8> {
    let mut body = Writer::new();
    body.varint(1, 3000011);
    body.varint(2, 3);
    body.varint(4, device_type);
    body.bytes(5, "HardwareService");
    CameraEnvelope {
        request: true,
        body: body.finish(),
    }
    .encode(own_id)
}

pub(crate) fn hardware_message(
    own_id: &str,
    hardware: &HardwareInfo,
    device_type: u64,
    action: u64,
) -> Vec<u8> {
    let mut body = Writer::new();
    body.varint(1, 3000011);
    body.varint(2, action);
    if action == 5 {
        body.varint(3, 1);
    }
    body.varint(4, device_type);
    body.bytes(5, "HardwareService");
    body.varint(6, 1);
    body.bytes(7, hardware.encode());
    CameraEnvelope {
        request: false,
        body: body.finish(),
    }
    .encode(own_id)
}

#[derive(Clone, Debug)]
pub(crate) struct CameraResponse {
    pub own_id: String,
    pub category: String,
    pub seq_id: u64,
    pub packet_type: u64,
    pub request_flag: u64,
    pub device_type: u64,
    pub action: u64,
    pub status: u64,
    pub hardware: Vec<HardwareInfo>,
}

impl CameraResponse {
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let envelope = Fields::decode(data)?;
        let body = envelope.message::<Fields>(8)?;
        Ok(Self {
            own_id: envelope.text(2),
            category: envelope.text(4),
            seq_id: envelope.uint(5),
            packet_type: envelope.uint(6),
            request_flag: envelope.uint(7),
            device_type: body.uint(4),
            action: body.uint(2),
            status: body.uint(8),
            hardware: body.repeated_message(7)?,
        })
    }
}

/// Response to the phone's hardware metadata request, used by the PC-side
/// distributedHardware service. The PC exposes no distributed hardware, so the
/// list is empty; a valid RESPONSE packet is still required for the phone to
/// keep the service online.
pub(crate) fn metadata_response(
    own_id: &str,
    seq_id: u64,
    device_type: u64,
    action: u64,
    status: u64,
) -> Vec<u8> {
    let mut body = Writer::new();
    body.varint(1, 3000011);
    body.varint(2, action);
    body.varint(4, device_type);
    body.bytes(5, "HardwareService");
    if status != 0 {
        body.varint(8, status);
    }
    let mut envelope = Writer::new();
    envelope.varint(1, 1);
    envelope.bytes(2, own_id);
    envelope.varint(3, epoch_ms());
    envelope.bytes(4, METADATA_CATEGORY);
    if seq_id != 0 {
        envelope.varint(5, seq_id);
    }
    envelope.varint(6, 2);
    // CrossDeviceData.responseCode: DistCodes.SUCC == 1. The phone's
    // CrossDeviceMessageManager marks a response successful only for 1.
    envelope.varint(7, 1);
    envelope.bytes(8, &body.finish());
    envelope.finish()
}

pub(crate) fn hardware_from_response(
    response: &CameraResponse,
    index: usize,
) -> Result<HardwareInfo> {
    let mut hardware = response
        .hardware
        .get(index)
        .cloned()
        .context("Phone did not expose the selected camera hardware")?;
    if hardware.device_id.is_empty() {
        hardware.device_id = response.own_id.clone();
    }
    if hardware.device_type == 0 {
        hardware.device_type = response.device_type;
    }
    Ok(hardware)
}
