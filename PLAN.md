# Custom Remote Desktop System — Plan (Windows RDP host + ground-up Linux RDP client)

## 1. Requirements recap

| # | Requirement | Notes |
|---|---|---|
| 1 | LAN-RDP-equivalent latency | Real RDP wire protocol, real Windows host — latency now depends on how well *our* client implements it |
| 2 | Full-screen multi-monitor | 3 displays: 1080x1920 (portrait), 2560x1440, 5120x1440 (ultrawide) |
| 3 | Host → client audio | |
| 4 | Low CPU/GPU usage | Favor RDP's tile-based RemoteFX codec over video/AVC decode (see §5) |
| 5 | Cross-platform client | **This pass: Linux client only**, built from scratch. Windows deferred (host already works with `mstsc` natively) |
| 6 | Clipboard sync | Text, images, files |
| 7 | Virtual monitors on host | Solved at the wire-protocol level by the display-control channel (see §5) |
| 8 | Azure AD (Entra ID) auth | Formally documented as its own RDP security protocol — see §4, this is good news for a from-scratch build |
| 9 | Reuse existing OSS/Windows components over building from scratch | Reused at the *host* (real Windows RDP) and for generic building blocks (TLS, codec math) — **not** for the RDP protocol implementation itself, see §4 |

## 2. Scope change

Previous drafts considered building the Linux client on top of an existing RDP engine (libfreerdp or IronRDP). That's now explicitly out: **this is a new RDP client implementation for Linux, built from scratch against Microsoft's protocol specifications — not on top of FreeRDP, IronRDP, rdesktop, or any other existing RDP library.**

One assumption carried forward, flagged for confirmation: "no existing solutions" is read as *no existing RDP-specific libraries*. Generic, non-RDP building blocks — a TLS library (OpenSSL/rustls/GnuTLS), an ASN.1 DER encoder for CredSSP-adjacent structures, a windowing toolkit, an audio backend — are still fair game and not a violation of this direction. Flag back if the intent is narrower than that (e.g. hand-rolling TLS too).

The host side is unchanged: plain RDP host enablement (not a full RDS role), `enablerdsaadauth` for Entra ID, RDP's own display driver for virtual monitors — all still reused as-is, only the *client* is being built from scratch.

## 3. Host-side setup (unchanged)

1. Enable Remote Desktop (`fDenyTSConnections = 0`) on the host — plain RDP host feature, not a full multi-session RDS role deployment.
2. Ensure the host is Entra-ID-joined (or hybrid-joined); enable "Use a web account to sign in to the remote computer" (`enablerdsaadauth`).
3. No virtual display driver install needed — RDP's own display driver creates virtual outputs matching whatever layout/resolution the client requests over the wire (see §5, display control channel).
4. Confirm audio redirection (`rdpsnd`) and clipboard redirection (`cliprdr`) are enabled at the host — these are the server-side counterparts our client implements against.

## 4. Why this is more feasible than it first sounds: Entra ID auth is an actual documented protocol

The biggest risk in every prior draft was whether Entra ID/AAD auth was even a well-defined thing to implement, since FreeRDP's own support is described as "limited" and IronRDP's was unconfirmed. Direct research into the protocol spec resolved this: **"RDS AAD Auth" is a formally specified External Security Protocol in [MS-RDPBCGR](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/422e59a0-98c8-4a28-af9f-235a572b6e4d)**, documented as its own named security mode (alongside plain TLS, CredSSP, and RDSTLS) specifically for authenticating to an Entra-ID-joined or hybrid-joined device, negotiated the same way as the other security protocols via the RDP Negotiation Request/Response.

This meaningfully de-risks the build: it means Entra ID auth can be implemented directly from Microsoft's own published spec, not reverse-engineered from FreeRDP source. It also justifies a useful scope cut for v1: **implement only TLS + RDS AAD Auth as the security path**, and explicitly skip classic CredSSP/NTLM/Kerberos (local-account NLA) — that's not a stated requirement, and dropping it avoids a large chunk of legacy auth complexity (SPNEGO, NTLM, Kerberos ticket handling) that existing clients need for broad compatibility but this project doesn't.

