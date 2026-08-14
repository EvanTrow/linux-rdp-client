use anyhow::{bail, Result};

fn round_up(n: u32, mult: u32) -> u32 {
    n.div_ceil(mult) * mult
}

struct PlaneLayout {
    /// Row stride (in bytes/pixels) of the luma plane's storage — equals `width` when chroma
    /// subsampling is off, but rounded up to a multiple of 8 when it's on (padding, even
    /// though luma itself is never subsampled).
    luma_stride: u32,
    luma_expected: usize,
    /// Row stride of the Co/Cg chroma planes' storage (`luma_stride / 2` when subsampling is
    /// on — same rounded-up width, not the raw image width).
    chroma_stride: u32,
    chroma_expected: usize,
    alpha_expected: usize,
}

fn plane_layout(width: u32, height: u32, subsampling: bool) -> PlaneLayout {
    let alpha_expected = (width as usize) * (height as usize);
    if subsampling {
        let luma_stride = round_up(width, 8);
        let chroma_stride = luma_stride / 2;
        let chroma_rows = round_up(height, 2) / 2;
        PlaneLayout {
            luma_stride,
            luma_expected: (luma_stride as usize) * (height as usize),
            chroma_stride,
            chroma_expected: (chroma_stride as usize) * (chroma_rows as usize),
            alpha_expected,
        }
    } else {
        PlaneLayout {
            luma_stride: width,
            luma_expected: alpha_expected,
            chroma_stride: width,
            chroma_expected: alpha_expected,
            alpha_expected,
        }
    }
}

/// MS-RDPNSC §3.1.8.1 per-plane RLE: byte-oriented, a repeated byte pair (`value, value`)
/// starts a run whose length follows (a 1-byte short form 0x00-0xFE => length 2-256, or a
/// 0xFF escape followed by a 4-byte LE length); anything else is a literal single byte. The
/// final 4 bytes of every plane are always raw/unencoded (`EndData`) — the `left == 5`
/// special case forces the byte just before that boundary to be treated as a literal even if
/// it would otherwise look like the start of a run, guaranteeing the loop only ever exits
/// with exactly 4 bytes remaining for that unconditional tail copy.
fn rle_decode(data: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    if expected_size < 4 {
        bail!("NSCodec RLE plane smaller than 4 bytes ({expected_size})");
    }
    let mut out = vec![0u8; expected_size];
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    let mut left = expected_size;

    while left > 4 {
        let value = *data.get(in_pos).ok_or_else(|| anyhow::anyhow!("NSCodec RLE input truncated"))?;
        in_pos += 1;

        if left == 5 {
            out[out_pos] = value;
            out_pos += 1;
            left -= 1;
            continue;
        }

        if data.get(in_pos) == Some(&value) {
            in_pos += 1;
            let ctrl = *data.get(in_pos).ok_or_else(|| anyhow::anyhow!("NSCodec RLE run length truncated"))?;
            in_pos += 1;
            let run_len = if ctrl < 0xFF {
                ctrl as usize + 2
            } else {
                let b = data
                    .get(in_pos..in_pos + 4)
                    .ok_or_else(|| anyhow::anyhow!("NSCodec RLE long run length truncated"))?;
                in_pos += 4;
                u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize
            };
            if run_len > left - 4 {
                bail!("NSCodec RLE run length {run_len} overruns plane (left={left})");
            }
            let dst = out
                .get_mut(out_pos..out_pos + run_len)
                .ok_or_else(|| anyhow::anyhow!("NSCodec RLE run overflows output buffer"))?;
            dst.fill(value);
            out_pos += run_len;
            left -= run_len;
        } else {
            out[out_pos] = value;
            out_pos += 1;
            left -= 1;
        }
    }

    // Unconditional final 4-byte raw tail (`EndData`) — the `left == 5` case above guarantees
    // the loop only ever exits with out_pos here exactly `expected_size - 4`.
    if out_pos + 4 != expected_size {
        bail!("NSCodec RLE plane didn't land on the expected 4-byte tail boundary");
    }
    let tail = data
        .get(in_pos..in_pos + 4)
        .ok_or_else(|| anyhow::anyhow!("NSCodec RLE tail input truncated"))?;
    out[out_pos..out_pos + 4].copy_from_slice(tail);
    Ok(out)
}

