//! Inbound Physical, Logical and data dispatch.
mod logical;
mod physical;

use super::PhysicalActor;
use crate::Result;

impl PhysicalActor {
    pub(super) async fn receive(&mut self, message: &[u8], physical_id: u8) -> Result<()> {
        let (mesh_kind, _data) = crate::protocol::pb::unmesh(message)?;
        match mesh_kind {
            1 => {
                self.receive_physical(
                    crate::protocol::messages::lyra::PhysicalFrame::decode(message)?,
                    physical_id,
                )
                .await
            }
            2 => {
                self.receive_logical(
                    crate::protocol::messages::lyra::LogicalEnvelope::decode(message)?,
                    physical_id,
                )
                .await
            }
            4 | 5 => {
                self.receive_data(
                    crate::protocol::messages::data::DataFrame::decode(message)?,
                    physical_id,
                )
                .await
            }
            _ => Ok(()),
        }
    }
}
