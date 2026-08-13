use super::onepassword::Credentials;
use aes_gcm::aead::{Aead, Generate, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Cached credentials are encrypted at rest with AES-256-GCM, keyed by a random key
/// generated on first use and stored alongside the cache in its own 0600 file. This
/// protects against casual disclosure (backups, screen shares, other unprivileged users)
/// but not against an attacker who already has your user account — there's no passphrase
/// prompt, since the whole point is zero extra interaction after the first run.
fn cache_dir() -> Result<PathBuf> {
    let base = dirs::config_dir()
        .context("could not determine a config directory ($XDG_CONFIG_HOME or $HOME)")?;
    let dir = base.join("rdp-client").join("aad-cache");
    fs::create_dir_all(&dir).context("creating AAD credential cache directory")?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).ok();
    Ok(dir)
}

fn key_path(dir: &Path) -> PathBuf {
    dir.join("cache.key")
}

fn blob_path(dir: &Path, item: &str) -> PathBuf {
    dir.join(format!("{}.enc", sanitize(item)))
}

fn sanitize(item: &str) -> String {
    item.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    f.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))
}

fn load_or_create_key_bytes(dir: &Path) -> Result<Vec<u8>> {
    let path = key_path(dir);
    if let Ok(bytes) = fs::read(&path) {
        if bytes.len() == 32 {
            return Ok(bytes);
        }
    }
    let key = Key::<Aes256Gcm>::generate();
    write_private(&path, key.as_slice())?;
    Ok(key.as_slice().to_vec())
}

#[derive(Serialize, Deserialize)]
struct EncryptedBlob {
    nonce: String,
    ciphertext: String,
}

#[derive(Serialize, Deserialize)]
struct CachedCredentials {
    username: String,
    password: String,
    totp_url: String,
}

impl From<Credentials> for CachedCredentials {
    fn from(c: Credentials) -> Self {
        Self {
            username: c.username,
            password: c.password,
            totp_url: c.totp_url,
        }
    }
}

/// Loads cached credentials for a 1Password item, if any. Returns `None` (not an error)
/// when nothing is cached yet — that's the expected first-run state, not a failure.
// `Array::from_slice` is deprecated in favor of `TryFrom`, but the `TryFrom` impl returns
// an owned `Array<u8, NonceSize>` whose size parameter isn't nameable here without pulling
// in `aead::AeadCore` associated-type spelling — not worth it for a well-understood,
// still-functional call.
#[allow(deprecated)]
pub fn load(item: &str) -> Result<Option<Credentials>> {
    let dir = cache_dir()?;
    let path = blob_path(&dir, item);
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(_) => return Ok(None),
    };

    let key_bytes = load_or_create_key_bytes(&dir)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));

    let blob: EncryptedBlob =
        serde_json::from_slice(&raw).context("parsing AAD credential cache file")?;
    let nonce_bytes = BASE64.decode(&blob.nonce).context("decoding cache nonce")?;
    let ciphertext = BASE64
        .decode(&blob.ciphertext)
        .context("decoding cache ciphertext")?;
    if nonce_bytes.len() != 12 {
        bail!("AAD credential cache at {} has a malformed nonce", path.display());
    }
    let nonce = Nonce::from_slice(&nonce_bytes);

    let plaintext = cipher.decrypt(nonce, ciphertext.as_ref()).map_err(|_| {
        anyhow!(
            "failed to decrypt AAD credential cache at {} (corrupt file or key mismatch) — \
             delete it and re-run to re-fetch from 1Password",
            path.display()
        )
    })?;
    let creds: CachedCredentials =
        serde_json::from_slice(&plaintext).context("parsing decrypted cache contents")?;
    Ok(Some(Credentials {
        username: creds.username,
        password: creds.password,
        totp_url: creds.totp_url,
    }))
}

/// Encrypts and saves credentials for a 1Password item, overwriting any existing cache.
#[allow(deprecated)]
pub fn save(item: &str, creds: &Credentials) -> Result<()> {
    let dir = cache_dir()?;
    let key_bytes = load_or_create_key_bytes(&dir)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));

    let nonce = Nonce::generate();
    let plaintext = serde_json::to_vec(&CachedCredentials::from(creds.clone()))
        .context("serializing credentials for caching")?;
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_ref())
        .map_err(|_| anyhow!("encrypting AAD credential cache"))?;

    let blob = EncryptedBlob {
        nonce: BASE64.encode(nonce.as_slice()),
        ciphertext: BASE64.encode(&ciphertext),
    };
    let path = blob_path(&dir, item);
    write_private(
        &path,
        serde_json::to_string(&blob)
            .context("serializing cache blob")?
            .as_bytes(),
    )
}
