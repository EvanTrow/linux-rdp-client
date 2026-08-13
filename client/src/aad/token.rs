use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::sha2::Sha256;
use rsa::signature::{RandomizedSigner, SignatureEncoding, Verifier};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::Deserialize;
use serde_json::json;
use std::io::{self, BufRead, Write as _};

/// Public client ID for the RDS AAD Auth flow. MS-RDPBCGR's own sample requests show a
/// different client id (81ec77fa-8ec7-4901-bf69-f1130545991d), but that one has no
/// service principal provisioned in this tenant and requires a tenant admin to visit an
/// /adminconsent URL before it's usable (AADSTS700016). This id is a Microsoft-owned
/// public client that's pre-consentable per-user (no tenant admin action needed) — same
/// OAuth mechanics, just a different (still Microsoft) app identifier. Not FreeRDP code
/// or library reuse: it's a UUID constant, exactly like the spec's own sample client_id.
const CLIENT_ID: &str = "a85cf173-4192-42f8-81fa-777a763e6e2c";
const REDIRECT_URI: &str = "https://login.microsoftonline.com/common/oauth2/nativeclient";
/// The tenant-agnostic multi-tenant authority. `/devicecode` 400s on this (AADSTS50059,
/// no tenant-identifying info) — but that's not used here. `/authorize` (interactive,
/// tenant resolved from whoever signs in) and `/token`/nonce (tenant is implied by the
/// authorization code / doesn't need one) all work fine against `/common`, so no tenant ID
/// needs to be configured anywhere.
pub const AUTHORITY: &str = "https://login.microsoftonline.com/common";

pub struct PopKey {
    pub private: RsaPrivateKey,
    pub public: RsaPublicKey,
    /// RFC 7638 JWK thumbprint of the public key, base64url-encoded.
    pub thumbprint: String,
}

impl PopKey {
    pub fn generate() -> Result<Self> {
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 2048).context("generating PoP RSA-2048 key")?;
        let public = RsaPublicKey::from(&private);
        let thumbprint = jwk_thumbprint(&public)?;
        Ok(Self {
            private,
            public,
            thumbprint,
        })
    }

    /// The JWK representation of the public key, for embedding in the RDP Assertion's `cnf.jwk`.
    pub fn public_jwk(&self) -> serde_json::Value {
        json!({
            "kty": "RSA",
            "n": b64url_biguint(&self.public.n().to_bytes_be()),
            "e": b64url_biguint(&self.public.e().to_bytes_be()),
        })
    }

    pub fn sign_rs256(&self, data: &[u8]) -> Result<Vec<u8>> {
        let signing_key = SigningKey::<Sha256>::new(self.private.clone());
        let mut rng = rand::thread_rng();
        let sig = signing_key.sign_with_rng(&mut rng, data);
        Ok(sig.to_bytes().to_vec())
    }

    pub fn verify_rs256(&self, data: &[u8], signature: &[u8]) -> bool {
        let verifying_key = VerifyingKey::<Sha256>::new(self.public.clone());
        let Ok(sig) = Signature::try_from(signature) else {
            return false;
        };
        verifying_key.verify(data, &sig).is_ok()
    }
}

fn b64url_biguint(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// RFC 7638 JWK thumbprint: SHA-256 over the minified canonical JSON containing only
/// the required members ("e", "kty", "n") in lexicographic order.
fn jwk_thumbprint(public: &RsaPublicKey) -> Result<String> {
    use sha2::Digest;
    let canonical = format!(
        r#"{{"e":"{}","kty":"RSA","n":"{}"}}"#,
        b64url_biguint(&public.e().to_bytes_be()),
        b64url_biguint(&public.n().to_bytes_be()),
    );
    let digest = sha2::Sha256::digest(canonical.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(digest))
}

pub struct RdpAccessToken {
    pub access_token: String,
}

/// How to obtain the OAuth authorization code from the AAD sign-in flow.
pub enum AuthCodeSource<'a> {
    /// Print the authorize URL and block on stdin for the user to paste back the
    /// redirect URL (or bare code) themselves.
    Manual,
    /// Drive the login automatically via [`crate::aad_auto`], sourcing credentials from
    /// the given 1Password item (cached locally after the first fetch).
    Auto { op_item: &'a str, headless: bool },
}

#[derive(Deserialize)]
struct TokenSuccessResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
    error_description: Option<String>,
}

