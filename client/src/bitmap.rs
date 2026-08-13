use anyhow::{bail, Context, Result};

const BITMAP_COMPRESSION: u16 = 0x0001;
const NO_BITMAP_COMPRESSION_HDR: u16 = 0x0400;

pub struct BitmapRect {
    pub dest_left: u16,
    pub dest_top: u16,
    pub width: u16,
    pub height: u16,
    /// BGRX8888, top-down, row-major — ready for `window::BitmapTile`.
    pub pixels: Vec<u8>,
    pub stride: usize,
}

/// Parses an Update Data PDU payload (starting at `updateType`, i.e. the Data PDU payload
/// per `capabilities::data_pdu_payload`). Returns `None` for non-bitmap update types
/// (Orders, Palette, Synchronize, ...) instead of erroring — we claim symmetric primary
/// drawing order support in Confirm Active (real Windows hosts reject an all-zero
/// orderSupport outright) without actually implementing an order decoder, so seeing an
/// Orders update here is expected and just gets skipped, not treated as a protocol error.
pub fn parse_update(data: &[u8]) -> Result<Option<Vec<BitmapRect>>> {
    if data.len() < 4 {
        bail!("Update PDU too short ({} bytes)", data.len());
    }
    let update_type = u16::from_le_bytes([data[0], data[1]]);
    if update_type != 0x0001 {
        return Ok(None);
    }
    parse_bitmap_update(data).map(Some)
}

fn parse_bitmap_update(data: &[u8]) -> Result<Vec<BitmapRect>> {
    let number_rectangles = u16::from_le_bytes([data[2], data[3]]) as usize;

    let mut pos = 4;
    let mut out = Vec::with_capacity(number_rectangles);
    for _ in 0..number_rectangles {
        if pos + 18 > data.len() {
            bail!("TS_BITMAP_DATA truncated");
        }
        let dest_left = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let dest_top = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        let _dest_right = u16::from_le_bytes([data[pos + 4], data[pos + 5]]);
        let _dest_bottom = u16::from_le_bytes([data[pos + 6], data[pos + 7]]);
        let width = u16::from_le_bytes([data[pos + 8], data[pos + 9]]);
        let height = u16::from_le_bytes([data[pos + 10], data[pos + 11]]);
        let bits_per_pixel = u16::from_le_bytes([data[pos + 12], data[pos + 13]]);
        let flags = u16::from_le_bytes([data[pos + 14], data[pos + 15]]);
        let mut bitmap_length = u16::from_le_bytes([data[pos + 16], data[pos + 17]]) as usize;
        pos += 18;

        let compressed = flags & BITMAP_COMPRESSION != 0;
        let mut uncompressed_size_hint = None;
        if compressed && flags & NO_BITMAP_COMPRESSION_HDR == 0 {
            if pos + 8 > data.len() {
                bail!("TS_CD_HEADER truncated");
            }
            let cb_comp_main_body_size = u16::from_le_bytes([data[pos + 2], data[pos + 3]]) as usize;
            let cb_uncompressed_size = u16::from_le_bytes([data[pos + 6], data[pos + 7]]);
            bitmap_length = cb_comp_main_body_size;
            uncompressed_size_hint = Some(cb_uncompressed_size as usize);
            pos += 8;
        }

        if pos + bitmap_length > data.len() {
            bail!("bitmapDataStream truncated (need {bitmap_length}, have {})", data.len() - pos);
        }
        let stream = &data[pos..pos + bitmap_length];
        pos += bitmap_length;

        let bytes_per_pixel = match bits_per_pixel {
            8 => 1,
            15 | 16 => 2,
            24 => 3,
            other => bail!("unsupported bitsPerPixel {other}"),
        };

        let pixels = if compressed {
            decode_rle(stream, width as usize, height as usize, bytes_per_pixel)
                .with_context(|| format!("RLE-decoding {width}x{height} @{bits_per_pixel}bpp bitmap"))?
        } else {
            decode_raw(stream, width as usize, height as usize, bytes_per_pixel)?
        };
        let _ = uncompressed_size_hint;

        let bgrx = to_bgrx8888(&pixels, width as usize, height as usize, bytes_per_pixel);
        out.push(BitmapRect {
            dest_left,
            dest_top,
            width,
            height,
            pixels: bgrx,
            stride: width as usize * 4,
        });
    }
    Ok(out)
}

