//! Deterministic replay regression tests for the graphics pipeline.
//!
//! RDP is a pure delta protocol: the server never resends a region, so a rendering defect is
//! permanent and only reproduces under the exact update sequence that caused it. That makes
//! a live session useless as a test. These tests instead build a graphics-channel byte
//! stream, write it out as a real `.rdpgfx` recording (identical in format to what
//! `RDP_RECORD=<path>` produces against the live host), and replay it through the same ZGFX
//! decompressor, PDU parser and surface compositor the client runs.
//!
//! The scenario in `ghost_regions_fixture` is the reproduction for the reported artifact:
//! window-chrome-shaped and horizontal-strip-shaped regions that keep showing content from
//! an earlier point in the session. It is built from the operations a Windows host actually
//! uses for that shape of update — `SURFACE_TO_SURFACE` (scroll optimisation and moving
//! window content) and `CACHE_TO_SURFACE` (repeated UI elements) — and it fails on a client
//! that mis-parses either of them.

use rdp_client::record::{self, RecordEntry};
use rdp_client::replay;
use std::path::PathBuf;

// ---------------------------------------------------------------------------------------
// Minimal RDPGFX stream builder. Deliberately hand-rolled rather than reusing the client's
// own writers: a test that encodes with the same code it decodes with cannot catch a byte
// layout error, which is exactly the class of bug this fixture exists to pin down.
// ---------------------------------------------------------------------------------------

fn pdu(cmd_id: u16, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&cmd_id.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes()); // flags
    v.extend_from_slice(&((8 + body.len()) as u32).to_le_bytes());
    v.extend_from_slice(body);
    v
}

/// Wraps raw PDU bytes in an uncompressed single-segment ZGFX container (descriptor 0xE0,
/// flags 0x00), which is what a server sends for incompressible data.
fn zgfx_uncompressed(payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0xE0, 0x00];
    v.extend_from_slice(payload);
    v
}

fn create_surface(id: u16, w: u16, h: u16) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&id.to_le_bytes());
    b.extend_from_slice(&w.to_le_bytes());
    b.extend_from_slice(&h.to_le_bytes());
    b.push(0x20); // GFX_PIXEL_FORMAT_XRGB_8888
    pdu(0x0009, &b)
}

fn map_surface_to_output(id: u16, x: u32, y: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&id.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // reserved
    b.extend_from_slice(&x.to_le_bytes());
    b.extend_from_slice(&y.to_le_bytes());
    pdu(0x000F, &b)
}

fn start_frame(frame_id: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_le_bytes()); // timestamp
    b.extend_from_slice(&frame_id.to_le_bytes());
    pdu(0x000B, &b)
}

fn end_frame(frame_id: u32) -> Vec<u8> {
    pdu(0x000C, &frame_id.to_le_bytes())
}

fn rect16(left: u16, top: u16, right: u16, bottom: u16) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&left.to_le_bytes());
    b.extend_from_slice(&top.to_le_bytes());
    b.extend_from_slice(&right.to_le_bytes());
    b.extend_from_slice(&bottom.to_le_bytes());
    b
}

fn solid_fill(surface: u16, bgr: [u8; 3], rects: &[(u16, u16, u16, u16)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&surface.to_le_bytes());
    b.extend_from_slice(&[bgr[0], bgr[1], bgr[2], 0x00]); // RDPGFX_COLOR32: B,G,R,XA
    b.extend_from_slice(&(rects.len() as u16).to_le_bytes());
    for &(l, t, r, bo) in rects {
        b.extend_from_slice(&rect16(l, t, r, bo));
    }
    pdu(0x0004, &b)
}

