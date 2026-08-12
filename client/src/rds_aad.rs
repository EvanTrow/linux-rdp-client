use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
use std::io::{Read, Write};

/// The RDS AAD Auth PDUs (Server Nonce / Authentication Request / Authentication Result,
/// MS-RDPBCGR 2.2.18.x) are documented as bare UTF-8 JSON with no length or type header —
/// framing is JSON's own brace balance, not a length prefix. We read byte-by-byte, tracking
/// string/escape state, until the top-level JSON object closes.
///
/// In practice each PDU is also NUL-terminated on the wire (confirmed against a real host:
/// the byte right after a PDU's closing `}` is `0x00`). We never explicitly consume that
/// trailing NUL after finishing a read (doing so would block forever on the final PDU of a
/// sequence, since nothing else follows it) — instead we treat a leading NUL as filler and
/// skip over it here, same as whitespace, so it's absorbed at the start of the *next* read.
fn read_json_pdu<T: DeserializeOwned, S: Read>(stream: &mut S) -> Result<T> {
    let mut buf = Vec::new();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut started = false;
    let mut byte = [0u8; 1];

    loop {
        stream
            .read_exact(&mut byte)
            .context("reading RDS AAD Auth PDU from TLS stream")?;
        let b = byte[0];

        if !started {
            if b.is_ascii_whitespace() || b == 0 {
                continue;
            }
            if b != b'{' {
                let mut trailing = vec![b];
                let mut extra = [0u8; 63];
                if let Ok(n) = stream.read(&mut extra) {
                    trailing.extend_from_slice(&extra[..n]);
                }
                bail!(
                    "expected JSON object start, got byte {:#04x}; next bytes (hex): {} (ascii: {:?})",
                    b,
                    trailing.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" "),
                    String::from_utf8_lossy(&trailing)
                );
            }
            started = true;
        }

        buf.push(b);

        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }

        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }

    serde_json::from_slice(&buf).context("parsing RDS AAD Auth PDU JSON")
}

fn write_json_pdu<T: Serialize, S: Write>(stream: &mut S, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).context("serializing RDS AAD Auth PDU")?;
    // Match the wire format observed on reads: PDUs are NUL-terminated (confirmed both by
    // capture against a real host and by FreeRDP's aad_send_auth_request, which appends a
    // single 0x00 byte after the JSON on the outgoing PDU too).
    bytes.push(0);
    stream
        .write_all(&bytes)
        .context("writing RDS AAD Auth PDU to TLS stream")?;
    stream.flush().context("flushing RDS AAD Auth PDU")
}

#[derive(serde::Deserialize)]
struct ServerNoncePdu {
    ts_nonce: String,
}

pub fn recv_server_nonce<S: Read>(stream: &mut S) -> Result<String> {
    let pdu: ServerNoncePdu = read_json_pdu(stream)?;
    Ok(pdu.ts_nonce)
}

pub fn send_authentication_request<S: Write>(stream: &mut S, rdp_assertion: &str) -> Result<()> {
    write_json_pdu(stream, &json!({ "rdp_assertion": rdp_assertion }))
}

#[derive(serde::Deserialize)]
struct AuthResultPdu {
    authentication_result: i64,
}

/// Returns Ok(()) on S_OK (0x00000000), Err with the HRESULT otherwise.
pub fn recv_authentication_result<S: Read>(stream: &mut S) -> Result<()> {
    let pdu: AuthResultPdu = read_json_pdu(stream)?;
    if pdu.authentication_result == 0 {
        Ok(())
    } else {
        bail!(
            "authentication failed, HRESULT {:#010x}",
            pdu.authentication_result as u32
        )
    }
}
