use std::collections::HashMap;

/// A GFX surface's pixel storage: BGRX8888, row-major, matching `window::blit_bgrx`'s
/// expected format directly (alpha byte is unused/ignored downstream, kept at 0xFF).
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Surface {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, pixels: vec![0u8; (width as usize) * (height as usize) * 4] }
    }

    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    pub fn set_pixel_bgr(&mut self, x: u32, y: u32, bgr: [u8; 3]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        self.pixels[i] = bgr[0];
        self.pixels[i + 1] = bgr[1];
        self.pixels[i + 2] = bgr[2];
        self.pixels[i + 3] = 0xFF;
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, bgra: [u8; 4]) {
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
            }
        }
    }

    /// Extracts a rectangular region as a standalone tightly-packed BGRX buffer (used both
    /// for `SurfaceToCache` and same/cross-surface `SurfaceToSurface`/`CacheToSurface`
    /// copies, so overlapping in-place copies never read already-overwritten pixels).
    pub fn extract_rect(&self, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        for row in 0..h {
            let sy = y + row;
            if sy >= self.height {
                break;
            }
            let src_start = (sy as usize * self.width as usize + x as usize) * 4;
            let src_end = (src_start + w as usize * 4).min(self.pixels.len());
            let copy_len = src_end.saturating_sub(src_start);
            let dst_start = row as usize * w as usize * 4;
            out[dst_start..dst_start + copy_len].copy_from_slice(&self.pixels[src_start..src_end]);
        }
        out
    }

    pub fn blit_rect(&mut self, dst_x: u32, dst_y: u32, w: u32, h: u32, src: &[u8]) {
        let src_stride = w as usize * 4;
        for row in 0..h {
            let py = dst_y + row;
            if py >= self.height {
                break;
            }
            let src_start = row as usize * src_stride;
            if src_start + src_stride > src.len() {
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
            }
        }
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

    pub fn delete(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
    }

    pub fn get(&self, surface_id: u16) -> Option<&Surface> {
        self.surfaces.get(&surface_id)
    }

    pub fn get_mut(&mut self, surface_id: u16) -> Option<&mut Surface> {
        self.surfaces.get_mut(&surface_id)
    }

    pub fn surface_to_cache(&mut self, surface_id: u16, cache_slot: u16, x: u32, y: u32, w: u32, h: u32) {
        let Some(surf) = self.surfaces.get(&surface_id) else { return };
        let pixels = surf.extract_rect(x, y, w, h);
        self.cache.insert(cache_slot, CachedRect { width: w, height: h, pixels });
    }

    pub fn cache_to_surface(&mut self, cache_slot: u16, surface_id: u16, dest_pts: &[(u16, u16)]) {
        let Some(cached) = self.cache.get(&cache_slot) else { return };
        let (w, h, pixels) = (cached.width, cached.height, cached.pixels.clone());
        let Some(surf) = self.surfaces.get_mut(&surface_id) else { return };
        for &(dx, dy) in dest_pts {
            surf.blit_rect(dx as u32, dy as u32, w, h, &pixels);
        }
    }

    pub fn evict_cache_entry(&mut self, cache_slot: u16) {
        self.cache.remove(&cache_slot);
    }
}
