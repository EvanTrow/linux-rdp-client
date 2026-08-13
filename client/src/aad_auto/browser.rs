use super::log::aad_log;
use super::onepassword::Credentials;
use super::totp;
use anyhow::{anyhow, bail, Context, Result};
use chromiumoxide::fetcher::{BrowserFetcher, BrowserFetcherOptions};
use chromiumoxide::{Browser, BrowserConfig, Element, Page};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;
use tokio::time::sleep;

const AUTOMATED_LOGIN_TIMEOUT: Duration = Duration::from_secs(60);
const MFA_FALLBACK_TIMEOUT: Duration = Duration::from_secs(15);
const MANUAL_LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_millis(400);
// Slack on top of the timeouts already enforced inside the login flows (element waits,
// redirect polling): those only check their deadline *between* individual CDP calls, so a
// single call that itself never returns (e.g. the handler task died) would hang forever
// without this outer, unconditional backstop.
const OUTER_TIMEOUT_SLACK: Duration = Duration::from_secs(30);
const AAD_WINDOW_WIDTH: u32 = 1280;
const AAD_WINDOW_HEIGHT: u32 = 720;

/// Best-effort primary-monitor geometry via `xrandr`, as `(width, height, x_offset,
/// y_offset)` in the X server's virtual-screen coordinate space, so the AAD login window
/// can be explicitly centered instead of relying on the window manager's default placement
/// — on at least one multi-monitor setup that default placement put new Chromium windows
/// (including their tab strip) off-screen entirely.
///
/// The offset matters: on a multi-monitor virtual desktop the primary output is very often
/// *not* at (0,0) (e.g. `5120x1440+1080+303` — starts 1080px right of the origin), and
/// `--window-position` is in absolute virtual-screen coordinates. Ignoring the offset would
/// center relative to the whole virtual desktop's corner rather than the primary monitor.
fn primary_monitor_geometry() -> Option<(u32, u32, i32, i32)> {
    let output = std::process::Command::new("xrandr").arg("--query").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);

    let mut first_connected = None;
    for line in text.lines() {
        // "disconnected" also contains "connected" as a substring, but never preceded by
        // a space the way " connected" (the actual status word) always is.
        if !line.contains(" connected") {
            continue;
        }
        if let Some(geom) = parse_xrandr_geometry(line) {
            if line.contains(" primary ") {
                return Some(geom);
            }
            first_connected.get_or_insert(geom);
        }
    }
    // No output was explicitly marked primary — fall back to whichever connected output
    // was listed first rather than giving up on positioning entirely.
    first_connected
}

/// Parses the `WxH+X+Y` geometry token out of an `xrandr --query` "connected" summary line,
/// e.g. `DP-3 connected primary 5120x1440+1080+303 (normal ...) 1190mm x 340mm`.
fn parse_xrandr_geometry(line: &str) -> Option<(u32, u32, i32, i32)> {
    for token in line.split_whitespace() {
        // Every token before the geometry one (interface name, "connected", "primary")
        // simply won't contain 'x' — skip to the next token rather than giving up on the
        // whole line the way `?` would.
        let Some((w, rest)) = token.split_once('x') else {
            continue;
        };
        if w.is_empty() || !w.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let mut rest_parts = rest.splitn(3, '+');
        let Some(h) = rest_parts.next() else {
            continue;
        };
        if h.is_empty() || !h.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let (Ok(width), Ok(height)) = (w.parse::<u32>(), h.parse::<u32>()) else {
            continue;
        };
        let x_off: i32 = rest_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let y_off: i32 = rest_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        return Some((width, height, x_off, y_off));
    }
    None
}

/// Downloads (once, cached under `~/.cache/chromiumoxide`) and locates a Chromium build to
/// drive — independent of whatever browser, if any, is installed system-wide.
async fn ensure_chrome() -> Result<PathBuf> {
    // The fetcher's default path is exactly this directory, but it doesn't create it —
    // on a genuinely first-ever run (no prior `~/.cache/chromiumoxide`) its download
    // step fails outright trying to write into a nonexistent directory.
    let cache_dir = dirs::cache_dir()
        .context("could not determine a cache directory ($XDG_CACHE_HOME or $HOME)")?
        .join("chromiumoxide");
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .with_context(|| format!("creating {}", cache_dir.display()))?;

    let options = BrowserFetcherOptions::builder()
        .with_path(&cache_dir)
        .build()
        .context("configuring Chromium fetcher")?;
    let fetcher = BrowserFetcher::new(options);
    let installation = fetcher
        .fetch()
        .await
        .context("downloading/locating a Chromium build for AAD login automation")?;
    Ok(installation.executable_path)
}

