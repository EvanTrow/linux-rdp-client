use crate::debug;
use std::collections::HashMap;

/// The outcome of one write into a `Surface`.
///
/// Every write primitive here clips to the surface bounds rather than panicking, but a clip
/// means part of a server update was silently thrown away — and on a delta protocol the
/// server will never resend it, so that is a permanent artifact, not a harmless no-op. The
/// primitives therefore report whether they applied everything they were asked to, and
/// callers are expected to escalate a `false` through `gfx_error!`.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Pixels the caller asked to write.
    pub requested: u64,
    /// Pixels actually written.
    pub written: u64,
    /// True when the source buffer was shorter than the requested rect.
    pub source_truncated: bool,
}

impl WriteOutcome {
    pub fn complete(&self) -> bool {
        self.written == self.requested && !self.source_truncated
    }

    pub fn dropped(&self) -> u64 {
        self.requested.saturating_sub(self.written)
    }
}

/// A GFX surface's pixel storage: BGRX8888, row-major, matching `window::blit_bgrx`'s
/// expected format directly (alpha byte is unused/ignored downstream, kept at 0xFF).
///
/// This is the single source of truth for the remote desktop's contents. Nothing else keeps
/// partial state: the window's framebuffer is a whole-surface mirror of it, refreshed in
/// full, so there is no way for a presented frame to hold pixels from an older frame than
/// this surface does.
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// Per-pixel "which frame last wrote this pixel" tag, parallel to `pixels`. Empty unless
    /// `RDP_DEBUG_TINT` is set — see `debug` module docs for why this is out-of-band rather
    /// than blended into `pixels` directly.
    tags: Vec<u8>,
    /// Tag stamped by every write until changed. Set once per applied PDU by the caller.
    current_tag: u8,
    /// Bounding box of every write since `begin_update`, used by `RDP_DEBUG_RECTS`.
    pending_update: Option<(u32, u32, u32, u32)>,
    track_updates: bool,
}

impl Surface {
    pub fn new(width: u32, height: u32) -> Self {
        let n = (width as usize) * (height as usize);
        Self {
            width,
            height,
            pixels: vec![0u8; n * 4],
            tags: if debug::needs_tag_plane() { vec![0u8; n] } else { Vec::new() },
            current_tag: 0,
            pending_update: None,
            track_updates: debug::needs_update_rects(),
        }
    }

    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// The frame tag stamped into the tag plane by subsequent writes.
    pub fn set_frame_tag(&mut self, tag: u8) {
        self.current_tag = tag;
    }

    pub fn tags(&self) -> &[u8] {
        &self.tags
    }

    /// Starts accumulating a destination bounding box for the next applied update.
    pub fn begin_update(&mut self) {
        self.pending_update = None;
    }

    /// Returns the bounding box of everything written since `begin_update`, as (x, y, w, h).
    pub fn take_update_rect(&mut self) -> Option<(u32, u32, u32, u32)> {
        self.pending_update.take().map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
    }

    #[inline]
    fn stamp(&mut self, x: u32, y: u32) {
        if !self.tags.is_empty() {
            self.tags[y as usize * self.width as usize + x as usize] = self.current_tag;
        }
        if self.track_updates {
            self.pending_update = Some(match self.pending_update {
                None => (x, y, x, y),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            });
        }
    }

