use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::process::Command;

/// AAD sign-in credentials pulled from a 1Password item: username, password, and the raw
/// TOTP seed as an `otpauth://totp/...?secret=...` URI (not a live 6-digit code — that URI
/// is what lets [`super::totp`] regenerate codes locally without calling `op` again).
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    pub totp_url: String,
}

#[derive(Deserialize)]
struct OpField {
    #[serde(default)]
    purpose: String,
    #[serde(default)]
    label: String,
    #[serde(rename = "type", default)]
    field_type: String,
    #[serde(default)]
    value: String,
}

#[derive(Deserialize)]
struct OpItem {
    fields: Vec<OpField>,
}

/// Fetches username, password, and the raw TOTP seed from a 1Password item via the `op`
/// CLI. Only called when there's no local cache yet (or the caller explicitly wants to
/// refresh it) — every other run uses the cached values instead, per [`super::cache`].
///
/// Requires the 1Password desktop app's CLI integration to be enabled (Settings >
/// Developer); the first `op` call in a while pops an "Allow" prompt there.
pub fn fetch_credentials(item: &str) -> Result<Credentials> {
    let output = Command::new("op")
        .args(["item", "get", item, "--format", "json", "--reveal"])
        .output()
        .context("running `op item get` — is the 1Password CLI (`op`) installed and on PATH?")?;

    if !output.status.success() {
        bail!(
            "1Password CLI failed for item {item:?}: {}\n\
             Is the 1Password CLI integration enabled in the desktop app (Settings > \
             Developer), or do you need `eval $(op signin)`?",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let parsed: OpItem =
        serde_json::from_slice(&output.stdout).context("parsing `op item get` JSON output")?;

    let username = field_by_purpose(&parsed, "USERNAME")
        .with_context(|| format!("1Password item {item:?} has no field with purpose USERNAME"))?;
    let password = field_by_purpose(&parsed, "PASSWORD")
        .with_context(|| format!("1Password item {item:?} has no field with purpose PASSWORD"))?;

    // Deliberately read the field's raw `value`, not `op read ...?attribute=otp` (which
    // returns a live-computed code, not the seed). Confirmed against a real OTP field:
    // `op item get --format json --reveal` reports the field's `value` as the full
    // `otpauth://totp/...?secret=...` URI, with `totp` as a separate key holding the
    // live code — `value` is what we want to cache so codes can be generated offline.
    let totp_url = parsed
        .fields
        .iter()
        .find(|f| f.field_type == "OTP" && f.label == "one-time password")
        .map(|f| f.value.clone())
        .with_context(|| {
            format!("1Password item {item:?} has no OTP field labeled exactly \"one-time password\"")
        })?;

    if !totp_url.starts_with("otpauth://") {
        bail!(
            "1Password item {item:?}'s \"one-time password\" field didn't return a raw \
             otpauth:// seed (got {totp_url:?}) — unexpected `op` CLI output format"
        );
    }

    Ok(Credentials {
        username,
        password,
        totp_url,
    })
}

fn field_by_purpose(item: &OpItem, purpose: &str) -> Option<String> {
    item.fields
        .iter()
        .find(|f| f.purpose == purpose)
        .map(|f| f.value.clone())
}
