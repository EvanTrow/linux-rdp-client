use anyhow::{bail, Context, Result};
use std::io::{Read, Write};

// MCS UserIDs/ChannelIDs allocated dynamically start at 1001 (T.125: StaticChannelID is
// 1..1000, DynamicChannelID/UserID is 1001..65535) — confirmed against FreeRDP's
// MCS_BASE_CHANNEL_ID usage in mcs.c/rdp.c.
const MCS_BASE_CHANNEL_ID: u16 = 1001;

// DomainMCSPDU choice values (T.125), confirmed against FreeRDP's mcs.h enum.
const PDU_ERECT_DOMAIN_REQUEST: u8 = 1;
const PDU_ATTACH_USER_REQUEST: u8 = 10;
const PDU_ATTACH_USER_CONFIRM: u8 = 11;
const PDU_CHANNEL_JOIN_REQUEST: u8 = 14;
const PDU_CHANNEL_JOIN_CONFIRM: u8 = 15;
const PDU_SEND_DATA_REQUEST: u8 = 25;
const PDU_SEND_DATA_INDICATION: u8 = 26;

// ---------------------------------------------------------------------------
// TPKT + X.224 Data framing, shared by every PDU from here on (MCS Connect
// Initial/Response through the rest of the connection). Identical to the
// framing used for the negotiation phase in x224.rs, just reusable for an
// arbitrary payload instead of the X.224-CR-specific structure.
// ---------------------------------------------------------------------------

pub fn wrap_tpkt_x224(payload: &[u8]) -> Vec<u8> {
    let total_len = 4 + 3 + payload.len();
    let mut out = Vec::with_capacity(total_len);
    out.push(3);
    out.push(0);
    out.extend_from_slice(&(total_len as u16).to_be_bytes());
    out.extend_from_slice(&[0x02, 0xF0, 0x80]); // X.224 Data TPDU: LI=2, code=0xF0, EOT=0x80
    out.extend_from_slice(payload);
    out
}

pub fn read_tpkt_x224<S: Read>(stream: &mut S) -> Result<Vec<u8>> {
    // The RDS AAD Auth PDUs that precede all MCS traffic on this same TLS stream are
    // NUL-terminated on the wire, and that trailing NUL is deliberately never consumed by
    // rds_aad.rs's reader (see its comment) — it's simplest to just skip leading NULs here
    // too, the same way rds_aad's JSON reader treats them as filler before its own `{`.
    let mut tpkt = [0u8; 4];
    loop {
        stream.read_exact(&mut tpkt[..1]).context("reading TPKT version byte")?;
        if tpkt[0] != 0 {
            break;
        }
    }
    stream.read_exact(&mut tpkt[1..]).context("reading TPKT header")?;
    if tpkt[0] != 3 {
        bail!("unexpected TPKT version {}", tpkt[0]);
    }
    let total_len = u16::from_be_bytes([tpkt[2], tpkt[3]]) as usize;
    if total_len < 7 {
        bail!("TPKT length {total_len} too small for X.224 Data TPDU");
    }
    let mut rest = vec![0u8; total_len - 4];
    stream.read_exact(&mut rest).context("reading X.224 Data TPDU body")?;
    if rest.len() < 3 || rest[0] != 0x02 || rest[1] != 0xF0 || rest[2] != 0x80 {
        bail!("expected X.224 Data TPDU (02 f0 80), got {:02x?}", &rest[..rest.len().min(3)]);
    }
    Ok(rest[3..].to_vec())
}

// ---------------------------------------------------------------------------
// BER encoding for the outer MCS Connect Initial / Connect Response (T.125
// §11.1). Verified against a worked byte-level example from the spec.
// ---------------------------------------------------------------------------

fn ber_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else if len <= 0xFF {
        vec![0x81, len as u8]
    } else {
        vec![0x82, (len >> 8) as u8, len as u8]
    }
}

fn ber_integer(buf: &mut Vec<u8>, value: u32) {
    let be = value.to_be_bytes();
    let val_bytes: &[u8] = match be.iter().position(|&b| b != 0) {
        Some(i) => &be[i..],
        None => &be[3..],
    };
    buf.push(0x02);
    buf.extend(ber_length(val_bytes.len()));
    buf.extend_from_slice(val_bytes);
}

