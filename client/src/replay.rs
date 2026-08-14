//! Deterministic replay of a recorded graphics stream through the live decode pipeline.
//!
//! This lives in the library, not in the `replay` binary, so the integration tests can call
//! it directly — a regression fixture is only meaningful if the test exercises the same code
//! path the binary and the live session do.

use crate::gfxstate::{GfxAction, GfxState};
use crate::record::{self, RecordEntry};
use crate::zgfx;
use anyhow::{Context, Result};
use std::path::Path;

/// The result of replaying a recording: the final composited desktop image plus everything
/// that went wrong on the way.
pub struct ReplayResult {
    pub width: u32,
    pub height: u32,
    /// BGRX8888, row-major, desktop-sized.
    pub pixels: Vec<u8>,
    pub frames: u32,
    pub summary: String,
    pub dropped_updates: u64,
    pub parse_failures: u64,
}

impl ReplayResult {
    pub fn digest(&self) -> u64 {
        record::image_digest(&self.pixels)
    }
}

/// Replays the entries of a recording through a fresh `GfxState`.
///
/// `on_frame(frame_number, width, height, pixels)` is called after every END_FRAME, which is
/// what makes per-frame dumps and "stop at frame N" bisection possible.
pub fn replay(
    entries: &[RecordEntry],
    desktop: (u32, u32),
    stop_after: Option<u32>,
    mut on_frame: impl FnMut(u32, u32, u32, &[u8]),
) -> Result<ReplayResult> {
    let mut zgfx = zgfx::ZgfxContext::new();
    let mut state = GfxState::new(desktop.0, desktop.1);
    // The desktop-sized composite of every mapped surface, kept across frames for the same
    // reason the live client keeps its framebuffer: it is the single source of truth, and
    // each present refreshes all of it rather than patching dirty rects into a buffer that
    // still holds an older frame's pixels.
    let (mut fb_w, mut fb_h) = desktop;
    let mut framebuffer = vec![0u8; (fb_w as usize) * (fb_h as usize) * 4];
    let mut frames = 0u32;

    'outer: for entry in entries {
        let RecordEntry::GfxMessage(raw) = entry else { continue };
        let msg = zgfx
            .decompress(raw)
            .context("ZGFX decompression failed — the history buffer is now desynchronised, so nothing after this is meaningful")?;
        for action in state.apply_message(&msg)? {
            let GfxAction::EndFrame { .. } = action;
            frames += 1;
            let (dw, dh) = state.desktop_size();
            if (dw, dh) != (fb_w, fb_h) {
                fb_w = dw;
                fb_h = dh;
                framebuffer = vec![0u8; (fb_w as usize) * (fb_h as usize) * 4];
            }
            for tile in state.present_tiles() {
                blit_into(&mut framebuffer, fb_w, fb_h, &tile);
            }
            on_frame(frames, fb_w, fb_h, &framebuffer);
            if stop_after == Some(frames) {
                break 'outer;
            }
        }
    }
    Ok(ReplayResult {
        width: fb_w,
        height: fb_h,
        pixels: framebuffer,
        frames,
        summary: state.summary(),
        dropped_updates: state.stats.dropped_updates,
        parse_failures: state.stats.parse_failures,
    })
}

fn blit_into(framebuffer: &mut [u8], fb_w: u32, fb_h: u32, tile: &crate::window::BitmapTile) {
    for row in 0..tile.height {
        let dy = tile.y + row;
        if dy >= fb_h {
            break;
        }
        let cols = tile.width.min(fb_w.saturating_sub(tile.x)) as usize;
        if cols == 0 {
            break;
        }
        let src = row as usize * tile.stride;
        let dst = ((dy * fb_w + tile.x) * 4) as usize;
        framebuffer[dst..dst + cols * 4].copy_from_slice(&tile.pixels[src..src + cols * 4]);
    }
}

/// Reads the desktop size out of the recording's first note, falling back to the 1280x800
/// this client requests.
pub fn desktop_from_notes(entries: &[RecordEntry]) -> (u32, u32) {
    for entry in entries {
        let RecordEntry::Note(note) = entry else { continue };
        if let Some(dims) = note.strip_prefix("negotiated desktop ") {
            if let Some((w, h)) = dims.split_once('x') {
                if let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse()) {
                    return (w, h);
                }
            }
        }
    }
    (1280, 800)
}

/// Replays a recording file end to end.
pub fn replay_file(path: &Path) -> Result<ReplayResult> {
    let entries = record::read(path)?;
    let desktop = desktop_from_notes(&entries);
    replay(&entries, desktop, None, |_, _, _, _| {})
}