/// Acquires an RDP Access Token for `scope` via the OAuth 2.0 Authorization Code Grant
/// (MS-RDPBCGR "Acquiring an RDP Access Token"), binding it to `pop_key` via `req_cnf`.
/// The `nativeclient` redirect URI is Microsoft's own mechanism for native clients that
/// have no local redirect listener — `code_source` determines how the code that lands
/// there gets back to us: pasted by hand, or captured by driving a browser ourselves.
pub fn acquire_rdp_access_token(
    http: &reqwest::blocking::Client,
    scope: &str,
    pop_key: &PopKey,
    code_source: &AuthCodeSource,
) -> Result<RdpAccessToken> {
    let req_cnf = URL_SAFE_NO_PAD.encode(format!(r#"{{"kid":"{}"}}"#, pop_key.thumbprint));

    let authorize_url = url::Url::parse_with_params(
        &format!("{AUTHORITY}/oauth2/v2.0/authorize"),
        &[
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("response_mode", "query"),
            ("scope", scope),
            ("redirect_uri", REDIRECT_URI),
        ],
    )
    .context("building authorize URL")?;

    let code = match code_source {
        AuthCodeSource::Manual => acquire_code_manually(&authorize_url)?,
        AuthCodeSource::Auto { op_item, headless } => {
            let redirect_url = crate::aad_auto::acquire_redirect_url(
                authorize_url.as_str(),
                REDIRECT_URI,
                op_item,
                *headless,
            )
            .context("automated AAD login")?;
            extract_code(&redirect_url).context("extracting `code` from captured redirect URL")?
        }
    };

    let resp = http
        .post(format!("{AUTHORITY}/oauth2/v2.0/token"))
        .form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("scope", scope),
            ("redirect_uri", REDIRECT_URI),
            ("req_cnf", &req_cnf),
        ])
        .send()
        .context("exchanging authorization code for access token")?;

    let status = resp.status();
    let body = resp.text().context("reading token response body")?;

    if !status.is_success() {
        let err: TokenErrorResponse = serde_json::from_str(&body).unwrap_or(TokenErrorResponse {
            error: format!("http_{status}"),
            error_description: Some(body.clone()),
        });
        bail!(
            "token endpoint error: {} ({})",
            err.error,
            err.error_description.unwrap_or_default()
        );
    }

    let parsed: TokenSuccessResponse = serde_json::from_str(&body).context("parsing token response")?;
    Ok(RdpAccessToken {
        access_token: parsed.access_token,
    })
}

/// Prints the authorize URL and blocks on stdin for the user to paste back the redirected
/// URL (or just its `code` parameter) after signing in themselves in any browser.
fn acquire_code_manually(authorize_url: &url::Url) -> Result<String> {
    println!("Open this URL in a browser and sign in with the account that should access this host:\n");
    println!("  {authorize_url}\n");
    println!("After sign-in you'll land on a login.microsoftonline.com/.../nativeclient page.");
    print!("Paste the resulting page URL (or just the `code` value) here: ");
    io::stdout().flush().ok();

    let mut pasted = String::new();
    io::stdin()
        .lock()
        .read_line(&mut pasted)
        .context("reading pasted redirect URL/code from stdin")?;

    extract_code(pasted.trim()).context("extracting `code` from pasted input")
}

/// Accepts either a bare authorization code or a full redirect URL containing `?code=...`.
fn extract_code(input: &str) -> Result<String> {
    if let Ok(parsed) = url::Url::parse(input) {
        if let Some((_, code)) = parsed.query_pairs().find(|(k, _)| k == "code") {
            return Ok(code.into_owned());
        }
        bail!("URL had no `code` query parameter");
    }
    if input.is_empty() {
        bail!("empty input");
    }
    Ok(input.to_string())
}
