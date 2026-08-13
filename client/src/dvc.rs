use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::Write;

const CMD_CREATE: u8 = 0x01;
const CMD_DATA_FIRST: u8 = 0x02;
const CMD_DATA: u8 = 0x03;
const CMD_CLOSE: u8 = 0x04;
const CMD_CAPABILITY: u8 = 0x05;

/// Width code (0/1/2 → 1/2/4 bytes) for a value, per MS-RDPEDYC's cbId/Sp/Len encoding.
fn width_code(n: u32) -> u8 {
    if n <= 0xFF {
        0
    } else if n <= 0xFFFF {
        1
    } else {
        2
    }
}

fn write_width_field(buf: &mut Vec<u8>, val: u32, code: u8) {
    match code {
        0 => buf.push(val as u8),
        1 => buf.extend_from_slice(&(val as u16).to_le_bytes()),
        _ => buf.extend_from_slice(&val.to_le_bytes()),
    }
}

fn read_width_field(data: &[u8], code: u8) -> Result<(u32, usize)> {
    match code {
        0 => Ok((*data.first().context("DVC field truncated")? as u32, 1)),
        1 => {
            if data.len() < 2 {
                bail!("DVC field truncated");
            }
            Ok((u16::from_le_bytes([data[0], data[1]]) as u32, 2))
        }
        _ => {
            if data.len() < 4 {
                bail!("DVC field truncated");
            }
            Ok((u32::from_le_bytes([data[0], data[1], data[2], data[3]]), 4))
        }
    }
}

pub enum DvcEvent {
    ChannelOpened { name: String, channel_id: u32 },
    /// A fully-reassembled DVC-layer message (both the static-channel CHANNEL_PDU_HEADER
    /// layer and the DVC Data/Data-First fragmentation layer have been stripped).
    Data { channel_id: u32, data: Vec<u8> },
}

/// Manages the `"drdynvc"` static channel: capability negotiation, opening named dynamic
/// channels, and fragmenting/reassembling DVC-layer Data PDUs. Message-driven: the caller
/// is responsible for reading raw MCS Send Data Indications, demultiplexing the static
/// channel's CHANNEL_PDU_HEADER framing (see `vchannel::ChannelDemux`), and feeding the
/// resulting complete static-channel messages into `handle_message` — this lets the same
/// receive loop also service the base I/O channel without one starving the other.
pub struct DvcManager {
    static_channel_id: u16,
    version: u16,
    caps_negotiated: bool,
    wanted: HashSet<String>,
    open_channels: HashMap<String, u32>,
    /// dynamic ChannelId -> (total_len, accumulated) for in-progress Data First reassembly
    partial: HashMap<u32, (usize, Vec<u8>)>,
}

impl DvcManager {
    pub fn new(static_channel_id: u16) -> Self {
        Self {
            static_channel_id,
            version: 3, // defensive default if the server skips capability negotiation
            caps_negotiated: false,
            wanted: HashSet::new(),
            open_channels: HashMap::new(),
            partial: HashMap::new(),
        }
    }

    pub fn static_channel_id(&self) -> u16 {
        self.static_channel_id
    }

    /// Registers a channel name we're willing to accept when the server offers to create
    /// it. Anything not registered gets rejected (a real Windows host offering `"disp"`/
    /// `"rdpsnd"`/etc. alongside `"rdpgfx"` is expected — we simply decline the rest).
    pub fn want_channel(&mut self, name: &str) {
        self.wanted.insert(name.to_string());
    }

    pub fn channel_id_for(&self, name: &str) -> Option<u32> {
        self.open_channels.get(name).copied()
    }

    fn send_raw<S: Write>(&self, stream: &mut S, user_id: u16, bytes: &[u8]) -> Result<()> {
        crate::vchannel::send(stream, user_id, self.static_channel_id, bytes)
    }

    fn send_caps_response<S: Write>(&self, stream: &mut S, user_id: u16) -> Result<()> {
        let mut pdu = Vec::with_capacity(4);
        pdu.push(CMD_CAPABILITY << 4); // Sp=0, cbId=0 (unused for this PDU)
        pdu.push(0); // Pad
        pdu.extend_from_slice(&self.version.to_le_bytes());
        self.send_raw(stream, user_id, &pdu)
    }