## 5. Protocol implementation plan

Building against Microsoft's Open Specifications, layer by layer:

| Layer | Spec | What it covers here |
|---|---|---|
| Transport | TCP + TLS (1.2/1.3) | Standard connection setup; TLS via a generic library, not hand-rolled |
| Connection/security negotiation | MS-RDPBCGR (core) | X.224/MCS connection sequence, RDP Negotiation Request/Response, selecting the RDS AAD Auth security protocol (§4) |
| Capability exchange | MS-RDPBCGR | Client/Server Core Data and capability sets — scoped to just what's needed (bitmap, multi-monitor/multi-transport, no legacy fallbacks) |
| Graphics/codec | MS-RDPEGFX (Graphics Pipeline Extension) + MS-RDPRFX (RemoteFX Codec Extension) | RemoteFX Progressive is itself a **tile-based** (64x64), CPU-only codec — this is the actual mechanism satisfying the original "low CPU/GPU, chunked updates" goal, implemented as real, documented RDP rather than a bespoke scheme. AVC420/444 video-codec paths are deliberately deferred/out of scope — they pull in H.264 decode complexity and cut against the low-CPU/GPU goal |
| Dynamic virtual channels | MS-RDPEDYC | Transport for the higher-level channels below |
| Display control (virtual monitors) | MS-RDPEDISP | This is the actual wire mechanism behind requirement #7 — the client advertises its 3 real monitor resolutions/layout, and the host's own display driver creates matching virtual outputs. No separate driver work needed on either side, it's just implementing this channel correctly |
| Clipboard | MS-RDPECLIP | Text, image, and file-list clipboard formats, including its own file-content request/response streaming for file data |
| Audio | MS-RDPEA | Host→client audio output channel |
| Input | MS-RDPBCGR (fast-path input PDUs) | Keyboard/mouse forwarding |

Client application layer on top of this: windowing (one fullscreen borderless window per real monitor, positioned per the negotiated layout), native Linux clipboard integration (X11/Wayland), and an audio playback backend (PipeWire/PulseAudio) — same responsibilities as before, just sitting on a from-scratch protocol implementation instead of a third-party engine.

## 6. Phased plan