/// `RDPGFX_WIRE_TO_SURFACE_PDU_1` carrying raw uncompressed BGRX.
fn wire_to_surface_1_uncompressed(surface: u16, l: u16, t: u16, r: u16, bo: u16, bgr: [u8; 3]) -> Vec<u8> {
    let (w, h) = ((r - l) as usize, (bo - t) as usize);
    let mut pixels = Vec::with_capacity(w * h * 4);
    for _ in 0..w * h {
        pixels.extend_from_slice(&[bgr[0], bgr[1], bgr[2], 0xFF]);
    }
    let mut b = Vec::new();
    b.extend_from_slice(&surface.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // codecId: RDPGFX_CODECID_UNCOMPRESSED
    b.push(0x20); // pixelFormat
    b.extend_from_slice(&rect16(l, t, r, bo));
    b.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    b.extend_from_slice(&pixels);
    pdu(0x0001, &b)
}

/// `RDPGFX_SURFACE_TO_SURFACE_PDU`, MS-RDPEGFX §2.2.2.5:
/// surfaceIdSrc(2) surfaceIdDest(2) rectSrc(8) destPtsCount(2) destPts(4 each).
fn surface_to_surface(src: u16, dst: u16, rect: (u16, u16, u16, u16), dest_pts: &[(u16, u16)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&src.to_le_bytes());
    b.extend_from_slice(&dst.to_le_bytes());
    b.extend_from_slice(&rect16(rect.0, rect.1, rect.2, rect.3));
    b.extend_from_slice(&(dest_pts.len() as u16).to_le_bytes());
    for &(x, y) in dest_pts {
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
    }
    pdu(0x0005, &b)
}

/// `RDPGFX_SURFACE_TO_CACHE_PDU`: surfaceId(2) cacheKey(8) cacheSlot(2) rectSrc(8).
fn surface_to_cache(surface: u16, slot: u16, rect: (u16, u16, u16, u16)) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&surface.to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes()); // cacheKey (opaque)
    b.extend_from_slice(&slot.to_le_bytes());
    b.extend_from_slice(&rect16(rect.0, rect.1, rect.2, rect.3));
    pdu(0x0006, &b)
}

/// `RDPGFX_CACHE_TO_SURFACE_PDU`: cacheSlot(2) surfaceId(2) destPtsCount(2) destPts(4 each).
fn cache_to_surface(slot: u16, surface: u16, dest_pts: &[(u16, u16)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&slot.to_le_bytes());
    b.extend_from_slice(&surface.to_le_bytes());
    b.extend_from_slice(&(dest_pts.len() as u16).to_le_bytes());
    for &(x, y) in dest_pts {
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
    }
    pdu(0x0007, &b)
}

const SURFACE_W: u16 = 256;
const SURFACE_H: u16 = 128;
const BLUE: [u8; 3] = [200, 0, 0];
const RED: [u8; 3] = [0, 0, 200];
const GREEN: [u8; 3] = [0, 180, 0];

/// The reproduction. Every operation here is one a real Windows host emits for exactly the
/// shapes reported as ghosting: a strip copied sideways (scroll), a chrome-sized block
/// copied to two places (window move / repeated UI), and a cached block stamped after its
/// source has been overwritten.
fn ghost_regions_stream() -> Vec<Vec<u8>> {
    let mut messages = Vec::new();

    // Handshake: one surface, mapped at the output origin.
    let mut setup = Vec::new();
    setup.extend(create_surface(1, SURFACE_W, SURFACE_H));
    setup.extend(map_surface_to_output(1, 0, 0));
    messages.push(setup);

    // Frame 1: paint the whole surface blue, then a distinctive red 64x32 block at (0, 0).
    let mut f1 = Vec::new();
    f1.extend(start_frame(1));
    f1.extend(solid_fill(1, BLUE, &[(0, 0, SURFACE_W, SURFACE_H)]));
    f1.extend(wire_to_surface_1_uncompressed(1, 0, 0, 64, 32, RED));
    f1.extend(end_frame(1));
    messages.push(f1);

    // Frame 2: the scroll-optimisation case. Copy that block, within the same surface, to
    // two destination points. This is the operation whose PDU layout was mis-parsed: the
    // destination surface id was read from the source rect's `bottom` field, which named a
    // surface that does not exist, so the entire copy was dropped and both destinations kept
    // their old (blue) pixels forever.
    let mut f2 = Vec::new();
    f2.extend(start_frame(2));
    f2.extend(surface_to_surface(1, 1, (0, 0, 64, 32), &[(128, 0), (128, 64)]));
    f2.extend(end_frame(2));
    messages.push(f2);

    // Frame 3: cache the red block, overwrite its source with green, then stamp the cached
    // copy at two more points. Tests that CACHE_TO_SURFACE honours every destPt and that the
    // cache holds a snapshot rather than a live reference.
    let mut f3 = Vec::new();
    f3.extend(start_frame(3));
    f3.extend(surface_to_cache(1, 5, (0, 0, 64, 32)));
    f3.extend(solid_fill(1, GREEN, &[(0, 0, 64, 32)]));
    f3.extend(cache_to_surface(5, 1, &[(0, 96), (192, 96)]));
    f3.extend(end_frame(3));
    messages.push(f3);

    messages
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ghost-regions.rdpgfx")
}

