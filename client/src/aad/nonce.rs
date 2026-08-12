use super::token::AUTHORITY;
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct NonceResponse {
    #[serde(rename = "Nonce")]
    nonce: String,
}

/// Acquires a fresh AAD Nonce, required on every RDS AAD Auth handshake per MS-RDPBCGR
/// "Acquiring an AAD Nonce".
pub fn acquire_aad_nonce(http: &reqwest::blocking::Client) -> Result<String> {
    let resp: NonceResponse = http
        .post(format!("{AUTHORITY}/oauth2/token"))
        .form(&[("grant_type", "srv_challenge")])
        .send()
        .context("requesting AAD nonce")?
        .error_for_status()
        .context("AAD nonce endpoint returned an error status")?
        .json()
        .context("parsing AAD nonce response")?;
    Ok(resp.nonce)
}
