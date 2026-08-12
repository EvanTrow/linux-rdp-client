use super::token::{PopKey, RdpAccessToken};
use anyhow::{Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

/// Builds the RDP Assertion (MS-RDPBCGR 2.2.18.2.1): a client-self-signed JWS containing
/// the RDP access token, PoP public key, and both nonces, proving possession of the PoP
/// private key that was bound to the access token at acquisition time.
pub fn build_rdp_assertion(
    access_token: &RdpAccessToken,
    resource_uri: &str,
    pop_key: &PopKey,
    server_nonce: &str,
    aad_nonce: &str,
) -> Result<String> {
    let header = json!({"alg": "RS256", "kid": pop_key.thumbprint});
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before epoch")?
        .as_secs();

    let client_claims = json!({"aad_nonce": aad_nonce}).to_string();
    let payload = json!({
        "ts": ts.to_string(),
        "at": access_token.access_token,
        "u": resource_uri,
        "nonce": server_nonce,
        "cnf": {"jwk": pop_key.public_jwk()},
        "client_claims": client_claims,
    });

    let header_b64 = URL_SAFE_NO_PAD.encode(header.to_string());
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string());
    let signing_input = format!("{header_b64}.{payload_b64}");

    let signature = pop_key.sign_rs256(signing_input.as_bytes())?;
    debug_assert!(pop_key.verify_rs256(signing_input.as_bytes(), &signature));
    let signature_b64 = URL_SAFE_NO_PAD.encode(signature);

    Ok(format!("{signing_input}.{signature_b64}"))
}