    pub fn set_pixel_bgr(&mut self, x: u32, y: u32, bgr: [u8; 3]) -> WriteOutcome {
        if x >= self.width || y >= self.height {
            return WriteOutcome { requested: 1, written: 0, source_truncated: false };
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        self.pixels[i] = bgr[0];
        self.pixels[i + 1] = bgr[1];
        self.pixels[i + 2] = bgr[2];
        self.pixels[i + 3] = 0xFF;
        self.stamp(x, y);
        WriteOutcome { requested: 1, written: 1, source_truncated: false }
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, bgra: [u8; 4]) -> WriteOutcome {
        let requested = w as u64 * h as u64;
        let mut written = 0u64;
        for row in 0..h {
            let py = y + row;
            if py >= self.height {
                break;
            }
            for col in 0..w {
                let px = x + col;
                if px >= self.width {
                    break;
                }
                let i = (py as usize * self.width as usize + px as usize) * 4;
                self.pixels[i..i + 4].copy_from_slice(&bgra);
                self.stamp(px, py);
                written += 1;
            }
        }
        WriteOutcome { requested, written, source_truncated: false }
    }

    /// Extracts a rectangular region as a standalone tightly-packed BGRX buffer (used both
    /// for `SurfaceToCache` and same/cross-surface `SurfaceToSurface`/`CacheToSurface`
    /// copies, so overlapping in-place copies never read already-overwritten pixels).
    ///
    /// Returns the pixels plus how much of the requested rect actually existed. A rect that
    /// runs off the surface yields zero (black) pixels for the missing part, and blitting
    /// those somewhere else paints real black onto real content — so the shortfall is
    /// reported rather than silently accepted.
    pub fn extract_rect(&self, x: u32, y: u32, w: u32, h: u32) -> (Vec<u8>, WriteOutcome) {
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        let requested = w as u64 * h as u64;
        if x >= self.width || y >= self.height {
            return (out, WriteOutcome { requested, written: 0, source_truncated: false });
        }
        // Clamp to this row's own bounds, not just the overall buffer length — otherwise a
        // rect extending past the right edge (x + w > self.width) reads into the START of
        // the NEXT row instead of stopping, silently splicing in unrelated pixels. Confirmed
        // as a real, previously-unfixed bug while investigating scroll-triggered ghosting.
        let row_cols = w.min(self.width - x) as usize;
        let mut written = 0u64;
        for row in 0..h {
            let sy = y + row;
            if sy >= self.height {
                break;
            }
            let src_start = (sy as usize * self.width as usize + x as usize) * 4;
            let src_end = src_start + row_cols * 4;
            let dst_start = row as usize * w as usize * 4;
            out[dst_start..dst_start + row_cols * 4].copy_from_slice(&self.pixels[src_start..src_end]);
            written += row_cols as u64;
        }
        (out, WriteOutcome { requested, written, source_truncated: false })
    }

    pub fn blit_rect(&mut self, dst_x: u32, dst_y: u32, w: u32, h: u32, src: &[u8]) -> WriteOutcome {
        let src_stride = w as usize * 4;
        let requested = w as u64 * h as u64;
        let mut written = 0u64;
        let mut source_truncated = false;
        for row in 0..h {
            let py = dst_y + row;
            if py >= self.height {
                break;
            }
            let src_start = row as usize * src_stride;
            if src_start + src_stride > src.len() {
                source_truncated = true;
                break;
            }
            for col in 0..w {
                let px = dst_x + col;
                if px >= self.width {
                    break;
                }
                let si = src_start + col as usize * 4;
                let di = (py as usize * self.width as usize + px as usize) * 4;
                self.pixels[di..di + 4].copy_from_slice(&src[si..si + 4]);
                self.stamp(px, py);
                written += 1;
            }
        }
        WriteOutcome { requested, written, source_truncated }
    }
}

/// One RDPGFX-level bitmap cache slot (MS-RDPEGFX §3.3.1.4) — a whole-surface-independent
/// pixel-rect cache, distinct from ClearCodec's own internal glyph/V-Bar caches (see
/// `clearcodec.rs`).
struct CachedRect {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Why a `cache_to_surface` did not fully apply. Each variant is a permanently wrong region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheToSurfaceError {
    /// The slot was never populated by a `SurfaceToCache`, or was evicted.
    MissingSlot,
    /// The destination surface id does not exist (never created, or already deleted).
    MissingSurface,
}

#[derive(Default)]
pub struct SurfaceManager {
    surfaces: HashMap<u16, Surface>,
    cache: HashMap<u16, CachedRect>,
}

impl SurfaceManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self, surface_id: u16, width: u16, height: u16) {
        self.surfaces.insert(surface_id, Surface::new(width as u32, height as u32));
    }

    pub fn delete(&mut self, surface_id: u16) -> bool {
        self.surfaces.remove(&surface_id).is_some()
    }

    pub fn ids(&self) -> Vec<u16> {
        self.surfaces.keys().copied().collect()
    }

    pub fn get(&self, surface_id: u16) -> Option<&Surface> {
        self.surfaces.get(&surface_id)
    }

    pub fn get_mut(&mut self, surface_id: u16) -> Option<&mut Surface> {
        self.surfaces.get_mut(&surface_id)
    }

    /// Snapshots a surface rect into a cache slot. Returns the extraction outcome so a
    /// source rect that ran off the surface (and therefore cached black pixels that will
    /// later be stamped onto real content) can be reported rather than silently accepted.
    pub fn surface_to_cache(
        &mut self,
        surface_id: u16,
        cache_slot: u16,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Option<WriteOutcome> {
        let surf = self.surfaces.get(&surface_id)?;
        let (pixels, outcome) = surf.extract_rect(x, y, w, h);
        self.cache.insert(cache_slot, CachedRect { width: w, height: h, pixels });
        Some(outcome)
    }

    /// Stamps a cached rect at every destination point (MS-RDPEGFX §2.2.2.7 — `destPtsCount`
    /// points, each an independent SRCCOPY; handling only the first would leave the rest
    /// permanently stale).
    pub fn cache_to_surface(
        &mut self,
        cache_slot: u16,
        surface_id: u16,
        dest_pts: &[(u16, u16)],
    ) -> Result<Vec<WriteOutcome>, CacheToSurfaceError> {
        let cached = self.cache.get(&cache_slot).ok_or(CacheToSurfaceError::MissingSlot)?;
        let (w, h, pixels) = (cached.width, cached.height, cached.pixels.clone());
        let surf = self.surfaces.get_mut(&surface_id).ok_or(CacheToSurfaceError::MissingSurface)?;
        Ok(dest_pts.iter().map(|&(dx, dy)| surf.blit_rect(dx as u32, dy as u32, w, h, &pixels)).collect())
    }

    pub fn evict_cache_entry(&mut self, cache_slot: u16) -> bool {
        self.cache.remove(&cache_slot).is_some()
    }

    pub fn cached_slots(&self) -> usize {
        self.cache.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, bgr: [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&[bgr[0], bgr[1], bgr[2], 0xFF]);
        }
        v
    }

