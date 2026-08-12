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