/// Reads a BER definite-length field starting at `data[0]`, returning (length, bytes the
/// length encoding itself occupied).
fn ber_read_length(data: &[u8]) -> Result<(usize, usize)> {
    if data.is_empty() {
        bail!("BER length: empty");
    }
    let b0 = data[0];
    if b0 & 0x80 == 0 {
        Ok((b0 as usize, 1))
    } else {
        let n = (b0 & 0x7F) as usize;
        if n == 0 || n > 4 || data.len() < 1 + n {
            bail!("BER length: unsupported/truncated long form (n={n})");
        }
        let mut len = 0usize;
        for &b in &data[1..1 + n] {
            len = (len << 8) | b as usize;
        }
        Ok((len, 1 + n))
    }
}

struct DomainParameters {
    max_channel_ids: u32,
    max_user_ids: u32,
    max_token_ids: u32,
    num_priorities: u32,
    min_throughput: u32,
    max_height: u32,
    max_mcs_pdu_size: u32,
    protocol_version: u32,
}

// Exact values used by real clients (FreeRDP mcs_send_connect_initial and this session's
// worked spec example agree byte-for-byte).
const TARGET_PARAMS: DomainParameters = DomainParameters {
    max_channel_ids: 34,
    max_user_ids: 2,
    max_token_ids: 0,
    num_priorities: 1,
    min_throughput: 0,
    max_height: 1,
    max_mcs_pdu_size: 0xFFFF,
    protocol_version: 2,
};
const MINIMUM_PARAMS: DomainParameters = DomainParameters {
    max_channel_ids: 1,
    max_user_ids: 1,
    max_token_ids: 1,
    num_priorities: 1,
    min_throughput: 0,
    max_height: 1,
    max_mcs_pdu_size: 0x0420,
    protocol_version: 2,
};
const MAXIMUM_PARAMS: DomainParameters = DomainParameters {
    max_channel_ids: 0xFFFF,
    max_user_ids: 0xFC17,
    max_token_ids: 0xFFFF,
    num_priorities: 1,
    min_throughput: 0,
    max_height: 1,
    max_mcs_pdu_size: 0xFFFF,
    protocol_version: 2,
};

fn write_domain_parameters(buf: &mut Vec<u8>, p: &DomainParameters) {
    let mut body = Vec::new();
    ber_integer(&mut body, p.max_channel_ids);
    ber_integer(&mut body, p.max_user_ids);
    ber_integer(&mut body, p.max_token_ids);
    ber_integer(&mut body, p.num_priorities);
    ber_integer(&mut body, p.min_throughput);
    ber_integer(&mut body, p.max_height);
    ber_integer(&mut body, p.max_mcs_pdu_size);
    ber_integer(&mut body, p.protocol_version);
    buf.push(0x30); // SEQUENCE
    buf.extend(ber_length(body.len()));
    buf.extend(body);
}

/// Sends the Client MCS Connect Initial PDU (MS-RDPBCGR 2.2.1.3), wrapping the already
/// GCC/PER-encoded `gcc_user_data` (see `gcc_conference_create_request`).
pub fn send_connect_initial<S: Write>(stream: &mut S, gcc_user_data: &[u8]) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0x04, 0x01, 0x01]); // callingDomainSelector: OCTET STRING 0x01
    body.extend_from_slice(&[0x04, 0x01, 0x01]); // calledDomainSelector: OCTET STRING 0x01
    body.extend_from_slice(&[0x01, 0x01, 0xFF]); // upwardFlag: BOOLEAN TRUE
    write_domain_parameters(&mut body, &TARGET_PARAMS);
    write_domain_parameters(&mut body, &MINIMUM_PARAMS);
    write_domain_parameters(&mut body, &MAXIMUM_PARAMS);
    body.push(0x04); // userData: OCTET STRING
    body.extend(ber_length(gcc_user_data.len()));
    body.extend_from_slice(gcc_user_data);

    let mut pdu = Vec::with_capacity(4 + body.len());
    pdu.extend_from_slice(&[0x7F, 0x65]); // APPLICATION 101 = Connect-Initial
    pdu.extend(ber_length(body.len()));
    pdu.extend(body);

    stream
        .write_all(&wrap_tpkt_x224(&pdu))
        .context("writing MCS Connect Initial PDU")
}

