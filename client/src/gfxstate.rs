//! The MS-RDPEGFX surface state machine: everything between "a parsed RDPGFX PDU" and
//! "pixels in a surface".
//!
//! This lives apart from the network loop on purpose. RDP is a pure delta protocol, so a
//! rendering defect only reproduces under the exact update sequence that caused it — which
//! means the only way to debug one is to replay a recorded stream through *the same* code
//! the live session runs. `session::run_session` and `bin/replay.rs` both drive this type,
//! so a replay is a true reproduction rather than a second implementation that might not
//! share the bug.
//!
//! Two rules this module exists to enforce:
//!
//! 1. **Nothing is dropped silently.** Every path that declines to apply a server update
//!    reports through `gfx_error!` with the operation, surface, rect and frame id, because on
//!    a delta protocol a dropped update is a permanent artifact, not a skipped frame.
//! 2. **The surface is the single source of truth.** All updates land in a persistent,
//!    full-size surface; presentation copies the whole thing. Nothing composites partial
//!    updates directly into a presentation buffer, so a presented frame can never hold
//!    pixels from an older frame than the surface does.

use crate::clearcodec::ClearCodecContext;
use crate::debug::{self, GfxErrorContext, OverlayRect};
use crate::gfx::{self, GfxPdu, SplitEntry};
use crate::gfx_error;
use crate::progressive::ProgressiveContext;
use crate::surface::{CacheToSurfaceError, SurfaceManager, WriteOutcome};
use crate::window::BitmapTile;

/// What the caller must do after a PDU was applied. Kept out of this module because both
/// actions need the network (send a frame ack) or the window (present), neither of which the
/// replay harness has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GfxAction {
    /// An END_FRAME was applied: acknowledge `frame_id` and present the surfaces.
    EndFrame { frame_id: u32, total_frames_decoded: u32 },
}

#[derive(Default, Debug, Clone)]
pub struct GfxStats {
    pub frames: u64,
    pub wire_to_surface_1: u64,
    pub wire_to_surface_2: u64,
    pub solid_fill: u64,
    pub surface_to_surface: u64,
    pub surface_to_cache: u64,
    pub cache_to_surface: u64,
    pub evict_cache_entry: u64,
    pub delete_encoding_context: u64,
    /// PDUs whose body could not be parsed at all.
    pub parse_failures: u64,
    /// Updates that were decoded but not (fully) applied — each one is a permanent artifact.
    pub dropped_updates: u64,
    /// Pixels a clip or short source buffer threw away.
    pub dropped_pixels: u64,
    pub unhandled_pdus: u64,
}

pub struct GfxState {
    pub surfaces: SurfaceManager,
    clear_ctx: ClearCodecContext,
    progressive_ctx: ProgressiveContext,
    /// Every surface currently mapped to an output region, as (surface_id, origin_x,
    /// origin_y). A `Vec` rather than a single slot because MAP_SURFACE_TO_OUTPUT is
    /// per-surface: a host with two output regions maps two surfaces, and tracking only the
    /// most recent one leaves the other region permanently unpainted.
    output_map: Vec<(u16, u32, u32)>,
    /// Desktop size, from RESET_GRAPHICS (or the negotiated size until one arrives).
    desktop: (u32, u32),
    frames_decoded: u32,
    /// frameId of the START_FRAME currently open, used to give every error and every tint
    /// tag the frame it belongs to.
    current_frame_id: u32,
    /// Set once any PDU has actually written pixels; before that there is nothing to show.
    saw_content: bool,
    overlays: Vec<(u16, OverlayRect)>,
    pub stats: GfxStats,
}

impl GfxState {
    pub fn new(desktop_width: u32, desktop_height: u32) -> Self {
        Self {
            surfaces: SurfaceManager::new(),
            clear_ctx: ClearCodecContext::new(),
            progressive_ctx: ProgressiveContext::new(),
            output_map: Vec::new(),
            desktop: (desktop_width, desktop_height),
            frames_decoded: 0,
            current_frame_id: 0,
            saw_content: false,
            overlays: Vec::new(),
            stats: GfxStats::default(),
        }
    }

    pub fn desktop_size(&self) -> (u32, u32) {
        self.desktop
    }

    pub fn frames_decoded(&self) -> u32 {
        self.frames_decoded
    }