/// A fresh, unique profile directory per launch. Without this, chromiumoxide falls back to
/// a *fixed* shared path (`$TMPDIR/chromiumoxide-runner`) for every launch — Chrome detects
/// that an existing process already has that profile open and just forwards the request to
/// it as a new tab in the existing window instead of starting an independent instance, so
/// every run would pile another tab onto whatever's still running from a prior one.
fn unique_profile_dir() -> PathBuf {
    let unique = format!(
        "rdp-client-aad-chrome-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    std::env::temp_dir().join(unique)
}

async fn launch(headless: bool) -> Result<(Browser, JoinHandle<()>, PathBuf)> {
    let exe = ensure_chrome().await?;
    let profile_dir = unique_profile_dir();
    let mut builder = BrowserConfig::builder()
        .chrome_executable(exe)
        .user_data_dir(&profile_dir)
        .window_size(AAD_WINDOW_WIDTH, AAD_WINDOW_HEIGHT)
        .no_sandbox();
    // Under a native Wayland session, Chromium by default runs as a native Wayland client
    // — and Wayland deliberately gives clients no way to position their own window; only
    // the compositor decides. `--window-position` is silently ignored there, which is
    // almost certainly why the window landed off-screen in the first place. Forcing the
    // X11/XWayland backend makes positioning actually take effect (XWayland windows are
    // placed the normal X11 way even under a Wayland compositor); it's a no-op on a
    // genuinely X11 session.
    builder = builder.arg("ozone-platform=x11");
    // Chromium scales `--window-position`/`--window-size` by whatever device scale factor
    // it auto-detects (2x was observed here even though every monitor in GNOME's own
    // config is set to 1x scale) — pin it to 1 so the pixel math below, computed straight
    // from `xrandr`'s physical geometry, lands exactly where intended instead of at 2x (or
    // whatever) the coordinates, off in some other monitor entirely.
    builder = builder.arg("force-device-scale-factor=1");
    let target_position = primary_monitor_geometry().map(|(screen_w, screen_h, x_off, y_off)| {
        let x = x_off + (screen_w.saturating_sub(AAD_WINDOW_WIDTH) / 2) as i32;
        let y = y_off + (screen_h.saturating_sub(AAD_WINDOW_HEIGHT) / 2) as i32;
        (x, y)
    });
    if let Some((x, y)) = target_position {
        builder = builder.arg(format!("window-position={x},{y}"));
    }
    if !headless {
        builder = builder.with_head();
    }
    let config = builder
        .build()
        .map_err(|e| anyhow!("building Chromium launch config: {e}"))?;
    let (mut browser, mut handler) = Browser::launch(config).await.context("launching Chromium")?;
    // The handler drives the underlying CDP connection; nothing else progresses (page
    // navigation, element queries...) unless this stream is polled continuously. Real login
    // pages generate plenty of transient/non-fatal event errors (redirects, aborted
    // sub-resource loads, etc.) — bailing out on the first one (as chromiumoxide's own
    // example does) leaves every subsequent command awaiting a response that will never
    // arrive, hanging the whole login attempt. Keep polling until the stream itself ends
    // (i.e. the connection actually closes).
    let handle = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(e) = event {
                aad_log!("[aad] chromiumoxide handler event error (continuing): {e}");
            }
        }
    });

    // `--window-position` above is a best-effort first try, but was observed landing at
    // 2x the requested coordinates on at least one real desktop (GNOME/XWayland) despite
    // every monitor being configured at 1x scale in GNOME's own settings — some scaling
    // step specific to Chromium-under-XWayland, unrelated to `--force-device-scale-factor`.
    // Correct for it authoritatively via the window manager directly: `wmctrl` reports (and
    // accepts) geometry in the same raw pixel space as `xrandr`, verified against this
    // session, so moving the window there after the fact sidesteps whatever is doubling
    // Chromium's own interpretation of the launch flag.
    if !headless {
        if let Some((x, y)) = target_position {
            reposition_window(&mut browser, x, y, AAD_WINDOW_WIDTH, AAD_WINDOW_HEIGHT).await;
        }
    }

    Ok((browser, handle, profile_dir))
}