/// Receives the Server MCS Connect Response PDU and returns the raw `userData` bytes
/// (the GCC ConferenceCreateResponse, still PER-wrapped — pass to
/// `gcc_conference_create_response_user_data` to extract the server data blocks).
pub fn recv_connect_response<S: Read>(stream: &mut S) -> Result<Vec<u8>> {
    let pdu = read_tpkt_x224(stream)?;
    if pdu.len() < 2 || pdu[0] != 0x7F || pdu[1] != 0x66 {
        bail!("expected BER APPLICATION 102 (Connect-Response), got {:02x?}", &pdu[..pdu.len().min(2)]);
    }
    let (total_len, len_bytes) = ber_read_length(&pdu[2..])?;
    let mut pos = 2 + len_bytes;
    if pos + total_len > pdu.len() {
        bail!("Connect Response BER length {total_len} exceeds PDU size");
    }

    // result: ENUMERATED (tag 0x0A)
    if pdu[pos] != 0x0A {
        bail!("expected ENUMERATED result tag, got {:#04x}", pdu[pos]);
    }
    let (result_len, n) = ber_read_length(&pdu[pos + 1..])?;
    pos += 1 + n;
    let result = pdu[pos];
    pos += result_len;
    if result != 0 {
        bail!("MCS Connect Response result={result} (not rt-successful)");
    }

    // calledConnectId: INTEGER (tag 0x02)
    if pdu[pos] != 0x02 {
        bail!("expected INTEGER calledConnectId tag, got {:#04x}", pdu[pos]);
    }
    let (id_len, n) = ber_read_length(&pdu[pos + 1..])?;
    pos += 1 + n + id_len;

    // domainParameters: SEQUENCE (tag 0x30) — skip over, we don't need the negotiated values.
    if pdu[pos] != 0x30 {
        bail!("expected SEQUENCE domainParameters tag, got {:#04x}", pdu[pos]);
    }
    let (dp_len, n) = ber_read_length(&pdu[pos + 1..])?;
    pos += 1 + n + dp_len;

    // userData: OCTET STRING (tag 0x04)
    if pdu[pos] != 0x04 {
        bail!("expected OCTET STRING userData tag, got {:#04x}", pdu[pos]);
    }
    let (ud_len, n) = ber_read_length(&pdu[pos + 1..])?;
    pos += 1 + n;
    if pos + ud_len > pdu.len() {
        bail!("Connect Response userData length {ud_len} exceeds PDU size");
    }
    Ok(pdu[pos..pos + ud_len].to_vec())
}

// ---------------------------------------------------------------------------
// GCC ConferenceCreateRequest/Response PER wrapper (T.124), verified byte-for-byte
// against FreeRDP's gcc_write_conference_create_request / per_write_* primitives.
// ---------------------------------------------------------------------------

const T124_OID: [u8; 6] = [0, 0, 20, 124, 0, 1];
const H221_CS_KEY: &[u8; 4] = b"Duca";
const H221_SC_KEY: &[u8; 4] = b"McDn";

fn per_length(len: u16) -> Vec<u8> {
    if len > 0x7F {
        let v = len | 0x8000;
        vec![(v >> 8) as u8, v as u8]
    } else {
        vec![len as u8]
    }
}

fn read_per_length(data: &[u8]) -> Result<(u16, usize)> {
    if data.is_empty() {
        bail!("PER length: empty");
    }
    let b0 = data[0];
    if b0 & 0x80 != 0 {
        if data.len() < 2 {
            bail!("PER length: truncated");
        }
        Ok(((((b0 & 0x7F) as u16) << 8) | data[1] as u16, 2))
    } else {
        Ok((b0 as u16, 1))
    }
}

/// Wraps concatenated `TS_UD_HEADER`-prefixed client data blocks (Client Core/Security/
/// Network Data) in the GCC ConnectData/ConferenceCreateRequest PER envelope, ready to be
/// used as `send_connect_initial`'s `gcc_user_data`.
pub fn gcc_conference_create_request(client_data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();

    // ConnectData: choice(object OID) + OID {0,0,20,124,0,1}
    out.push(0x00);
    out.push(0x05); // OID length
    out.push(T124_OID[0] * 40 + T124_OID[1]);
    out.extend_from_slice(&T124_OID[2..]);

    // ConnectData::connectPDU (OCTET STRING) length. FreeRDP hardcodes this as
    // client_data.len()+14, which assumes the 2-byte PER length form for the trailing
    // userData::value field below — true whenever client_data exceeds 127 bytes, which it
    // always does here (Client Core Data alone is >200 bytes).
    debug_assert!(client_data.len() > 0x7F, "connectPDU length formula assumes 2-byte PER length form");
    out.extend(per_length((client_data.len() + 14) as u16));

    out.push(0x00); // ConnectGCCPDU choice: conferenceCreateRequest
    out.push(0x08); // selection: optional userData field present

    // ConferenceCreateRequest::conferenceName (NumericString "1", 1 char, min 1)
    out.push(0x00); // per length: mlength = 1-1 = 0
    out.push(0x10); // packed digits: ('1'-'0')<<4 | ('0'-'0') = 0x10

    out.push(0x00); // padding(1)

    out.push(0x01); // number of UserData sets = 1
    out.push(0xC0); // UserData::value present + select h221NonStandard

    out.push(0x00); // h221NonStandard per length: mlength = 4-4 = 0
    out.extend_from_slice(H221_CS_KEY);

    // userData::value (OCTET STRING, min 0 => length field = client_data.len())
    out.extend(per_length(client_data.len() as u16));
    out.extend_from_slice(client_data);

    out
}

