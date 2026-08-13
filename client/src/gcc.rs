use anyhow::{bail, Result};

// GCC User Data Header types (MS-RDPBCGR 2.2.1.3.1).
const CS_CORE: u16 = 0xC001;
const CS_SECURITY: u16 = 0xC002;
const CS_NET: u16 = 0xC003;
const SC_CORE: u16 = 0x0C01;
const SC_SECURITY: u16 = 0x0C02;
const SC_NET: u16 = 0x0C03;

fn write_header(buf: &mut Vec<u8>, block_type: u16, length: u16) {
    buf.extend_from_slice(&block_type.to_le_bytes());
    buf.extend_from_slice(&length.to_le_bytes());
}

/// UTF-16LE-encodes `s`, truncated and null-padded to exactly `total_bytes`.
fn utf16_fixed(s: &str, total_bytes: usize) -> Vec<u8> {
    let mut units: Vec<u16> = s.encode_utf16().collect();
    let max_units = total_bytes / 2 - 1; // leave room for the null terminator
    units.truncate(max_units);
    let mut out = Vec::with_capacity(total_bytes);
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.resize(total_bytes, 0);
    out
}

/// Client Core Data (TS_UD_CS_CORE, MS-RDPBCGR 2.2.1.3.2). `selected_protocol` is the
/// selectedProtocol we received in RDP_NEG_RSP (PROTOCOL_RDSAAD for this client).
pub fn client_core_data(desktop_width: u16, desktop_height: u16, selected_protocol: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0008_0011u32.to_le_bytes()); // version: RDP 10.12
    body.extend_from_slice(&desktop_width.to_le_bytes());
    body.extend_from_slice(&desktop_height.to_le_bytes());
    body.extend_from_slice(&0xCA01u16.to_le_bytes()); // colorDepth (ignored; postBeta2ColorDepth present)
    body.extend_from_slice(&0xAA03u16.to_le_bytes()); // SASSequence: RNS_UD_SAS_DEL
    body.extend_from_slice(&0x0000_0409u32.to_le_bytes()); // keyboardLayout: US English
    body.extend_from_slice(&19041u32.to_le_bytes()); // clientBuild
    body.extend_from_slice(&utf16_fixed("linux-rdp-client", 32)); // clientName
    body.extend_from_slice(&4u32.to_le_bytes()); // keyboardType: IBM enhanced 101/102-key
    body.extend_from_slice(&0u32.to_le_bytes()); // keyboardSubType
    body.extend_from_slice(&12u32.to_le_bytes()); // keyboardFunctionKey
    body.resize(body.len() + 64, 0); // imeFileName
    body.extend_from_slice(&0xCA01u16.to_le_bytes()); // postBeta2ColorDepth: RNS_UD_COLOR_8BPP (ignored; highColorDepth present)
    body.extend_from_slice(&1u16.to_le_bytes()); // clientProductId
    body.extend_from_slice(&0u32.to_le_bytes()); // serialNumber
    body.extend_from_slice(&0x0018u16.to_le_bytes()); // highColorDepth: HIGH_COLOR_24BPP (fallback for 32bpp request)
    body.extend_from_slice(&0x000Fu16.to_le_bytes()); // supportedColorDepths: 24|16|15|32 bpp
    // earlyCapabilityFlags: SUPPORT_ERRINFO_PDU | WANT_32BPP_SESSION | VALID_CONNECTION_TYPE
    // | SUPPORT_NETCHAR_AUTODETECT | SUPPORT_DYNVC_GFX_PROTOCOL. The GFX pipeline flag is
    // the load-bearing one: without it, this host never offers the "rdpgfx" dynamic
    // virtual channel at all (confirmed — 8 other channels get offered, never rdpgfx,
    // until this flag was added). Spec requires NETCHAR_AUTODETECT be set alongside it;
    // we don't fully implement network characteristics auto-detection, but any Server
    // Auto-Detect Request PDU that results is just logged and skipped like any other
    // unrecognized Data PDU, which is harmless.
    body.extend_from_slice(&0x01A3u16.to_le_bytes());
    body.resize(body.len() + 64, 0); // clientDigProductId
    body.push(0x06); // connectionType: CONNECTION_TYPE_LAN
    body.push(0); // pad1octet
    body.extend_from_slice(&selected_protocol.to_le_bytes()); // serverSelectedProtocol

    let mut out = Vec::with_capacity(4 + body.len());
    write_header(&mut out, CS_CORE, (4 + body.len()) as u16);
    out.extend_from_slice(&body);
    out
}

