use crate::surface::Surface;
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
    glyph_cache: Vec<Option<(u16, u16, Vec<u8>)>>,
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
    ) -> Result<()> {
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
                let (gw, gh, pixels) = self.glyph_cache[idx]
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("ClearCodec glyph cache miss at index {idx}"))?;
                if gw as u32 != width || gh as u32 != height {
                    bail!("ClearCodec glyph cache size mismatch at index {idx}");
                }
                blit_bgr_buffer(surface, dest_x, dest_y, width, height, &pixels);
                return Ok(());
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
        blit_bgr_buffer(surface, dest_x, dest_y, width, height, &flat);

        if let Some(idx) = glyph_index {
            self.glyph_cache[idx] = Some((width as u16, height as u16, flat));
        }
        Ok(())
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
                let pixels: Vec<[u8; 3]> = if header & 0x8000 != 0 {
                    let idx = (header & 0x7FFF) as usize;
                    self.vbar_storage[idx]
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("ClearCodec V-Bar cache miss at index {idx}"))?
                } else if header & 0xC000 == 0x4000 {
                    let idx = (header & 0x3FFF) as usize;
                    let y_on = r.u8()? as usize;
                    let short = self.short_vbar_storage[idx]
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("ClearCodec short V-Bar cache miss at index {idx}"))?;
                    let mut full = vec![bkg; vbar_height];
                    for (i, p) in short.iter().enumerate() {
                        if y_on + i < vbar_height {
                            full[y_on + i] = *p;
                        }
                    }
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
                bail!("ClearCodec subCodecId=NSCodec (0x01) not implemented");
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

fn blit_bgr_buffer(surface: &mut Surface, dest_x: u32, dest_y: u32, width: u32, height: u32, bgr: &[u8]) {
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 3) as usize;
            if i + 2 >= bgr.len() {
                continue;
            }
            surface.set_pixel_bgr(dest_x + x, dest_y + y, [bgr[i], bgr[i + 1], bgr[i + 2]]);
        }
    }
}
