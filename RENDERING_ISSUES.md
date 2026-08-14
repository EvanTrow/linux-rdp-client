# Known rendering issues

## Issue 4: stale/misplaced "ghost" regions — ROOT CAUSE FOUND AND FIXED 2026-08-14

**Root cause: `RDPGFX_SURFACE_TO_SURFACE_PDU` was parsed two bytes out of alignment, so
every surface-to-surface copy in the session was silently discarded.**

MS-RDPEGFX §2.2.2.5 lays the PDU out as `surfaceIdSrc(2) surfaceIdDest(2) rectSrc(8)
destPtsCount(2)` — 14 fixed bytes, confirmed against FreeRDP's
`rdpgfx_recv_surface_to_surface_pdu`, which requires exactly 14 and reads the destination
surface id *before* the source rect. `client/src/gfx.rs` read `rectSrc` from offset 2 and
`surfaceIdDest` from offset 10, i.e. everything after `surfaceIdSrc` shifted by two bytes:

| field | what it should be | what it was |
|---|---|---|
| `rectSrc.left`   | rect left   | `surfaceIdDest` |
| `rectSrc.top`    | rect top    | rect left   |
| `rectSrc.right`  | rect right  | rect top    |
| `rectSrc.bottom` | rect bottom | rect right  |
| `surfaceIdDest`  | dest surface | rect **bottom** |

The destination surface id therefore came out as a screen coordinate. A y-coordinate
essentially never names a live surface (this host uses surface ids 1 and 2), so
`surfaces.get_mut(dst)` returned `None` and — because that lookup was an `if let` with no
`else` — the entire copy was dropped with no log line, no error, and no counter.

Why the symptom looked the way it did: `SURFACE_TO_SURFACE` is what a Windows host uses for
**scroll optimisation** (copy a strip up or down instead of re-encoding it) and for **moving
window content** (copy a block of chrome to its new position). Those are exactly the
"horizontal strips" and "window-chrome shaped" regions in the report, and once a copy is
dropped the server never resends it, so the old pixels stay for the rest of the session.
On the rarer occasions the bogus id *did* hit a live surface, the equally bogus source rect
copied real content to the wrong place — the "decoded content at the wrong coordinates" half
of the symptom, from the same single defect.

Also fixed, each independently capable of producing permanent artifacts:

* **The frame hand-off dropped the newest frame, not the oldest.** Frames went to the UI
  thread over a `sync_channel(1)`, and a bounded channel keeps the *queued* item and rejects
  the *new* one. At the tail of any burst — drag released, scroll stopped, menu closed — the
  final full-surface snapshot was discarded, and since no further `END_FRAME` arrives once
  the server idles, the screen stayed one frame behind indefinitely. Replaced with
  `window::FrameMailbox`, a latest-wins slot; superseding an unconsumed snapshot is safe
  precisely because each one is a whole surface rather than a delta.
* **`MAP_SURFACE_TO_OUTPUT`'s `outputOriginX/Y` were parsed and then discarded**; every
  surface was presented at (0, 0), and only the most recently mapped surface was tracked at
  all. Now every mapped surface is presented at its own origin.
* **`RESET_GRAPHICS` was printed and otherwise ignored.** It now updates the desktop size,
  invalidates the output mapping, and drops the RemoteFX Progressive per-tile baselines —
  which are indexed by tile grid position and are meaningless across a geometry change.
  This is the *only* place they are dropped; clearing them anywhere else breaks the
  legitimate cross-frame `RFX_TILE_DIFFERENCE` chain and would *cause* ghosting.
* **`DELETE_SURFACE`/`CREATE_SURFACE` left progressive baselines behind**, so a reused
  surface id inherited the dead surface's tiles and added later diff deltas onto unrelated
  pixels. Both now purge that surface's state.
* **One malformed PDU discarded its whole batch.** `split_pdus` returned `Err` for the
  entire message, so four good updates sharing a message with one bad one became five
  permanent artifacts. Framing is recoverable (`pduLength` is read before the body), so a
  body-level failure is now reported and skipped while its neighbours still apply. The same
  change was made to the RemoteFX Progressive tile and region loops.
* **Two parser panics** that would have killed the network thread and frozen the window:
  `SURFACE_TO_SURFACE` validated 12 bytes then indexed byte 13, and `SOLID_FILL` sliced
  `&body[pos..]` with an unchecked `pos`.
* **`u16` rect subtraction** (`right - left`) in four places would panic in debug and wrap to
  ~65535 in release on an inverted rect, driving a multi-gigabyte allocation. Now
  `RectU16::size()`, which returns `None` and reports.

### Second root cause: `RFX_TILE_DIFFERENCE` baselines did not accumulate