- **Phase 0 — Transport + security handshake**: TCP/TLS connection, RDP Negotiation Request/Response, RDS AAD Auth security protocol handshake against the host from §3. Goal: prove authentication succeeds end-to-end before any graphics work starts — this is the highest-risk, spec-newest part.
- **Phase 1 — Core connection + single-monitor bitmap output**: MCS connection sequence, capability exchange, basic (non-GFX) bitmap updates rendered to one fullscreen window, keyboard/mouse input forwarded. Goal: a visibly working single-monitor remote desktop, even before RemoteFX tiling is in place.
- **Phase 2 — RemoteFX Progressive codec (MS-RDPEGFX/MS-RDPRFX)**: swap the naive bitmap path for the real tile-based codec — this is where the latency and CPU/GPU usage requirements actually get met.
- **Phase 3 — Multi-monitor via MS-RDPEDISP**: advertise and negotiate the 3 real resolutions/layout; verify the host creates matching virtual outputs and per-monitor rendering/coordinate mapping is correct.
- **Phase 4 — Audio (MS-RDPEA) + clipboard (MS-RDPECLIP)**: audio playback round trip; clipboard text → images → files, in that order of complexity.
- **Phase 5 (deferred)** — Windows client: out of scope for this pass (host already supports `mstsc` natively with zero extra work whenever it's wanted).

## 7. Open questions / risks

- **RDS AAD Auth handshake details**: the spec section exists, but the exact token/credential exchange (what Entra ID token is presented, how it's tied to the TLS channel per MS-RDPBCGR's anti-replay design) needs careful reading of the full spec before Phase 0 starts — flag any spec ambiguity found rather than guessing.
- **RemoteFX Progressive complexity**: still nontrivial (wavelet-based tiling with progressive refinement) — Phase 1's simpler bitmap path exists specifically so there's a working fallback while Phase 2 is being built, not as a permanent parallel path.
- **Scope of "no existing solutions"**: confirm the generic-library assumption in §2 (TLS, ASN.1, windowing, audio libraries are fine; RDP-specific code is not).
- **CredSSP/NTLM auth is being dropped** for this pass per §4 — confirm that's acceptable (i.e. the host will only ever be accessed via Entra ID auth, never a local account fallback).
- Wayland vs. X11 differences in clipboard and input handling should be spiked early.

## 8. Explicitly out of scope (for this pass)

- Windows client (deferred — see §6 Phase 5).
- CredSSP/NTLM/Kerberos ("classic" NLA) authentication — Entra ID (RDS AAD Auth) only.
- AVC420/444 video-codec graphics path — RemoteFX Progressive only.
- Any custom (non-RDP) capture/encode/transport scheme, or a custom virtual display driver — this plan implements real RDP's own mechanisms instead.
- WAN/relay access — LAN-first per the latency requirement.
- Multi-user concurrent sessions / full RDS role deployment.

## 9. Progress log

**Phase 0 — done.** TCP/TLS + RDS AAD Auth handshake works end-to-end against the real host (Windows 10 IoT Enterprise LTSC 2024 / Windows 11 24H2 kernel, a private-network host, redacted). Two real-world deviations from a literal spec reading, both resolved:
- The spec's own sample OAuth client ID has no service principal in this tenant and needs tenant-admin consent we don't have; swapped to a different Microsoft-owned public client ID (the one `xfreerdp` uses) — still just an OAuth identifier, no third-party code involved.
- Several assertion/PDU wire-format details (field types, the `u` claim's exact contents, NUL-termination on writes as well as reads) needed cross-checking against FreeRDP's `aad.c` to resolve spec ambiguity.

**Phase 1 — connection sequence done; graphics output pivoted to Phase 2's pipeline.** MCS connection, Client Info, licensing, and capability exchange (Demand Active/Confirm Active) all work end-to-end. Key finding: **this host never sends legacy slow-path Bitmap Update PDUs at all** — confirmed by testing (Refresh Rect and Suppress Output PDUs both rejected/ineffective) and by comparison against `xfreerdp /sec:aad`, which only renders via the modern MS-RDPEGFX pipeline. Decision: pull Phase 2's graphics pipeline forward rather than finishing Phase 1's originally-scoped legacy path — see below. The legacy path code (RLE bitmap decoder, capability negotiation) stays in the tree; capability exchange was a prerequisite either way, and the RLE decoder may still matter for other/older hosts.

Two capability-exchange gotchas worth knowing if picking this back up:
- Real Windows hosts reject an all-zero Order Capability Set's `orderSupport` with `ERRINFO_BADCAPABILITIES`, contradicting the spec's own "server should gracefully fall back to bitmap updates" text. Fix: `capabilities::extract_order_capability` mirrors the server's own advertised `orderFlags`/`orderSupport` back at it verbatim.
- `RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL` (0x0100) in the Client Core Data `earlyCapabilityFlags` is **load-bearing**: without it, the server never offers the graphics dynamic virtual channel at all (confirmed — 8 unrelated channels get offered, never the graphics one, until this flag is set).

**Phase 2 — MS-RDPEDYC + MS-RDPEGFX handshake and frame pipeline now working end-to-end against the real host.** `"drdynvc"` static channel setup, DVC capability negotiation, and opening the graphics dynamic channel all work — whose wire name on this host is the **long form `"Microsoft::Windows::RDS::Graphics"`**, not the short `"rdpgfx"` alias used in FreeRDP's source (that name is apparently just FreeRDP's internal constant, not a protocol constant). `RDPGFX_CAPS_ADVERTISE_PDU` sends successfully.

**Root cause of the previous blocker, now resolved**: the server's first response after Caps Advertise arrived as a DVC Data PDU whose 16-byte payload (`e0 24 09 e3 18 0a 44 8d d8 e5 8d d1 42 23 80 04`) didn't parse as `RDPGFX_HEADER`. The DVC-layer framing was never the problem — **the server wraps every PDU it sends on this channel in a ZGFX container** (informal FreeRDP name for RDP 8.0 bulk compression, MS-RDPEGFX §2.2.5/§3.1.9.1.2; not mentioned anywhere in the prior research pass). `0xE0` is the `ZGFX_SEGMENTED_SINGLE` descriptor and `0x24` is the flags byte with `PACKET_COMPRESSED` (`0x20`) set. Implemented in `client/src/zgfx.rs`: full decompressor (39-row Huffman-style token table, MSB-first bit reader, unary-coded match lengths, a 2,500,000-byte ring-buffer match history shared for the channel's lifetime, plus the `0xE1` multipart-segment and uncompressed-segment cases). The client's own *outgoing* PDUs (e.g. `RDPGFX_CAPS_ADVERTISE_PDU`) do **not** need ZGFX wrapping — confirmed both empirically (the raw unwrapped PDU we already sent was accepted) and against FreeRDP source (`rdpgfx_send_*` never calls its ZGFX compressor; the client-side `ZGFX_CONTEXT` is receive-only).

Confirmed end-to-end with real frame data: caps confirmed → reset graphics (1280×800) → create surface → map surface to output → start/end frame cycles with `RDPGFX_FRAME_ACKNOWLEDGE_PDU` sent back. Two frames fully decoded and acknowledged in the latest run.

**ClearCodec + surface cache — implemented and verified working end-to-end.** The server's actual `WIRE_TO_SURFACE_1` traffic uses `codecId=0x0008` (**ClearCodec**), not `0x0009` (RemoteFX Progressive) as the prior capability-negotiation research assumed a bare V8 capset would force — ClearCodec turns out not to be gated by those capset flags at all, real servers use it freely for small/cacheable UI regions. Implemented: the full ClearCodec decoder (`client/src/clearcodec.rs` — residual RLE layer, bands/V-Bar layer with its own glyph/V-Bar/short-V-Bar caches, subcodec layer with raw and RLEX sub-modes; NSCodec sub-mode not implemented, not observed from this host), `RDPGFX_SURFACE_TO_CACHE_PDU`/`CACHE_TO_SURFACE_PDU`/`EVICT_CACHE_ENTRY_PDU` parsing (`client/src/gfx.rs`), and surface/cache pixel state (`client/src/surface.rs`). Along the way, found and fixed a real pre-existing bug in `RDPGFX_SOLID_FILL_PDU` parsing (missing leading `surfaceId` field, silently misaligning every field after it).

**Also found and fixed a real bug in the ZGFX layer itself** while testing this: the unencoded-run escape (§ zgfx_compression memory) was byte-aligning *before* reading its 15-bit count field instead of after, which permanently bit-shifted every subsequent byte in any message containing a raw/unencoded run — this caused several rounds of confusing downstream failures ("invalid ZGFX huffman code", garbage `RDPGFX_HEADER` fields) that only manifested on messages large enough to hit that code path, well after the ZGFX layer had already "worked" on many smaller messages. Fixed and reverified.

**Verified end-to-end**: ran 60 real frames against the host with zero decode errors, dumped the mapped surface's pixel buffer to a PPM, and visually confirmed it renders a real, recognizable Windows RDS "Please wait" session-loading screen — correctly-positioned window chrome (minimize/maximize/close buttons), panel content, no corruption. This is the first visual proof the whole pipeline (auth → connection → DVC → GFX handshake → ZGFX → ClearCodec → surface compositing) produces genuinely correct output, not just "doesn't crash."

**Live window — implemented and verified.** Wired the existing `window.rs` winit/softbuffer scaffolding to the network loop: `run_session` (formerly `main`'s body) now runs on a background thread and sends a full-surface `BitmapTile` over an `mpsc` channel after every decoded frame; the main thread owns the winit event loop (a hard requirement) via a small `NetworkDriver: SessionDriver` adapter that drains the channel each tick. Confirmed with a real screenshot of the live window (via ImageMagick `import`, since this is a native GUI window, not a browser) showing the same correctly-rendered "Please wait" RDS session screen as the earlier static dump, updating in real time. One environment gotcha worth recording: this desktop is a native Wayland session (GNOME/Mutter), and winit defaults to its Wayland backend where X11 tools like `wmctrl`/`import` can't see the window at all — `WINIT_UNIX_BACKEND` (the old fix) was removed in winit 0.29+; the current fix is unsetting `WAYLAND_DISPLAY` before launch to force winit onto X11/XWayland, same workaround already in use for the AAD browser automation.

**Interactive input — implemented and verified.** The live window was initially read-only (no mouse/keyboard forwarded), which turned out to matter a lot: this host's RDS session sits on a "Please wait" loading screen indefinitely without any client input activity (confirmed via `query session` over SSH — the session shows genuinely `Active` server-side the whole time, so this isn't a connection bug, just a session that needs real input to finish initializing, same as it would for any client). Implemented real Fast-Path Input PDU support (`client/src/input.rs` — mouse move/click, keyboard scancodes via a winit-`KeyCode`→PS/2-Set-1-scancode table in `client/src/window.rs`) and fixed a real capability-negotiation gap along the way: the client's `TS_INPUT_CAPABILITYSET` never advertised `INPUT_FLAG_FASTPATH_INPUT`/`FASTPATH_INPUT2` (`0x0008`/`0x0020`), without which a modern RDS host may just ignore fast-path input entirely (`capabilities.rs`, was `0x0005`, now `0x002d`).

Wiring this in required solving a real architecture problem: rustls' `ClientConnection` can't be safely split across threads for independent concurrent read/write, so a naive second "input thread" writing to a cloned socket wasn't viable. Solved with `client/src/input.rs`'s `DuplexStream` — wraps the TLS stream, sets a short (30ms) read timeout on the underlying socket, and treats every timeout as a cue to drain and flush any pending input events before resuming the blocking read. This required no changes to any of the existing read call sites (`mcs.rs`, `vchannel.rs`, etc. all just use the generic `Read`/`Write` traits) — one wrapper, applied once right after the TLS handshake.

**Verified end-to-end** with real (harness-only) input tests: after wiring this up, the session did progress past "Please wait" into genuine rich desktop content (Outlook, an "Acumatica Dev Tools" panel, a "Time and Expenses" app) — which then surfaced the *next* real bug (below), previously invisible because testing never got past the login screen.

**ClearCodec NSCodec sub-mode — implemented, fixed the large black content blocks.** Once real desktop content was reachable, most of it rendered as solid black rectangles. Diagnosis: 274 of 362 `ClearCodec decode failed` errors in one test session were exactly `subCodecId=NSCodec (0x01) not implemented` — the one ClearCodec subcodec sub-mode left unimplemented in the original pass, now confirmed to be what real desktop apps' bulkier content (editor backgrounds, panel fills) actually uses, unlike the small cacheable UI elements (icons, glyphs, taskbar) that had been the only content exercised by testing up to that point. Implemented in `client/src/nscodec.rs`: the `NSCODEC_BITMAP_STREAM` header, 4-plane (Luma/OrangeChroma/GreenChroma/Alpha, "AYCoCg") layout with correct chroma-subsampling row-stride math, the plane-level byte-oriented RLE scheme (with its own `left==5` end-of-plane boundary special case, structurally the same *kind* of operation-order bug class as the ZGFX unencoded-run fix earlier — a byte 5 positions from a plane's end must be forced literal or a run could stomp the mandatory unencoded 4-byte tail), and the integer AYCoCg→BGR inverse transform (`shift = ColorLossLevel - 1`, not `ColorLossLevel` — an easy off-by-one against the spec prose that silently desaturates colors rather than erroring). After this fix, NSCodec-related failures dropped to 0 in the same test scenario.

**Cascading ClearCodec cache-miss desync — found and fixed.** Even after the NSCodec fix, ~130 `V-Bar`/`short V-Bar cache miss` errors remained per session, with the referenced index vs. this client's own cache cursor position diverging *further* as the session went on (0 apart at the first miss, 2000+ apart later) — a classic self-amplifying cascade. Root cause: a cache-miss was a hard `bail!` that aborted the *entire* ClearCodec message, meaning any `SHORT_VBAR_CACHE_MISS` entries later in that same message (which populate the cache and advance the cursor) never got processed — permanently widening the gap on every subsequent message. Fixed in `client/src/clearcodec.rs` by making a single missing cache reference non-fatal: fall back to the band's background color for just that one V-Bar (or skip the draw entirely for a glyph-cache miss, since there's no fallback content available at all in that case) and keep processing the rest of the message. After this fix: 0 `ClearCodec decode failed` errors and the previously-black regions render correctly — confirmed via a live screenshot of a fully-rendered Outlook window (email list, open message, calendar panel).

The likely underlying reason these caches ever miss at all (root cause of the *first* miss, before any cascading): this RDS host's session supports reconnect (`query session` showed it toggling `Active`/`Disc` as our client connects/disconnects), and ClearCodec's glyph/V-Bar caches are correctly *not* reset on `ResetGraphics` per spec — but if the server's own encoder-side cache is tied to something more persistent than a single client TCP connection (e.g. survives a reconnect to the same RDS session), a fresh client will legitimately not have entries the server assumes are already cached. Not fully confirmed; the graceful-fallback fix handles it regardless of the precise cause.

**Remaining gaps**: `client/src/gfx.rs`'s cmdId `0x0002`/`0x0003` (`WireToSurface2`/`DeleteEncodingContext`) still fall through to the generic `Other` skip path — ~100+ occurrences per session, not yet identified what content (if any) this costs visually now that the black-block issues are resolved; minor cosmetic text-ghosting artifacts observed in the latest screenshot, likely a dirty-rect/redraw-timing issue (currently every decoded frame triggers a full-surface re-blit, no partial-update tracking) rather than a decode bug — not yet investigated. The previously-researched RemoteFX Progressive/RLGR1 decoder (`phase2_gfx_pipeline.md` memory) is still unexercised by this host.

Debug `eprintln!`s from earlier blockers are still in place in `main.rs`'s `ChannelRouter` and `dvc.rs`'s `handle_message`; all other temporary trace instrumentation added during this session's debugging has been removed now that the bugs it was tracking are fixed.

**Tooling gotcha — never use OS-level input injection (`ydotool`/`xdotool`/etc.) to test this client.** It affects the real local desktop, not just the app window — hit this directly during testing and the user (rightly) asked for it to stop. Use `input.rs`'s own PDU-sending functions (only touch the *remote* session) or ask the user to interact with the window themselves.

See memory (`phase2_gfx_pipeline.md`, `zgfx_compression.md`, `clearcodec_and_surfaces.md`, and `no_local_input_injection.md`) for the full byte-level reference, and `phase1_mcs_capability_pdus.md` for the Phase 1 equivalent.

**Tooling — automated AAD sign-in.** The manual "paste the redirect URL" flow from Phase 0 now has an automated alternative: `--aad-op-item <1password-item-uuid>` drives the whole Entra ID login (username, password, 2FA) via a native Rust Chrome DevTools Protocol client (`chromiumoxide`), sourcing credentials from a 1Password item and caching them (AES-256-GCM encrypted) so only the first run ever calls out to 1Password. Falls back to a visible-browser manual flow — with automatic redirect capture, no paste needed — if the automated attempt fails for any reason. See [`client/README.md`](client/README.md) for full usage/setup and [`client/src/aad_auto/`](client/src/aad_auto/) for the implementation. Not part of the core from-scratch-RDP-protocol scope in §2 — this is client-side login UX tooling, using only generic building blocks (a CDP crate, a TOTP crate, AES-GCM) same as the rest of the "generic non-RDP building blocks are fair game" carve-out.