/// Raw (uncompressed) bitmap data: bottom-up rows, each padded to a 4-byte boundary.
/// Returns top-down, unpadded rows (bytes_per_pixel * width per row).
fn decode_raw(data: &[u8], width: usize, height: usize, bpp: usize) -> Result<Vec<u8>> {
    let row_size = width * bpp;
    let padded_row_size = (row_size + 3) & !3;
    if data.len() < padded_row_size * height {
        bail!(
            "raw bitmap data too short: need {} bytes, have {}",
            padded_row_size * height,
            data.len()
        );
    }
    let mut out = vec![0u8; row_size * height];
    for y in 0..height {
        let src_row = height - 1 - y; // bottom-up -> top-down
        let src = &data[src_row * padded_row_size..src_row * padded_row_size + row_size];
        out[y * row_size..(y + 1) * row_size].copy_from_slice(src);
    }
    Ok(out)
}

/// Converts decoded pixel data (top-down, `bpp` bytes/pixel, no row padding) to BGRX8888.
fn to_bgrx8888(data: &[u8], width: usize, height: usize, bpp: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 4];
    let row_size = width * bpp;
    for y in 0..height {
        for x in 0..width {
            let src = &data[y * row_size + x * bpp..];
            let (b, g, r) = match bpp {
                1 => {
                    // 8bpp palette isn't tracked (no Palette Update handling yet) — treat
                    // the index as a grayscale value so something reasonable renders.
                    let v = src[0];
                    (v, v, v)
                }
                2 => {
                    let v = u16::from_le_bytes([src[0], src[1]]);
                    // Assume 5-6-5 (RNS_UD_COLOR_16BPP_565); close enough for 5-5-5 too.
                    let r5 = ((v >> 11) & 0x1F) as u8;
                    let g6 = ((v >> 5) & 0x3F) as u8;
                    let b5 = (v & 0x1F) as u8;
                    ((b5 << 3) | (b5 >> 2), (g6 << 2) | (g6 >> 4), (r5 << 3) | (r5 >> 2))
                }
                3 => (src[0], src[1], src[2]),
                _ => (0, 0, 0),
            };
            let dst = (y * width + x) * 4;
            out[dst] = b;
            out[dst + 1] = g;
            out[dst + 2] = r;
            out[dst + 3] = 0xFF;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Interleaved RLE Bitmap Codec (MS-RDPBCGR 2.2.9.1.1.3.1.2.4 / §3.1.9). RLE
// decompression is NOT optional per spec: "All clients have to be capable of
// decompressing compressed bitmap data; this capability is not negotiable."
// ---------------------------------------------------------------------------

fn decode_rle(data: &[u8], width: usize, height: usize, bpp: usize) -> Result<Vec<u8>> {
    let row_delta = width * bpp;
    let mut out = vec![0u8; row_delta * height];
    let mut src = 0usize;
    let mut dst = 0usize;
    let mut fg_pel: u32 = white(bpp);
    let mut insert_fg_pel = false;

    let read_pel = |b: &[u8], off: usize| -> u32 {
        match bpp {
            1 => b[off] as u32,
            2 => u16::from_le_bytes([b[off], b[off + 1]]) as u32,
            3 => (b[off] as u32) | ((b[off + 1] as u32) << 8) | ((b[off + 2] as u32) << 16),
            _ => 0,
        }
    };
    let write_pel = |b: &mut [u8], off: usize, v: u32| match bpp {
        1 => b[off] = v as u8,
        2 => b[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes()),
        3 => {
            b[off] = v as u8;
            b[off + 1] = (v >> 8) as u8;
            b[off + 2] = (v >> 16) as u8;
        }
        _ => {}
    };
    let get_prev = |out: &[u8], dst: usize, row_delta: usize| -> u32 {
        if dst < row_delta {
            0
        } else {
            read_pel(out, dst - row_delta)
        }
    };

    while src < data.len() && dst < out.len() {
        let first_line = dst < row_delta;
        let header = data[src];
        let code = header >> 5; // regular-form code (top 3 bits)
        let mega = header; // full byte, for MEGA_MEGA (top-bits==0b111) comparisons

        // --- Background Run ---
        if code == 0x0 || mega == 0xF0 {
            let (run_len, adv) = if mega == 0xF0 {
                (u16::from_le_bytes([data[src + 1], data[src + 2]]) as usize, 3)
            } else {
                let rl = (header & 0x1F) as usize;
                if rl == 0 {
                    ((data[src + 1] as usize) + 32, 2)
                } else {
                    (rl, 1)
                }
            };
            src += adv;
            let mut remaining = run_len;
            if insert_fg_pel && remaining > 0 {
                let v = if first_line { fg_pel } else { get_prev(&out, dst, row_delta) ^ fg_pel };
                write_pel(&mut out, dst, v);
                dst += bpp;
                remaining -= 1;
            }
            for _ in 0..remaining {
                if dst + bpp > out.len() {
                    break;
                }
                let v = if first_line { 0 } else { get_prev(&out, dst, row_delta) };
                write_pel(&mut out, dst, v);
                dst += bpp;
            }
            insert_fg_pel = true;
            continue;
        }
        insert_fg_pel = false;

        // --- Foreground Run (regular, mega, lite-set, mega-set) ---
        if code == 0x1 || mega == 0xF1 || (header >> 4) == 0xC || mega == 0xF6 {
            let is_set = (header >> 4) == 0xC || mega == 0xF6;
            let (run_len, adv) = if mega == 0xF1 || mega == 0xF6 {
                (u16::from_le_bytes([data[src + 1], data[src + 2]]) as usize, 3)
            } else if is_set {
                let rl = (header & 0x0F) as usize;
                if rl == 0 {
                    ((data[src + 1] as usize) + 16, 2)
                } else {
                    (rl, 1)
                }
            } else {
                let rl = (header & 0x1F) as usize;
                if rl == 0 {
                    ((data[src + 1] as usize) + 32, 2)
                } else {
                    (rl, 1)
                }
            };
            src += adv;
            if is_set {
                fg_pel = read_pel(data, src);
                src += bpp;
            }
            for _ in 0..run_len {
                if dst + bpp > out.len() {
                    break;
                }
                let v = if first_line { fg_pel } else { get_prev(&out, dst, row_delta) ^ fg_pel };
                write_pel(&mut out, dst, v);
                dst += bpp;
            }
            continue;
        }

        // --- Dithered Run (lite, mega) ---
        if (header >> 4) == 0xE || mega == 0xF8 {
            let (run_len, adv) = if mega == 0xF8 {
                (u16::from_le_bytes([data[src + 1], data[src + 2]]) as usize, 3)
            } else {
                let rl = (header & 0x0F) as usize;
                if rl == 0 {
                    ((data[src + 1] as usize) + 16, 2)
                } else {
                    (rl, 1)
                }
            };
            src += adv;
            let color_a = read_pel(data, src);
            let color_b = read_pel(data, src + bpp);
            src += 2 * bpp;
            for i in 0..run_len {
                for &c in &[color_a, color_b] {
                    if dst + bpp > out.len() {
                        break;
                    }
                    write_pel(&mut out, dst, c);
                    dst += bpp;
                }
                let _ = i;
            }
            continue;
        }

        // --- Color Run (regular, mega) ---
        if code == 0x3 || mega == 0xF3 {
            let (run_len, adv) = if mega == 0xF3 {
                (u16::from_le_bytes([data[src + 1], data[src + 2]]) as usize, 3)
            } else {
                let rl = (header & 0x1F) as usize;
                if rl == 0 {
                    ((data[src + 1] as usize) + 32, 2)
                } else {
                    (rl, 1)
                }
            };
            src += adv;
            let color = read_pel(data, src);
            src += bpp;
            for _ in 0..run_len {
                if dst + bpp > out.len() {
                    break;
                }
                write_pel(&mut out, dst, color);
                dst += bpp;
            }
            continue;
        }

        // --- FG/BG Image (regular, mega, lite-set, mega-set) ---
        if code == 0x2 || mega == 0xF2 || (header >> 4) == 0xD || mega == 0xF7 {
            let is_set = (header >> 4) == 0xD || mega == 0xF7;
            let (run_len, adv) = if mega == 0xF2 || mega == 0xF7 {
                (u16::from_le_bytes([data[src + 1], data[src + 2]]) as usize, 3)
            } else if is_set {
                let rl = (header & 0x0F) as usize;
                if rl == 0 {
                    ((data[src + 1] as usize) + 1, 2)
                } else {
                    (rl * 8, 1)
                }
            } else {
                let rl = (header & 0x1F) as usize;
                if rl == 0 {
                    ((data[src + 1] as usize) + 1, 2)
                } else {
                    (rl * 8, 1)
                }
            };
            src += adv;
            if is_set {
                fg_pel = read_pel(data, src);
                src += bpp;
            }
            let mut remaining = run_len;
            while remaining > 0 {
                let bitmask = data[src];
                src += 1;
                let n = remaining.min(8);
                for bit in 0..n {
                    if dst + bpp > out.len() {
                        break;
                    }
                    let set = bitmask & (1 << bit) != 0;
                    let v = if first_line {
                        if set {
                            fg_pel
                        } else {
                            0
                        }
                    } else {
                        let prev = get_prev(&out, dst, row_delta);
                        if set {
                            prev ^ fg_pel
                        } else {
                            prev
                        }
                    };
                    write_pel(&mut out, dst, v);
                    dst += bpp;
                }
                remaining -= n;
            }
            continue;
        }

        // --- Color Image (regular, mega): raw pixel copy ---
        if code == 0x4 || mega == 0xF4 {
            let (run_len, adv) = if mega == 0xF4 {
                (u16::from_le_bytes([data[src + 1], data[src + 2]]) as usize, 3)
            } else {
                let rl = (header & 0x1F) as usize;
                if rl == 0 {
                    ((data[src + 1] as usize) + 32, 2)
                } else {
                    (rl, 1)
                }
            };
            src += adv;
            let n = run_len * bpp;
            if src + n > data.len() || dst + n > out.len() {
                bail!("RLE color image run overruns buffer");
            }
            out[dst..dst + n].copy_from_slice(&data[src..src + n]);
            src += n;
            dst += n;
            continue;
        }

        // --- Special orders (no run length) ---
        match mega {
            0xF9 | 0xFA => {
                // SPECIAL_FGBG_1 (bitmask 0x03) / SPECIAL_FGBG_2 (bitmask 0x05): fixed 8-pixel chunk.
                src += 1;
                let bitmask: u8 = if mega == 0xF9 { 0x03 } else { 0x05 };
                for bit in 0..8 {
                    if dst + bpp > out.len() {
                        break;
                    }
                    let set = bitmask & (1 << bit) != 0;
                    let v = if first_line {
                        if set {
                            fg_pel
                        } else {
                            0
                        }
                    } else {
                        let prev = get_prev(&out, dst, row_delta);
                        if set {
                            prev ^ fg_pel
                        } else {
                            prev
                        }
                    };
                    write_pel(&mut out, dst, v);
                    dst += bpp;
                }
                continue;
            }
            0xFD => {
                src += 1;
                if dst + bpp <= out.len() {
                    write_pel(&mut out, dst, white(bpp));
                    dst += bpp;
                }
                continue;
            }
            0xFE => {
                src += 1;
                if dst + bpp <= out.len() {
                    write_pel(&mut out, dst, 0);
                    dst += bpp;
                }
                continue;
            }
            _ => bail!("unrecognized RLE order code {header:#04x} at src offset {src}"),
        }
    }

    Ok(out)
}

fn white(bpp: usize) -> u32 {
    match bpp {
        1 => 0xFF,
        2 => 0xFFFF, // works for both 5-5-5 (0x7FFF) and 5-6-5 (0xFFFF) close enough visually
        3 => 0x00FF_FFFF,
        _ => 0,
    }
}