Found after the first fix, by recording a real session and replaying it. The per-tile
coefficient baseline was left untouched by a diff decode, so a *run* of difference tiles at
one grid position rendered `base+d1`, then `base+d2`, then `base+d3` — instead of
`base+d1`, `base+d1+d2`, `base+d1+d2+d3`.

FreeRDP's `progressive_rfx_dwt_2d_decode` is normative here:

```c
if (reverse)         memcpy(buffer, current, bsize);
else if (!coeffDiff) memcpy(current, buffer, bsize);
else                 prims->add_16s_inplace(buffer, current, belements);
```

and `add_16s_inplace` writes the sum into **both** arguments (`pSrcDst1[x] = pSrcDst2[x] =
(INT16)k`), i.e. the baseline advances to the accumulated total on every diff tile. Now
mirrored exactly by `progressive::accumulate_diff`, with unit tests for the chaining and for
the `INT16` wrap.

**And the dequant-shift-mismatch skip was wrong and has been removed.** Issue 3 below left
this as an open lead; it is now closed. Dequantization is exactly what maps quantized
coefficients into the shared *absolute* coefficient domain, so a delta and a baseline
dequantized with different shifts are already on the same scale — adding them is correct, and
FreeRDP has no such check anywhere in this path. The earlier reasoning ("adds numbers on
different numeric scales") had it backwards. That check was discarding roughly a third of all
diff tiles (~1,000–2,000 in a 2½-minute session); the smearing it appeared to fix was really
the missing accumulation above.

Measured on the recorded session (`replay` of 1,616 frames, 4,747 ClearCodec + 542
Progressive PDUs): **4.6% of the final frame's pixels changed**, and the diff image shows the
change confined exactly to the previously-ghosted regions — mail-list rows, the reply-button
strip, sidebar icons, the taskbar clock — with the rest bit-identical.

### Third fix: `TILE_UPGRADE` implemented

The first two fixes removed all *wrong* content, but the instrumentation showed 4,000–8,000
`TILE_UPGRADE` blocks per session still being counted and thrown away. Each one refines an
already-decoded tile by adding lower-order bit planes, so skipping them froze tiles at their
first-pass quality.

Implemented in `decode_tile_upgrade` / `UpgradeState`, per MS-RDPEGFX §2.2.4.4.3 and the
[Progressive Entropy Encode and Decode](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/b069be72-7bfd-428a-9501-2a85022c2367)
description of SRL:

* Per tile we now persist what FreeRDP keeps per `RFX_PROGRESSIVE_TILE`: the accumulated
  coefficients (`current`), the pre-dequantization RLGR output (`sign`, which says which
  coefficients are significant and with what sign), and the per-subband bit position.
* `bitPos = quant + quantProg` (no `-1`), `numBits = oldBitPos - newBitPos`,
  `shift = newBitPos - 1`, and each refined coefficient gets `+= input << shift`.
* Coefficients already significant read `numBits` refinement bits from the **raw** stream;
  ones still zero read from the **SRL** stream, whose adaptive run-length coding can make
  them significant for the first time — and their sign is then remembered for later passes.

**Validation, in order of strength:**

1. A header-layout self-check: the six SRL/raw stream lengths must sum exactly to the bytes
   remaining in the block. This passed on every upgrade block in a 1,616-frame session, which
   is strong evidence the field layout is right — a one-byte error would fail immediately
   rather than decode plausible garbage.
2. Zero orphaned upgrades and zero bit-position underflows (`checked_sub` bails if a pass
   would *raise* a subband's bit position), so the per-tile state keying and arithmetic hold.
3. 5.6% of the final frame changed with a **median channel delta of 5** — the signature of a
   refinement, not a broken decode, which would show large random deltas.
4. Visually: the taskbar icons went from chroma-banded (green fringing on the OpenAI mark, a
   green blob on Teams, blotchy background) to clean and correctly saturated. Body text is
   bit-identical, as expected for regions already at full quality.
5. Unit tests for the SRL state machine (adaptive zero runs and `kp` adaptation, unary
   magnitude, magnitude saturation at `(1 << numBits) - 1`), raw refinement of positive and
   negative coefficients, the no-op when `numBits == 0`, and that both subband layouts tile
   all 4096 coefficients.

Note: FreeRDP hardcodes the *extrapolate* subband layout in its upgrade path. This
implementation selects the layout from the region's `RFX_DWT_REDUCE_EXTRAPOLATE` flag, which
is identical whenever the flag is set (always, on this host) and correct rather than
misaligned when it is not.

### Fourth fix: ClearCodec glyph slots are keyed by pixel count, not shape

Found by the instrumentation during a second live session — two `gfx-error` lines in 1,648
frames:

```
ClearCodec glyph cache hit for index 329 has the wrong size (8x1, want 1x8)
ClearCodec glyph cache hit for index 254 has the wrong size (4x9, want 6x6)
```

8x1 and 1x8 are both 8 pixels; 4x9 and 6x6 are both 36. Equal counts every time. Per
FreeRDP's `clear.c`, a glyph slot records only its pixel **count**; a hit is valid whenever
`nWidth * nHeight <= count`, and the cached buffer is then re-read at the *requested*
dimensions (source stride `nWidth * bpp`). This host reuses slots by pixel count and reshapes
them, which is legal and expected.

Requiring the dimensions to match rejected exactly those glyphs, leaving a small rect
permanently unpainted. Fixed to store the flat buffer and validate only the count. Note this
content was being dropped **before** this session's work as well — the old code skipped the
mismatch silently. The change from a silent skip to a reported failure is what surfaced it,
which is the argument for keeping `gfx_error!` and `RDP_DEBUG_STRICT` permanently.

Verified by replaying the capture that produced the errors: `dropped_updates=0`.

### Review finding: upgrade passes must be all-or-nothing

`decode_tile_upgrade` originally resolved each component's bit-position arithmetic inside the
per-component loop, so a failure on Cb would leave Y already refined and the tile's bit
positions inconsistent with its coefficients. Nothing redraws a tile in that state, so every
later difference and upgrade pass at that grid position would compound the damage — a
permanent artifact of exactly the kind this work exists to remove. The arithmetic for all
three components is now resolved before the first mutation. Confirmed a no-op on real
traffic: the pinned capture's digest is unchanged.

### Deliverable 1 — swallowed errors found in the graphics path

Every item below silently discarded a server update. All are now reported through
`gfx_error!` with operation, surface id, frame id and rect, and abort the process under
`RDP_DEBUG_STRICT=1` (a runtime check, not a `debug_assert!` — the Progressive decoder is too
slow in debug builds to reproduce anything, so strict mode has to work in `--release`).

| # | Site (pre-fix) | What it hid |
|---|---|---|
| 1 | `main.rs:600` `if let Some(src) = surfaces.get(...)` / `if let Some(dst) = ...` | **The root cause.** Whole `SurfaceToSurface` dropped when the (misparsed) destination surface id did not exist. No log at all. |
| 2 | `main.rs:537`, `main.rs:571` `if let Some(surf) = surfaces.get_mut(w.surface_id)` | `WIRE_TO_SURFACE_1`/`_2` targeting an unknown surface — an entire decoded bitmap discarded, no log. |
| 3 | `main.rs:585` `if let Some(surf) = ...` in `SolidFill` | Same, for fills. |
| 4 | `main.rs:513` `Err(TrySendError::Full(_)) => {}` | The newest frame snapshot, dropped at every burst tail. |
| 5 | `main.rs:548`, `main.rs:579` `println!("... decode failed")` | ClearCodec / Progressive decode failures reported to stdout with no rect, surface or frame, then execution continued as if applied. |
| 6 | `main.rs:559` `other => println!("(codec not yet implemented, skipping)")` | An unimplemented codec on a real update. |
| 7 | `main.rs:624` `GfxPdu::Other { cmd_id }` | Every unhandled RDPGFX PDU, printed as a bare hex number. |
| 8 | `surface.rs:78,86` `break` in `blit_rect`'s row/col loops | A blit clipped at the surface edge silently lost the remainder of the rect. |
| 9 | `surface.rs:36,41` `break` in `fill_rect` | Same for fills. |
| 10 | `surface.rs:63` `break` in `extract_rect` | A source rect past the bottom edge returned **zero-filled (black)** rows, which `blit_rect` then painted as real black over real content elsewhere. |
| 11 | `surface.rs:144` `let Some(cached) = ... else { return false }` | Cache slot miss vs. missing destination surface were indistinguishable; both returned `false`. |
| 12 | `clearcodec.rs:169` glyph-cache hit falls through to `return Ok(())` | A glyph cache miss or size mismatch drew nothing and reported success. |
| 13 | `clearcodec.rs:450` `if i + 2 >= bgr.len() { continue }` | A short decoded tile silently left pixels unwritten. |
| 14 | `progressive.rs` `decode_tile(...)?` inside the region loop | One bad tile discarded every already-decoded tile in that region. |
| 15 | `progressive.rs:800,815` diff-tile skips | Shift-mismatched and baseline-less `RFX_TILE_DIFFERENCE` tiles, logged only at powers of two. |
| 16 | `progressive.rs:947` `WBT_TILE_UPGRADE` | Refinement passes counted, never drawn. |
| 17 | `progressive.rs:1005` `_ => {}` | Unknown Progressive block types. |
| 18 | `main.rs:97` `if ch != drdynvc { continue }` and `recv_dvc_data`'s implicit drop | **Every** base I/O-channel message during the graphics loop — legacy Bitmap Updates, Deactivate All, Set Error Info — discarded with no counter. This is what made "this host never sends legacy bitmap updates" an assumption rather than an observation. Now counted and reported by `ChannelRouter::note_skipped_io`. |
| 19 | `capabilities.rs:105,245` doc comments | Claimed `bitmap::parse_update` was a "safety net" for the drawing orders we over-claim. It has **zero callers**; the entire `bitmap` module is dead code. Comment corrected. |

Deliberately left as-is: `progressive.rs`'s `rlgr1_decode` zero-padding a short bitstream and
`read_bits`' `unwrap_or(0)` both match FreeRDP's reference behaviour, and ClearCodec's
V-Bar cache-miss fallback must stay (see Theory 1 under Issue 2 — restoring a hard failure
there reintroduces a self-amplifying cache-desync cascade).

### Deliverable 2 — debug modes, and which failure mode the tint revealed

Four permanent flags, documented in `client/src/debug.rs`:

| env var | effect |
|---|---|
| `RDP_DEBUG_TINT=1` | tints every region by the frame that last wrote it, 16 hues, ~15% blend |
| `RDP_DEBUG_RECTS=1` | 1px outline + frame id on every applied update rect, fading over 30 frames |
| `RDP_DEBUG_STRICT=1` | every swallowed graphics failure becomes fatal |
| `RDP_DEBUG_TRACE=1` | per-PDU trace of every graphics operation |

The tint is stored in a parallel per-pixel tag plane (`Surface::tags`) written by *every*
surface write path — ClearCodec, Progressive, SolidFill, SurfaceToSurface, CacheToSurface all
funnel through `Surface`'s primitives — and blended in only at presentation. Baking it into
the pixels would compound on each cache-to-surface copy and poison the bitmap cache with
tinted source pixels.

**Which failure mode: stale hue in the right place — the update never arrived at the
framebuffer.** The ghosted regions were never written by any recent frame, so their tag plane
still held an old frame's tag while everything around them advanced. That pointed at the
receive/apply path rather than the blit math, and the two dominant causes were both there:
a `SurfaceToSurface` that was discarded before it ever reached a surface, and a completed
frame that was discarded before it ever reached the window. The secondary
"correct-hue-in-the-wrong-place" reports are the same root cause on the occasions the
misparsed destination id happened to name a live surface.

**Confirmed against the live host 2026-08-14.** A 2½-minute real session (1,616 frames)
produced **zero** `gfx-error` lines — no dropped updates, no parse failures, no clipped
writes, no unhandled PDUs. That negative result was itself the decisive clue: the artifacts
that remained could not be anything the instrumentation watches, which pointed straight at
the two deferred paths that bypass it (`TILE_UPGRADE` and the diff-tile skip), and led to the
second root cause above. Recording the session and replaying it offline — 1,616 frames in
1.6 s — is what made iterating on it practical.

### Deliverable 3 — record/replay harness

* `RDP_RECORD=<path>` dumps the raw inbound graphics-channel byte stream, captured *before*
  ZGFX decompression so the stateful history buffer replays too.
* `cargo run --bin replay -- <recording> --out-dir DIR [--frames] [--stop-after N]` replays it
  through the same `GfxState` the live session drives — no network, no timing — and dumps
  PPMs plus a digest and a dropped-update count.
* Fixtures, both driven by `client/tests/replay_regression.rs`:
  * `ghost-regions.rdpgfx` — synthetic, pins the PDU *layout* (surface-to-surface offsets,
    every destPt, cache snapshot semantics) with per-pixel assertions.
  * `live-outlook.rdpgfx` — **a real capture from the live host**, 178 frames trimmed to
    ~1.4 MB, pinned to an exact image digest. This one pins the *codecs*: ZGFX history across
    messages, ClearCodec, and the Progressive difference-accumulation chain. Verified
    sensitive to the accumulation fix (digest `886c0a26b0a1adf3` before, `9bbebc51e56547b0`
    after).
* The synthetic fixture **does reproduce the parser bug**: against the pre-fix parser
  the test fails with `dropped_updates=1` and
  `[gfx-error] op=SURFACE_TO_SURFACE surface=1 frame=2 rect=(1,0)-(0,64) :: inverted rectSrc`
  — where `1` is the destination surface id and `64` the real `right`, the two-byte shift
  visible directly in the diagnostic.

### Deliverable 4 — capability bisect

Switches added (`capabilities::bisect`): `RDP_BISECT_NO_BITMAP_CACHE`,
`RDP_BISECT_NO_GLYPH_CACHE`, `RDP_BISECT_NO_OFFSCREEN_CACHE`, `RDP_BISECT_ZERO_ORDERS`,
`RDP_BISECT_NO_EGFX`. **The bisect was not run** — it needs the live host. It should no longer
be necessary for this artifact (the cause is identified and fixture-reproduced), but the
switches stay for the next one.

What was settled by reading the code rather than by bisecting:

1. **Persistent bitmap cache — not applicable, and not a lie.** Only the Bitmap Cache *Rev2*
   set (type 19) carries `PERSISTENT_KEYS_EXPECTED_FLAG`; this client sends *Rev1* (type 4)
   and never Rev2. No Persistent Key List PDU exists anywhere in the crate. The "client
   advertises cache keys for bitmaps it does not hold" scenario cannot occur here.
2. **Bitmap cache rev1/rev2** — Rev1 sent with all six cell counts zero; Rev2 never sent.
3. **Offscreen cache** — `offscreenSupportLevel=0`, size 0, entries 0. Honest.
4. **EGFX** — the only graphics path this host uses.
5. **The one real over-claim is the Order Capability Set**: `build_confirm_active` echoes the
   server's `orderFlags` and 32-byte `orderSupport` back verbatim while the client implements
   no drawing order at all. This is a deliberate, empirically-forced choice (a real Windows 11
   24H2 host rejects an all-zero `orderSupport` with `ERRINFO_BADCAPABILITIES`), but the
   comment justifying it was wrong and is now corrected, and `RDP_BISECT_ZERO_ORDERS=1` exists
   to test the alternative. Orders would arrive on the base I/O channel, which the graphics
   loop does not read — now counted (item 18 above) so this is measurable.

### Deliverable 5 — Phase 3 per-item verdicts

| Item | Verdict |
|---|---|
| **3.1** Shadow framebuffer / double buffering | **CORRECT (architecture) — INCORRECT (delivery), fixed.** `Surface` is a persistent full-size source of truth, `window::framebuffer` is a persistent full-resolution shadow, and `present()` copies *all* of it — no dirty-rect upload into an acquired image, so the frame-N-2 failure mode cannot occur. (`softbuffer`, not `wgpu`.) The defect was in *delivery*: the `sync_channel(1)` dropped the newest snapshot. `present()` also now copies row-wise with clipping and handles `WindowEvent::Resized`, so a window whose size the WM overrode cannot panic or present a misaligned image. |
| **3.2** `TS_BITMAP_DATA` coordinate math | **NOT APPLICABLE.** This host never sends legacy slow-path Bitmap Updates; `bitmap::parse_update` has zero callers and the whole module is dead code. The inclusive-`destRight`/padded-width/`cbScanWidth`/`bitmapComprHdr`/bottom-up checks all concern that dead path. Note the *live* path uses `RDPGFX_RECT16`, whose `right`/`bottom` are **exclusive** — the opposite convention — now encoded once in `RectU16::size()` with a test, rather than open-coded at each use. Recommend deleting `bitmap.rs` or wiring it up; keeping dead code that comments claim is a safety net is how item 19 happened. |
| **3.3** RFX tile origin + bounds | **INCORRECT, fixed.** Tiles were placed at `xIdx*64` with no enclosing-command origin — correct only because `WIRE_TO_SURFACE_2` always has a zero origin, and silently wrong for a CAProgressive `WIRE_TO_SURFACE_1` (which was not handled at all). `decompress_at` now adds the origin to **both** the tile position and the region's clipping rects, matching FreeRDP's `progressive_decompress`. Out-of-range tiles are **rejected and counted**, not clamped. No desktop-sized bound is ever passed for a tile-sized buffer; `blit_rect` is bounds-checked independently. |
| **3.4** Progressive codec state | **INCORRECT, now fixed in full.** Upgrade passes are implemented and locate the existing per-tile entry rather than creating a new one; difference tiles accumulate. Per-tile baselines are keyed `(surfaceId, xIdx, yIdx)` and survive across frames — correct. They were **never** cleared, so a reused surface id inherited a dead surface's tiles: now purged on `DELETE_SURFACE` and `CREATE_SURFACE`, and cleared wholesale on `RESET_GRAPHICS` **only**. `DELETE_ENCODING_CONTEXT` is now parsed and counted instead of falling into `Other`; it deliberately drops no state, matching FreeRDP, whose handler is also a no-op — this PDU fires ~80×/session and dropping baselines on it would break the legitimate diff chain. |
| **3.5** EGFX PDUs | **`CACHE_TO_SURFACE` CORRECT** — always looped every `destPts` entry (now covered by a test). **`SURFACE_TO_SURFACE` INCORRECT** — the root cause; src/dst were not semantically transposed, the *byte offsets* were wrong. Overlap handling was already correct (extract to an owned buffer first) and now has a test. **Cache lifecycle CORRECT** — `CACHE_IMPORT_OFFER` is never sent, so no slot is ever claimed that we cannot back; `EVICT_CACHE_ENTRY` handled. **`FRAME_ACKNOWLEDGE` CORRECT** — one per `END_FRAME`, right field order, `queueDepth=0`, monotonic `totalFramesDecoded`. **Surface lifecycle INCORRECT, fixed** — deleting a mapped surface left a dangling output mapping, `RESET_GRAPHICS` was ignored, and writes to unknown surfaces were silent. |
| **3.6** Suppress Output | **NOT APPLICABLE — verified clean.** `TS_SUPPRESS_OUTPUT_PDU` is never sent: `build_suppress_output_allow` has zero callers and only ever sets `allowDisplayUpdates=TRUE`. We advertise `suppressOutputSupport=0`. Nothing here suppresses updates. (Note we also advertise `refreshRectSupport=0`, so the manual Refresh Rect diagnostic is unavailable on this host without changing negotiation first — worth knowing before reaching for it.) |

### Deliverable 6 — tests

39 tests, all passing. Coordinate math is covered as pure functions with hand-written
fixtures: exclusive-vs-inclusive rect conversion and inverted-rect rejection
(`gfx::tests`), the `SURFACE_TO_SURFACE` byte layout against the spec, RFX tile origin
offsetting with and without an enclosing origin, region-rect clipping, out-of-range tile
rejection, `extract_rect` row-wrap, overlapping same-surface copy, every-destPt stamping,
latest-wins frame delivery vs. the bounded-channel behaviour it replaced, and the replay
fixture end to end.

---

# Earlier issues (as of 2026-08-13)

Status: paused mid-investigation at the user's request. This documents what's confirmed,
what's ruled out, and what to try next, so a future session doesn't have to re-derive it.

The client is functional end-to-end (auth, connection, GFX pipeline, live window, real
interactive input all work — see `PLAN.md` §9), but real-world desktop content still
renders with two distinct, separate visual defects. Fixing both is required before this
is a usable replacement for a normal RDP client.

*(screenshot `black-blocks-and-garbled-text.png` — not committed: it shows live remote-desktop content. Kept locally in `docs/screenshots/`, which is git-ignored. Caption: Both issues at once: large black rectangles (top and bottom bars) plus garbled/missing letters in the sidebar text)*

## Issue 1: large solid black blocks AND stale/ghost content bleed-through — root cause known, fix not yet built

Large regions of real application UI (browser tab bars, status bars, some panel
backgrounds) render as solid black instead of their actual content. See the top and
bottom bars in the screenshot above.

**Fuller picture found during 2026-08-13 live-host testing** (after moving windows/switching
focus for a while): the visual impact isn't only solid black. Regions that were drawn once but
should later be *replaced* with new content instead keep showing their old, now-wrong content —
overlapping fragments of previously-focused windows (an old Teams sidebar, old browser chrome)
frozen in place under/around whatever's currently on top. This follows directly from the same
root cause below: every `RDPGFX_WIRE_TO_SURFACE_PDU_2` is dropped outright, so a region either
never got drawn (stays at the surface's zero-initialized black) or got drawn once and never
updated again (stays on stale content) — 660 of these PDUs were received and silently skipped in
one normal-length interactive session, so this is the dominant unimplemented-content path, not
an edge case.

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
event loop the same way `WireToSurface1` is handled.

**Status 2026-08-14: implemented (`client/src/progressive.rs`), verified against the real
host, black blocks eliminated.** Full RLGR1 + differential LL3 decode + dequant + 3-level
inverse DWT (both the plain and `RFX_DWT_REDUCE_EXTRAPOLATE` variants — the host turned out
to use extrapolate mode for the *majority* of traffic, not the rare case the initial research
assumed) + YCbCr→RGB, all cross-checked byte-exact against FreeRDP's `progressive.c`/
`rfx_rlgr.c`/`rfx_dwt.c`/`prim_colors.c`. Deliberately still deferred: `TILE_UPGRADE`
(bit-plane quality refinement, counted not drawn) and `RFX_TILE_DIFFERENCE` tiles whose
dequant shift doesn't match their cached baseline's (see Issue 2 below — skipped rather than
corrupting the surface). Performance note: this is numerically heavy (RLGR1 bit-by-bit
decode, per-tile IDWT) — **debug builds (`cargo run` without `--release`) are 10-50x too
slow** to keep up with fast-changing content (grid scrolling, animation) and will misleadingly
look like a rendering bug; always test with `cargo run --release`.

## Issue 2: sporadic missing/wrong individual letters within otherwise-correct text — FIXED and verified 2026-08-13

**Root cause found** by diffing `client/src/clearcodec.rs`'s `decode_bands` line-by-line against
FreeRDP's reference implementation (`libfreerdp/codec/clear.c`, `clear_decompress_bands_data`).
Two real bugs, the first almost certainly the primary cause:

1. **Missing V-Bar-cache cursor advance on `SHORT_VBAR_CACHE_HIT`.** Per FreeRDP, both
   `SHORT_VBAR_CACHE_HIT` (header `0x4000`) and `SHORT_VBAR_CACHE_MISS` are "vBarUpdate" events:
   after composing the full-height V-Bar (background + cached/inline short run + background),
   the result is written into V-Bar Storage at the current cursor and the cursor advances. This
   client's `decode_bands` only did that in the miss branch — a `SHORT_VBAR_CACHE_HIT` composed
   its result and returned without ever touching `vbar_storage`/`vbar_cursor`. Every cache-hit
   event this client's cursor silently fell one slot further behind the server's, so every later
   *absolute-indexed* `VBAR_CACHE_HIT` (header `0x8000`, `vBarIndex = header & 0x7FFF`) resolved
   to the wrong slot for the rest of the connection — plausible-looking but wrong pixels, no
   error raised anywhere. This drifts continuously from the very first `SHORT_VBAR_CACHE_HIT` in
   a session (not tied to reconnects), which fits both: the "individual, scattered, no-error"
   symptom, and why Theory 2 (reconnect drift) was correctly ruled out — the real drift source
   isn't reconnects at all.
2. **No resize on `VBAR_CACHE_HIT` height mismatch.** FreeRDP's `resize_vbar_entry` explicitly
   handles a cached V-Bar whose stored length differs from the current band's height (logged as
   an error condition in FreeRDP, meaning it's a real, expected occurrence — the 32768-slot cache
   gets reused across bands of different pixel heights over a session) by resizing: truncating if
   the cached entry is longer, zero-extending if shorter. This client used the cached entry
   verbatim regardless of length, so a too-short cached entry silently left some rows of the
   column unpainted (default black in the scratch tile) instead of erroring — another
   "structurally valid, wrong pixel value" case.

**Fix applied**: `decode_bands`'s `SHORT_VBAR_CACHE_HIT` branch now stores its composed result
into `vbar_storage[vbar_cursor]` and advances the cursor, matching the miss branch. The
`VBAR_CACHE_HIT` branch now resizes (truncate/zero-extend) a length-mismatched cached entry
instead of using it as-is. Verified against FreeRDP's `clear.c` for every other decode path in
this file (residual RLE, RLEX subcodec, run-length escalation, glyph cache, seqNumber handling,
raw subcodec) — no further discrepancies found.

**Verified against the real host same day**: ran a full session (`cargo run -- <host> --aad-op-item ...`), 1,300 frames / 4,134 `WireToSurface1` (ClearCodec) PDUs decoded, including
heavy real interaction (windows moved, focus changed repeatedly, multiple apps: Dynamics-style
web app, Teams chat, browser). **Zero** `ClearCodec decode failed` errors. Visually confirmed via
screenshots: every piece of legible text checked — sidebar labels, chat messages, browser UI,
dialog text — rendered with no missing/wrong letters, including text that only appeared after
interaction (not just first-load content). Issue confirmed fixed.

### Original investigation notes (background/history — read above for the current status)

Real text (menu labels, list items, document content) mostly renders legibly, but
individual characters or short spans within words are randomly missing or wrong — e.g.
"Receivables" rendering as "ceivables", "Purchases" as "Purch ees". Not the same defect as
Issue 1 — this happens in regions that ARE getting real content, just with per-character
corruption, and no full black rectangles.

*(screenshot `missing-letters-in-text.png` — not committed: it shows live remote-desktop content. Kept locally in `docs/screenshots/`, which is git-ignored. Caption: Sidebar and content text with individual missing letters — "ceivables" instead of "Receivables", "Rec ivables" in the sidebar, etc. — while most of each word is correct)*

*(screenshot `fresh-session-still-corrupted.png` — not committed: it shows live remote-desktop content. Kept locally in `docs/screenshots/`, which is git-ignored. Caption: The same corruption pattern on a completely fresh RDS session (server-side session logged off and reconnected), ruling out accumulated-reconnect-state as the cause)*

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
   day) to the same long-lived user session could cause the gap between the
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

## Issue 3: Progressive "ghost chunks" — stale/wrong content in the wrong spot, mostly fixed 2026-08-14, one open lead remains

Distinct from Issues 1/2 (both otherwise resolved as of this session): after the RemoteFX
Progressive decoder went in, black blocks were gone but fragments of unrelated/stale content
would appear overlapping current content — most visible during fast-changing content (grid
scrolling in Excel/browser tables, window switching). Went through several wrong turns before
landing on the real cause; recorded here so the wrong turns aren't retried.

**Wrong turn 1 — blitting the full 64×64 tile regardless of the region's declared rects.**
`RFX_PROGRESSIVE_REGION.rects` isn't just a damage-tracking hint (as originally assumed) — it
genuinely clips what gets composited (confirmed against FreeRDP's `update_tiles` in
`progressive.c`, which intersects each tile's footprint against `clippingRects` before
calling `freerdp_image_copy_no_overlap`). Fixed by clipping each freshly-decoded tile's blit
to the union of its region's rects.

**Wrong turn 2 — recompositing from a whole-session Progressive tile-pixel cache.** To handle
a rect exposing an area whose content hasn't changed (so no fresh tile is resent), tried
caching every decoded tile's pixels keyed by grid position for the whole session, and
recompositing from that cache whenever a later rect covered the same position (matching what
`update_tiles` appears to do with `surface->numUpdatedTiles`, which never resets). This
**caused a worse regression**: this decoder's Progressive cache has no visibility into
non-Progressive draws (ClearCodec draws text/UI directly to the shared `Surface`, bypassing
this cache entirely), so an old cached Progressive tile could get resurrected and clobber
genuinely newer content drawn by a different codec — confirmed by the actual symptom
(unrelated UI fragments — a jump list, a search box placeholder — appearing overlapped in the
wrong place, not just staleness). Reverted to a same-region-only tile list (cleared and
refilled per region, never resurrected across regions/frames) — strictly safe since it can
only ever draw what the current region itself just decoded, clipped to what that same region
declared as changed. Net effect versus wrong-turn-1: same, since real traffic on this host
doesn't seem to need the cross-region case in practice.

**Root cause (confirmed, ~50% of real traffic) — `RFX_TILE_DIFFERENCE` tiles applied with a
mismatched dequant shift.** Added diagnostic logging comparing a diff tile's dequant shift
(`quant + quantProg - 1` per subband) against the shift used when its cached baseline was
established. **~40-50% of all diff-tile applications on this host have a mismatch** —
confirmed via direct log output, e.g. baseline `hl1=8` vs a later diff tile's `hl1=9` for the
same grid position, consistently across many tiles. Adding two coefficient sets dequantized
with different shifts adds numbers on different scales, not a meaningful delta — producing
exactly the smeared/doubled visual artifact reported. FreeRDP has a quant-consistency check
(`progressive_rfx_quant_cmp_equal`) but it only exists for the separate `TILE_UPGRADE`
bit-plane path, and even there it only *warns* rather than correcting — no evidence this is
meant to be silently tolerated for a plain coefficient add in the `TILE_FIRST`/`TILE_SIMPLE`
diff path. **Fix**: skip (don't draw) a diff tile when its shift doesn't match its cached
baseline's, rather than applying a numerically-wrong addition. Verified via live host testing:
doubled/overlapping text gone; remaining artifacts are thin stale strips (tiles that got
skipped, waiting for a future full redraw) rather than active wrong-content corruption — a
real but much less severe residual, and the strictly safer failure mode.

**RESOLVED 2026-08-14 — see Issue 4.** The skip was wrong: dequantization is what puts a
delta and its baseline on the same absolute scale, so a shift difference is not a problem and
FreeRDP has no such check. The real defect was that the baseline never accumulated across a
run of diff tiles. Both are fixed; the skip is gone. Original note follows.

**Open lead, not yet investigated**: *why* does this server send a different quant/quality for
a diff tile than its baseline roughly half the time? If it's legitimate (TILE_FIRST's
`quality` field genuinely varies per-message for bandwidth reasons, independent of any
refinement intent), the current skip-on-mismatch is the correct permanent behavior. If it's
this client mis-indexing `region->quantProgVals[quality]` somehow (the lookup is a direct
array index by the raw `quality` byte, not a search — see `decode_tile` in `progressive.rs`),
that would be a client bug producing an *artificial* mismatch, and worth re-verifying against
a fresh wire capture with the mismatching tile's raw bytes hand-decoded, the same way the
ZGFX/NSCodec/ClearCodec bugs earlier in this project were confirmed.

See also: `phase2_gfx_pipeline.md` memory file for the byte-exact RemoteFX Progressive wire
format, and `client/src/progressive.rs`'s module-level doc comment for the full list of
deliberately-deferred paths (`TILE_UPGRADE`, `RFX_DWT_REDUCE_EXTRAPOLATE`'s edge cases).