    /// Applies every PDU in one already-ZGFX-decompressed graphics-channel message.
    pub fn apply_message(&mut self, message: &[u8]) -> anyhow::Result<Vec<GfxAction>> {
        let mut actions = Vec::new();
        for entry in gfx::split_pdus(message)? {
            match entry {
                SplitEntry::Pdu(pdu) => {
                    if let Some(action) = self.apply(pdu) {
                        actions.push(action);
                    }
                }
                SplitEntry::Failed { cmd_id, pdu_length, error } => {
                    self.stats.parse_failures += 1;
                    self.stats.dropped_updates += 1;
                    let ctx = GfxErrorContext::new("parse", 0, self.current_frame_id);
                    gfx_error!(
                        &ctx,
                        "unparseable {} PDU (cmdId={cmd_id:#06x}, pduLength={pdu_length}): {error:#}",
                        gfx::cmd_name(cmd_id)
                    );
                }
            }
        }
        Ok(actions)
    }

    /// Records that `outcome` came from applying `op`, escalating anything less than a
    /// complete write. A clipped or truncated write means the server painted something the
    /// user will never see, and it will never be resent.
    fn check(&mut self, ctx: &GfxErrorContext, outcome: WriteOutcome) {
        if outcome.complete() {
            return;
        }
        self.stats.dropped_updates += 1;
        self.stats.dropped_pixels += outcome.dropped();
        gfx_error!(
            ctx,
            "update only partially applied: {}/{} pixels written{}",
            outcome.written,
            outcome.requested,
            if outcome.source_truncated { ", source buffer was short" } else { "" }
        );
    }

    /// Prepares a surface to receive one PDU's worth of writes: stamps the tint tag plane
    /// with the current frame and starts a fresh update-rect bounding box.
    fn begin(&mut self, surface_id: u16) -> bool {
        let tag = debug::frame_tag(self.current_frame_id);
        match self.surfaces.get_mut(surface_id) {
            Some(s) => {
                s.set_frame_tag(tag);
                s.begin_update();
                true
            }
            None => false,
        }
    }

    /// Closes an update started by `begin`, recording its destination rect for the
    /// `RDP_DEBUG_RECTS` overlay.
    fn end(&mut self, surface_id: u16) {
        if !debug::needs_update_rects() {
            return;
        }
        let frame_id = self.current_frame_id;
        if let Some(s) = self.surfaces.get_mut(surface_id) {
            if let Some((x, y, w, h)) = s.take_update_rect() {
                self.overlays.push((surface_id, OverlayRect { x, y, w, h, frame_id, age: 0 }));
            }
        }
    }

    fn missing_surface(&mut self, op: &'static str, surface_id: u16) {
        self.stats.dropped_updates += 1;
        let ctx = GfxErrorContext::new(op, surface_id, self.current_frame_id);
        gfx_error!(&ctx, "no such surface — the entire update is discarded and will never be resent");
    }

