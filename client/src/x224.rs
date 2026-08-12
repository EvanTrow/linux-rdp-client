use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;

pub const PROTOCOL_RDSAAD: u32 = 0x0000_0010;

const TYPE_RDP_NEG_REQ: u8 = 0x01;
const TYPE_RDP_NEG_RSP: u8 = 0x02;
const TYPE_RDP_NEG_FAILURE: u8 = 0x03;

/// Sends the X.224 Connection Request PDU (TPKT + X.224 CR TPDU + cookie + RDP_NEG_REQ)
/// requesting RDS AAD Auth, per MS-RDPBCGR 2.2.1.1 / 2.2.1.1.1.
pub fn send_connection_request(stream: &mut TcpStream, username_hint: &str) -> Result<()> {
    let cookie = format!("Cookie: mstshash={username_hint}\r\n");
    let cookie_bytes = cookie.as_bytes();

    // rdpNegReq: type(1) + flags(1) + length(2 LE) + requestedProtocols(4 LE)
    let mut rdp_neg_req = Vec::with_capacity(8);
    rdp_neg_req.push(TYPE_RDP_NEG_REQ);
    rdp_neg_req.push(0x00); // flags
    rdp_neg_req.extend_from_slice(&8u16.to_le_bytes());
    rdp_neg_req.extend_from_slice(&PROTOCOL_RDSAAD.to_le_bytes());

    // x224Crq variable part: cookie + rdpNegReq
    let mut x224_variable = Vec::new();
    x224_variable.extend_from_slice(cookie_bytes);
    x224_variable.extend_from_slice(&rdp_neg_req);

    // x224Crq fixed part (7 bytes total including the LI byte itself):
    // LI(1) + CR-CDT(1)=0xE0 + dst-ref(2)=0 + src-ref(2)=0 + class/options(1)=0
    let li = (6 + x224_variable.len()) as u8; // length indicator excludes the LI byte itself
    let mut x224_crq = Vec::with_capacity(7 + x224_variable.len());
    x224_crq.push(li);
    x224_crq.push(0xE0); // CR TPDU code, credit=0
    x224_crq.extend_from_slice(&0u16.to_be_bytes()); // dst-ref
    x224_crq.extend_from_slice(&0u16.to_be_bytes()); // src-ref
    x224_crq.push(0x00); // class 0, no options
    x224_crq.extend_from_slice(&x224_variable);

    // tpktHeader: version(1)=3 + reserved(1)=0 + length(2 BE, whole packet incl. tpkt header)
    let total_len = 4 + x224_crq.len();
    let mut packet = Vec::with_capacity(total_len);
    packet.push(3);
    packet.push(0);
    packet.extend_from_slice(&(total_len as u16).to_be_bytes());
    packet.extend_from_slice(&x224_crq);

    stream
        .write_all(&packet)
        .context("writing X.224 Connection Request PDU")?;
    Ok(())
}

/// Reads and parses the X.224 Connection Confirm PDU, returning the selectedProtocol
/// from RDP_NEG_RSP. Errors (including RDP_NEG_FAILURE) are surfaced as Err.
pub fn recv_connection_confirm(stream: &mut TcpStream) -> Result<u32> {
    // tpktHeader
    let mut tpkt = [0u8; 4];
    stream
        .read_exact(&mut tpkt)
        .context("reading TPKT header of X.224 Connection Confirm")?;
    if tpkt[0] != 3 {
        bail!("unexpected TPKT version {}", tpkt[0]);
    }
    let total_len = u16::from_be_bytes([tpkt[2], tpkt[3]]) as usize;
    if total_len < 4 {
        bail!("TPKT length {} too small", total_len);
    }
    let mut rest = vec![0u8; total_len - 4];
    stream
        .read_exact(&mut rest)
        .context("reading X.224 Connection Confirm body")?;

    // x224Ccf: LI(1) + CC-CDT(1) + dst-ref(2) + src-ref(2) + class/options(1) = 7 bytes incl LI
    if rest.len() < 7 {
        bail!("X.224 Connection Confirm body too short ({} bytes)", rest.len());
    }
    let li = rest[0] as usize;
    if rest[1] != 0xD0 {
        bail!("expected X.224 CC TPDU code 0xD0, got {:#04x}", rest[1]);
    }

    // Anything after the fixed 6 bytes (post-LI) up to li total is rdpNegData, if present.
    let fixed_len = 6; // CC-CDT + dst-ref + src-ref + class/options
    if li < fixed_len {
        bail!("X.224 CC length indicator {} smaller than fixed part", li);
    }
    let neg_data_len = li - fixed_len;
    if neg_data_len == 0 {
        // No Enhanced RDP Security negotiated; implicitly PROTOCOL_RDP.
        return Ok(0);
    }
    let neg_start = 1 + fixed_len; // skip LI byte + fixed part
    if rest.len() < neg_start + neg_data_len {
        bail!("X.224 CC body shorter than declared length indicator");
    }
    let neg = &rest[neg_start..neg_start + neg_data_len];
    if neg.len() < 8 {
        bail!("rdpNegData too short ({} bytes)", neg.len());
    }

    match neg[0] {
        TYPE_RDP_NEG_RSP => {
            let selected = u32::from_le_bytes([neg[4], neg[5], neg[6], neg[7]]);
            Ok(selected)
        }
        TYPE_RDP_NEG_FAILURE => {
            let failure_code = u32::from_le_bytes([neg[4], neg[5], neg[6], neg[7]]);
            bail!("server sent RDP_NEG_FAILURE, failureCode={:#010x}", failure_code);
        }
        other => bail!("unexpected rdpNegData type {:#04x}", other),
    }
}