/// Writes the fixture in the same on-disk format `RDP_RECORD=<path>` produces, so it can be
/// fed to the `replay` binary by hand:
/// `cargo run --bin replay -- client/tests/fixtures/ghost-regions.rdpgfx --out-dir /tmp/out`
fn write_fixture(path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut rec = record::Recorder::create(path).unwrap();
    rec.note(&format!("negotiated desktop {SURFACE_W}x{SURFACE_H}")).unwrap();
    rec.note("synthetic reproduction of strip/chrome-shaped ghost regions (see tests/replay_regression.rs)").unwrap();
    for msg in ghost_regions_stream() {
        rec.gfx_message(&zgfx_uncompressed(&msg)).unwrap();
    }
}

fn load_fixture() -> Vec<RecordEntry> {
    let path = fixture_path();
    if !path.exists() {
        write_fixture(&path);
    }
    record::read(&path).unwrap()
}

fn pixel(r: &replay::ReplayResult, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * r.width + x) * 4) as usize;
    [r.pixels[i], r.pixels[i + 1], r.pixels[i + 2]]
}

#[test]
fn ghost_regions_fixture_replays_with_every_region_in_the_right_place() {
    let entries = load_fixture();
    let desktop = replay::desktop_from_notes(&entries);
    assert_eq!(desktop, (SURFACE_W as u32, SURFACE_H as u32));

    let result = replay::replay(&entries, desktop, None, |_, _, _, _| {}).unwrap();
    assert_eq!(result.frames, 3);

    // Nothing may be silently discarded. On a delta protocol every dropped update is a
    // permanent artifact, so a clean replay must report zero.
    assert_eq!(result.dropped_updates, 0, "replay dropped updates: {}", result.summary);
    assert_eq!(result.parse_failures, 0, "replay failed to parse a PDU: {}", result.summary);

    // The two SurfaceToSurface destinations. These are the ghosts: with the source rect and
    // the destination surface id read at the wrong offsets, the copy targeted a nonexistent
    // surface and was dropped, leaving both of these blue for the rest of the session.
    assert_eq!(pixel(&result, 128, 0), RED, "SurfaceToSurface destPts[0] was not painted");
    assert_eq!(pixel(&result, 191, 31), RED, "SurfaceToSurface destPts[0] is the wrong size");
    assert_eq!(pixel(&result, 128, 64), RED, "SurfaceToSurface destPts[1] was not painted");
    assert_eq!(pixel(&result, 191, 95), RED, "SurfaceToSurface destPts[1] is the wrong size");

    // ...and nothing may spill outside them.
    assert_eq!(pixel(&result, 127, 0), BLUE, "SurfaceToSurface painted left of its destination");
    assert_eq!(pixel(&result, 192, 0), BLUE, "SurfaceToSurface painted right of its destination");
    assert_eq!(pixel(&result, 128, 32), BLUE, "SurfaceToSurface painted below its destination");

    // Both CacheToSurface destination points, and the cache holding a snapshot rather than a
    // live view of a source that has since been overwritten green.
    assert_eq!(pixel(&result, 0, 96), RED, "CacheToSurface destPts[0] was not painted");
    assert_eq!(pixel(&result, 192, 96), RED, "CacheToSurface destPts[1] was not painted — only the first point was handled");
    assert_eq!(pixel(&result, 0, 0), GREEN, "the cached block's source should have been overwritten");
}

