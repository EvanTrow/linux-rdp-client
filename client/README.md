# rdp-client

A from-scratch Linux RDP client (Rust), built directly against Microsoft's Open
Specifications rather than on top of FreeRDP/IronRDP/rdesktop. See
[`../PLAN.md`](../PLAN.md) for the overall project plan and phase-by-phase progress log.

## Build

```
cargo build --release
```

## Usage

```
rdp-client <host[:port]> [--aad-op-item <1password-item-uuid>] [--headless]
```

The host must have Remote Desktop enabled and "Use a web account to sign in to the
remote computer" (`enablerdsaadauth`) turned on — this client only implements the RDS
AAD Auth (Entra ID) security path, not classic NLA/CredSSP with a local account.

### Manual sign-in (default)

Without `--aad-op-item`, the client prints the Microsoft sign-in URL and waits on stdin
for you to complete sign-in in your own browser and paste back the resulting redirect
URL (or just its `code` parameter):

```
rdp-client myhost.local
```

### Automated sign-in

Passing `--aad-op-item <uuid>` drives the whole Entra ID login automatically instead —
username, password, and 2FA — using credentials sourced from a 1Password item, with a
visible-browser fallback if anything about the automated attempt doesn't work.

```
rdp-client myhost.local --aad-op-item a1b2c3d4-...
rdp-client myhost.local --aad-op-item a1b2c3d4-... --headless
```

#### Setup

1. **1Password CLI**: install `op` and enable "Integrate with 1Password CLI" in the
   desktop app (Settings > Developer). Confirm it works with:
   ```
   op item get <item name or UUID> --format json --reveal
   ```
2. **1Password item**: a Login item with:
   - a Username field (purpose `USERNAME`)
   - a Password field (purpose `PASSWORD`)
   - a One-Time Password field labeled exactly `one-time password`, set up from the
     *same* QR code/secret given to Microsoft Authenticator (or entered as a manual
     TOTP secret), so the codes match

   Always reference the item by UUID, not name — find it with:
   ```
   op item get "<item name>" --format json | jq -r .id
   ```
3. A Chromium build is downloaded automatically on first use (cached under
   `~/.cache/chromiumoxide`) — no system browser install required.

#### How it works

1. **First run** (or whenever there's no local cache yet): fetches the username,
   password, and raw TOTP seed from the given 1Password item. This is the only step
   that ever talks to 1Password — it's what triggers the "Allow" prompt in the
   1Password desktop app the first time. The fetched credentials are cached, encrypted,
   for every later run.
2. **Every run**: attempts a fully automated Chromium-driven login — fills username,
   password, and a locally-computed 6-digit TOTP code (RFC 6238, generated from the
   cached seed — no `op` call needed after the first run), including the fallback to
   "use a verification code instead" if the tenant defaults to a push notification.
3. **If the automated attempt fails** for any reason (stale password, unexpected
   prompt, timeout...): falls back to a visible browser window for you to complete
   sign-in by hand. The resulting redirect URL is still captured automatically — no
   manual copy/paste needed even in the fallback case.

`--headless` only affects step 2 (the automated attempt); the manual fallback in step 3
is always a visible window, since a human needs to see and interact with it.

#### Credential cache

Cached at `~/.config/rdp-client/aad-cache/`, one AES-256-GCM–encrypted file per
1Password item UUID, plus a `cache.key` file holding the encryption key (both written
0600). There's no passphrase — by design, the whole point is zero extra prompts after
the first run — so this protects against casual disclosure (backups, screen shares,
other unprivileged users on the same machine) but not a full compromise of your user
account. Delete the relevant `<item-uuid>.enc` file (or the whole `aad-cache/`
directory) to force re-fetching from 1Password on the next run, e.g. after a password
change.

#### Troubleshooting

Every run truncates and writes `~/.cache/rdp-client/aad-session.log`, mirroring
everything printed to the terminal — check it if a run is closed before you can read
the scrollback.

If the automated attempt fails, it also saves a screenshot of the browser at the moment
of failure to `~/.cache/rdp-client/aad-failure.png`, alongside the page's URL and
visible text in the log. That combination is normally enough to tell whether it's an
unexpected screen (Conditional Access prompt, a tenant-specific sign-in option) versus
a real bug in the automation.

The AAD login window is sized 1280x720 and centered on the primary monitor (best
effort, using `xrandr`/`wmctrl` — X11/XWayland only; not applicable under native
Wayland, though the client forces Chromium onto XWayland specifically so this still
works under a Wayland session).