/// Client Security Data (TS_UD_CS_SEC, MS-RDPBCGR 2.2.1.3.3). Both fields are zero: this
/// data block only matters for Standard RDP Security, which RDS AAD Auth doesn't use (TLS
/// + the RDS AAD Auth PDU exchange handle security instead).
pub fn client_security_data() -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    write_header(&mut out, CS_SECURITY, 12);
    out.extend_from_slice(&0u32.to_le_bytes()); // encryptionMethods
    out.extend_from_slice(&0u32.to_le_bytes()); // extEncryptionMethods
    out
}

/// Client Network Data (TS_UD_CS_NET, MS-RDPBCGR 2.2.1.3.4). No static virtual channels
/// requested: `"drdynvc"` (MS-RDPEDYC Dynamic Virtual Channels) — required to open the
/// `"rdpgfx"` dynamic channel, which this host uses exclusively for graphics (it never
/// sends legacy slow-path Bitmap Update PDUs, confirmed by testing).
pub fn client_network_data() -> Vec<u8> {
    const CHANNEL_OPTION_INITIALIZED: u32 = 0x8000_0000;
    const CHANNEL_OPTION_COMPRESS_RDP: u32 = 0x0080_0000;

    let mut channel_defs = Vec::new();
    let mut name = [0u8; 8];
    name[..7].copy_from_slice(b"drdynvc");
    channel_defs.extend_from_slice(&name);
    channel_defs.extend_from_slice(&(CHANNEL_OPTION_INITIALIZED | CHANNEL_OPTION_COMPRESS_RDP).to_le_bytes());

    let mut out = Vec::with_capacity(8 + channel_defs.len());
    write_header(&mut out, CS_NET, (8 + channel_defs.len()) as u16);
    out.extend_from_slice(&1u32.to_le_bytes()); // channelCount
    out.extend_from_slice(&channel_defs);
    out
}

pub struct ServerCoreData {
    pub version: u32,
}

pub struct ServerNetworkData {
    pub io_channel_id: u16,
    pub channel_ids: Vec<u16>,
}

pub struct ServerGccData {
    pub core: Option<ServerCoreData>,
    pub network: Option<ServerNetworkData>,
}

/// Parses the concatenated GCC server data blocks out of an MCS Connect Response's
/// userData (SC_CORE / SC_SECURITY / SC_NET, each prefixed by a TS_UD_HEADER).
pub fn parse_server_gcc_data(mut data: &[u8]) -> Result<ServerGccData> {
    let mut result = ServerGccData { core: None, network: None };

    while data.len() >= 4 {
        let block_type = u16::from_le_bytes([data[0], data[1]]);
        let length = u16::from_le_bytes([data[2], data[3]]) as usize;
        if length < 4 || length > data.len() {
            bail!("GCC server data block length {length} invalid (type {block_type:#06x}, {} bytes remain)", data.len());
        }
        let body = &data[4..length];

        match block_type {
            SC_CORE => {
                if body.len() < 4 {
                    bail!("SC_CORE block too short ({} bytes)", body.len());
                }
                let version = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                result.core = Some(ServerCoreData { version });
            }
            SC_NET => {
                if body.len() < 4 {
                    bail!("SC_NET block too short ({} bytes)", body.len());
                }
                let io_channel_id = u16::from_le_bytes([body[0], body[1]]);
                let channel_count = u16::from_le_bytes([body[2], body[3]]) as usize;
                let mut channel_ids = Vec::with_capacity(channel_count);
                let mut off = 4;
                for _ in 0..channel_count {
                    if off + 2 > body.len() {
                        bail!("SC_NET channelIdArray truncated");
                    }
                    channel_ids.push(u16::from_le_bytes([body[off], body[off + 1]]));
                    off += 2;
                }
                result.network = Some(ServerNetworkData { io_channel_id, channel_ids });
            }
            SC_SECURITY => {
                // Not needed: RDS AAD Auth doesn't use Standard RDP Security, so we don't
                // need the server's encryption method/certificate.
            }
            _ => {}
        }

        data = &data[length..];
    }

    Ok(result)
}