/// Codec-level regression against a *real* recorded session.
///
/// No such recording is committed, and none ever should be: a capture is the remote
/// desktop's pixels, so a real one contains whatever was on screen — mail, customer names,
/// account identifiers. `RDP_RECORD=` output is git-ignored for that reason.
///
/// To use this locally, drop a capture at the path below and set the expected digest from
/// the first run. It pins what the synthetic fixture cannot: ZGFX history across messages,
/// ClearCodec, and both stateful RemoteFX Progressive paths (`RFX_TILE_DIFFERENCE`
/// accumulation and `TILE_UPGRADE` refinement). Prefer capturing a session showing nothing
/// confidential — an empty desktop exercises the same code paths.
#[test]
fn a_local_recorded_session_replays_to_a_stable_reference() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/local-capture.rdpgfx");
    if !path.exists() {
        eprintln!("skipping: no local capture at {} (see this test's doc comment)", path.display());
        return;
    }
    let a = replay::replay_file(&path).unwrap();
    let b = replay::replay_file(&path).unwrap();
    assert_eq!(a.digest(), b.digest(), "replay must be deterministic");
    assert_eq!(a.parse_failures, 0, "{}", a.summary);
    assert_eq!(a.dropped_updates, 0, "real traffic must not drop updates: {}", a.summary);
}

#[test]
fn replay_is_deterministic() {
    let entries = load_fixture();
    let desktop = replay::desktop_from_notes(&entries);
    let a = replay::replay(&entries, desktop, None, |_, _, _, _| {}).unwrap();
    let b = replay::replay(&entries, desktop, None, |_, _, _, _| {}).unwrap();
    assert_eq!(a.digest(), b.digest(), "the same recording must always produce the same image");
}

#[test]
fn stopping_early_shows_the_state_before_the_copy() {
    // Bisecting an artifact means being able to ask "what did the screen look like at frame
    // N?". After frame 1 the copy has not happened yet, so the destinations are still blue —
    // which is also precisely what the broken build showed *after* frame 2.
    let entries = load_fixture();
    let desktop = replay::desktop_from_notes(&entries);
    let result = replay::replay(&entries, desktop, Some(1), |_, _, _, _| {}).unwrap();
    assert_eq!(result.frames, 1);
    assert_eq!(pixel(&result, 0, 0), RED);
    assert_eq!(pixel(&result, 128, 0), BLUE);
}

#[test]
fn the_committed_fixture_matches_the_stream_this_test_describes() {
    // Guards against the fixture and the scenario drifting apart: if someone edits the
    // builder above without regenerating the file, the on-disk fixture would silently keep
    // testing the old scenario.
    let entries = load_fixture();
    let recorded: Vec<&Vec<u8>> = entries
        .iter()
        .filter_map(|e| match e {
            RecordEntry::GfxMessage(m) => Some(m),
            _ => None,
        })
        .collect();
    let expected: Vec<Vec<u8>> = ghost_regions_stream().iter().map(|m| zgfx_uncompressed(m)).collect();
    assert_eq!(recorded.len(), expected.len(), "fixture message count drifted — delete it and re-run to regenerate");
    for (i, (got, want)) in recorded.iter().zip(expected.iter()).enumerate() {
        assert_eq!(got.as_slice(), want.as_slice(), "fixture message {i} drifted — delete it and re-run to regenerate");
    }
}
