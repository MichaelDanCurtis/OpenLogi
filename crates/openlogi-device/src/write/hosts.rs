//! Easy-Switch host slots: HostsInfo (`0x1815`) slot status and friendly
//! names, read on an open channel.

use std::sync::Arc;

use hidpp::{
    channel::HidppChannel,
    device::Device,
    feature::hosts_info::{HostIndex, HostSlotStatus, HostsInfoFeature},
};
use openlogi_core::hid::{HostSlotInfo, HostSlots};
use tracing::debug;

use crate::SharedChannel;

use super::{HidppOperation, WriteError, classify_hidpp_error, open_feature};

const FEATURE: u16 = 0x1815;

/// Read host slots on an already-open [`SharedChannel`].
pub async fn read_host_slots_on(shared: &SharedChannel) -> Result<HostSlots, WriteError> {
    read_host_slots_on_channel(shared.channel(), shared.device_index()).await
}

async fn read_host_slots_on_channel(
    channel: &Arc<HidppChannel>,
    index: u8,
) -> Result<HostSlots, WriteError> {
    let mut device = Device::new(Arc::clone(channel), index)
        .await
        .map_err(|_| WriteError::DeviceUnreachable { index })?;
    let hosts = open_feature::<HostsInfoFeature>(&mut device).await?;
    let info = hosts
        .get_feature_info()
        .await
        .map_err(|e| classify_hidpp_error(e, HidppOperation::ReadHostSlots, FEATURE))?;
    let current = match info.current_host {
        HostIndex::Slot(index) => Some(index.saturating_add(1)),
        // `HostIndex::Current` should not appear here (the device reports a
        // concrete slot for `current_host`), and `HostIndex` is
        // `#[non_exhaustive]`, so every other case — including `Current` —
        // falls back to "not reported".
        _ => None,
    };
    let mut slots = Vec::with_capacity(usize::from(info.host_count));
    for host in 0..info.host_count {
        // A slot whose status is unreadable is reported, not fatal: the
        // switch path treats an unknown status as "allowed", and so does this.
        let (paired, name) = match hosts.get_host_info(HostIndex::Slot(host)).await {
            Ok(slot) => {
                let paired = Some(slot.status == HostSlotStatus::Paired);
                let name = read_name(&hosts, host, slot.name_len).await;
                (paired, name)
            }
            Err(error) => {
                debug!(host, %error, "host slot status unreadable");
                (None, None)
            }
        };
        slots.push(HostSlotInfo {
            slot: host.saturating_add(1),
            paired,
            name,
        });
    }
    Ok(HostSlots { current, slots })
}

async fn read_name(hosts: &HostsInfoFeature, host: u8, name_len: u8) -> Option<String> {
    let mut chunks = Vec::new();
    let mut offset: u8 = 0;
    while offset < name_len {
        match hosts
            .get_host_friendly_name(HostIndex::Slot(host), offset)
            .await
        {
            Ok(chunk) => chunks.push(chunk.bytes),
            Err(error) => {
                debug!(host, offset, %error, "host name chunk unreadable");
                return None;
            }
        }
        offset = offset.saturating_add(14);
    }
    assemble_name(name_len, chunks.into_iter())
}

/// Join 14-byte name chunks, keep exactly `name_len` bytes, and require valid
/// UTF-8. `None` for an empty or undecodable name.
fn assemble_name(name_len: u8, chunks: impl Iterator<Item = [u8; 14]>) -> Option<String> {
    if name_len == 0 {
        return None;
    }
    let bytes: Vec<u8> = chunks.flatten().take(usize::from(name_len)).collect();
    String::from_utf8(bytes)
        .ok()
        // A device may pad with NULs inside `name_len` rather than shrinking
        // it, so trim them before deciding whether anything real was read.
        .map(|name| name.trim_end_matches('\0').to_owned())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::assemble_name;

    fn chunk(text: &str) -> [u8; 14] {
        let mut bytes = [0; 14];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        bytes
    }

    #[test]
    fn names_span_chunks_and_stop_at_name_len() {
        // U+2019 is 3 bytes, so the chunk boundary lands mid-name.
        let name = assemble_name(
            20,
            [chunk("Michael’s Ma"), chunk("cBook Pro-junk")].into_iter(),
        );
        assert_eq!(name.as_deref(), Some("Michael’s MacBook "));
    }

    #[test]
    fn padded_names_lose_trailing_nuls() {
        // `name_len` covers the whole chunk, but the device only wrote "Mac"
        // into it and left the rest zeroed.
        let name = assemble_name(14, [chunk("Mac")].into_iter());
        assert_eq!(name.as_deref(), Some("Mac"));
    }

    #[test]
    fn empty_and_invalid_names_are_none() {
        assert_eq!(assemble_name(0, std::iter::empty()), None);
        assert_eq!(
            assemble_name(
                3,
                [[0xff, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]].into_iter()
            ),
            None
        );
    }
}