/// Waits (briefly) for the newly-launched Chromium window to register with the window
/// manager, then moves it to `(x, y)` at size `(w, h)` via `wmctrl`. Matches by the child
/// process's PID rather than window title, since the title changes with every page (and on
/// the real login flow, starts as the authorize URL's page before Microsoft's own title
/// loads). Best-effort: silently does nothing if `wmctrl` or PID lookup isn't available
/// (e.g. non-X11, no PID support in this window manager) — the launch-flag attempt above is
/// still in place as a fallback in that case.
///
/// Compensates for a real, reproducible quirk observed on GNOME/mutter over XWayland: a
/// requested move lands at exactly 2x the requested (x, y) — position only, size is
/// unaffected — even with every monitor explicitly at 1x scale in GNOME's own settings, and
/// even issuing the identical move directly via `wmctrl` with no Chromium/CDP involved at
/// all. Rather than hardcode "divide by 2" (which could easily be wrong on a setup that
/// doesn't have this quirk), measure where the window actually landed after the first
/// request and issue one corrected request compensating for whatever ratio was observed.
async fn reposition_window(browser: &mut Browser, x: i32, y: i32, w: u32, h: u32) {
    let Some(child) = browser.get_mut_child() else {
        return;
    };
    let Some(pid) = child.as_mut_inner().id() else {
        return;
    };

    let mut window_id = None;
    for _ in 0..25 {
        if let Some(id) = find_window_id_for_pid(pid) {
            window_id = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let Some(window_id) = window_id else {
        return;
    };
    // Give the window a moment to finish whatever placement/animation the compositor does
    // right after creation before touching it — moving it too early seemed to make
    // corrections land inconsistently (observed on GNOME/mutter: sometimes fully ignored,
    // sometimes applied to position only and not the immediately-following size).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Up to two attempts: measure where a move actually landed, derive the ratio between
    // requested and actual, and try again compensating for it. A correct first attempt (no
    // quirk present) converges immediately since `off_by_enough` is false right away.
    let mut want = (x, y);
    for _ in 0..2 {
        move_window(&window_id, want.0, want.1, w, h);
        tokio::time::sleep(Duration::from_millis(500)).await;

        let Some((actual_x, actual_y, _, _)) = window_geometry(&window_id) else {
            return;
        };
        let off_by_enough = (actual_x - x).abs() > 5 || (actual_y - y).abs() > 5;
        if !off_by_enough {
            return;
        }
        // A ratio only makes sense to derive from a nonzero target; a 0 target combined
        // with a nonzero actual position isn't a multiplicative discrepancy this can fix.
        if want.0 == 0 || want.1 == 0 {
            return;
        }
        let ratio_x = actual_x as f64 / want.0 as f64;
        let ratio_y = actual_y as f64 / want.1 as f64;
        if ratio_x <= 0.0 || ratio_y <= 0.0 {
            return;
        }
        want = (
            (x as f64 / ratio_x).round() as i32,
            (y as f64 / ratio_y).round() as i32,
        );
    }
}

fn move_window(window_id: &str, x: i32, y: i32, w: u32, h: u32) {
    let geometry = format!("0,{x},{y},{w},{h}");
    let _ = std::process::Command::new("wmctrl")
        .args(["-i", "-r", window_id, "-e", &geometry])
        .output();
}

fn window_geometry(window_id: &str) -> Option<(i32, i32, u32, u32)> {
    let out = std::process::Command::new("wmctrl").args(["-l", "-G"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let id = parts.next()?;
        if id != window_id {
            continue;
        }
        let _desktop = parts.next();
        let x: i32 = parts.next()?.parse().ok()?;
        let y: i32 = parts.next()?.parse().ok()?;
        let w: u32 = parts.next()?.parse().ok()?;
        let h: u32 = parts.next()?.parse().ok()?;
        return Some((x, y, w, h));
    }
    None
}

fn find_window_id_for_pid(pid: u32) -> Option<String> {
    let out = std::process::Command::new("wmctrl").args(["-l", "-p"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let win_id = parts.next()?;
        let _desktop = parts.next();
        let line_pid: u32 = parts.next()?.parse().ok()?;
        if line_pid == pid {
            return Some(win_id.to_string());
        }
    }
    None
}

async fn close_browser(mut browser: Browser, handle: JoinHandle<()>, profile_dir: &Path) {
    // Graceful close depends on the CDP connection still being responsive; if it isn't,
    // don't hang here indefinitely — kill the child process directly instead.
    if tokio::time::timeout(Duration::from_secs(5), browser.close())
        .await
        .is_err()
    {
        let _ = browser.kill().await;
    } else {
        let _ = tokio::time::timeout(Duration::from_secs(5), browser.wait()).await;
    }
    handle.abort();
    let _ = tokio::fs::remove_dir_all(profile_dir).await;
}

/// chromiumoxide's `find_element` returns immediately (error if absent) rather than
/// polling like Playwright's locators — this restores that "wait for it to show up"
/// behavior with a plain retry loop.
///
/// Also requires the match to actually be visible, checking every element that matches
/// `selector` (not just the first) and skipping any with no rendered box. Microsoft's login
/// pages have shown up with more than one `input[name='passwd']` etc. in the DOM at once —
/// a hidden template for a screen you haven't navigated to yet, alongside the live one —
/// and `find_element`/`querySelector` has no way to know which is which; typing into
/// whichever it happens to return first can silently land in the hidden one.
async fn wait_for_element(page: &Page, selector: &str, timeout: Duration) -> Result<Element> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(elements) = page.find_elements(selector).await {
            for el in elements {
                if is_visible(&el).await {
                    return Ok(el);
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for a visible {selector:?} to appear");
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// Checks visibility via JS (`offsetParent`/computed `display`/`visibility`) rather than
/// just a nonzero CDP bounding box. Microsoft's login page reuses the same id
/// (`#idSIButton9`) for the "Next"/"Sign in" button on every step; a bounding-box-only check
/// can't tell a stale, no-longer-active step's button (hidden via `visibility:hidden`, which
/// still occupies layout space — a nonzero box — unlike `display:none`) from the one that's
/// actually on screen right now. `offsetParent === null` alone doesn't catch that case
/// either (it's specific to `display:none`), hence the explicit `visibility` check too.
async fn is_visible(el: &Element) -> bool {
    let script = "function() { \
        if (this.offsetParent === null) return false; \
        const style = window.getComputedStyle(this); \
        return style.display !== 'none' && style.visibility !== 'hidden'; \
    }";
    match el.call_js_fn(script, false).await {
        Ok(ret) => ret.result.value.and_then(|v| v.as_bool()).unwrap_or(false),
        Err(_) => false,
    }
}

/// Focuses `el` directly via JS instead of `Element::click()`'s simulated mouse click at a
/// geometrically-computed point. A screenshot taken right where automated login was stuck
/// showed a completely empty password field with a fully clickable "Sign in" button next to
/// it — i.e. typing silently went nowhere, not a disabled-element problem. Microsoft's
/// floating-label inputs render a label/underline overlapping the actual `<input>`; a
/// geometric click can land on that decoration instead of the input itself, leaving the
/// real field unfocused while `type_str` (which types into whatever the *page* currently
/// has focused, not `el` specifically) sends keystrokes nowhere. JS `.focus()` targets the
/// exact DOM node unambiguously, no hit-testing involved.
async fn focus_via_js(el: &Element) -> Result<()> {
    el.call_js_fn("function() { this.focus(); }", false)
        .await
        .context("focusing element via JS")?;
    Ok(())
}

/// Clicks `el` via JS's native `.click()` instead of `Element::click()`'s simulated mouse
/// click at a geometrically-computed point — same rationale as [`focus_via_js`], and same
/// symptom: `click_when_ready` reported success (found a visible, enabled "Sign in" button
/// and dispatched a click with no error) but the page never advanced, meaning the simulated
/// click's computed point landed on something other than the real button.
async fn click_via_js(el: &Element) -> Result<()> {
    el.call_js_fn("function() { this.click(); }", false)
        .await
        .context("clicking element via JS")?;
    Ok(())
}

async fn eval_bool(page: &Page, script: String) -> Result<bool> {
    page.evaluate_expression(script)
        .await
        .context("evaluating page script")?
        .into_value()
        .context("reading script result")
}

async fn page_contains_text(page: &Page, needle: &str) -> Result<bool> {
    let script = format!(
        "!!(document.body && document.body.innerText.toLowerCase().includes({}))",
        serde_json::to_string(&needle.to_lowercase()).unwrap()
    );
    eval_bool(page, script).await
}

/// Finds the first visible leaf element whose text contains `needle` and clicks it via JS
/// (used for links/buttons whose id/class isn't stable, e.g. "use a verification code
/// instead"). Returns whether a match was found and clicked.
async fn click_text_if_present(page: &Page, needle: &str) -> Result<bool> {
    let script = format!(
        r#"(() => {{
            const needle = {needle};
            const els = Array.from(document.querySelectorAll('a, div, span, button'));
            const match = els.find(el =>
                el.children.length === 0 &&
                el.innerText &&
                el.innerText.toLowerCase().includes(needle) &&
                el.offsetParent !== null
            );
            if (match) {{ match.click(); return true; }}
            return false;
        }})()"#,
        needle = serde_json::to_string(&needle.to_lowercase()).unwrap()
    );
    eval_bool(page, script).await
}

/// Same visibility concern as [`wait_for_element`]: Microsoft's login pages have shown up
/// with more than one element sharing an id like `#idSIButton9` (a hidden template for a
/// screen not yet reached, alongside the live one). Clicking whichever `find_element`
/// happens to return first can silently click a hidden, inert button — the page just
/// doesn't advance, with no error to show for it. Check every match, click the first
/// visible one.
async fn click_if_present(page: &Page, selector: &str) -> bool {
    let Ok(elements) = page.find_elements(selector).await else {
        return false;
    };
    for el in elements {
        if is_visible(&el).await {
            return click_via_js(&el).await.is_ok();
        }
    }
    false
}

/// Like [`click_if_present`], but polls briefly for a visible match that also isn't
/// disabled before clicking, instead of trying exactly once. Microsoft's "Next"/"Sign in"
/// button is visible immediately but can stay disabled for a moment after typing while
/// client-side validation runs; a single immediate click can land on it while it's still
/// inert and silently do nothing — which looks identical to a successful click from the
/// caller's side (`click_if_present` returns the same `true` either way, since the element
/// was found and `.click()` itself didn't error). Used for the submit clicks that actually
/// need to land (advancing past username/password), not for the fire-and-forget clicks
/// used to drain optional interstitials.
async fn click_when_ready(page: &Page, selector: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(elements) = page.find_elements(selector).await {
            for el in elements {
                if !is_visible(&el).await {
                    continue;
                }
                let disabled = el
                    .property("disabled")
                    .await
                    .ok()
                    .flatten()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if disabled {
                    continue;
                }
                return click_via_js(&el).await.is_ok();
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(150)).await;
    }
}

/// Polls until the page lands on `redirect_prefix` (the OAuth native-client redirect — it
/// doesn't need to actually load, we just need the URL) or `timeout` elapses.
///
/// When `auto_advance` is set, also drains "Stay signed in?" and consent/permissions
/// interstitials the same way the reference Playwright bot does — Microsoft reuses
/// `#idSIButton9` as the primary/forward action (Next, Sign in, Accept) and `#idBtn_Back`
/// as the secondary action (Cancel, No, Back) across all these screens; the only screen
/// where the secondary action is what we *want* is "Stay signed in?" (declining consent
/// elsewhere would produce access_denied). This must stay off during a human-driven manual
/// login: `#idSIButton9` is the same id Microsoft uses for the username/password "Next"
/// buttons, so auto-clicking it every poll would resubmit the form out from under someone
/// still typing their password.
async fn wait_for_redirect(
    page: &Page,
    redirect_prefix: &str,
    timeout: Duration,
    auto_advance: bool,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(url)) = page.url().await {
            if url.starts_with(redirect_prefix) {
                return Ok(url);
            }
        }

        if auto_advance {
            if page_contains_text(page, "stay signed in").await.unwrap_or(false) {
                click_if_present(page, "#idBtn_Back").await;
            } else {
                click_if_present(page, "#idSIButton9").await;
            }
        }

        if tokio::time::Instant::now() >= deadline {
            bail!(
                "timed out waiting for redirect to {redirect_prefix} — login likely did not \
                 complete (wrong credentials, extra Conditional Access prompt, etc.)"
            );
        }
        sleep(POLL_INTERVAL).await;
    }
}

fn check_for_error(redirect_url: &str) -> Result<()> {
    let parsed = url::Url::parse(redirect_url).context("parsing redirect URL")?;
    if let Some((_, error)) = parsed.query_pairs().find(|(k, _)| k == "error") {
        let desc = parsed
            .query_pairs()
            .find(|(k, _)| k == "error_description")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_else(|| "(no description)".to_string());
        bail!("AAD login returned error={error:?}: {desc}");
    }
    Ok(())
}

async fn automated_login_inner(
    browser: &Browser,
    authorize_url: &str,
    redirect_prefix: &str,
    creds: &Credentials,
) -> Result<String> {
    let page = browser
        .new_page(authorize_url)
        .await
        .context("opening AAD authorize URL")?;

    let result = automated_login_steps(&page, redirect_prefix, creds).await;
    if result.is_err() {
        dump_page_diagnostics(&page).await;
    }
    result
}

/// Prints the page's current URL and visible text on an automated-login failure — without
/// this there's no way to tell, after the fact, whether it timed out on a genuinely
/// different screen (a "choose how to sign in" tile picker, an unexpected Conditional
/// Access prompt, etc.) versus a real bug in the selectors/flow.
async fn dump_page_diagnostics(page: &Page) {
    let url = page.url().await.ok().flatten().unwrap_or_default();
    aad_log!("[aad] automated: failed — current page url: {url}");
    match page_text_snippet(page).await {
        Ok(text) => aad_log!("[aad] automated: visible page text (first 800 chars):\n{text}"),
        Err(e) => aad_log!("[aad] automated: couldn't read page text: {e:#}"),
    }
    match save_diagnostic_screenshot(page).await {
        Ok(path) => aad_log!("[aad] automated: saved a screenshot to {}", path.display()),
        Err(e) => aad_log!("[aad] automated: couldn't take a screenshot: {e:#}"),
    }
}

async fn page_text_snippet(page: &Page) -> Result<String> {
    page.evaluate_expression("(document.body && document.body.innerText || '').slice(0, 800)")
        .await
        .context("evaluating diagnostic script")?
        .into_value()
        .context("reading diagnostic script result")
}

async fn save_diagnostic_screenshot(page: &Page) -> Result<PathBuf> {
    let bytes = page
        .screenshot(chromiumoxide::page::ScreenshotParams::builder().build())
        .await
        .context("capturing screenshot")?;
    let dir = dirs::cache_dir()
        .context("determining cache dir")?
        .join("rdp-client");
    tokio::fs::create_dir_all(&dir)
        .await
        .context("creating diagnostics dir")?;
    let path = dir.join("aad-failure.png");
    tokio::fs::write(&path, &bytes)
        .await
        .context("writing screenshot")?;
    Ok(path)
}

async fn automated_login_steps(page: &Page, redirect_prefix: &str, creds: &Credentials) -> Result<String> {
    aad_log!("[aad] automated: waiting for username field...");
    let username_field = wait_for_element(page, "input[name='loginfmt']", AUTOMATED_LOGIN_TIMEOUT)
        .await
        .context("waiting for the username field")?;
    focus_via_js(&username_field).await.context("focusing username field")?;
    username_field.type_str(creds.username.as_str()).await?;
    if !click_when_ready(page, "#idSIButton9", Duration::from_secs(5)).await {
        bail!("username \"Next\" button never became clickable");
    }

    aad_log!("[aad] automated: waiting for password field...");
    let password_field = wait_for_element(page, "input[name='passwd']", AUTOMATED_LOGIN_TIMEOUT)
        .await
        .context("waiting for the password field")?;
    focus_via_js(&password_field).await.context("focusing password field")?;
    password_field.type_str(creds.password.as_str()).await?;
    if !click_when_ready(page, "#idSIButton9", Duration::from_secs(5)).await {
        bail!("password \"Sign in\" button never became clickable");
    }

    // Some tenants default to a push notification. If we don't see the code field
    // shortly, look for the "use a verification code instead" option and switch to it.
    aad_log!("[aad] automated: waiting for one-time-code field...");
    let otc = match wait_for_element(page, "input[name='otc']", MFA_FALLBACK_TIMEOUT).await {
        Ok(el) => el,
        Err(_) => {
            aad_log!(
                "[aad] automated: no code field yet — looking for a \"use a verification \
                 code\" fallback link..."
            );
            if !click_text_if_present(page, "verification code").await? {
                bail!(
                    "expected a one-time-code field but didn't see one, and no \"use a \
                     verification code\" fallback link was found"
                );
            }
            wait_for_element(page, "input[name='otc']", AUTOMATED_LOGIN_TIMEOUT)
                .await
                .context("waiting for the one-time-code field after switching to it")?
        }
    };
    let code = totp::generate_code(&creds.totp_url).context("generating TOTP code")?;
    aad_log!("[aad] automated: submitting TOTP code...");
    focus_via_js(&otc).await.context("focusing one-time-code field")?;
    otc.type_str(code.as_str()).await?;
    click_if_present(page, "#idSubmit_SAOTCC_Continue").await;

    aad_log!("[aad] automated: waiting for redirect (draining consent/stay-signed-in screens)...");
    let redirect_url = wait_for_redirect(page, redirect_prefix, AUTOMATED_LOGIN_TIMEOUT, true).await?;
    check_for_error(&redirect_url)?;
    Ok(redirect_url)
}

/// Drives the Microsoft login form end-to-end using cached/fetched credentials: username,
/// password, then a locally-generated TOTP code (falling back to "use a verification code
/// instead" if the tenant defaults to a push notification). Fails fast on anything
/// unexpected — the caller is expected to fall back to [`manual_login_capture`].
pub async fn automated_login(
    authorize_url: &str,
    redirect_prefix: &str,
    creds: &Credentials,
    headless: bool,
) -> Result<String> {
    let (browser, handle, profile_dir) = launch(headless).await?;
    let result = tokio::time::timeout(
        AUTOMATED_LOGIN_TIMEOUT + OUTER_TIMEOUT_SLACK,
        automated_login_inner(&browser, authorize_url, redirect_prefix, creds),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow!("automated login timed out (browser became unresponsive)")));
    close_browser(browser, handle, &profile_dir).await;
    result
}

/// Opens a *visible* browser at the authorize URL and waits for the user to complete
/// sign-in themselves, capturing the resulting redirect URL automatically — no manual
/// copy/paste of the redirect URL required. Always headed, regardless of the `--headless`
/// flag: there's no point hiding the window a human is meant to interact with.
pub async fn manual_login_capture(authorize_url: &str, redirect_prefix: &str) -> Result<String> {
    let (browser, handle, profile_dir) = launch(false).await?;
    let inner = async {
        let page = browser
            .new_page(authorize_url)
            .await
            .context("opening AAD authorize URL")?;
        aad_log!("[aad] complete sign-in in the browser window that just opened...");
        let redirect_url = wait_for_redirect(&page, redirect_prefix, MANUAL_LOGIN_TIMEOUT, false).await?;
        check_for_error(&redirect_url)?;
        Ok(redirect_url)
    };
    let result = tokio::time::timeout(MANUAL_LOGIN_TIMEOUT + OUTER_TIMEOUT_SLACK, inner)
        .await
        .unwrap_or_else(|_| Err(anyhow!("manual login timed out (browser became unresponsive)")));
    close_browser(browser, handle, &profile_dir).await;
    result
}




