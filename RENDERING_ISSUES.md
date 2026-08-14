# Known rendering issues (as of 2026-08-13)

Status: paused mid-investigation at the user's request. This documents what's confirmed,
what's ruled out, and what to try next, so a future session doesn't have to re-derive it.

The client is functional end-to-end (auth, connection, GFX pipeline, live window, real
interactive input all work — see `PLAN.md` §9), but real-world desktop content still
renders with two distinct, separate visual defects. Fixing both is required before this
is a usable replacement for a normal RDP client.

![Both issues at once: large black rectangles (top and bottom bars) plus garbled/missing letters in the sidebar text](docs/screenshots/black-blocks-and-garbled-text.png)

## Issue 1: large solid black rectangular blocks — root cause known, fix not yet built

Large regions of real application UI (browser tab bars, status bars, some panel
backgrounds) render as solid black instead of their actual content. See the top and
bottom bars in the screenshot above.

**Root cause, high confidence**: these are `RDPGFX_WIRE_TO_SURFACE_PDU_2` (cmdId `0x0002`)
messages, which the client currently parses only far enough to skip (no case for it in
`client/src/gfx.rs`'s `GfxPdu` enum — falls through to `GfxPdu::Other` and is ignored).
Confirmed via a dedicated research pass (cross-checked against the MS-RDPEGFX spec and
FreeRDP source) that `RDPGFX_WIRE_TO_SURFACE_PDU_2`'s `codecId` field is spec-restricted to
**only** `RDPGFX_CODECID_CAPROGRESSIVE` (`0x0009`, RemoteFX Progressive) — no other codec is
spec-legal there. This client has never implemented the RemoteFX Progressive bitstream
decoder itself (RLGR1 entropy coding + inverse DWT + YCbCr→RGB) — only its wire format was
researched in an earlier session (see the `phase2_gfx_pipeline.md` memory file, rated HIGH
confidence for the container/RLGR1 layers). At 100-150+ of these PDUs observed per session
against the real test host, this is a substantial, real content gap, not an edge case.

**Exact wire format** (confirmed via research, byte-exact):
```
header          (8 bytes)  RDPGFX_HEADER: cmdId(2)=0x0002 + flags(2)=0 + pduLength(4)
surfaceId       (2 bytes)  u16 LE
codecId         (2 bytes)  u16 LE  — always 0x0009 (CAProgressive) per spec
codecContextId  (4 bytes)  u32 LE  — FreeRDP logs it but never uses it; track progressive
                                     tile-cache state keyed by surfaceId instead, same as
                                     FreeRDP does
pixelFormat     (1 byte)   RDPGFX_PIXELFORMAT (0x20=XRGB_8888, 0x21=ARGB_8888)
bitmapDataLength(4 bytes)  u32 LE
bitmapData      (variable) bitmapDataLength bytes — an RFX_PROGRESSIVE_DATABLOCK stream,
                                                     same container format as researched
                                                     for WIRE_TO_SURFACE_1's CAProgressive
                                                     path (see phase2_gfx_pipeline.md)
```
No `destRect` field — unlike WIRE_TO_SURFACE_1, the destination region comes entirely from
parsing `RFX_PROGRESSIVE_REGION.rects` inside the bitstream itself (confirmed via FreeRDP
source comment: "the cmd's top/left/right/bottom/width/height members are always zero!
The update region is determined during decompression").

`RDPGFX_CMDID_DELETEENCODINGCONTEXT` (cmdId `0x0003`, ~80+/session, also currently
skipped) is confirmed safe to continue skipping — FreeRDP's own handler for it is a literal
no-op; the actual cleanup happens on `DeleteSurface` instead, keyed by `surfaceId`.

**Fix**: implement the RemoteFX Progressive decoder (RLGR1 + DWT + color conversion — wire
format already documented at HIGH confidence in `phase2_gfx_pipeline.md`; TILE_UPGRADE
refinement passes were flagged there as LOW confidence / deferrable), add a
`WireToSurface2` variant to `gfx::GfxPdu` and its parser, and wire it into `main.rs`'s GFX
event loop the same way `WireToSurface1` is handled. Not yet started.

## Issue 2: sporadic missing/wrong individual letters within otherwise-correct text — root cause NOT found

Real text (menu labels, list items, document content) mostly renders legibly, but
individual characters or short spans within words are randomly missing or wrong — e.g.
"Receivables" rendering as "ceivables", "Purchases" as "Purch ees". Not the same defect as
Issue 1 — this happens in regions that ARE getting real content, just with per-character
corruption, and no full black rectangles.

![Sidebar and content text with individual missing letters — "ceivables" instead of "Receivables", "Rec ivables" in the sidebar, etc. — while most of each word is correct](docs/screenshots/missing-letters-in-text.png)

![The same corruption pattern on a completely fresh RDS session (server-side session logged off and reconnected), ruling out accumulated-reconnect-state as the cause](docs/screenshots/fresh-session-still-corrupted.png)

### Theories investigated and ruled out

1. **Cascading ClearCodec cache-miss desync (partially fixed, not the full story).**
   `client/src/clearcodec.rs`'s V-Bar/short-V-Bar/glyph caches are populated via a
   monotonically-incrementing cursor per `ClearCodecContext` (session-lifetime, per MS
   spec — correctly never reset on `ResetGraphics`). Early investigation found a real bug:
   a cache-miss was a hard `bail!()` that aborted the *entire* ClearCodec message,
   preventing any later cache-populating entries in that same message from ever being
   processed — a self-amplifying cascade (empirically: the gap between a missed index and
   the local cursor grew from 0 to 2000+ over one session). **Fixed** by falling back to
   the band's background color for a single missing V-Bar/short-V-Bar (or skipping the
   draw for a glyph-cache miss) instead of aborting the message — this eliminated all
   `ClearCodec decode failed` errors and visibly reduced large solid-black gaps. However,
   the sporadic missing-letter symptom **persisted after this fix**, so it is not the full
   explanation — likely just a milder tail of the same underlying-but-unidentified cause.

2. **Server-side session-persistent cache (ruled out).** Theory: the server's ClearCodec
   encoder-side cache-slot counter might be tied to the RDS *session* lifetime rather than
   the individual RDP *connection*, so repeated reconnects (15+ times during testing that
   day) to the same long-lived `<user>` session could cause the gap between the
   server's assumed cache state and a fresh client's actual state to grow with each
   reconnect. **Tested and refuted**: force-logged-off the RDS session entirely via SSH
   (`logoff 2`, confirmed gone via `query session`), reconnected fresh — the same
   missing-letter corruption appeared almost immediately on the brand-new session, same as
   before. Whatever causes this, it is not accumulated reconnect drift.

3. **RDPGFX-level bitmap cache misses (ruled out).** Separate from ClearCodec's own
   internal caches, `RDPGFX_CACHE_TO_SURFACE_PDU`/`SURFACE_TO_CACHE_PDU`
   (`client/src/surface.rs`'s `SurfaceManager`) is a completely different caching
   mechanism, heavily used for real text rendering (a `SurfaceToCache` followed by
   hundreds of `CacheToSurface` stamps is the dominant pattern observed for text/UI
   elements). Added instrumentation (`SurfaceManager::cache_to_surface` now returns
   `bool`, logged as `"cache-to-surface slot=... MISS"` in `main.rs` when a referenced
   slot was never populated) — **result: zero misses observed** across a full test session
   with real corruption visibly present at the same time. This mechanism is not the cause.

4. **Transport-layer message loss/reordering (ruled out, indirectly).** ClearCodec's own
   `seqNumber` field is checked strictly (`bail!("ClearCodec seqNumber gap...")` on any
   non-consecutive value) — **zero seqNumber gap errors observed**, across all test runs,
   including ones with heavy visible corruption. Since every ClearCodec message we do
   receive has a perfectly consecutive sequence number, the ZGFX/DVC transport layer is not
   silently dropping or reordering whole ClearCodec messages.

5. **`WireToSurface2` carrying ClearCodec content (ruled out).** Considered whether the
   skipped `WireToSurface2` PDUs (Issue 1) might also be populating/expected-to-populate
   ClearCodec's V-Bar caches, explaining both issues with one root cause. Refuted by the
   same research pass that confirmed Issue 1's root cause: `WireToSurface2`'s `codecId` is
   spec-restricted to CAProgressive only, ClearCodec structurally never travels through
   that envelope.

### Where this leaves it

No confirmed root cause yet. What's established: it's not a transport drop/reorder issue
(seqNumber proves in-order, complete delivery of ClearCodec messages), not a cache-miss at
either the ClearCodec-internal or RDPGFX-level caching layer (both instrumented, both show
zero/near-zero misses while corruption is visibly present), and not explained by
`WireToSurface2`. That points toward a genuine **pixel-level decode bug** somewhere in
`client/src/clearcodec.rs`'s residual/bands/subcodec composition or
`client/src/surface.rs`'s blit paths that produces plausible-but-wrong output *without*
tripping any of the validity checks currently in place — i.e. structurally well-formed data
being decoded to the wrong pixel values, not malformed data being rejected.

**Suggested next steps for whoever picks this up**:
- Re-audit `clearcodec.rs`'s `decode_bands` V-Bar/short-V-Bar pixel-placement math and
  `decode_rlex`'s palette/suite-walk logic line-by-line against the byte-exact spec
  transcription in the `clearcodec_and_surfaces.md` memory file, looking specifically for
  an off-by-one or byte-order slip that would produce a *wrong-but-valid-looking* pixel
  rather than a decode error — this is the same bug *class* (not the same bug) as the
  ZGFX unencoded-run and NSCodec RLE-tail issues found and fixed earlier this session,
  both of which were exactly this kind of "structurally valid, semantically wrong"
  operation-order mistake.
- Consider whether `surface.rs`'s `Surface::blit_rect`/`extract_rect` (used by
  `SurfaceToSurface` and the RDPGFX-level cache, both heavily exercised by real text
  rendering) could be reading/writing a slightly wrong region — a rect off by a few pixels
  would look exactly like "some letters missing/wrong" without causing any error.
- If a byte-exact re-audit doesn't turn up the bug, consider adding a way to dump a single
  ClearCodec-decoded glyph tile (before it's composited into the surface) to a small image
  alongside logging its source bytes, then hand-verify one specific observed-wrong
  character against its raw wire bytes the same way the ZGFX/NSCodec bugs were confirmed
  earlier — this session's diagnosis relied heavily on hand-decoding real captured bytes
  against the spec, which was effective but hasn't yet been applied to this specific bug.
- Do NOT reintroduce ClearCodec cache-miss hard failures (see Theory 1) — the graceful
  fallback is correct and necessary regardless of whatever this bug turns out to be.

See also: `clearcodec_and_surfaces.md`, `zgfx_compression.md`, and `phase2_gfx_pipeline.md`
memory files for the byte-exact wire-format references this investigation was checked
against.
