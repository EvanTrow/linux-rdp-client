pub mod browser;
pub mod cache;
pub(crate) mod log;
pub mod onepassword;
pub mod totp;

use anyhow::{Context, Result};
use log::aad_log;

/// Obtains the OAuth redirect URL for an AAD authorize request, automating as much of the
/// login as possible:
///
/// 1. Use cached credentials if present; otherwise fetch (and cache) them from the given
///    1Password item — the only step that ever talks to 1Password. First run only: this
///    triggers the "Allow" prompt in the 1Password desktop app.
/// 2. Attempt an automated browser login with those credentials.
/// 3. If that fails for any reason (stale password, unexpected prompt, timeout...), fall
///    back to a visible browser the user completes sign-in in by hand — the resulting
///    redirect URL is still captured automatically, no manual paste needed.
pub fn acquire_redirect_url(
    authorize_url: &str,
    redirect_prefix: &str,
    op_item: &str,
    headless: bool,
) -> Result<String> {
    log::init();

    let creds = match cache::load(op_item).context("loading cached AAD credentials")? {
        Some(creds) => creds,
        None => {
            aad_log!(
                "[aad] no cached credentials for 1Password item {op_item:?} — fetching \
                 (approve the prompt in the 1Password app if asked)..."
            );
            let creds = onepassword::fetch_credentials(op_item)?;
            cache::save(op_item, &creds).context("caching AAD credentials")?;
            creds
        }
    };

    let rt = tokio::runtime::Runtime::new().context("starting browser automation runtime")?;
    match rt.block_on(browser::automated_login(authorize_url, redirect_prefix, &creds, headless)) {
        Ok(redirect_url) => Ok(redirect_url),
        Err(e) => {
            aad_log!(
                "[aad] automated login failed ({e:#}) — falling back to manual login; \
                 complete sign-in yourself, the redirect will still be captured automatically"
            );
            rt.block_on(browser::manual_login_capture(authorize_url, redirect_prefix))
        }
    }
}