/// Parses the GCC ConnectData/ConferenceCreateResponse PER envelope, returning the raw
/// concatenated server data blocks (pass to `gcc::parse_server_gcc_data`).
pub fn gcc_conference_create_response_user_data(data: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0usize;

    // ConnectData: choice(1) + OID(1 len byte + 5 tuple bytes)
    if data.len() < 8 {
        bail!("GCC ConferenceCreateResponse too short");
    }
    pos += 1 + 6;

    // ConnectData::connectPDU length (ignored value per spec)
    let (_pdu_len, n) = read_per_length(&data[pos..])?;
    pos += n;

    // ConnectGCCPDU choice(1)
    pos += 1;

    // ConferenceCreateResponse::nodeID (UserID, per_integer16 — fixed 2 bytes, no length prefix)
    pos += 2;

    // ConferenceCreateResponse::tag (INTEGER, length-prefixed)
    let (tag_len, n) = read_per_length(&data[pos..])?;
    pos += n + tag_len as usize;

    // ConferenceCreateResponse::result (ENUMERATED, 1 raw byte)
    if pos >= data.len() {
        bail!("GCC ConferenceCreateResponse truncated before result");
    }
    let result = data[pos];
    pos += 1;
    if result != 0 {
        bail!("GCC ConferenceCreateResponse result={result} (not success)");
    }

    // number of UserData sets (1 raw byte)
    pos += 1;
    // choice: UserData::value present + h221NonStandard (1 raw byte, expect 0xC0)
    pos += 1;

    // h221NonStandard: length-prefixed octet string, min 4 -> length field then 4 raw bytes
    let (h221_mlen, n) = read_per_length(&data[pos..])?;
    pos += n;
    let h221_actual_len = h221_mlen as usize + 4;
    if pos + h221_actual_len > data.len() {
        bail!("GCC ConferenceCreateResponse h221NonStandard truncated");
    }
    let key = &data[pos..pos + h221_actual_len];
    if key != H221_SC_KEY {
        bail!("unexpected h221NonStandard key {:02x?} (expected McDn)", key);
    }
    pos += h221_actual_len;

    // userData (OCTET STRING, min 0)
    let (ud_len, n) = read_per_length(&data[pos..])?;
    pos += n;
    if pos + ud_len as usize > data.len() {
        bail!("GCC ConferenceCreateResponse userData truncated");
    }
    Ok(data[pos..pos + ud_len as usize].to_vec())
}

// ---------------------------------------------------------------------------
// MCS Domain PDUs: Erect Domain Request, Attach User Request/Confirm, Channel
// Join Request/Confirm. Verified against FreeRDP's mcs.c.
// ---------------------------------------------------------------------------

fn domain_pdu_header(pdu_type: u8, options: u8) -> u8 {
    (pdu_type << 2) | options
}

pub fn send_erect_domain_request<S: Write>(stream: &mut S) -> Result<()> {
    let mut pdu = vec![domain_pdu_header(PDU_ERECT_DOMAIN_REQUEST, 0)];
    // subHeight (INTEGER, PER length-prefixed) = 0, subInterval (INTEGER) = 0
    pdu.push(0x01);
    pdu.push(0x00);
    pdu.push(0x01);
    pdu.push(0x00);
    stream
        .write_all(&wrap_tpkt_x224(&pdu))
        .context("writing MCS Erect Domain Request")
}

pub fn send_attach_user_request<S: Write>(stream: &mut S) -> Result<()> {
    let pdu = vec![domain_pdu_header(PDU_ATTACH_USER_REQUEST, 0)];
    stream
        .write_all(&wrap_tpkt_x224(&pdu))
        .context("writing MCS Attach User Request")
}

