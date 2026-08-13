use anyhow::{bail, Result};

const ERROR_ALERT: u8 = 0xFF;
const ST_NO_TRANSITION: u32 = 0x0000_0002;

/// Checks a Server License PDU (received via MCS Send Data Indication on the I/O channel,
/// security-header-wrapped). We only support the common "License Error PDU - Valid Client"
/// path (MS-RDPBCGR 2.2.1.12) where the server immediately skips full licensing — no
/// licensing PDU needs to be sent back by the client in this path.
///
/// `payload` starts at the security header (Basic Security Header, since encryption is
/// forced off under RDS AAD Auth): flags(2) + flagsHi(2), then the licensing preamble.
pub fn check_valid_client(payload: &[u8]) -> Result<()> {
    if payload.len() < 4 {
        bail!("License PDU too short for security header ({} bytes)", payload.len());
    }
    let flags = u16::from_le_bytes([payload[0], payload[1]]);
    const SEC_LICENSE_PKT: u16 = 0x0080;
    if flags & SEC_LICENSE_PKT == 0 {
        bail!("expected SEC_LICENSE_PKT flag in security header, got {flags:#06x}");
    }

    let body = &payload[4..];
    if body.len() < 4 {
        bail!("licensing preamble too short ({} bytes)", body.len());
    }
    let msg_type = body[0];
    if msg_type != ERROR_ALERT {
        bail!("unsupported licensing message type {msg_type:#04x} (only ERROR_ALERT / Valid Client is implemented)");
    }

    // LICENSE_VALID_CLIENT_DATA: preamble(4) + dwErrorCode(4) + dwStateTransition(4) + bbErrorInfo(...)
    if body.len() < 12 {
        bail!("License Error PDU too short ({} bytes)", body.len());
    }
    let state_transition = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
    if state_transition != ST_NO_TRANSITION {
        bail!("unexpected dwStateTransition {state_transition:#010x} (expected ST_NO_TRANSITION)");
    }
    Ok(())
}