    fn parse_create_request(&self, data: &[u8], cb_id: u8) -> Result<(u32, String)> {
        let (channel_id, consumed) = read_width_field(data, cb_id)?;
        let name_bytes = &data[consumed..];
        let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len());
        let name = String::from_utf8_lossy(&name_bytes[..end]).into_owned();
        Ok((channel_id, name))
    }

    fn send_create_response<S: Write>(
        &self,
        stream: &mut S,
        user_id: u16,
        channel_id: u32,
        cb_id: u8,
        accept: bool,
    ) -> Result<()> {
        let mut pdu = Vec::new();
        pdu.push((CMD_CREATE << 4) | cb_id);
        write_width_field(&mut pdu, channel_id, cb_id);
        let status: i32 = if accept { 0 } else { -2147024896 }; // 0 or E_FAIL (0x80004005)
        pdu.extend_from_slice(&status.to_le_bytes());
        self.send_raw(stream, user_id, &pdu)
    }

    /// Processes one fully-reassembled static-channel message for `"drdynvc"`, returning
    /// any resulting higher-level events (a channel we wanted just opened, or a complete
    /// DVC-layer data message arrived on some already-open channel).
    pub fn handle_message<S: Write>(&mut self, stream: &mut S, user_id: u16, msg: &[u8]) -> Result<Vec<DvcEvent>> {
        if msg.is_empty() {
            return Ok(Vec::new());
        }
        let header = msg[0];
        let cmd = header >> 4;
        let field2 = (header >> 2) & 0x3; // Sp (Data) or Len (DataFirst)
        let cb_id = header & 0x3;
        let mut events = Vec::new();
        eprintln!("[debug] DVC msg: header={header:#04x} cmd={cmd} field2={field2} cb_id={cb_id} len={}", msg.len());

        match cmd {
            CMD_CAPABILITY => {
                if msg.len() < 4 {
                    bail!("DVC capability PDU too short");
                }
                self.version = u16::from_le_bytes([msg[2], msg[3]]);
                eprintln!("[debug] DVC capability request: version={:#06x}", self.version);
                self.send_caps_response(stream, user_id)?;
                self.caps_negotiated = true;
            }
            CMD_CREATE => {
                if !self.caps_negotiated {
                    // Server skipped capability negotiation — assume v3 and respond anyway
                    // (MS-RDPEDYC resilience note, matches FreeRDP's defensive behavior).
                    self.version = 3;
                    self.send_caps_response(stream, user_id)?;
                    self.caps_negotiated = true;
                }
                let (channel_id, name) = self.parse_create_request(&msg[1..], cb_id)?;
                let accept = self.wanted.contains(&name);
                eprintln!("[debug] DVC create request: channel_id={channel_id} name={name:?} wanted={:?} accept={accept}", self.wanted);
                self.send_create_response(stream, user_id, channel_id, cb_id, accept)?;
                if accept {
                    self.open_channels.insert(name.clone(), channel_id);
                    events.push(DvcEvent::ChannelOpened { name, channel_id });
                }
            }
            CMD_DATA => {
                let (channel_id, consumed) = read_width_field(&msg[1..], cb_id)?;
                let chunk = &msg[1 + consumed..];
                eprintln!("[debug] DVC data: channel_id={channel_id} chunk_len={} chunk={:02x?}", chunk.len(), chunk);
                if let Some((total, buf)) = self.partial.get_mut(&channel_id) {
                    buf.extend_from_slice(chunk);
                    if buf.len() >= *total {
                        let (_, complete) = self.partial.remove(&channel_id).unwrap();
                        events.push(DvcEvent::Data { channel_id, data: complete });
                    }
                } else {
                    // Not preceded by a Data First — this chunk IS the complete message.
                    events.push(DvcEvent::Data {
                        channel_id,
                        data: chunk.to_vec(),
                    });
                }
            }
            CMD_DATA_FIRST => {
                let (channel_id, consumed) = read_width_field(&msg[1..], cb_id)?;
                let (total_len, consumed2) = read_width_field(&msg[1 + consumed..], field2)?;
                let chunk = &msg[1 + consumed + consumed2..];
                if chunk.len() >= total_len as usize {
                    events.push(DvcEvent::Data {
                        channel_id,
                        data: chunk[..total_len as usize].to_vec(),
                    });
                } else {
                    self.partial.insert(channel_id, (total_len as usize, chunk.to_vec()));
                }
            }
            CMD_CLOSE => {
                let (channel_id, _) = read_width_field(&msg[1..], cb_id)?;
                self.open_channels.retain(|_, id| *id != channel_id);
                self.partial.remove(&channel_id);
            }
            _ => {}
        }

        Ok(events)
    }

    /// Sends `data` as one or more DVC Data/Data First PDUs on `channel_id` (an already-open
    /// dynamic channel), which are themselves sent over the static channel (possibly
    /// further chunked by `vchannel::send`).
    pub fn send_data<S: Write>(&self, stream: &mut S, user_id: u16, channel_id: u32, data: &[u8]) -> Result<()> {
        const DVC_DATA_CHUNK: usize = 1590;
        let cb_id = width_code(channel_id);

        if data.len() <= DVC_DATA_CHUNK {
            let mut pdu = Vec::with_capacity(1 + 4 + data.len());
            pdu.push((CMD_DATA << 4) | cb_id);
            write_width_field(&mut pdu, channel_id, cb_id);
            pdu.extend_from_slice(data);
            return self.send_raw(stream, user_id, &pdu);
        }

        let len_code = width_code(data.len() as u32);
        let mut first = Vec::with_capacity(1 + 4 + 4 + DVC_DATA_CHUNK);
        first.push((CMD_DATA_FIRST << 4) | (len_code << 2) | cb_id);
        write_width_field(&mut first, channel_id, cb_id);
        write_width_field(&mut first, data.len() as u32, len_code);
        first.extend_from_slice(&data[..DVC_DATA_CHUNK]);
        self.send_raw(stream, user_id, &first)?;

        let mut offset = DVC_DATA_CHUNK;
        while offset < data.len() {
            let end = (offset + DVC_DATA_CHUNK).min(data.len());
            let mut pdu = Vec::with_capacity(1 + 4 + (end - offset));
            pdu.push((CMD_DATA << 4) | cb_id);
            write_width_field(&mut pdu, channel_id, cb_id);
            pdu.extend_from_slice(&data[offset..end]);
            self.send_raw(stream, user_id, &pdu)?;
            offset = end;
        }
        Ok(())
    }
}
