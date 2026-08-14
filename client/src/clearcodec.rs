use crate::surface::{Surface, WriteOutcome};
use anyhow::{bail, Result};

const FLAG_GLYPH_INDEX: u8 = 0x01;
const FLAG_GLYPH_HIT: u8 = 0x02;
const FLAG_CACHE_RESET: u8 = 0x04;

const GLYPH_CACHE_SIZE: usize = 4000;
const VBAR_SIZE: usize = 32768;
const SHORT_VBAR_SIZE: usize = 16384;

const SUBCODEC_UNCOMPRESSED: u8 = 0x00;
const SUBCODEC_NSCODEC: u8 = 0x01;
const SUBCODEC_RLEX: u8 = 0x02;

struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn u8(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or_else(|| anyhow::anyhow!("ClearCodec stream truncated"))?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16> {
        if self.remaining() < 2 {
            bail!("ClearCodec stream truncated");
        }
        let v = u16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    fn u32(&mut self) -> Result<u32> {
        if self.remaining() < 4 {
            bail!("ClearCodec stream truncated");
        }
        let v = u32::from_le_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    fn bgr(&mut self) -> Result<[u8; 3]> {
        let b = self.u8()?;
        let g = self.u8()?;
        let r = self.u8()?;
        Ok([b, g, r])
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            bail!("ClearCodec stream truncated");
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
}

/// Escalating run-length field shared by every ClearCodec RLE variant (MS-RDPEGFX's
/// generic "ClearCodec Run-Length Encoding" note): 1/2/4-byte value depending on how large
/// the run is.
fn read_run_length(r: &mut ByteReader) -> Result<u32> {
    let f1 = r.u8()?;
    if f1 < 0xFF {
        return Ok(f1 as u32);
    }
    let f2 = r.u16()?;
    if f2 < 0xFFFF {
        return Ok(f2 as u32);
    }
    r.u32()
}

fn bits_needed(x: u32) -> u8 {
    if x == 0 {
        1
    } else {
        (32 - x.leading_zeros()) as u8
    }
}

/// Persistent decoder state for the ClearCodec bitstream (MS-RDPEGFX §2.2.4.1): three
/// codec-private caches, entirely separate from the RDPGFX-level bitmap cache in
/// `surface::SurfaceManager` (see `zgfx_compression.md`/`clearcodec` memory for why). Lives
/// for the whole graphics-channel connection — never reset by ResetGraphics, only by the
/// in-stream `CLEARCODEC_FLAG_CACHE_RESET` flag (and even then only the V-Bar cursors, not
/// the glyph cache).
pub struct ClearCodecContext {
    seq_number: Option<u8>,
    /// Glyph cache slots, each a flat BGR pixel buffer.
    ///
    /// Dimensions are deliberately **not** stored. Per FreeRDP's `clear.c`, a slot records
    /// only its pixel *count*, a hit is valid whenever `nWidth * nHeight <= count`, and the
    /// buffer is then re-read at the *requested* dimensions (source stride `nWidth * bpp`).
    /// Real traffic depends on this: this host reuses a slot for an 8x1 glyph and later reads
    /// it back as 1x8, and a 4x9 slot as 6x6 — equal pixel counts, different shapes. Requiring
    /// the dimensions to match drops those glyphs entirely, which on a delta protocol leaves
    /// the destination rect permanently unpainted.
    glyph_cache: Vec<Option<Vec<u8>>>,
    vbar_storage: Vec<Option<Vec<[u8; 3]>>>,
    short_vbar_storage: Vec<Option<Vec<[u8; 3]>>>,
    vbar_cursor: usize,
    short_vbar_cursor: usize,
}

impl ClearCodecContext {
    pub fn new() -> Self {
        Self {
            seq_number: None,
            glyph_cache: vec![None; GLYPH_CACHE_SIZE],
            vbar_storage: vec![None; VBAR_SIZE],
            short_vbar_storage: vec![None; SHORT_VBAR_SIZE],
            vbar_cursor: 0,
            short_vbar_cursor: 0,
        }
    }

    /// Decodes one `RDPGFX_WIRE_TO_SURFACE_PDU_1` ClearCodec (`codecId=0x0008`) bitmap
    /// stream and blits the result directly into `surface` at `(dest_x, dest_y)`.
    pub fn decompress(
        &mut self,
        data: &[u8],
        surface: &mut Surface,
        dest_x: u32,
        dest_y: u32,
        width: u32,
        height: u32,
    ) -> Result<WriteOutcome> {
        let mut r = ByteReader::new(data);
        let flags = r.u8()?;
        let seq = r.u8()?;
        if let Some(prev) = self.seq_number {
            let expected = prev.wrapping_add(1);
            if seq != expected {
                bail!("ClearCodec seqNumber gap: expected {expected}, got {seq}");
            }
        }
        self.seq_number = Some(seq);

        if flags & FLAG_CACHE_RESET != 0 {
            self.vbar_cursor = 0;
            self.short_vbar_cursor = 0;
        }

        let glyph_index = if flags & FLAG_GLYPH_INDEX != 0 { Some(r.u16()? as usize) } else { None };

        if let Some(idx) = glyph_index {
            if idx >= GLYPH_CACHE_SIZE {
                bail!("ClearCodec glyphIndex {idx} out of range");
            }
            if flags & FLAG_GLYPH_HIT != 0 {
                // A miss/size-mismatch here (same reconnect-cache-predates-connection cause
                // as the V-Bar case below) means there is no compositePayload in this message
                // to fall back to at all — nothing can be drawn, and the destination rect
                // keeps whatever was there before, permanently. The session continues (the
                // caller logs and moves to the next PDU), but this reports rather than
                // returning Ok as if the update had been applied.
                let Some(pixels) = self.glyph_cache[idx].clone() else {
                    bail!("ClearCodec glyph cache hit for index {idx}, which was never populated — nothing was drawn");
                };
                let needed = (width as usize) * (height as usize) * 3;
                if pixels.len() < needed {
                    bail!(
                        "ClearCodec glyph cache entry {idx} holds {} pixels, too few for the requested {width}x{height} \
                         — nothing was drawn",
                        pixels.len() / 3
                    );
                }
                return Ok(blit_bgr_buffer(surface, dest_x, dest_y, width, height, &pixels));
            }
        }

        // Decode residual -> bands -> subcodec into a scratch BGR tile buffer, composited
        // in that order (each layer may overwrite pixels the previous one drew), then blit
        // once. A scratch tile (not the destination surface directly) is needed so a
        // GLYPH_INDEX (non-hit) draw can also snapshot the composited result into the glyph
        // cache afterward.
        let mut tile = vec![[0u8, 0, 0]; (width as usize) * (height as usize)];

        let residual_len = r.u32()? as usize;
        let bands_len = r.u32()? as usize;
        let subcodec_len = r.u32()? as usize;

        if residual_len > 0 {
            let seg = r.bytes(residual_len)?;
            decode_residual(seg, width, height, &mut tile)?;
        }
        if bands_len > 0 {
            let seg = r.bytes(bands_len)?;
            self.decode_bands(seg, width, height, &mut tile)?;
        }
        if subcodec_len > 0 {
            let seg = r.bytes(subcodec_len)?;
            decode_subcodecs(seg, width, height, &mut tile)?;
        }

        let flat: Vec<u8> = tile.iter().flat_map(|p| p.iter().copied()).collect();
        let outcome = blit_bgr_buffer(surface, dest_x, dest_y, width, height, &flat);

        if let Some(idx) = glyph_index {
            self.glyph_cache[idx] = Some(flat);
        }
        Ok(outcome)
    }

    fn decode_bands(&mut self, data: &[u8], width: u32, height: u32, tile: &mut [[u8; 3]]) -> Result<()> {
        let mut r = ByteReader::new(data);
        while r.remaining() > 0 {
            let x_start = r.u16()? as u32;
            let x_end = r.u16()? as u32; // inclusive
            let y_start = r.u16()? as u32;
            let y_end = r.u16()? as u32; // inclusive
            let bkg = r.bgr()?;
            if y_end < y_start {
                bail!("ClearCodec band has yEnd < yStart");
            }
            let vbar_height = (y_end - y_start + 1) as usize;
            if vbar_height > 52 {
                bail!("ClearCodec V-Bar height {vbar_height} exceeds 52");
            }
            if x_end < x_start {
                bail!("ClearCodec band has xEnd < xStart");
            }

            for x in x_start..=x_end {
                let header = r.u16()?;
                // A cache miss here (referencing a V-Bar/short-V-Bar index this client
                // hasn't actually populated — e.g. because the server's encoder-side cache
                // predates this connection, such as after reconnecting to an existing RDS
                // session) must NOT abort the rest of this message: doing so would skip any
                // later SHORT_VBAR_CACHE_MISS entries still to come in this same stream that
                // populate the cache, permanently widening the gap between this client's
                // cursor and the server's on every subsequent message — a self-amplifying
                // cascade confirmed empirically (missed-index-minus-cursor grew from 0 to
                // 2000+ over one session when this was a hard bail). Falling back to the
                // band's background color for just this one V-Bar keeps the rest of the
                // message — and the cache population within it — intact.
                let pixels: Vec<[u8; 3]> = if header & 0x8000 != 0 {
                    // VBAR_CACHE_HIT: reuse a previously-composed full V-Bar verbatim. The
                    // 32768-slot cache is reused across bands of different heights over a
                    // session (confirmed against FreeRDP's clear.c, which explicitly
                    // resizes here rather than trusting the stored length), so the cached
                    // entry's length may not match this band's vbar_height. Resize
                    // (truncate, or zero-extend like FreeRDP's resize_vbar_entry) instead
                    // of silently leaving extra rows unpainted.
                    let idx = (header & 0x7FFF) as usize;
                    let mut cached = self.vbar_storage[idx].clone().unwrap_or_default();
                    if cached.len() != vbar_height {
                        cached.resize(vbar_height, [0, 0, 0]);
                    }
                    cached
                } else if header & 0xC000 == 0x4000 {
                    let idx = (header & 0x3FFF) as usize;
                    let y_on = r.u8()? as usize;
                    let short = self.short_vbar_storage[idx].clone();
                    let mut full = vec![bkg; vbar_height];
                    if let Some(short) = short {
                        for (i, p) in short.iter().enumerate() {
                            if y_on + i < vbar_height {
                                full[y_on + i] = *p;
                            }
                        }
                    }
                    // A SHORT_VBAR_CACHE_HIT is also a "vBarUpdate" event per FreeRDP's
                    // clear_decompress_bands_data: the recomposed full V-Bar gets written
                    // into V-Bar Storage at the current cursor, which then advances — same
                    // as the cache-miss path below. Skipping this (as this client
                    // previously did) leaves the cursor permanently behind the server's,
                    // so every later VBAR_CACHE_HIT index reference resolves to the wrong
                    // slot for the rest of the connection.
                    self.vbar_storage[self.vbar_cursor] = Some(full.clone());
                    self.vbar_cursor = (self.vbar_cursor + 1) % VBAR_SIZE;
                    full
                } else {
                    let y_on = (header & 0xFF) as usize;
                    let y_off = ((header >> 8) & 0x3F) as usize;
                    if y_off < y_on {
                        bail!("ClearCodec short V-Bar has yOff < yOn");
                    }
                    let count = y_off - y_on;
                    if count > 52 {
                        bail!("ClearCodec short V-Bar run {count} exceeds 52");
                    }
                    let mut short_pixels = Vec::with_capacity(count);
                    for _ in 0..count {
                        short_pixels.push(r.bgr()?);
                    }
                    self.short_vbar_storage[self.short_vbar_cursor] = Some(short_pixels.clone());
                    self.short_vbar_cursor = (self.short_vbar_cursor + 1) % SHORT_VBAR_SIZE;

                    let mut full = vec![bkg; vbar_height];
                    for (i, p) in short_pixels.iter().enumerate() {
                        if y_on + i < vbar_height {
                            full[y_on + i] = *p;
                        }
                    }
                    self.vbar_storage[self.vbar_cursor] = Some(full.clone());
                    self.vbar_cursor = (self.vbar_cursor + 1) % VBAR_SIZE;
                    full
                };

                for (i, p) in pixels.iter().enumerate() {
                    let py = y_start as usize + i;
                    if py > y_end as usize {
                        break;
                    }
                    set_tile_pixel(tile, width, height, x, py as u32, *p);
                }
            }
        }
        Ok(())
    }
}

fn set_tile_pixel(tile: &mut [[u8; 3]], width: u32, height: u32, x: u32, y: u32, p: [u8; 3]) {
    if x >= width || y >= height {
        return;
    }
    tile[(y * width + x) as usize] = p;
}

fn decode_residual(data: &[u8], width: u32, height: u32, tile: &mut [[u8; 3]]) -> Result<()> {
    let mut r = ByteReader::new(data);
    let pixel_count = (width as usize) * (height as usize);
    let mut idx = 0usize;
    while r.remaining() > 0 && idx < pixel_count {
        let color = r.bgr()?;
        let run = read_run_length(&mut r)? as usize;
        for _ in 0..run {
            if idx >= pixel_count {
                break;
            }
            tile[idx] = color;
            idx += 1;
        }
    }
    Ok(())
}

fn decode_subcodecs(data: &[u8], tile_width: u32, tile_height: u32, tile: &mut [[u8; 3]]) -> Result<()> {
    let mut r = ByteReader::new(data);
    while r.remaining() > 0 {
        let x_start = r.u16()? as u32;
        let y_start = r.u16()? as u32;
        let width = r.u16()? as u32;
        let height = r.u16()? as u32;
        let bitmap_len = r.u32()? as usize;
        let sub_codec_id = r.u8()?;
        let bitmap_data = r.bytes(bitmap_len)?;

        match sub_codec_id {
            SUBCODEC_UNCOMPRESSED => {
                if bitmap_data.len() != (width as usize) * (height as usize) * 3 {
                    bail!("ClearCodec raw subcodec size mismatch");
                }
                let mut idx = 0;
                for y in 0..height {
                    for x in 0..width {
                        let p = [bitmap_data[idx], bitmap_data[idx + 1], bitmap_data[idx + 2]];
                        idx += 3;
                        set_tile_pixel(tile, tile_width, tile_height, x_start + x, y_start + y, p);
                    }
                }
            }
            SUBCODEC_RLEX => {
                decode_rlex(bitmap_data, width, height, tile_width, tile_height, tile, x_start, y_start)?;
            }
            SUBCODEC_NSCODEC => {
                let pixels = crate::nscodec::decompress(bitmap_data, width, height)?;
                for y in 0..height {
                    for x in 0..width {
                        let p = pixels[(y * width + x) as usize];
                        set_tile_pixel(tile, tile_width, tile_height, x_start + x, y_start + y, p);
                    }
                }
            }
            other => bail!("unknown ClearCodec subCodecId {other:#04x}"),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_rlex(
    data: &[u8],
    width: u32,
    height: u32,
    tile_width: u32,
    tile_height: u32,
    tile: &mut [[u8; 3]],
    dest_x: u32,
    dest_y: u32,
) -> Result<()> {
    let mut r = ByteReader::new(data);
    let palette_count = r.u8()? as usize;
    if palette_count > 0x7F {
        bail!("ClearCodec RLEX palette too large ({palette_count})");
    }
    let mut palette = Vec::with_capacity(palette_count);
    for _ in 0..palette_count {
        palette.push(r.bgr()?);
    }
    let num_bits = bits_needed((palette_count as u32).saturating_sub(1));
    let mask = ((1u16 << num_bits) - 1).max(1) as u16;

    let pixel_count = (width as usize) * (height as usize);
    let mut idx = 0usize;
    while r.remaining() > 0 && idx < pixel_count {
        let byte0 = r.u8()?;
        let stop_index = (byte0 as u16) & mask;
        let suite_depth = (byte0 >> num_bits) as u16;
        let run = read_run_length(&mut r)? as usize;

        let start_index_signed = stop_index as i32 - suite_depth as i32;
        if start_index_signed < 0 || start_index_signed as usize >= palette.len() || stop_index as usize >= palette.len() {
            bail!("ClearCodec RLEX palette index out of range");
        }
        let start_index = start_index_signed as usize;
        let start_color = palette[start_index];

        for _ in 0..run {
            if idx >= pixel_count {
                break;
            }
            let x = (idx as u32) % width;
            let y = (idx as u32) / width;
            set_tile_pixel(tile, tile_width, tile_height, dest_x + x, dest_y + y, start_color);
            idx += 1;
        }
        for i in start_index..=stop_index as usize {
            if idx >= pixel_count {
                break;
            }
            let x = (idx as u32) % width;
            let y = (idx as u32) / width;
            set_tile_pixel(tile, tile_width, tile_height, dest_x + x, dest_y + y, palette[i]);
            idx += 1;
        }
    }
    Ok(())
}

/// Blits a tightly-packed 24-bit BGR tile into a surface, reporting how much of it landed.
/// A ClearCodec draw whose destRect runs off the surface is a dropped server update like any
/// other, so the shortfall is returned rather than quietly clipped away.
fn blit_bgr_buffer(surface: &mut Surface, dest_x: u32, dest_y: u32, width: u32, height: u32, bgr: &[u8]) -> WriteOutcome {
    let mut out = WriteOutcome { requested: width as u64 * height as u64, written: 0, source_truncated: false };
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 3) as usize;
            if i + 2 >= bgr.len() {
                out.source_truncated = true;
                continue;
            }
            out.written += surface.set_pixel_bgr(dest_x + x, dest_y + y, [bgr[i], bgr[i + 1], bgr[i + 2]]).written;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header for a glyph-cache *hit*: flags = GLYPH_INDEX | GLYPH_HIT, then the index.
    fn glyph_hit_message(seq: u8, index: u16) -> Vec<u8> {
        let mut v = vec![FLAG_GLYPH_INDEX | FLAG_GLYPH_HIT, seq];
        v.extend_from_slice(&index.to_le_bytes());
        v
    }

    /// A glyph-cache *populate*: flags = GLYPH_INDEX, index, then empty residual/bands/
    /// subcodec segments, so the composed tile is all zeroes of the given size.
    fn glyph_store_message(seq: u8, index: u16) -> Vec<u8> {
        let mut v = vec![FLAG_GLYPH_INDEX, seq];
        v.extend_from_slice(&index.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // residualByteCount
        v.extend_from_slice(&0u32.to_le_bytes()); // bandsByteCount
        v.extend_from_slice(&0u32.to_le_bytes()); // subcodecByteCount
        v
    }

    /// Real traffic from the test host reuses one glyph slot at different shapes with the
    /// same pixel count — an 8x1 glyph read back as 1x8, a 4x9 read back as 6x6. FreeRDP's
    /// `clear.c` stores only the pixel count and re-reads the buffer at the requested
    /// dimensions, so these are hits, not errors. Requiring equal dimensions drew nothing and
    /// left the destination permanently unpainted.
    #[test]
    fn a_glyph_slot_may_be_reused_at_a_different_shape_with_the_same_pixel_count() {
        let mut ctx = ClearCodecContext::new();
        let mut surface = Surface::new(64, 64);

        // Populate slot 329 as 8x1.
        let _ = ctx.decompress(&glyph_store_message(0, 329), &mut surface, 0, 0, 8, 1).expect("store 8x1");
        // Read it back as 1x8 — same 8 pixels, transposed shape.
        let outcome = ctx.decompress(&glyph_hit_message(1, 329), &mut surface, 0, 0, 1, 8).expect("1x8 hit must draw");
        assert!(outcome.complete(), "the whole 1x8 rect must be painted");

        // And the 4x9 -> 6x6 case (36 pixels either way).
        let _ = ctx.decompress(&glyph_store_message(2, 254), &mut surface, 0, 0, 4, 9).expect("store 4x9");
        let outcome = ctx.decompress(&glyph_hit_message(3, 254), &mut surface, 0, 0, 6, 6).expect("6x6 hit must draw");
        assert!(outcome.complete());
    }

    #[test]
    fn a_glyph_slot_too_small_for_the_request_is_reported_not_drawn() {
        let mut ctx = ClearCodecContext::new();
        let mut surface = Surface::new(64, 64);
        let _ = ctx.decompress(&glyph_store_message(0, 7), &mut surface, 0, 0, 2, 2).expect("store 2x2");
        // 4x4 needs 16 pixels; the slot holds 4. Drawing it would read past the buffer.
        let err = ctx.decompress(&glyph_hit_message(1, 7), &mut surface, 0, 0, 4, 4).unwrap_err();
        assert!(err.to_string().contains("too few"), "got: {err}");
    }

    #[test]
    fn a_glyph_hit_on_an_unpopulated_slot_is_reported() {
        let mut ctx = ClearCodecContext::new();
        let mut surface = Surface::new(16, 16);
        let err = ctx.decompress(&glyph_hit_message(0, 100), &mut surface, 0, 0, 4, 4).unwrap_err();
        assert!(err.to_string().contains("never populated"), "got: {err}");
    }

    #[test]
    fn a_sequence_number_gap_is_rejected() {
        // The seqNumber check is what proves the transport is not dropping whole ClearCodec
        // messages; it must stay strict.
        let mut ctx = ClearCodecContext::new();
        let mut surface = Surface::new(16, 16);
        let _ = ctx.decompress(&glyph_store_message(0, 1), &mut surface, 0, 0, 2, 2).unwrap();
        let err = ctx.decompress(&glyph_store_message(5, 2), &mut surface, 0, 0, 2, 2).unwrap_err();
        assert!(err.to_string().contains("seqNumber gap"), "got: {err}");
    }
}
