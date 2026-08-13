use crate::mcs;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::io::Write;

const CHANNEL_FLAG_FIRST: u32 = 0x0000_0001;
const CHANNEL_FLAG_LAST: u32 = 0x0000_0002;
/// Default VCChunkSize (MS-RDPBCGR 2.2.7.1.10) when not otherwise negotiated.
const CHUNK_SIZE: usize = 1600;

/// Sends `data` over a static virtual channel (e.g. `"drdynvc"`), chunked per
/// CHANNEL_PDU_HEADER framing (MS-RDPBCGR 2.2.6.1.1) if it exceeds one chunk.
pub fn send<S: Write>(stream: &mut S, user_id: u16, channel_id: u16, data: &[u8]) -> Result<()> {
    let mut offset = 0;
    loop {
        let end = (offset + CHUNK_SIZE).min(data.len());
        let mut flags = 0u32;
        if offset == 0 {
            flags |= CHANNEL_FLAG_FIRST;
        }
        if end == data.len() {
            flags |= CHANNEL_FLAG_LAST;
        }
        let mut pdu = Vec::with_capacity(8 + (end - offset));
        pdu.extend_from_slice(&(data.len() as u32).to_le_bytes());
        pdu.extend_from_slice(&flags.to_le_bytes());
        pdu.extend_from_slice(&data[offset..end]);
        mcs::send_data_request(stream, user_id, channel_id, &pdu)?;
        offset = end;
        if offset >= data.len() {
            break;
        }
    }
    Ok(())
}

/// Reassembles CHANNEL_PDU_HEADER-chunked static virtual channel messages, one buffer per
/// MCS channel — needed because traffic for different channels (e.g. the base I/O channel
/// and `"drdynvc"`) can arrive interleaved on the wire.
#[derive(Default)]
pub struct ChannelDemux {
    partial: HashMap<u16, (usize, Vec<u8>)>,
}

impl ChannelDemux {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one raw MCS Send Data Indication payload for `channel_id`. Returns the fully
    /// reassembled message once complete, or `None` if more chunks are still expected.
    pub fn feed(&mut self, channel_id: u16, payload: &[u8]) -> Result<Option<Vec<u8>>> {
        if payload.len() < 8 {
            bail!("Channel PDU Header truncated ({} bytes)", payload.len());
        }
        let flags = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
        let chunk = &payload[8..];

        let entry = self.partial.entry(channel_id).or_insert((0, Vec::new()));
        if flags & CHANNEL_FLAG_FIRST != 0 {
            let length = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
            entry.0 = length;
            entry.1.clear();
        }
        entry.1.extend_from_slice(chunk);

        if flags & CHANNEL_FLAG_LAST != 0 {
            let (_, msg) = self.partial.remove(&channel_id).unwrap();
            return Ok(Some(msg));
        }
        Ok(None)
    }
}
