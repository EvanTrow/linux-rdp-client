mod aad;
mod rds_aad;
mod tls;
mod x224;

use anyhow::{bail, Context, Result};
use std::net::TcpStream;

/// The mstshash cookie in the X.224 Connection Request just needs to be *a* string
/// (traditionally a username hint) — sign-in happens interactively in the browser, so this
/// isn't tied to any real account.
const MSTSHASH_COOKIE: &str = "rdp-client";
const DEFAULT_PORT: u16 = 3389;

/// Splits a `<host>` or `<host>:<port>` CLI argument into (host:port for TCP, bare hostname
/// for the RDS AAD Auth resource URI — domain suffix stripped, matching how the RDP host
/// itself reports its short hostname via `dsregcmd /status`).
fn parse_target(arg: &str) -> (String, String) {
    let (host_part, port) = match arg.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, p.to_string()),
        _ => (arg, DEFAULT_PORT.to_string()),
    };
    let host_name = host_part.split('.').next().unwrap_or(host_part).to_string();
    (format!("{host_part}:{port}"), host_name)
}

fn main() -> Result<()> {
    let target = std::env::args()
        .nth(1)
        .context("usage: rdp-client <host[:port]>")?;
    let (host_addr, host_name) = parse_target(&target);

    let scope = format!("ms-device-service://termsrv.wvd.microsoft.com/name/{host_name}/user_impersonation");
    // Per FreeRDP's aad.c (libfreerdp/core/aad.c, aad_create_jws_payload): the assertion's
    // `u` claim is the resource + /name/<hostname> WITHOUT the /user_impersonation suffix —
    // different from the scope string used to acquire the token.
    let resource_uri = format!("ms-device-service://termsrv.wvd.microsoft.com/name/{host_name}");

    println!("== Phase 0: RDS AAD Auth handshake against {host_addr} ==");

    println!("[1/6] generating PoP key + acquiring RDP access token...");
    let http = reqwest::blocking::Client::new();
    let pop_key = aad::PopKey::generate().context("generating PoP key")?;
    let access_token =
        aad::acquire_rdp_access_token(&http, &scope, &pop_key).context("acquiring RDP access token")?;
    println!("      access token acquired ({} bytes)", access_token.access_token.len());

    println!("[2/6] acquiring AAD nonce...");
    let aad_nonce = aad::acquire_aad_nonce(&http).context("acquiring AAD nonce")?;

    println!("[3/6] connecting to {host_addr}...");
    let mut stream = TcpStream::connect(&host_addr).context("connecting to RDP host")?;

    println!("[4/6] X.224 negotiation (requesting PROTOCOL_RDSAAD)...");
    x224::send_connection_request(&mut stream, MSTSHASH_COOKIE)?;
    let selected = x224::recv_connection_confirm(&mut stream)?;
    if selected != x224::PROTOCOL_RDSAAD {
        bail!("server did not select PROTOCOL_RDSAAD (selectedProtocol={selected:#010x}) — is enablerdsaadauth actually negotiable here?");
    }
    println!("      server selected PROTOCOL_RDSAAD");

    println!("[5/6] TLS handshake...");
    let sni_name = host_addr.split(':').next().unwrap_or(&host_addr);
    let mut tls_stream = tls::upgrade(stream, sni_name).context("TLS handshake")?;

    println!("[6/6] RDS AAD Auth PDU exchange...");
    let server_nonce = rds_aad::recv_server_nonce(&mut tls_stream).context("receiving Server Nonce PDU")?;
    let assertion = aad::build_rdp_assertion(&access_token, &resource_uri, &pop_key, &server_nonce, &aad_nonce)
        .context("building RDP Assertion")?;
    rds_aad::send_authentication_request(&mut tls_stream, &assertion)
        .context("sending Authentication Request PDU")?;
    rds_aad::recv_authentication_result(&mut tls_stream).context("receiving Authentication Result PDU")?;

    println!("\n✅ RDS AAD Auth succeeded — authentication proven end-to-end.");
    Ok(())
}