    pub fn apply(&mut self, pdu: GfxPdu) -> Option<GfxAction> {
        match pdu {
            GfxPdu::CapsConfirm => {
                if debug::trace() {
                    eprintln!("[gfx] caps confirmed");
                }
            }

            GfxPdu::ResetGraphics { width, height } => {
                // MS-RDPEGFX §2.2.2.14: the server is redefining the output geometry. The
                // surfaces themselves survive (the server re-maps them), but every piece of
                // geometry-derived decoder state must go, and the output mapping is stale
                // until the server sends fresh MAP_SURFACE_TO_OUTPUT PDUs.
                eprintln!("[gfx] reset graphics: {}x{} (was {}x{})", width, height, self.desktop.0, self.desktop.1);
                self.desktop = (width, height);
                self.output_map.clear();
                self.overlays.clear();
                // Per-tile RemoteFX Progressive baselines are indexed by tile grid position,
                // which only means anything relative to a fixed surface geometry. Keeping
                // them across a reset lets a diff tile add its delta onto a baseline from a
                // different layout. This is the one place they may be dropped: dropping them
                // at any other time would break the legitimate cross-frame diff chain.
                self.progressive_ctx.reset_all();
            }

            GfxPdu::CreateSurface(s) => {
                if debug::trace() {
                    eprintln!("[gfx] create surface {}: {}x{} format={:#04x}", s.surface_id, s.width, s.height, s.pixel_format);
                }
                // A surface id may be reused after deletion with different dimensions, so
                // any per-tile state left over from the previous tenant must not survive
                // into the new one.
                self.progressive_ctx.reset_surface(s.surface_id);
                self.surfaces.create(s.surface_id, s.width, s.height);
            }

            GfxPdu::DeleteSurface { surface_id } => {
                if debug::trace() {
                    eprintln!("[gfx] delete surface {surface_id}");
                }
                if !self.surfaces.delete(surface_id) {
                    let ctx = GfxErrorContext::new("DELETE_SURFACE", surface_id, self.current_frame_id);
                    gfx_error!(&ctx, "server deleted a surface we never created — our surface table is out of sync");
                }
                // FreeRDP frees the codec context with the surface; leaving it behind means a
                // later surface reusing this id inherits tile baselines from the dead one and
                // adds diff deltas onto unrelated pixels.
                self.progressive_ctx.reset_surface(surface_id);
                self.output_map.retain(|&(id, _, _)| id != surface_id);
                self.overlays.retain(|&(id, _)| id != surface_id);
            }

            GfxPdu::MapSurfaceToOutput(m) => {
                if debug::trace() {
                    eprintln!("[gfx] map surface {} to output at ({}, {})", m.surface_id, m.output_origin_x, m.output_origin_y);
                }
                if self.surfaces.get(m.surface_id).is_none() {
                    let ctx = GfxErrorContext::new("MAP_SURFACE_TO_OUTPUT", m.surface_id, self.current_frame_id);
                    gfx_error!(&ctx, "mapping a surface that does not exist — nothing will be presented for this output region");
                }
                self.output_map.retain(|&(id, _, _)| id != m.surface_id);
                self.output_map.push((m.surface_id, m.output_origin_x, m.output_origin_y));
            }

            GfxPdu::StartFrame { frame_id } => {
                self.current_frame_id = frame_id;
                if debug::trace() {
                    eprintln!("[gfx] start frame {frame_id}");
                }
            }

            GfxPdu::EndFrame { frame_id } => {
                self.frames_decoded += 1;
                self.stats.frames += 1;
                if debug::trace() {
                    eprintln!("[gfx] end frame {frame_id} (total {})", self.frames_decoded);
                }
                return Some(GfxAction::EndFrame { frame_id, total_frames_decoded: self.frames_decoded });
            }

            GfxPdu::WireToSurface1(w) => {
                self.stats.wire_to_surface_1 += 1;
                let Some((width, height)) = w.dest_rect.size() else {
                    self.stats.dropped_updates += 1;
                    let ctx = GfxErrorContext::new("WIRE_TO_SURFACE_1", w.surface_id, self.current_frame_id).with_rect(
                        w.dest_rect.left as u32,
                        w.dest_rect.top as u32,
                        w.dest_rect.right as u32,
                        w.dest_rect.bottom as u32,
                    );
                    gfx_error!(&ctx, "inverted destRect (right < left or bottom < top)");
                    return None;
                };
                let ctx = GfxErrorContext::new("WIRE_TO_SURFACE_1", w.surface_id, self.current_frame_id).with_rect(
                    w.dest_rect.left as u32,
                    w.dest_rect.top as u32,
                    w.dest_rect.right as u32,
                    w.dest_rect.bottom as u32,
                );
                if debug::trace() {
                    eprintln!("[gfx] {ctx} codec={:#06x} bytes={}", w.codec_id, w.bitmap_data.len());
                }
                if !self.begin(w.surface_id) {
                    self.missing_surface("WIRE_TO_SURFACE_1", w.surface_id);
                    return None;
                }
                let (dx, dy) = (w.dest_rect.left as u32, w.dest_rect.top as u32);
                match w.codec_id {
                    gfx::CODEC_CLEARCODEC => {
                        let surf = self.surfaces.get_mut(w.surface_id).expect("checked by begin()");
                        match self.clear_ctx.decompress(&w.bitmap_data, surf, dx, dy, width, height) {
                            Ok(outcome) => self.check(&ctx, outcome),
                            Err(e) => {
                                self.stats.dropped_updates += 1;
                                gfx_error!(&ctx, "ClearCodec decode failed: {e:#}");
                            }
                        }
                    }
                    gfx::CODEC_CAPROGRESSIVE => {
                        // Legal in this envelope as well as in WIRE_TO_SURFACE_2. Unlike
                        // PDU_2 the destination origin comes from destRect, and the region
                        // rects inside the bitstream are relative to it.
                        let surface_id = w.surface_id;
                        let surf = self.surfaces.get_mut(surface_id).expect("checked by begin()");
                        if let Err(e) = self.progressive_ctx.decompress_at(surface_id, &w.bitmap_data, surf, dx, dy) {
                            self.stats.dropped_updates += 1;
                            gfx_error!(&ctx, "RemoteFX Progressive decode failed: {e:#}");
                        }
                    }
                    gfx::CODEC_UNCOMPRESSED => {
                        let expected = (width as usize) * (height as usize) * 4;
                        if w.bitmap_data.len() < expected {
                            self.stats.dropped_updates += 1;
                            gfx_error!(&ctx, "uncompressed bitmapData too short ({} < {expected})", w.bitmap_data.len());
                        } else {
                            let surf = self.surfaces.get_mut(w.surface_id).expect("checked by begin()");
                            let outcome = surf.blit_rect(dx, dy, width, height, &w.bitmap_data);
                            self.check(&ctx, outcome);
                        }
                    }
                    other => {
                        self.stats.dropped_updates += 1;
                        gfx_error!(&ctx, "codec {other:#06x} is not implemented — this region will stay stale forever");
                    }
                }
                self.saw_content = true;
                self.end(w.surface_id);
            }

            GfxPdu::WireToSurface2(w) => {
                self.stats.wire_to_surface_2 += 1;
                let ctx = GfxErrorContext::new("WIRE_TO_SURFACE_2", w.surface_id, self.current_frame_id);
                if debug::trace() {
                    eprintln!("[gfx] {ctx} codec={:#06x} bytes={}", w.codec_id, w.bitmap_data.len());
                }
                if !self.begin(w.surface_id) {
                    self.missing_surface("WIRE_TO_SURFACE_2", w.surface_id);
                    return None;
                }
                if w.codec_id != gfx::CODEC_CAPROGRESSIVE {
                    // Per MS-RDPEGFX §2.2.2.2 this envelope only ever carries
                    // RDPGFX_CODECID_CAPROGRESSIVE; anything else means we have misparsed.
                    self.stats.dropped_updates += 1;
                    gfx_error!(&ctx, "codec {:#06x} in a WIRE_TO_SURFACE_2 envelope (spec allows only CAPROGRESSIVE)", w.codec_id);
                    self.end(w.surface_id);
                    return None;
                }
                let surface_id = w.surface_id;
                let surf = self.surfaces.get_mut(surface_id).expect("checked by begin()");
                if let Err(e) = self.progressive_ctx.decompress(surface_id, &w.bitmap_data, surf) {
                    self.stats.dropped_updates += 1;
                    gfx_error!(&ctx, "RemoteFX Progressive decode failed: {e:#}");
                }
                self.saw_content = true;
                self.end(surface_id);
            }

            GfxPdu::SolidFill(f) => {
                self.stats.solid_fill += 1;
                if !self.begin(f.surface_id) {
                    self.missing_surface("SOLID_FILL", f.surface_id);
                    return None;
                }
                // RDPGFX_COLOR32 is blue,green,red,xA(reserved) — matches our BGRX surface
                // storage directly, alpha forced opaque.
                let bgra = [f.color_bgra[0], f.color_bgra[1], f.color_bgra[2], 0xFF];
                for rect in &f.fill_rects {
                    let ctx = GfxErrorContext::new("SOLID_FILL", f.surface_id, self.current_frame_id).with_rect(
                        rect.left as u32,
                        rect.top as u32,
                        rect.right as u32,
                        rect.bottom as u32,
                    );
                    let Some((w, h)) = rect.size() else {
                        self.stats.dropped_updates += 1;
                        gfx_error!(&ctx, "inverted fillRect");
                        continue;
                    };
                    let surf = self.surfaces.get_mut(f.surface_id).expect("checked by begin()");
                    let outcome = surf.fill_rect(rect.left as u32, rect.top as u32, w, h, bgra);
                    self.check(&ctx, outcome);
                }
                self.saw_content = true;
                self.end(f.surface_id);
            }

            GfxPdu::SurfaceToSurface(s) => {
                self.stats.surface_to_surface += 1;
                let ctx = GfxErrorContext::new("SURFACE_TO_SURFACE", s.surface_id_src, self.current_frame_id).with_rect(
                    s.rect_src.left as u32,
                    s.rect_src.top as u32,
                    s.rect_src.right as u32,
                    s.rect_src.bottom as u32,
                );
                let Some((w, h)) = s.rect_src.size() else {
                    self.stats.dropped_updates += 1;
                    gfx_error!(&ctx, "inverted rectSrc");
                    return None;
                };
                if debug::trace() {
                    eprintln!("[gfx] {ctx} -> surface {} at {:?}", s.surface_id_dst, s.dest_pts);
                }
                // Extract to an owned buffer before touching the destination so an
                // overlapping same-surface copy (the scroll-optimisation case, which is the
                // common one) never reads pixels it has already overwritten.
                let Some(src) = self.surfaces.get(s.surface_id_src) else {
                    self.missing_surface("SURFACE_TO_SURFACE(src)", s.surface_id_src);
                    return None;
                };
                let (extracted, extract_outcome) = src.extract_rect(s.rect_src.left as u32, s.rect_src.top as u32, w, h);
                self.check(&ctx, extract_outcome);
                if !self.begin(s.surface_id_dst) {
                    self.missing_surface("SURFACE_TO_SURFACE(dst)", s.surface_id_dst);
                    return None;
                }
                let surf = self.surfaces.get_mut(s.surface_id_dst).expect("checked by begin()");
                let outcomes: Vec<_> =
                    s.dest_pts.iter().map(|&(dx, dy)| surf.blit_rect(dx as u32, dy as u32, w, h, &extracted)).collect();
                for (i, outcome) in outcomes.into_iter().enumerate() {
                    let (dx, dy) = s.dest_pts[i];
                    let dctx = GfxErrorContext::new("SURFACE_TO_SURFACE(dst)", s.surface_id_dst, self.current_frame_id)
                        .with_rect(dx as u32, dy as u32, dx as u32 + w, dy as u32 + h);
                    self.check(&dctx, outcome);
                }
                self.saw_content = true;
                self.end(s.surface_id_dst);
            }

            GfxPdu::SurfaceToCache(s) => {
                self.stats.surface_to_cache += 1;
                let ctx = GfxErrorContext::new("SURFACE_TO_CACHE", s.surface_id, self.current_frame_id).with_rect(
                    s.rect_src.left as u32,
                    s.rect_src.top as u32,
                    s.rect_src.right as u32,
                    s.rect_src.bottom as u32,
                );
                let Some((w, h)) = s.rect_src.size() else {
                    self.stats.dropped_updates += 1;
                    gfx_error!(&ctx, "inverted rectSrc");
                    return None;
                };
                if debug::trace() {
                    eprintln!("[gfx] {ctx} -> cache slot {}", s.cache_slot);
                }
                match self.surfaces.surface_to_cache(s.surface_id, s.cache_slot, s.rect_src.left as u32, s.rect_src.top as u32, w, h)
                {
                    // A source rect running off the surface caches black pixels, and those
                    // black pixels get stamped over real content by every later
                    // CACHE_TO_SURFACE that uses the slot.
                    Some(outcome) => self.check(&ctx, outcome),
                    None => self.missing_surface("SURFACE_TO_CACHE", s.surface_id),
                }
            }

            GfxPdu::CacheToSurface(s) => {
                self.stats.cache_to_surface += 1;
                if !self.begin(s.surface_id) {
                    self.missing_surface("CACHE_TO_SURFACE", s.surface_id);
                    return None;
                }
                let ctx = GfxErrorContext::new("CACHE_TO_SURFACE", s.surface_id, self.current_frame_id);
                match self.surfaces.cache_to_surface(s.cache_slot, s.surface_id, &s.dest_pts) {
                    Ok(outcomes) => {
                        for (i, outcome) in outcomes.into_iter().enumerate() {
                            let (dx, dy) = s.dest_pts[i];
                            // Width/height live in the cache slot, so the rect here marks
                            // the destination point rather than a full extent.
                            let dctx = ctx.with_rect(dx as u32, dy as u32, dx as u32, dy as u32);
                            self.check(&dctx, outcome);
                        }
                        self.saw_content = true;
                    }
                    Err(CacheToSurfaceError::MissingSlot) => {
                        self.stats.dropped_updates += 1;
                        gfx_error!(
                            &ctx,
                            "cache slot {} was never populated (or was evicted) — {} destination points left stale",
                            s.cache_slot,
                            s.dest_pts.len()
                        );
                    }
                    Err(CacheToSurfaceError::MissingSurface) => self.missing_surface("CACHE_TO_SURFACE", s.surface_id),
                }
                self.end(s.surface_id);
            }

            GfxPdu::EvictCacheEntry { cache_slot } => {
                self.stats.evict_cache_entry += 1;
                if !self.surfaces.evict_cache_entry(cache_slot) && debug::trace() {
                    eprintln!("[gfx] evict of never-populated cache slot {cache_slot} (harmless)");
                }
            }

            GfxPdu::DeleteEncodingContext { surface_id, codec_context_id } => {
                self.stats.delete_encoding_context += 1;
                // Deliberately a no-op on decoder state, matching FreeRDP, whose handler for
                // this PDU is also a no-op: the RemoteFX Progressive per-tile baselines are
                // keyed by surface and are freed on DELETE_SURFACE. This PDU fires many times
                // per session, so dropping baselines here would break the legitimate
                // cross-frame RFX_TILE_DIFFERENCE chain and reintroduce ghosting rather than
                // fix it. Parsed and counted (rather than lumped into `Other`) so it is
                // visible in the stats.
                if debug::trace() {
                    eprintln!("[gfx] delete encoding context surface={surface_id} ctx={codec_context_id} (no decoder state to drop)");
                }
            }

            GfxPdu::Other { cmd_id } => {
                self.stats.unhandled_pdus += 1;
                let ctx = GfxErrorContext::new("unhandled", 0, self.current_frame_id);
                gfx_error!(&ctx, "no handler for {} PDU (cmdId={cmd_id:#06x})", gfx::cmd_name(cmd_id));
            }
        }
        None
    }