/// Returns the assigned MCS User ID.
pub fn recv_attach_user_confirm<S: Read>(stream: &mut S) -> Result<u16> {
    let pdu = read_tpkt_x224(stream)?;
    if pdu.is_empty() {
        bail!("empty Attach User Confirm PDU");
    }
    let pdu_type = pdu[0] >> 2;
    if pdu_type != PDU_ATTACH_USER_CONFIRM {
        bail!("expected AttachUserConfirm (type {PDU_ATTACH_USER_CONFIRM}), got type {pdu_type}");
    }
    if pdu.len() < 4 {
        bail!("Attach User Confirm PDU too short ({} bytes)", pdu.len());
    }
    let result = pdu[1];
    if result != 0 {
        bail!("Attach User Confirm result={result} (not rt-successful)");
    }
    let initiator = u16::from_be_bytes([pdu[2], pdu[3]]);
    Ok(initiator + MCS_BASE_CHANNEL_ID)
}

pub fn send_channel_join_request<S: Write>(stream: &mut S, user_id: u16, channel_id: u16) -> Result<()> {
    let mut pdu = vec![domain_pdu_header(PDU_CHANNEL_JOIN_REQUEST, 0)];
    pdu.extend_from_slice(&(user_id - MCS_BASE_CHANNEL_ID).to_be_bytes());
    pdu.extend_from_slice(&channel_id.to_be_bytes());
    stream
        .write_all(&wrap_tpkt_x224(&pdu))
        .context("writing MCS Channel Join Request")
}

pub fn recv_channel_join_confirm<S: Read>(stream: &mut S) -> Result<()> {
    let pdu = read_tpkt_x224(stream)?;
    if pdu.is_empty() {
        bail!("empty Channel Join Confirm PDU");
    }
    let pdu_type = pdu[0] >> 2;
    if pdu_type != PDU_CHANNEL_JOIN_CONFIRM {
        bail!("expected ChannelJoinConfirm (type {PDU_CHANNEL_JOIN_CONFIRM}), got type {pdu_type}");
    }
    if pdu.len() < 2 {
        bail!("Channel Join Confirm PDU too short ({} bytes)", pdu.len());
    }
    let result = pdu[1];
    if result != 0 {
        bail!("Channel Join Confirm result={result} (not rt-successful)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MCS Send Data Request/Indication — wraps every PDU sent after channel join
// completes (Client Info, capability exchange, bitmap updates, input).
// Verified against FreeRDP's rdp_write_header/rdp_read_header.
// ---------------------------------------------------------------------------

pub fn send_data_request<S: Write>(stream: &mut S, user_id: u16, channel_id: u16, payload: &[u8]) -> Result<()> {
    let mut pdu = vec![domain_pdu_header(PDU_SEND_DATA_REQUEST, 0)];
    pdu.extend_from_slice(&(user_id - MCS_BASE_CHANNEL_ID).to_be_bytes());
    pdu.extend_from_slice(&channel_id.to_be_bytes());
    pdu.push(0x70); // dataPriority + segmentation (fixed, matches every real client)
    let len = payload.len() as u16 | 0x8000; // always 2-byte PER length form
    pdu.extend_from_slice(&len.to_be_bytes());
    pdu.extend_from_slice(payload);
    stream.write_all(&wrap_tpkt_x224(&pdu)).context("writing MCS Send Data Request")
}

/// Returns (channelId, payload).
pub fn recv_data_indication<S: Read>(stream: &mut S) -> Result<(u16, Vec<u8>)> {
    let pdu = read_tpkt_x224(stream)?;
    if pdu.is_empty() {
        bail!("empty Send Data Indication PDU");
    }
    let pdu_type = pdu[0] >> 2;
    if pdu_type != PDU_SEND_DATA_INDICATION {
        bail!("expected SendDataIndication (type {PDU_SEND_DATA_INDICATION}), got type {pdu_type}");
    }
    if pdu.len() < 6 {
        bail!("Send Data Indication PDU too short ({} bytes)", pdu.len());
    }
    let channel_id = u16::from_be_bytes([pdu[3], pdu[4]]);
    // pdu[5] = dataPriority + segmentation, ignored
    let (len, n) = read_per_length(&pdu[6..])?;
    let start = 6 + n;
    if start + len as usize > pdu.len() {
        bail!("Send Data Indication userData length {len} exceeds PDU size");
    }
    Ok((channel_id, pdu[start..start + len as usize].to_vec()))
}