    #[test]
    fn extract_rect_does_not_wrap_into_the_next_row() {
        let mut s = Surface::new(4, 2);
        // Row 0 red, row 1 blue.
        let _ = s.fill_rect(0, 0, 4, 1, [0, 0, 255, 255]);
        let _ = s.fill_rect(0, 1, 4, 1, [255, 0, 0, 255]);
        // A 3-wide rect starting at x=2 runs one pixel off the right edge.
        let (px, outcome) = s.extract_rect(2, 0, 3, 1);
        assert_eq!(outcome.written, 2, "only the 2 in-bounds pixels exist");
        assert!(!outcome.complete());
        // The two real pixels are red; the third must be zero-fill, NOT row 1's blue.
        assert_eq!(&px[0..4], &[0, 0, 255, 255]);
        assert_eq!(&px[4..8], &[0, 0, 255, 255]);
        assert_eq!(&px[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn blit_rect_clips_and_reports_the_shortfall() {
        let mut s = Surface::new(4, 4);
        let outcome = s.blit_rect(2, 2, 4, 4, &solid(4, 4, [1, 2, 3]));
        assert_eq!(outcome.requested, 16);
        assert_eq!(outcome.written, 4, "only the 2x2 overlap is inside the surface");
        assert!(!outcome.complete(), "a clipped blit is a dropped update, not a no-op");
    }

    #[test]
    fn blit_rect_reports_a_short_source_buffer() {
        let mut s = Surface::new(8, 8);
        let outcome = s.blit_rect(0, 0, 4, 4, &solid(4, 2, [9, 9, 9]));
        assert!(outcome.source_truncated);
        assert_eq!(outcome.written, 8);
    }

    #[test]
    fn cache_to_surface_stamps_every_destination_point() {
        let mut m = SurfaceManager::new();
        m.create(1, 16, 16);
        let _ = m.get_mut(1).unwrap().fill_rect(0, 0, 4, 4, [7, 8, 9, 255]);
        let _ = m.surface_to_cache(1, 42, 0, 0, 4, 4).unwrap();
        let pts = [(8u16, 0u16), (8, 8), (0, 8)];
        let outcomes = m.cache_to_surface(42, 1, &pts).unwrap();
        assert_eq!(outcomes.len(), 3, "every destPt must be stamped, not just destPts[0]");
        assert!(outcomes.iter().all(|o| o.complete()));
        let s = m.get(1).unwrap();
        for (dx, dy) in pts {
            let i = ((dy as usize) * 16 + dx as usize) * 4;
            assert_eq!(&s.pixels[i..i + 3], &[7, 8, 9], "destination point ({dx},{dy}) not stamped");
        }
    }

    #[test]
    fn cache_to_surface_distinguishes_a_missing_slot_from_a_missing_surface() {
        let mut m = SurfaceManager::new();
        assert_eq!(m.cache_to_surface(1, 1, &[(0, 0)]), Err(CacheToSurfaceError::MissingSlot));
        m.create(1, 4, 4);
        let _ = m.surface_to_cache(1, 7, 0, 0, 2, 2).unwrap();
        assert_eq!(m.cache_to_surface(7, 99, &[(0, 0)]), Err(CacheToSurfaceError::MissingSurface));
    }

    #[test]
    fn overlapping_same_surface_copy_reads_pre_copy_pixels() {
        // Scroll optimisation: SurfaceToSurface with overlapping src/dst on one surface.
        // Extracting to a standalone buffer first is what makes this correct.
        let mut m = SurfaceManager::new();
        m.create(1, 8, 1);
        for x in 0..8u32 {
            let _ = m.get_mut(1).unwrap().set_pixel_bgr(x, 0, [x as u8, 0, 0]);
        }
        let (src, _) = m.get(1).unwrap().extract_rect(0, 0, 6, 1);
        let _ = m.get_mut(1).unwrap().blit_rect(2, 0, 6, 1, &src);
        let s = m.get(1).unwrap();
        let got: Vec<u8> = (0..8).map(|x| s.pixels[(x * 4) as usize]).collect();
        assert_eq!(got, vec![0, 1, 0, 1, 2, 3, 4, 5], "overlapping copy must not read its own output");
    }
}