    /// Produces the tiles to present: one per mapped surface, positioned at that surface's
    /// output origin.
    ///
    /// Each tile is a whole-surface snapshot, never a partial update. That is what makes the
    /// presentation path safe: the window's framebuffer is refreshed in full from a buffer
    /// that already holds every update ever applied, so there is no double-buffered image
    /// that could still be showing frame N-2 in the regions this frame did not touch.
    pub fn present_tiles(&mut self) -> Vec<BitmapTile> {
        if !self.saw_content {
            return Vec::new();
        }
        let tint = debug::flags().tint;
        let rects = debug::needs_update_rects();

        let mut tiles = Vec::with_capacity(self.output_map.len());
        for &(surface_id, ox, oy) in &self.output_map {
            let Some(surf) = self.surfaces.get(surface_id) else { continue };
            let (w, h) = (surf.width, surf.height);
            let mut pixels = surf.pixels.clone();

            if tint {
                for (i, tag) in surf.tags().iter().enumerate() {
                    debug::apply_tint_bgrx(&mut pixels[i * 4..i * 4 + 4], *tag);
                }
            }
            if rects {
                for (id, rect) in &self.overlays {
                    if *id == surface_id {
                        debug::draw_overlay_rect(&mut pixels, w, h, rect);
                    }
                }
            }

            tiles.push(BitmapTile { x: ox, y: oy, width: w, height: h, pixels, stride: (w as usize) * 4 });
        }

        if rects {
            for (_, rect) in self.overlays.iter_mut() {
                rect.age += 1;
            }
            self.overlays.retain(|(_, r)| r.age < debug::OVERLAY_LIFETIME_FRAMES);
        }
        tiles
    }

    pub fn mapped_surfaces(&self) -> &[(u16, u32, u32)] {
        &self.output_map
    }

    /// A one-line summary of everything that went wrong, for the end of a session or replay.
    pub fn summary(&self) -> String {
        let s = &self.stats;
        format!(
            "frames={} w2s1={} w2s2={} solidfill={} s2s={} s2c={} c2s={} evict={} delctx={} \
             | parse_failures={} dropped_updates={} dropped_pixels={} unhandled={}",
            s.frames,
            s.wire_to_surface_1,
            s.wire_to_surface_2,
            s.solid_fill,
            s.surface_to_surface,
            s.surface_to_cache,
            s.cache_to_surface,
            s.evict_cache_entry,
            s.delete_encoding_context,
            s.parse_failures,
            s.dropped_updates,
            s.dropped_pixels,
            s.unhandled_pdus,
        )
    }
}