/// Decodes one plane's bytes (empty/RLE/raw, decided by comparing the wire byte count
/// against the independently-computed expected raw size) into exactly `expected_size` bytes.
fn decode_plane(data: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    if data.is_empty() {
        // No data sent for this plane at all (spec formally restricts this to Alpha, but
        // FreeRDP applies the same 0xFF-fill fallback generically for any plane).
        return Ok(vec![0xFFu8; expected_size]);
    }
    if data.len() >= expected_size {
        let raw = data
            .get(..expected_size)
            .ok_or_else(|| anyhow::anyhow!("NSCodec raw plane shorter than expected"))?;
        return Ok(raw.to_vec());
    }
    rle_decode(data, expected_size)
}

/// Decodes an `NSCODEC_BITMAP_STREAM` (MS-RDPNSC §2.2.2) — as delegated to unmodified from
/// `CLEARCODEC_SUBCODEC.bitmapData` when `subCodecId=0x01` — into `width*height` BGR
/// triplets. `width`/`height` come from the encapsulating structure, never from this stream.
pub fn decompress(data: &[u8], width: u32, height: u32) -> Result<Vec<[u8; 3]>> {
    if data.len() < 20 {
        bail!("NSCodec header truncated ({} bytes)", data.len());
    }
    let plane_byte_counts = [
        u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize,
        u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize,
        u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize,
        u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize,
    ];
    let color_loss_level = data[16];
    if !(1..=7).contains(&color_loss_level) {
        bail!("NSCodec ColorLossLevel {color_loss_level} out of range (1-7)");
    }
    let chroma_subsampling = data[17] != 0;
    // data[18..20] reserved, ignored.

    let layout = plane_layout(width, height, chroma_subsampling);
    let expected = [layout.luma_expected, layout.chroma_expected, layout.chroma_expected, layout.alpha_expected];

    let mut pos = 20usize;
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(4);
    for (i, &count) in plane_byte_counts.iter().enumerate() {
        let plane_data = data
            .get(pos..pos + count)
            .ok_or_else(|| anyhow::anyhow!("NSCodec plane {i} data truncated"))?;
        planes.push(decode_plane(plane_data, expected[i])?);
        pos += count;
    }
    let (luma, co, cg, alpha) = (&planes[0], &planes[1], &planes[2], &planes[3]);

    // Per FreeRDP's nsc_decode: shift = ColorLossLevel - 1, not ColorLossLevel itself — an
    // easy off-by-one against the spec prose alone that produces plausible-but-desaturated
    // colors rather than an obvious failure.
    let shift = (color_loss_level - 1) as i16;

    let mut out = vec![[0u8; 3]; (width as usize) * (height as usize)];
    for y in 0..height {
        let (luma_row, chroma_row) = if chroma_subsampling {
            (y * layout.luma_stride, (y >> 1) * layout.chroma_stride)
        } else {
            (y * width, y * width)
        };
        for x in 0..width {
            let luma_idx = (luma_row + x) as usize;
            let chroma_idx = if chroma_subsampling { (chroma_row + (x >> 1)) as usize } else { (chroma_row + x) as usize };
            let alpha_idx = (y * width + x) as usize;

            let y_val = *luma.get(luma_idx).ok_or_else(|| anyhow::anyhow!("NSCodec luma index out of range"))? as i16;
            let co_raw = *co.get(chroma_idx).ok_or_else(|| anyhow::anyhow!("NSCodec Co index out of range"))? as i16;
            let cg_raw = *cg.get(chroma_idx).ok_or_else(|| anyhow::anyhow!("NSCodec Cg index out of range"))? as i16;
            let _ = alpha.get(alpha_idx); // decoded for correctness/bounds-checking; unused — our surfaces have no alpha channel

            // (INT16)(INT8)(((INT16)*plane) << shift) — widen, shift, truncate to i8
            // (reinterpreting the low byte as signed), sign-extend back to i16.
            let co_val = (((co_raw << shift) as i8) as i16).max(-255).min(255);
            let cg_val = (((cg_raw << shift) as i8) as i16).max(-255).min(255);

            let r = (y_val + co_val - cg_val).clamp(0, 255) as u8;
            let g = (y_val + cg_val).clamp(0, 255) as u8;
            let b = (y_val - co_val - cg_val).clamp(0, 255) as u8;
            out[(y * width + x) as usize] = [b, g, r];
        }
    }
    Ok(out)
}
