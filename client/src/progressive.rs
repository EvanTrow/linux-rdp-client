//! RemoteFX Progressive (`RDPGFX_CODECID_CAPROGRESSIVE`, 0x0009) decoder for
//! `RDPGFX_WIRE_TO_SURFACE_PDU_2` (MS-RDPEGFX §2.2.4.2). Wire formats, the RLGR1 entropy
//! decoder, and the non-extrapolate 3-level inverse DWT are all cross-checked byte-exact
//! against FreeRDP's `libfreerdp/codec/progressive.c`, `rfx_rlgr.c`, `rfx_dwt.c`, and
//! `libfreerdp/primitives/prim_colors.c` (see `phase2_gfx_pipeline.md` memory for the
//! original wire-format research this was built from).
//!
//! Scoped to TILE_SIMPLE/TILE_FIRST at any progressive quality, in both the plain and the
//! `RFX_DWT_REDUCE_EXTRAPOLATE` IDWT variants (this host uses extrapolate mode for the
//! majority of its traffic). `RFX_TILE_DIFFERENCE` tiles are applied by adding their delta
//! onto the cached per-tile baseline in `ProgressiveContext::tile_baseline`.
//!
//! `RFX_TILE_DIFFERENCE` deltas accumulate into that baseline — see `accumulate_diff`.
//!
//! A note on dequant shifts, because this was got wrong once and the wrong version looked
//! reasonable: a diff tile's shift often differs from the shift its baseline was decoded
//! with, and that is fine. Dequantization is precisely what maps quantized coefficients into
//! the shared absolute coefficient domain, so two values dequantized with *different* shifts
//! are already on the *same* scale and adding them is correct. FreeRDP has no shift-
//! consistency check anywhere in this path. An earlier version of this file skipped such
//! tiles (about a third of all diff tiles on this host), which threw away real content; the
//! smearing that motivated the check was actually the missing baseline accumulation.
//!
//! `TILE_UPGRADE` blocks are implemented too: each one refines an already-decoded tile by
//! reading additional low-order bit planes, from an SRL stream for coefficients that are
//! still zero and from a raw stream for those already significant. See `decode_tile_upgrade`
//! and `UpgradeState`. Skipping these (as this decoder did until 2026-08-14, several
//! thousand times per session) left tiles stuck at their first-pass quality, which showed up
//! as chroma banding on small colourful UI — taskbar icons especially.
//!
//! Tile placement: tiles are positioned at `(origin + xIdx*64, origin + yIdx*64)` in surface
//! coordinates, where `origin` is the enclosing command's destination — (0, 0) for
//! `WIRE_TO_SURFACE_2`, which carries no destRect, and the destRect's top-left for a
//! CAProgressive `WIRE_TO_SURFACE_1`. The same origin is added to the region's clipping
//! rects, matching FreeRDP's `progressive_decompress`. Tile indices are *not* absolute
//! screen coordinates; treating them as such happens to work whenever the origin is zero.

use crate::surface::Surface;
use anyhow::{bail, Result};

const WBT_SYNC: u16 = 0xCCC0;
const WBT_FRAME_BEGIN: u16 = 0xCCC1;
const WBT_FRAME_END: u16 = 0xCCC2;
const WBT_CONTEXT: u16 = 0xCCC3;
const WBT_REGION: u16 = 0xCCC4;
const WBT_TILE_SIMPLE: u16 = 0xCCC5;
const WBT_TILE_FIRST: u16 = 0xCCC6;
const WBT_TILE_UPGRADE: u16 = 0xCCC7;

const REGION_FLAG_DWT_REDUCE_EXTRAPOLATE: u8 = 0x01;
const TILE_FLAG_DIFFERENCE: u8 = 0x01;

struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
}


impl<'a> ByteReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or_else(|| anyhow::anyhow!("Progressive stream truncated"))?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.data.len() - self.pos < n {
            bail!("Progressive stream truncated");
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
}

/// `RFX_COMPONENT_CODEC_QUANT` (base quant) and the per-component quant deltas inside
/// `RFX_PROGRESSIVE_CODEC_QUANT` share the same 10-nibble shape, so one type covers both.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct ComponentQuant {
    hl1: i32,
    lh1: i32,
    hh1: i32,
    hl2: i32,
    lh2: i32,
    hh2: i32,
    hl3: i32,
    lh3: i32,
    hh3: i32,
    ll3: i32,
}

impl ComponentQuant {
    /// 5 bytes = 10 nibbles, order LL3,HL3,LH3,HH3,HL2,LH2,HH2,HL1,LH1,HH1.
    fn read(r: &mut ByteReader) -> Result<Self> {
        let b0 = r.u8()?;
        let ll3 = (b0 & 0x0F) as i32;
        let hl3 = (b0 >> 4) as i32;
        let b1 = r.u8()?;
        let lh3 = (b1 & 0x0F) as i32;
        let hh3 = (b1 >> 4) as i32;
        let b2 = r.u8()?;
        let hl2 = (b2 & 0x0F) as i32;
        let lh2 = (b2 >> 4) as i32;
        let b3 = r.u8()?;
        let hh2 = (b3 & 0x0F) as i32;
        let hl1 = (b3 >> 4) as i32;
        let b4 = r.u8()?;
        let lh1 = (b4 & 0x0F) as i32;
        let hh1 = (b4 >> 4) as i32;
        Ok(Self { hl1, lh1, hh1, hl2, lh2, hh2, hl3, lh3, hh3, ll3 })
    }

    fn add(&self, o: &Self) -> Self {
        Self {
            hl1: self.hl1 + o.hl1,
            lh1: self.lh1 + o.lh1,
            hh1: self.hh1 + o.hh1,
            hl2: self.hl2 + o.hl2,
            lh2: self.lh2 + o.lh2,
            hh2: self.hh2 + o.hh2,
            hl3: self.hl3 + o.hl3,
            lh3: self.lh3 + o.lh3,
            hh3: self.hh3 + o.hh3,
            ll3: self.ll3 + o.ll3,
        }
    }

    /// The ten subbands in the order they appear in the coefficient buffer:
    /// HL1, LH1, HH1, HL2, LH2, HH2, HL3, LH3, HH3, LL3.
    fn bands(&self) -> [i32; 10] {
        [self.hl1, self.lh1, self.hh1, self.hl2, self.lh2, self.hh2, self.hl3, self.lh3, self.hh3, self.ll3]
    }

    /// Per-subband `self - other`, used to turn two bit positions into the number of new
    /// bits an upgrade pass carries. A negative result means the server moved a subband's
    /// bit position *up*, which is not a legal refinement.
    fn checked_sub(&self, other: &Self) -> Result<[i32; 10]> {
        let (a, b) = (self.bands(), other.bands());
        let mut out = [0i32; 10];
        for i in 0..10 {
            if a[i] < b[i] {
                bail!("Progressive upgrade would raise subband {i}'s bit position ({} -> {})", a[i], b[i]);
            }
            out[i] = a[i] - b[i];
        }
        Ok(out)
    }

    /// The dequant shift used by `progressive_rfx_decode_component` is `(quant +
    /// quantProg) - 1` per subband (FreeRDP comment: "-6 + 5 = -1"). Bails, matching
    /// FreeRDP's `progressive_rfx_quant_lsub` returning FALSE, if that would go negative.
    fn sub1(&self) -> Result<Self> {
        let f = |v: i32| -> Result<i32> {
            if v < 1 {
                bail!("Progressive quant underflow (shift would be negative)");
            }
            Ok(v - 1)
        };
        Ok(Self {
            hl1: f(self.hl1)?,
            lh1: f(self.lh1)?,
            hh1: f(self.hh1)?,
            hl2: f(self.hl2)?,
            lh2: f(self.lh2)?,
            hh2: f(self.hh2)?,
            hl3: f(self.hl3)?,
            lh3: f(self.lh3)?,
            hh3: f(self.hh3)?,
            ll3: f(self.ll3)?,
        })
    }
}

/// MSB-first bit reader over a byte slice, matching the bit order `BitStream_*` (FreeRDP's
/// `winpr/bitstream.h`) reads in — bit 7 of byte 0 first.
struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
    total_bits: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0, total_bits: data.len() * 8 }
    }

    fn remaining(&self) -> usize {
        self.total_bits - self.bit_pos
    }

    fn peek_bit(&self) -> Option<u8> {
        if self.bit_pos >= self.total_bits {
            return None;
        }
        let byte = self.data[self.bit_pos / 8];
        Some((byte >> (7 - (self.bit_pos % 8))) & 1)
    }

    fn read_bit(&mut self) -> Option<u8> {
        let b = self.peek_bit()?;
        self.bit_pos += 1;
        Some(b)
    }

    fn read_bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit().unwrap_or(0) as u32;
        }
        v
    }

    /// Consumes bits equal to `target` until a different bit is seen (not consumed) or the
    /// stream is exhausted. Returns the count consumed — the unary run-length prefix used
    /// throughout RLGR1.
    fn consume_run(&mut self, target: u8) -> u32 {
        let mut count = 0u32;
        while self.peek_bit() == Some(target) {
            self.bit_pos += 1;
            count += 1;
        }
        count
    }
}

const RLGR_KPMAX: u32 = 80;
const RLGR_LSGR: u32 = 3;
const RLGR_UP_GR: u32 = 4;
const RLGR_DN_GR: u32 = 6;
const RLGR_UQ_GR: u32 = 3;
const RLGR_DQ_GR: u32 = 3;

/// RLGR1 entropy decode (MS-RDPRFX §3.1.8.1.7.3), byte-exact port of FreeRDP's
/// `rfx_rlgr_decode(RLGR1, ...)`. Decodes into `out`, zero-padding whatever the bitstream
/// doesn't cover (a truncated/short stream is not an error here, matching the reference).
fn rlgr1_decode(data: &[u8], out: &mut [i16; 4096]) {
    let mut br = BitReader::new(data);
    let mut kp: u32 = 1 << RLGR_LSGR;
    let mut k: u32 = kp >> RLGR_LSGR;
    let mut krp: u32 = 1 << RLGR_LSGR;
    let mut kr: u32 = krp >> RLGR_LSGR;

    let mut out_idx = 0usize;
    while br.remaining() > 0 && out_idx < out.len() {
        if k != 0 {
            // Run-Length mode.
            let vk = br.consume_run(0);
            if br.read_bit().is_none() {
                break;
            }
            let mut run: u64 = 0;
            for _ in 0..vk {
                run += 1u64 << k;
                kp = (kp + RLGR_UP_GR).min(RLGR_KPMAX);
                k = kp >> RLGR_LSGR;
            }
            if br.remaining() < k as usize {
                break;
            }
            run += br.read_bits(k) as u64;

            let sign = match br.read_bit() {
                Some(b) => b,
                None => break,
            };

            let vk2 = br.consume_run(1);
            if br.read_bit().is_none() {
                break;
            }
            if br.remaining() < kr as usize {
                break;
            }
            let remainder = if kr > 0 { br.read_bits(kr) } else { 0 };
            let code = remainder | (vk2 << kr);

            if vk2 == 0 {
                krp = krp.saturating_sub(2);
            } else if vk2 != 1 {
                krp = (krp + vk2).min(RLGR_KPMAX);
            }
            kr = krp >> RLGR_LSGR;

            kp = kp.saturating_sub(RLGR_DN_GR);
            k = kp >> RLGR_LSGR;

            let magnitude = (code as i32) + 1;
            let mag = if sign != 0 { -magnitude } else { magnitude };

            let mut zeros_left = run;
            while zeros_left > 0 && out_idx < out.len() {
                out[out_idx] = 0;
                out_idx += 1;
                zeros_left -= 1;
            }
            if out_idx < out.len() {
                out[out_idx] = mag as i16;
                out_idx += 1;
            }
        } else {
            // Golomb-Rice mode.
            let vk = br.consume_run(1);
            if br.read_bit().is_none() {
                break;
            }
            if br.remaining() < kr as usize {
                break;
            }
            let remainder = if kr > 0 { br.read_bits(kr) } else { 0 };
            let code = remainder | (vk << kr);

            if vk == 0 {
                krp = krp.saturating_sub(2);
            } else if vk != 1 {
                krp = (krp + vk).min(RLGR_KPMAX);
            }
            kr = krp >> RLGR_LSGR;

            let mag = if code == 0 {
                kp = (kp + RLGR_UQ_GR).min(RLGR_KPMAX);
                0
            } else {
                kp = kp.saturating_sub(RLGR_DQ_GR);
                if code & 1 != 0 {
                    -(((code + 1) >> 1) as i32)
                } else {
                    (code >> 1) as i32
                }
            };
            k = kp >> RLGR_LSGR;

            if out_idx < out.len() {
                out[out_idx] = mag as i16;
                out_idx += 1;
            }
        }
    }
    // Remaining entries stay at their caller-initialized zero.
}

/// Running-sum delta decode, in place (MS-RDPRFX `rfx_differential_decode`).
fn differential_decode(buffer: &mut [i16]) {
    for i in 0..buffer.len().saturating_sub(1) {
        buffer[i + 1] = buffer[i].wrapping_add(buffer[i + 1]);
    }
}

fn dequant(region: &mut [i16], shift: i32) -> Result<()> {
    if shift == 0 {
        return Ok(());
    }
    if !(0..=30).contains(&shift) {
        bail!("Progressive dequant shift {shift} out of sane range");
    }
    for v in region.iter_mut() {
        *v = ((*v as i32) << shift) as i16;
    }
    Ok(())
}

/// One 2D inverse-DWT level, byte-exact port of FreeRDP's `rfx_dwt_2d_decode_block`
/// (non-extrapolate path, `rfx_dwt.c`). `buffer[base..]` holds the 4 subbands (HL, LH, HH,
/// LL, each `subband_width²`) on entry and the reconstructed `(2*subband_width)²` spatial
/// block on exit, written back in place. `temp` is scratch, reused across levels by the
/// caller (needs `4 * subband_width²` entries, so sized for the largest level used).
fn idwt_block(buffer: &mut [i16], base: usize, temp: &mut [i16], subband_width: usize) {
    let total_width = subband_width * 2;

    let ll_off = base + subband_width * subband_width * 3;
    let hl_off = base;
    let lh_off = base + subband_width * subband_width;
    let hh_off = base + subband_width * subband_width * 2;

    let l_dst_off = 0usize;
    let h_dst_off = subband_width * subband_width * 2;

    // Horizontal pass: (LL + HL -> L) and (LH + HH -> H), one output row per input row.
    let mut ll_row = ll_off;
    let mut hl_row = hl_off;
    let mut l_dst_row = l_dst_off;
    let mut lh_row = lh_off;
    let mut hh_row = hh_off;
    let mut h_dst_row = h_dst_off;

    for _ in 0..subband_width {
        temp[l_dst_row] = (buffer[ll_row] as i32 - ((buffer[hl_row] as i32 * 2 + 1) >> 1)) as i16;
        temp[h_dst_row] = (buffer[lh_row] as i32 - ((buffer[hh_row] as i32 * 2 + 1) >> 1)) as i16;
        for n in 1..subband_width {
            let x = n << 1;
            temp[l_dst_row + x] =
                (buffer[ll_row + n] as i32 - ((buffer[hl_row + n - 1] as i32 + buffer[hl_row + n] as i32 + 1) >> 1)) as i16;
            temp[h_dst_row + x] =
                (buffer[lh_row + n] as i32 - ((buffer[hh_row + n - 1] as i32 + buffer[hh_row + n] as i32 + 1) >> 1)) as i16;
        }

        let mut n = 0usize;
        while n < subband_width - 1 {
            let x = n << 1;
            let ld = ((buffer[hl_row + n] as i32) << 1) + ((temp[l_dst_row + x] as i32 + temp[l_dst_row + x + 2] as i32) >> 1);
            let hd = ((buffer[hh_row + n] as i32) << 1) + ((temp[h_dst_row + x] as i32 + temp[h_dst_row + x + 2] as i32) >> 1);
            temp[l_dst_row + x + 1] = ld as i16;
            temp[h_dst_row + x + 1] = hd as i16;
            n += 1;
        }
        let x = n << 1;
        let ld = ((buffer[hl_row + n] as i32) << 1) + temp[l_dst_row + x] as i32;
        let hd = ((buffer[hh_row + n] as i32) << 1) + temp[h_dst_row + x] as i32;
        temp[l_dst_row + x + 1] = ld as i16;
        temp[h_dst_row + x + 1] = hd as i16;

        ll_row += subband_width;
        hl_row += subband_width;
        l_dst_row += total_width;
        lh_row += subband_width;
        hh_row += subband_width;
        h_dst_row += total_width;
    }

    // Vertical pass: (L + H -> LL), written back into `buffer` at `base`.
    for x in 0..total_width {
        let mut l = l_dst_off + x;
        let mut h = h_dst_off + x;
        let mut dst = base + x;

        let dd = temp[l] as i32 - ((temp[h] as i32 * 2 + 1) >> 1);
        buffer[dst] = dd as i16;

        for _ in 1..subband_width {
            l += total_width;
            h += total_width;

            let d2 = temp[l] as i32 - ((temp[h - total_width] as i32 + temp[h] as i32 + 1) >> 1);
            buffer[dst + 2 * total_width] = d2 as i16;

            let d = ((temp[h - total_width] as i32) << 1) + ((buffer[dst] as i32 + buffer[dst + 2 * total_width] as i32) >> 1);
            buffer[dst + total_width] = d as i16;

            dst += 2 * total_width;
        }

        let d = ((temp[h] as i32) << 1) + ((buffer[dst] as i32 * 2) >> 1);
        buffer[dst + total_width] = d as i16;
    }
}

/// 3-level inverse DWT over the full 4096-coefficient subband layout (HL1@0, LH1@1024,
/// HH1@2048, HL2@3072, LH2@3328, HH2@3584, HL3@3840, LH3@3904, HH3@3968, LL3@4032),
/// coarsest level first — each level's output becomes the next level's LL input.
fn idwt_3level(buffer: &mut [i16; 4096]) {
    let mut temp = [0i16; 4096];
    idwt_block(buffer, 3840, &mut temp, 8);
    idwt_block(buffer, 3072, &mut temp, 16);
    idwt_block(buffer, 0, &mut temp, 32);
}

fn clampi16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// One row of the extrapolate-mode horizontal IDWT half-pass (combines a low-band row with
/// a high-band row into a reconstructed row of `low_count + high_count` samples), byte-exact
/// port of FreeRDP's `progressive_rfx_idwt_x`. `low`/`high` read from `src`, one row per
/// outer iteration (rows are `low_step`/`high_step` apart); output goes to `dst`.
#[allow(clippy::too_many_arguments)]
fn idwt_x(
    src: &[i16],
    low_base: usize,
    low_step: usize,
    high_base: usize,
    high_step: usize,
    dst: &mut [i16],
    dst_base: usize,
    dst_step: usize,
    low_count: usize,
    high_count: usize,
    dst_count: usize,
) {
    let mut low_row = low_base;
    let mut high_row = high_base;
    let mut dst_row = dst_base;

    for _ in 0..dst_count {
        let mut p_l = low_row;
        let mut p_h = high_row;
        let mut p_x = dst_row;

        let mut h0 = src[p_h];
        p_h += 1;
        let mut l0 = src[p_l];
        p_l += 1;
        let mut x0 = clampi16(l0 as i32 - h0 as i32);
        let mut x2 = x0;

        for _ in 0..(high_count - 1) {
            let h1 = src[p_h];
            p_h += 1;
            l0 = src[p_l];
            p_l += 1;
            x2 = clampi16(l0 as i32 - ((h0 as i32 + h1 as i32) / 2));
            let x1 = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0 as i32);
            dst[p_x] = x0;
            dst[p_x + 1] = x1;
            p_x += 2;
            x0 = x2;
            h0 = h1;
        }

        if low_count <= high_count + 1 {
            if low_count <= high_count {
                dst[p_x] = x2;
                dst[p_x + 1] = clampi16(x2 as i32 + 2 * h0 as i32);
            } else {
                l0 = src[p_l];
                x0 = clampi16(l0 as i32 - h0 as i32);
                dst[p_x] = x2;
                dst[p_x + 1] = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0 as i32);
                dst[p_x + 2] = x0;
            }
        } else {
            l0 = src[p_l];
            p_l += 1;
            x0 = clampi16(l0 as i32 - (h0 as i32 / 2));
            dst[p_x] = x2;
            dst[p_x + 1] = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0 as i32);
            dst[p_x + 2] = x0;
            l0 = src[p_l];
            dst[p_x + 3] = clampi16((x0 as i32 + l0 as i32) / 2);
        }

        low_row += low_step;
        high_row += high_step;
        dst_row += dst_step;
    }
}

/// Vertical counterpart of `idwt_x` — same algorithm, but walks columns (`low_step`/
/// `high_step`/`dst_step` are row-to-row strides within a column; the outer loop advances
/// to the next column by 1). Byte-exact port of `progressive_rfx_idwt_y`.
#[allow(clippy::too_many_arguments)]
fn idwt_y(
    src: &[i16],
    low_base: usize,
    low_step: usize,
    high_base: usize,
    high_step: usize,
    dst: &mut [i16],
    dst_base: usize,
    dst_step: usize,
    low_count: usize,
    high_count: usize,
    dst_count: usize,
) {
    for i in 0..dst_count {
        let mut p_l = low_base + i;
        let mut p_h = high_base + i;
        let mut p_x = dst_base + i;

        let mut h0 = src[p_h];
        p_h += high_step;
        let mut l0 = src[p_l];
        p_l += low_step;
        let mut x0 = clampi16(l0 as i32 - h0 as i32);
        let mut x2 = x0;

        for _ in 0..(high_count - 1) {
            let h1 = src[p_h];
            p_h += high_step;
            l0 = src[p_l];
            p_l += low_step;
            x2 = clampi16(l0 as i32 - ((h0 as i32 + h1 as i32) / 2));
            let x1 = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0 as i32);
            dst[p_x] = x0;
            p_x += dst_step;
            dst[p_x] = x1;
            p_x += dst_step;
            x0 = x2;
            h0 = h1;
        }

        if low_count <= high_count + 1 {
            if low_count <= high_count {
                dst[p_x] = x2;
                p_x += dst_step;
                dst[p_x] = clampi16(x2 as i32 + 2 * h0 as i32);
            } else {
                l0 = src[p_l];
                x0 = clampi16(l0 as i32 - h0 as i32);
                dst[p_x] = x2;
                p_x += dst_step;
                dst[p_x] = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0 as i32);
                p_x += dst_step;
                dst[p_x] = x0;
            }
        } else {
            l0 = src[p_l];
            p_l += low_step;
            x0 = clampi16(l0 as i32 - (h0 as i32 / 2));
            dst[p_x] = x2;
            p_x += dst_step;
            dst[p_x] = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0 as i32);
            p_x += dst_step;
            dst[p_x] = x0;
            p_x += dst_step;
            l0 = src[p_l];
            dst[p_x] = clampi16((x0 as i32 + l0 as i32) / 2);
        }
    }
}

/// `nBandL`/`nBandH` per DWT level for extrapolate mode (FreeRDP's
/// `progressive_rfx_get_band_l_count`/`_h_count`) — one more sample than the plain
/// power-of-two subband width, used to reduce IDWT edge artifacts.
fn get_band_l_count(level: usize) -> usize {
    (64 >> level) + 1
}

fn get_band_h_count(level: usize) -> usize {
    if level == 1 {
        (64 >> 1) - 1
    } else {
        (64 + (1usize << (level - 1))) >> level
    }
}

/// One extrapolate-mode 2D IDWT level, byte-exact port of FreeRDP's
/// `progressive_rfx_dwt_2d_decode_block`. Unlike the plain `idwt_block`, subband sizes
/// aren't a fixed power of two — `buffer[base..]` holds HL(`nBandH*nBandL`),
/// LH(`nBandL*nBandH`), HH(`nBandH*nBandH`), LL(`nBandL*nBandL`) in that order, and the
/// reconstructed `(nBandL+nBandH)²` block is written back at `base`.
fn idwt_block_extrapolate(buffer: &mut [i16], base: usize, temp: &mut [i16], level: usize) {
    let n_band_l = get_band_l_count(level);
    let n_band_h = get_band_h_count(level);

    let hl_off = base;
    let lh_off = hl_off + n_band_h * n_band_l;
    let hh_off = lh_off + n_band_l * n_band_h;
    let ll_off = hh_off + n_band_h * n_band_h;

    let dst_step = n_band_l + n_band_h;
    let l_off = 0usize;
    let h_off = n_band_l * dst_step;

    // horizontal (LL + HL -> L)
    idwt_x(buffer, ll_off, n_band_l, hl_off, n_band_h, temp, l_off, dst_step, n_band_l, n_band_h, n_band_l);
    // horizontal (LH + HH -> H)
    idwt_x(buffer, lh_off, n_band_l, hh_off, n_band_h, temp, h_off, dst_step, n_band_l, n_band_h, n_band_h);
    // vertical (L + H -> LL), written back into buffer at `base`
    let temp_snapshot: &[i16] = temp;
    idwt_y(temp_snapshot, l_off, dst_step, h_off, dst_step, buffer, base, dst_step, n_band_l, n_band_h, n_band_l + n_band_h);
}

/// 3-level extrapolate-mode inverse DWT (FreeRDP's `rfx_dwt_2d_extrapolate_decode`).
/// Subband offsets differ from the plain-mode layout because extrapolate mode's subbands
/// carry one extra edge sample per axis (see `decode_component_coeffs_extrapolate`).
fn idwt_3level_extrapolate(buffer: &mut [i16; 4096]) {
    let mut temp = [0i16; 4096];
    idwt_block_extrapolate(buffer, 3807, &mut temp, 3);
    idwt_block_extrapolate(buffer, 3007, &mut temp, 2);
    idwt_block_extrapolate(buffer, 0, &mut temp, 1);
}

/// RLGR1-decode, differential-decode LL3, and dequantize one Y/Cb/Cr component's 64×64
/// tile data into raw (pre-IDWT) coefficients. Kept separate from the IDWT step because a
/// `RFX_TILE_DIFFERENCE` tile needs to add a cached previous tile's raw coefficients onto
/// this result *before* running IDWT (see `ProgressiveContext::tile_baseline`).
fn decode_component_coeffs(data: &[u8], shift: &ComponentQuant) -> Result<([i16; 4096], [i16; 4096])> {
    let mut buffer = [0i16; 4096];
    rlgr1_decode(data, &mut buffer);
    // FreeRDP's `CopyMemory(sign, buffer, 4096 * 2)`, taken *before* the differential decode
    // and dequantization: a later TILE_UPGRADE pass uses it to tell an already-significant
    // coefficient (refine its magnitude from the raw stream) from one that is still zero
    // (read the SRL stream, which may make it significant for the first time).
    let sign = buffer;
    differential_decode(&mut buffer[4032..4096]);
    dequant(&mut buffer[0..1024], shift.hl1)?;
    dequant(&mut buffer[1024..2048], shift.lh1)?;
    dequant(&mut buffer[2048..3072], shift.hh1)?;
    dequant(&mut buffer[3072..3328], shift.hl2)?;
    dequant(&mut buffer[3328..3584], shift.lh2)?;
    dequant(&mut buffer[3584..3840], shift.hh2)?;
    dequant(&mut buffer[3840..3904], shift.hl3)?;
    dequant(&mut buffer[3904..3968], shift.lh3)?;
    dequant(&mut buffer[3968..4032], shift.hh3)?;
    dequant(&mut buffer[4032..4096], shift.ll3)?;
    Ok((buffer, sign))
}

/// Extrapolate-mode counterpart of `decode_component_coeffs`. Same RLGR1 stream, but the
/// subband byte layout is different: one extra edge sample per axis per level shifts every
/// offset/length (HL1@0(1023), LH1@1023(1023), HH1@2046(961), HL2@3007(272), LH2@3279(272),
/// HH2@3551(256), HL3@3807(72), LH3@3879(72), HH3@3951(64), LL3@4015(81)) — byte-exact port
/// of the `extrapolate` branch of FreeRDP's `progressive_rfx_decode_component`.
fn decode_component_coeffs_extrapolate(data: &[u8], shift: &ComponentQuant) -> Result<([i16; 4096], [i16; 4096])> {
    let mut buffer = [0i16; 4096];
    rlgr1_decode(data, &mut buffer);
    let sign = buffer;
    dequant(&mut buffer[0..1023], shift.hl1)?;
    dequant(&mut buffer[1023..2046], shift.lh1)?;
    dequant(&mut buffer[2046..3007], shift.hh1)?;
    dequant(&mut buffer[3007..3279], shift.hl2)?;
    dequant(&mut buffer[3279..3551], shift.lh2)?;
    dequant(&mut buffer[3551..3807], shift.hh2)?;
    dequant(&mut buffer[3807..3879], shift.hl3)?;
    dequant(&mut buffer[3879..3951], shift.lh3)?;
    dequant(&mut buffer[3951..4015], shift.hh3)?;
    differential_decode(&mut buffer[4015..4096]);
    dequant(&mut buffer[4015..4096], shift.ll3)?;
    Ok((buffer, sign))
}

// Fixed-point YCbCr->RGB constants for divisor=16 (FreeRDP prim_colors.c's
// `ycbcr_constants[16]`), used by the BGRX-format fast path
// (`general_yCbCrToRGB_16s8u_P3AC4R_BGRX`) since this client's surfaces are BGRX8888.
const YCBCR_CR_R: i64 = 91916; // 1.402525 * 2^16
const YCBCR_CR_G: i64 = 46819; // 0.714401 * 2^16
const YCBCR_CB_G: i64 = 22527; // 0.343730 * 2^16
const YCBCR_CB_B: i64 = 115992; // 1.769905 * 2^16

/// Converts one 64×64 Y/Cb/Cr plane triple (post-IDWT, still in extended fixed-point range)
/// into a tightly-packed 64×64 BGRX8888 buffer ready for `Surface::blit_rect`.
fn ycbcr_to_bgrx(y: &[i16; 4096], cb: &[i16; 4096], cr: &[i16; 4096]) -> [u8; 64 * 64 * 4] {
    let mut out = [0u8; 64 * 64 * 4];
    for i in 0..4096 {
        let yv = (y[i] as i64 + 4096) << 16;
        let crv = cr[i] as i64;
        let cbv = cb[i] as i64;
        let cr_r = crv * YCBCR_CR_R;
        let cr_g = crv * YCBCR_CR_G;
        let cb_g = cbv * YCBCR_CB_G;
        let cb_b = cbv * YCBCR_CB_B;
        let r = ((cr_r + yv) >> 21).clamp(0, 255) as u8;
        let g = ((yv - cb_g - cr_g) >> 21).clamp(0, 255) as u8;
        let b = ((cb_b + yv) >> 21).clamp(0, 255) as u8;
        let o = i * 4;
        out[o] = b;
        out[o + 1] = g;
        out[o + 2] = r;
        out[o + 3] = 0xFF;
    }
    out
}

/// Byte offsets and lengths of the ten subbands within the 4096-coefficient buffer, in the
/// order HL1, LH1, HH1, HL2, LH2, HH2, HL3, LH3, HH3, LL3.
///
/// Extrapolate mode carries one extra edge sample per axis per level, so every offset
/// differs. `progressive_rfx_upgrade_component` in FreeRDP hardcodes the extrapolate layout;
/// selecting by the region's flag (as here) is the same thing whenever the flag is set, and
/// correct rather than silently misaligned when it is not.
fn subband_layout(extrapolate: bool) -> [(usize, usize); 10] {
    if extrapolate {
        [
            (0, 1023),
            (1023, 1023),
            (2046, 961),
            (3007, 272),
            (3279, 272),
            (3551, 256),
            (3807, 72),
            (3879, 72),
            (3951, 64),
            (4015, 81),
        ]
    } else {
        [
            (0, 1024),
            (1024, 1024),
            (2048, 1024),
            (3072, 256),
            (3328, 256),
            (3584, 256),
            (3840, 64),
            (3904, 64),
            (3968, 64),
            (4032, 64),
        ]
    }
}

/// Decoder state for one `TILE_UPGRADE` component: two independent MSB-first bitstreams (the
/// SRL stream, which codes runs of still-zero coefficients, and the raw stream, which codes
/// refinement bits for coefficients already known to be non-zero) plus the SRL adaptive
/// run-length state.
struct UpgradeState<'a> {
    srl: BitReader<'a>,
    raw: BitReader<'a>,
    /// Remaining length of the zero run currently being emitted.
    nz: u32,
    /// Adaptive run-length parameter, 0..=80; `k = kp / 8`.
    kp: i32,
    /// 0 = expecting a zero-run header next, 1 = expecting a value next.
    mode: u8,
}

const SRL_KP_MAX: i32 = 80;

impl<'a> UpgradeState<'a> {
    fn new(srl: &'a [u8], raw: &'a [u8]) -> Self {
        Self { srl: BitReader::new(srl), raw: BitReader::new(raw), nz: 0, kp: 8, mode: 0 }
    }

    /// Reads one coefficient from the SRL stream (MS-RDPEGFX "Simplified Run-Length").
    ///
    /// Zero runs are coded adaptively: a `0` header means "a run of exactly `1 << k` zeros"
    /// and grows `kp`; a `1` header means "a run of the next `k` bits' worth of zeros" and
    /// shrinks `kp`. A run of zero length is followed immediately by a value: a sign bit,
    /// then — unless only one bit of precision is being added — a unary magnitude capped at
    /// `(1 << numBits) - 1`.
    fn srl_read(&mut self, num_bits: u32) -> i16 {
        if self.nz > 0 {
            self.nz -= 1;
            return 0;
        }
        // `k` is taken from `kp` as it stands on entry, before this call's adaptation.
        let k = (self.kp / 8) as u32;

        if self.mode == 0 {
            if self.srl.read_bit().unwrap_or(0) == 0 {
                self.nz = 1u32 << k;
                self.kp = (self.kp + 4).min(SRL_KP_MAX);
                self.nz -= 1;
                return 0;
            }
            self.mode = 1;
            self.nz = if k > 0 { self.srl.read_bits(k) } else { 0 };
            if self.nz > 0 {
                self.nz -= 1;
                return 0;
            }
        }

        self.kp = (self.kp - 6).max(0);
        self.mode = 0;

        let negative = self.srl.read_bit().unwrap_or(0) != 0;
        if num_bits == 1 {
            return if negative { -1 } else { 1 };
        }
        let max = (1u32 << num_bits) - 1;
        let mut mag = 1u32;
        while mag < max {
            if self.srl.read_bit().unwrap_or(0) != 0 {
                break;
            }
            mag += 1;
        }
        if negative {
            -(mag as i16)
        } else {
            mag as i16
        }
    }

    /// Refines one subband in place by `num_bits` additional bits of precision.
    ///
    /// `non_ll` is false only for LL3, whose coefficients are always significant and are
    /// therefore refined straight from the raw stream with no sign tracking.
    fn upgrade_block(&mut self, current: &mut [i16], sign: &mut [i16], shift: u32, num_bits: u32, non_ll: bool) {
        if num_bits == 0 {
            // No new precision for this subband, and — importantly — no bits consumed from
            // either stream.
            return;
        }
        for i in 0..current.len() {
            // LL3 coefficients are always significant, so they take the same path as an
            // already-positive coefficient: refinement bits straight from the raw stream.
            let input: i32 = if !non_ll || sign[i] > 0 {
                self.raw.read_bits(num_bits) as i32
            } else if sign[i] < 0 {
                -(self.raw.read_bits(num_bits) as i32)
            } else {
                // Still zero after every previous pass: the SRL stream decides whether this
                // pass makes it significant, and if so its sign is remembered for later
                // passes.
                let v = self.srl_read(num_bits);
                sign[i] = v;
                v as i32
            };
            // FreeRDP: `buffer[index] += (INT16)((UINT32)input << shift)` — the new bits sit
            // at the tile's new bit position, below everything decoded so far.
            current[i] = current[i].wrapping_add(input.wrapping_shl(shift) as i16);
        }
    }
}

/// Applies one `TILE_UPGRADE` refinement pass to one component's persistent coefficient and
/// sign planes, in the subband order the bitstreams are written in.
fn upgrade_component(
    current: &mut [i16; 4096],
    sign: &mut [i16; 4096],
    srl: &[u8],
    raw: &[u8],
    shift: &ComponentQuant,
    num_bits: &[i32; 10],
    extrapolate: bool,
) {
    let mut state = UpgradeState::new(srl, raw);
    let layout = subband_layout(extrapolate);
    let shifts = shift.bands();
    for band in 0..10 {
        let (off, len) = layout[band];
        // Only LL3 (the last entry) is read straight from the raw stream.
        let non_ll = band < 9;
        state.upgrade_block(
            &mut current[off..off + len],
            &mut sign[off..off + len],
            shifts[band].max(0) as u32,
            num_bits[band].max(0) as u32,
            non_ll,
        );
    }
    // FreeRDP's `progressive_rfx_upgrade_state_finish` byte-aligns both streams afterwards.
    // That only matters for its own end-of-stream validation; both readers are dropped here.
}

/// Applies one `RFX_TILE_DIFFERENCE` delta onto a tile's running coefficient state.
///
/// Byte-exact equivalent of FreeRDP's `add_16s_inplace(buffer, current, 4096)`, which writes
/// the sum into **both** buffers:
///
/// ```c
/// INT32 k = pSrcDst1[x] + pSrcDst2[x];
/// pSrcDst1[x] = pSrcDst2[x] = (INT16)k;
/// ```
///
/// Updating the baseline is the whole point, and getting it wrong is subtle: a run of diff
/// tiles at one grid position must render base+d1, then base+d1+d2, then base+d1+d2+d3. A
/// decoder that adds each delta onto the *original* baseline instead renders base+d2 for the
/// second tile — plausible-looking but permanently wrong pixels for that tile, which is what
/// the smeared/doubled text in this client turned out to be.
fn accumulate_diff(delta: &mut [i16; 4096], baseline: &mut [i16; 4096]) {
    for i in 0..4096 {
        let sum = delta[i].wrapping_add(baseline[i]);
        delta[i] = sum;
        baseline[i] = sum;
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_tile(
    ctx: &mut ProgressiveContext,
    surface_id: u16,
    body: &[u8],
    is_first: bool,
    extrapolate: bool,
    quant_vals: &[ComponentQuant],
    quant_prog_vals: &[(ComponentQuant, ComponentQuant, ComponentQuant)],
) -> Result<()> {
    let mut r = ByteReader::new(body);
    let quant_idx_y = r.u8()? as usize;
    let quant_idx_cb = r.u8()? as usize;
    let quant_idx_cr = r.u8()? as usize;
    let x_idx = r.u16()? as u32;
    let y_idx = r.u16()? as u32;
    let flags = r.u8()?;
    let quality = if is_first { r.u8()? } else { 0xFF };
    let y_len = r.u16()? as usize;
    let cb_len = r.u16()? as usize;
    let cr_len = r.u16()? as usize;
    let tail_len = r.u16()? as usize;
    let y_data = r.bytes(y_len)?;
    let cb_data = r.bytes(cb_len)?;
    let cr_data = r.bytes(cr_len)?;
    let _tail = r.bytes(tail_len)?;

    let quant_y = quant_vals.get(quant_idx_y).ok_or_else(|| anyhow::anyhow!("Progressive tile quantIdxY {quant_idx_y} out of range"))?;
    let quant_cb = quant_vals.get(quant_idx_cb).ok_or_else(|| anyhow::anyhow!("Progressive tile quantIdxCb {quant_idx_cb} out of range"))?;
    let quant_cr = quant_vals.get(quant_idx_cr).ok_or_else(|| anyhow::anyhow!("Progressive tile quantIdxCr {quant_idx_cr} out of range"))?;

    // quality==0xFF (always true for TILE_SIMPLE, the common case for TILE_FIRST too) means
    // full quality: quantProg is the all-zero struct, matching FreeRDP's
    // `quantProgValFull` (calloc'd, never populated). Otherwise it's a direct index into
    // the region's quantProgVals (NOT a search by the per-entry quality byte, which is
    // informational only — confirmed against `progressive_decompress_tile_first`).
    let (prog_y, prog_cb, prog_cr) = if quality == 0xFF {
        (ComponentQuant::default(), ComponentQuant::default(), ComponentQuant::default())
    } else {
        let entry = quant_prog_vals
            .get(quality as usize)
            .ok_or_else(|| anyhow::anyhow!("Progressive tile quality {quality} out of range"))?;
        *entry
    };

    let shift_y = quant_y.add(&prog_y).sub1()?;
    let shift_cb = quant_cb.add(&prog_cb).sub1()?;
    let shift_cr = quant_cr.add(&prog_cr).sub1()?;

    // The tile's bit position per subband is `quant + quantProg` *without* the -1 that the
    // dequant shift carries (FreeRDP sets `tile->yBitPos` before `progressive_rfx_quant_lsub`
    // subtracts it). A later TILE_UPGRADE pass computes its new bits from the difference.
    let bit_pos_y = quant_y.add(&prog_y);
    let bit_pos_cb = quant_cb.add(&prog_cb);
    let bit_pos_cr = quant_cr.add(&prog_cr);

    let decode = if extrapolate { decode_component_coeffs_extrapolate } else { decode_component_coeffs };
    let (mut y_coeffs, sign_y) = decode(y_data, &shift_y)?;
    let (mut cb_coeffs, sign_cb) = decode(cb_data, &shift_cb)?;
    let (mut cr_coeffs, sign_cr) = decode(cr_data, &shift_cr)?;

    let key = (surface_id, x_idx, y_idx);
    let diff = flags & TILE_FLAG_DIFFERENCE != 0;
    if diff {
        // `RFX_TILE_DIFFERENCE`: this tile carries a delta against the running per-tile
        // coefficient state, which is then *replaced by the sum*. FreeRDP's
        // `progressive_rfx_dwt_2d_decode` is the normative shape:
        //
        //     if (reverse)        memcpy(buffer, current, bsize);
        //     else if (!coeffDiff) memcpy(current, buffer, bsize);
        //     else                 prims->add_16s_inplace(buffer, current, belements);
        //
        // and `add_16s_inplace` writes the sum into **both** arguments. So the baseline
        // accumulates: two consecutive diff tiles at one grid position render base+d1 and
        // then base+d1+d2, not base+d1 and base+d2.
        //
        // This client previously left the baseline untouched on a diff decode, so the
        // second delta of any run landed on stale coefficients — permanently wrong pixels
        // for that tile, and the direct cause of the smeared/doubled text this was built to
        // investigate.
        match ctx.tile_baseline.get_mut(&key) {
            Some(state) => {
                let [base_y, base_cb, base_cr] = &mut state.current;
                accumulate_diff(&mut y_coeffs, base_y);
                accumulate_diff(&mut cb_coeffs, base_cb);
                accumulate_diff(&mut cr_coeffs, base_cr);
                state.sign = [sign_y, sign_cb, sign_cr];
                state.bit_pos = [bit_pos_y, bit_pos_cb, bit_pos_cr];
                ctx.diff_tile_applied_count += 1;
            }
            None => {
                // No baseline yet: the first tile seen at this grid position arrived as a
                // diff, or the surface was just reset. There is nothing to add onto.
                ctx.diff_without_baseline_count += 1;
                if ctx.diff_without_baseline_count.is_power_of_two() {
                    eprintln!(
                        "[gfx] Progressive: {} RFX_TILE_DIFFERENCE tile(s) with no cached baseline skipped (latest {x_idx},{y_idx})",
                        ctx.diff_without_baseline_count
                    );
                }
                return Ok(());
            }
        }
    } else {
        ctx.tile_baseline.insert(
            key,
            TileState {
                current: [y_coeffs, cb_coeffs, cr_coeffs],
                sign: [sign_y, sign_cb, sign_cr],
                bit_pos: [bit_pos_y, bit_pos_cb, bit_pos_cr],
            },
        );
    }

    // IDWT is in-place — decode fresh copies so the cached baseline stays in raw
    // (pre-IDWT) coefficient form for any future diff tile at this position.
    let mut y_plane = y_coeffs;
    let mut cb_plane = cb_coeffs;
    let mut cr_plane = cr_coeffs;
    if extrapolate {
        idwt_3level_extrapolate(&mut y_plane);
        idwt_3level_extrapolate(&mut cb_plane);
        idwt_3level_extrapolate(&mut cr_plane);
    } else {
        idwt_3level(&mut y_plane);
        idwt_3level(&mut cb_plane);
        idwt_3level(&mut cr_plane);
    }

    let bgrx = ycbcr_to_bgrx(&y_plane, &cb_plane, &cr_plane);
    ctx.pending_tiles.push((x_idx, y_idx, bgrx));
    Ok(())
}

/// Decodes one `RFX_PROGRESSIVE_TILE_UPGRADE` block (MS-RDPEGFX §2.2.4.4.3).
///
/// An upgrade carries no coefficients of its own — it adds lower-order bits to the tile
/// already sitting in `tile_baseline`, lowering that tile's bit position. Skipping these (as
/// this decoder previously did, thousands of times per session) leaves the tile stuck at
/// whatever coarse quality its first pass had, which reads as a permanently blurry patch
/// among sharp neighbours.
#[allow(clippy::too_many_arguments)]
fn decode_tile_upgrade(
    ctx: &mut ProgressiveContext,
    surface_id: u16,
    body: &[u8],
    extrapolate: bool,
    quant_vals: &[ComponentQuant],
    quant_prog_vals: &[(ComponentQuant, ComponentQuant, ComponentQuant)],
) -> Result<()> {
    let mut r = ByteReader::new(body);
    // Unlike TILE_SIMPLE/TILE_FIRST there is no `flags` byte here.
    let quant_idx_y = r.u8()? as usize;
    let quant_idx_cb = r.u8()? as usize;
    let quant_idx_cr = r.u8()? as usize;
    let x_idx = r.u16()? as u32;
    let y_idx = r.u16()? as u32;
    let quality = r.u8()?;
    let y_srl_len = r.u16()? as usize;
    let y_raw_len = r.u16()? as usize;
    let cb_srl_len = r.u16()? as usize;
    let cb_raw_len = r.u16()? as usize;
    let cr_srl_len = r.u16()? as usize;
    let cr_raw_len = r.u16()? as usize;

    // If the header layout above were wrong by even one byte, these six lengths would not
    // add up to what is left of the block. Checking it turns a misparse into a loud error
    // instead of six bitstreams decoded from the wrong offsets into plausible garbage.
    let declared = y_srl_len + y_raw_len + cb_srl_len + cb_raw_len + cr_srl_len + cr_raw_len;
    let available = body.len() - r.pos;
    if declared != available {
        bail!("Progressive TILE_UPGRADE stream lengths sum to {declared} but {available} bytes remain in the block");
    }

    let y_srl = r.bytes(y_srl_len)?;
    let y_raw = r.bytes(y_raw_len)?;
    let cb_srl = r.bytes(cb_srl_len)?;
    let cb_raw = r.bytes(cb_raw_len)?;
    let cr_srl = r.bytes(cr_srl_len)?;
    let cr_raw = r.bytes(cr_raw_len)?;

    let quant_y = quant_vals.get(quant_idx_y).ok_or_else(|| anyhow::anyhow!("upgrade quantIdxY {quant_idx_y} out of range"))?;
    let quant_cb = quant_vals.get(quant_idx_cb).ok_or_else(|| anyhow::anyhow!("upgrade quantIdxCb {quant_idx_cb} out of range"))?;
    let quant_cr = quant_vals.get(quant_idx_cr).ok_or_else(|| anyhow::anyhow!("upgrade quantIdxCr {quant_idx_cr} out of range"))?;

    let (prog_y, prog_cb, prog_cr) = if quality == 0xFF {
        (ComponentQuant::default(), ComponentQuant::default(), ComponentQuant::default())
    } else {
        *quant_prog_vals.get(quality as usize).ok_or_else(|| anyhow::anyhow!("upgrade quality {quality} out of range"))?
    };

    let new_bit_pos = [quant_y.add(&prog_y), quant_cb.add(&prog_cb), quant_cr.add(&prog_cr)];
    let key = (surface_id, x_idx, y_idx);

    let planes = {
        let Some(state) = ctx.tile_baseline.get_mut(&key) else {
            // Nothing to refine: no TILE_FIRST has been seen for this grid position (or the
            // surface was reset since). The upgrade's bitstreams are meaningless on their own.
            ctx.upgrade_without_baseline_count += 1;
            if ctx.upgrade_without_baseline_count.is_power_of_two() {
                eprintln!(
                    "[gfx] Progressive: {} TILE_UPGRADE block(s) with no cached tile skipped (latest {x_idx},{y_idx})",
                    ctx.upgrade_without_baseline_count
                );
            }
            return Ok(());
        };

        // Resolve the arithmetic for all three components *before* touching any of them.
        // Refinement mutates the persistent tile state in place, so bailing part-way through
        // would leave the tile with Y refined and Cb/Cr not — and its bit positions
        // inconsistent with its coefficients. Nothing would redraw that tile, so every later
        // difference and upgrade pass at this grid position would compound the damage.
        // Failing before the first mutation leaves the tile exactly as it was.
        // numBits = how far this pass lowers the bit position; shift = where the new bits
        // land, which is the new bit position less the usual 1.
        let mut plan = [([0i32; 10], ComponentQuant::default()); 3];
        for c in 0..3 {
            plan[c] = (state.bit_pos[c].checked_sub(&new_bit_pos[c])?, new_bit_pos[c].sub1()?);
        }

        let srl = [y_srl, cb_srl, cr_srl];
        let raw = [y_raw, cb_raw, cr_raw];
        for (c, (num_bits, shift)) in plan.iter().enumerate() {
            upgrade_component(&mut state.current[c], &mut state.sign[c], srl[c], raw[c], shift, num_bits, extrapolate);
            state.bit_pos[c] = new_bit_pos[c];
        }
        state.current
    };

    // FreeRDP finishes an upgrade with `progressive_rfx_dwt_2d_decode(..., reverse=TRUE)`,
    // whose reverse branch copies `current` into the IDWT scratch buffer — the refined
    // coefficients are the tile, not a delta against it.
    let [mut y_plane, mut cb_plane, mut cr_plane] = planes;
    if extrapolate {
        idwt_3level_extrapolate(&mut y_plane);
        idwt_3level_extrapolate(&mut cb_plane);
        idwt_3level_extrapolate(&mut cr_plane);
    } else {
        idwt_3level(&mut y_plane);
        idwt_3level(&mut cb_plane);
        idwt_3level(&mut cr_plane);
    }

    ctx.tile_upgrade_count += 1;
    ctx.pending_tiles.push((x_idx, y_idx, ycbcr_to_bgrx(&y_plane, &cb_plane, &cr_plane)));
    Ok(())
}

/// Composites every tile decoded in *this region* (`tiles`, cleared and refilled per
/// region — never resurrects tiles from an earlier region or an earlier WireToSurface2
/// message), clipped to the union of this region's declared damage rects.
///
/// An earlier version of this recomposited from a whole-session tile cache (matching what
/// FreeRDP's `update_tiles` appears to do with `surface->numUpdatedTiles`), to handle a rect
/// that exposes an area without resending fresh tile data. That fixed one bug but caused a
/// worse one: this decoder's Progressive tile cache has no visibility into non-Progressive
/// draws (ClearCodec, SolidFill, SurfaceToSurface all write the shared `Surface` directly),
/// so recompositing an old cached Progressive tile could clobber genuinely newer content
/// drawn by a completely different codec in the meantime — confirmed by real symptoms
/// (unrelated UI fragments appearing in the wrong place, not just stale content). Scoping
/// this to same-region-only tiles is strictly safe: it can only ever draw what this region
/// itself just decoded, clipped to what this region itself declared as changed.
/// `origin` is the destination origin of the enclosing Set Surface Bits command, in surface
/// coordinates. It is (0, 0) for `RDPGFX_WIRE_TO_SURFACE_PDU_2`, which carries no destRect
/// at all, and the destRect's top-left for a CAProgressive `RDPGFX_WIRE_TO_SURFACE_PDU_1`.
/// It is added to *both* the tile position and the region's clipping rects, matching
/// FreeRDP's `progressive_decompress`, which offsets `updateRect` and `clippingRect` by
/// `nXDst`/`nYDst` alike. Treating tile indices as absolute screen coordinates happens to
/// work whenever the origin is zero and silently misplaces every tile when it is not.
fn composite_region_rects(
    tiles: &[(u32, u32, [u8; 64 * 64 * 4])],
    rects: &[(u32, u32, u32, u32)],
    surface: &mut Surface,
    origin: (u32, u32),
    rejected: &mut u32,
    dropped_pixels: &mut u64,
) {
    for &(x_idx, y_idx, ref bgrx) in tiles {
        let tile_x = origin.0 + x_idx * 64;
        let tile_y = origin.1 + y_idx * 64;

        // Reject rather than clamp. A tile whose grid position places it entirely outside
        // the surface means we have misparsed the stream, and silently clamping it into
        // range would paint decoded pixels at coordinates the server never asked for — the
        // "correct content, wrong place" failure mode. (Writing past the destination bounds
        // outright is a known CVE class in other RemoteFX implementations; `blit_rect` is
        // bounds-checked, so this is a correctness guard rather than the only one.)
        if tile_x >= surface.width || tile_y >= surface.height {
            *rejected += 1;
            continue;
        }

        for &(rx, ry, rw, rh) in rects {
            let (rx, ry) = (origin.0 + rx, origin.1 + ry);
            if rw == 0 || rh == 0 {
                continue;
            }
            let ix0 = tile_x.max(rx);
            let iy0 = tile_y.max(ry);
            let ix1 = (tile_x + 64).min(rx + rw);
            let iy1 = (tile_y + 64).min(ry + rh);
            if ix0 >= ix1 || iy0 >= iy1 {
                continue;
            }
            let iw = ix1 - ix0;
            let ih = iy1 - iy0;
            let src_x = ix0 - tile_x;
            let src_y = iy0 - tile_y;

            let mut sub = vec![0u8; (iw * ih * 4) as usize];
            for row in 0..ih {
                let src_off = (((src_y + row) * 64 + src_x) * 4) as usize;
                let dst_off = (row * iw * 4) as usize;
                let len = (iw * 4) as usize;
                sub[dst_off..dst_off + len].copy_from_slice(&bgrx[src_off..src_off + len]);
            }
            let outcome = surface.blit_rect(ix0, iy0, iw, ih, &sub);
            *dropped_pixels += outcome.dropped();
        }
    }
}

fn decode_region(
    ctx: &mut ProgressiveContext,
    surface_id: u16,
    body: &[u8],
    surface: &mut Surface,
    origin: (u32, u32),
) -> Result<()> {
    let mut r = ByteReader::new(body);
    let tile_size = r.u8()?;
    let num_rects = r.u16()? as usize;
    let num_quant = r.u8()? as usize;
    let num_prog_quant = r.u8()? as usize;
    let flags = r.u8()?;
    let _num_tiles = r.u16()? as usize;
    let tile_data_size = r.u32()? as usize;

    if tile_size != 64 {
        bail!("Progressive region tileSize {tile_size} != 64");
    }

    // These clip what actually gets composited to the destination — see
    // `composite_region_rects`'s doc comment for why blindly drawing full tiles is wrong.
    let mut rects = Vec::with_capacity(num_rects);
    for _ in 0..num_rects {
        let x = r.u16()? as u32;
        let y = r.u16()? as u32;
        let w = r.u16()? as u32;
        let h = r.u16()? as u32;
        rects.push((x, y, w, h));
    }

    let mut quant_vals = Vec::with_capacity(num_quant);
    for _ in 0..num_quant {
        quant_vals.push(ComponentQuant::read(&mut r)?);
    }

    let mut quant_prog_vals = Vec::with_capacity(num_prog_quant);
    for _ in 0..num_prog_quant {
        let _quality_byte = r.u8()?; // informational only; lookup is by tile.quality index
        let y = ComponentQuant::read(&mut r)?;
        let cb = ComponentQuant::read(&mut r)?;
        let cr = ComponentQuant::read(&mut r)?;
        quant_prog_vals.push((y, cb, cr));
    }

    let tile_bytes = r.bytes(tile_data_size)?;
    let extrapolate = flags & REGION_FLAG_DWT_REDUCE_EXTRAPOLATE != 0;
    ctx.pending_tiles.clear();

    // A tile that fails to decode must not take the region's other tiles down with it. The
    // tile stream is self-framing via blockLen, so the next tile's offset is still known
    // after a body-level failure — and on a delta protocol, discarding twenty good tiles
    // because the twenty-first was malformed converts one defect into twenty-one permanent
    // artifacts. Failures are collected and reported after the good tiles are composited.
    let mut failures: Vec<String> = Vec::new();
    let mut pos = 0usize;
    while pos + 6 <= tile_bytes.len() {
        let block_type = u16::from_le_bytes([tile_bytes[pos], tile_bytes[pos + 1]]);
        let block_len = u32::from_le_bytes([tile_bytes[pos + 2], tile_bytes[pos + 3], tile_bytes[pos + 4], tile_bytes[pos + 5]]) as usize;
        if block_len < 6 || pos + block_len > tile_bytes.len() {
            // Framing is unrecoverable: without a trustworthy blockLen there is no next
            // offset to resume from, so this ends the tile stream.
            failures.push(format!("tile block length {block_len} invalid at offset {pos}"));
            break;
        }
        let tbody = &tile_bytes[pos + 6..pos + block_len];
        let result = match block_type {
            WBT_TILE_SIMPLE => decode_tile(ctx, surface_id, tbody, false, extrapolate, &quant_vals, &quant_prog_vals),
            WBT_TILE_FIRST => decode_tile(ctx, surface_id, tbody, true, extrapolate, &quant_vals, &quant_prog_vals),
            WBT_TILE_UPGRADE => decode_tile_upgrade(ctx, surface_id, tbody, extrapolate, &quant_vals, &quant_prog_vals),
            other => Err(anyhow::anyhow!("unexpected Progressive block type {other:#06x} in tile data")),
        };
        if let Err(e) = result {
            failures.push(format!("tile block {block_type:#06x} at offset {pos}: {e:#}"));
        }
        pos += block_len;
    }

    let tiles = std::mem::take(&mut ctx.pending_tiles);
    let mut rejected = 0u32;
    let mut dropped_pixels = 0u64;
    composite_region_rects(&tiles, &rects, surface, origin, &mut rejected, &mut dropped_pixels);
    if dropped_pixels > 0 {
        failures.push(format!("{dropped_pixels} pixel(s) fell outside the {}x{} surface and were clipped away", surface.width, surface.height));
    }
    if rejected > 0 {
        ctx.out_of_range_tiles += rejected as u64;
        failures.push(format!(
            "{rejected} tile(s) rejected for falling outside the {}x{} surface",
            surface.width, surface.height
        ));
    }
    if !failures.is_empty() {
        bail!("{} of this region's blocks failed ({} composited): {}", failures.len(), tiles.len(), failures.join("; "));
    }
    Ok(())
}

/// Everything a tile grid position must remember between frames for the stateful paths
/// (`RFX_TILE_DIFFERENCE` and `TILE_UPGRADE`) to work. Mirrors the `current`, `sign` and
/// `yBitPos`/`cbBitPos`/`crBitPos` fields FreeRDP keeps per `RFX_PROGRESSIVE_TILE`.
struct TileState {
    /// Accumulated dequantized coefficients, pre-IDWT, per Y/Cb/Cr.
    current: [[i16; 4096]; 3],
    /// Pre-dequantization RLGR output per Y/Cb/Cr — which coefficients are significant, and
    /// with what sign. Updated as an upgrade pass makes new ones significant.
    sign: [[i16; 4096]; 3],
    /// Per-component bit position per subband: how many low-order bits are still unknown.
    /// An upgrade pass lowers it and fills in the bits it uncovers.
    bit_pos: [ComponentQuant; 3],
}

/// Persistent decoder state: each tile grid position's last non-diff-decoded raw (pre-IDWT)
/// coefficients, keyed by `(surfaceId, xIdx, yIdx)`, so a later `RFX_TILE_DIFFERENCE` tile
/// at that position can add its delta onto them (mirrors FreeRDP's per-tile `current`
/// buffer in `PROGRESSIVE_SURFACE_CONTEXT`). Bounded by the surface's tile grid size (e.g.
/// 1280×800 = 260 entries × 24KB), not by session length or frame count.
#[derive(Default)]
pub struct ProgressiveContext {
    tile_baseline: std::collections::HashMap<(u16, u32, u32), TileState>,
    /// Tiles decoded so far in the region currently being processed — cleared at the start
    /// of each region, drained by `composite_region_rects` at the end. Deliberately *not*
    /// session-persistent (see `composite_region_rects`'s doc comment for why).
    pending_tiles: Vec<(u32, u32, [u8; 64 * 64 * 4])>,
    // Diagnostic counters for the two remaining known-deferred/notable paths — logged
    // periodically (not every occurrence, to avoid flooding real-time traffic) so a test
    // session's console output says something concrete about how often each is actually
    // hit, instead of guessing from visual symptoms alone.
    tile_upgrade_count: u64,
    diff_tile_applied_count: u64,
    diff_without_baseline_count: u64,
    upgrade_without_baseline_count: u64,
    out_of_range_tiles: u64,
}

impl ProgressiveContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every per-tile baseline for one surface. Called when that surface is deleted or
    /// re-created, because a baseline is only meaningful relative to a specific surface
    /// geometry: a reused surface id would otherwise inherit the dead surface's tiles and
    /// add later `RFX_TILE_DIFFERENCE` deltas onto completely unrelated pixels.
    pub fn reset_surface(&mut self, surface_id: u16) {
        self.tile_baseline.retain(|&(id, _, _), _| id != surface_id);
    }

    /// Drops every per-tile baseline. Only valid on RESET_GRAPHICS, which redefines the
    /// output geometry all the baselines were indexed against. Resetting at any other point
    /// would break the legitimate cross-frame diff chain and *cause* ghosting.
    pub fn reset_all(&mut self) {
        self.tile_baseline.clear();
        self.pending_tiles.clear();
    }

    pub fn baseline_count(&self) -> usize {
        self.tile_baseline.len()
    }

    /// Decodes one `RDPGFX_WIRE_TO_SURFACE_PDU_2` RemoteFX Progressive bitstream (a
    /// sequence of `RFX_PROGRESSIVE_DATABLOCK`s: `[CONTEXT?] FRAME_BEGIN
    /// REGION{tiles}... FRAME_END`) and blits every decoded tile directly into `surface`
    /// at its own grid position.
    pub fn decompress(&mut self, surface_id: u16, data: &[u8], surface: &mut Surface) -> Result<()> {
        self.decompress_at(surface_id, data, surface, 0, 0)
    }

    /// As `decompress`, but for a CAProgressive stream arriving in an
    /// `RDPGFX_WIRE_TO_SURFACE_PDU_1` envelope, which *does* carry a destRect. Both the
    /// tiles and the region's clipping rects are offset by that origin.
    pub fn decompress_at(
        &mut self,
        surface_id: u16,
        data: &[u8],
        surface: &mut Surface,
        dest_x: u32,
        dest_y: u32,
    ) -> Result<()> {
        let origin = (dest_x, dest_y);
        // As in `decode_region`: one bad region must not discard the regions after it, since
        // each block is self-framing via blockLen and every discarded region is a permanent
        // artifact.
        let mut failures: Vec<String> = Vec::new();
        let mut pos = 0usize;
        while pos + 6 <= data.len() {
            let block_type = u16::from_le_bytes([data[pos], data[pos + 1]]);
            let block_len = u32::from_le_bytes([data[pos + 2], data[pos + 3], data[pos + 4], data[pos + 5]]) as usize;
            if block_len < 6 || pos + block_len > data.len() {
                bail!("Progressive block length {block_len} invalid at offset {pos}");
            }
            let body = &data[pos + 6..pos + block_len];
            match block_type {
                WBT_REGION => {
                    if let Err(e) = decode_region(self, surface_id, body, surface, origin) {
                        failures.push(format!("region at offset {pos}: {e:#}"));
                    }
                }
                WBT_SYNC | WBT_FRAME_BEGIN | WBT_FRAME_END | WBT_CONTEXT => {}
                WBT_TILE_UPGRADE => {} // shouldn't appear at top level, but harmless to skip
                _ => {} // unknown block type, skip via blockLen like DELETEENCODINGCONTEXT
            }
            pos += block_len;
        }
        if !failures.is_empty() {
            bail!("{} region(s) failed: {}", failures.len(), failures.join("; "));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(x_idx: u32, y_idx: u32, color: u8) -> (u32, u32, [u8; 64 * 64 * 4]) {
        let mut px = [0u8; 64 * 64 * 4];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&[color, color, color, 0xFF]);
        }
        (x_idx, y_idx, px)
    }

    #[test]
    fn tiles_land_at_their_grid_position_when_the_origin_is_zero() {
        let mut s = Surface::new(256, 256);
        let (mut rejected, mut dropped) = (0, 0);
        composite_region_rects(&[tile(1, 2, 200)], &[(0, 0, 256, 256)], &mut s, (0, 0), &mut rejected, &mut dropped);
        assert_eq!(rejected, 0);
        // Tile (1,2) covers x 64..128, y 128..192.
        assert_eq!(s.pixels[((128 * 256 + 64) * 4) as usize], 200);
        assert_eq!(s.pixels[((127 * 256 + 64) * 4) as usize], 0, "must not spill above its row");
        assert_eq!(s.pixels[((128 * 256 + 63) * 4) as usize], 0, "must not spill left of its column");
    }

    #[test]
    fn the_enclosing_commands_origin_offsets_both_tiles_and_clipping_rects() {
        let mut s = Surface::new(256, 256);
        let (mut rejected, mut dropped) = (0, 0);
        // Same tile and same region rect, but the enclosing command puts the origin at
        // (100, 50). Everything must shift, not just the tile.
        composite_region_rects(&[tile(0, 0, 90)], &[(0, 0, 64, 64)], &mut s, (100, 50), &mut rejected, &mut dropped);
        assert_eq!(rejected, 0);
        assert_eq!(s.pixels[((50 * 256 + 100) * 4) as usize], 90, "tile must move to the command origin");
        assert_eq!(s.pixels[0], 0, "nothing may be painted at the surface origin");
        // The clipping rect moved with it, so the far corner of the tile is still inside.
        assert_eq!(s.pixels[((113 * 256 + 163) * 4) as usize], 90);
    }

    #[test]
    fn region_rects_clip_the_tile_they_overlap() {
        let mut s = Surface::new(256, 256);
        let (mut rejected, mut dropped) = (0, 0);
        // Only the left half of tile (0,0) is declared as changed.
        composite_region_rects(&[tile(0, 0, 77)], &[(0, 0, 32, 64)], &mut s, (0, 0), &mut rejected, &mut dropped);
        assert_eq!(s.pixels[(31 * 4) as usize], 77, "inside the declared rect");
        assert_eq!(s.pixels[(32 * 4) as usize], 0, "outside the declared rect must stay untouched");
    }

    #[test]
    fn out_of_range_tiles_are_rejected_not_clamped() {
        let mut s = Surface::new(128, 128);
        let (mut rejected, mut dropped) = (0, 0);
        // xIdx 100 => x 6400, far outside a 128px-wide surface.
        composite_region_rects(&[tile(100, 0, 255)], &[(0, 0, 128, 128)], &mut s, (0, 0), &mut rejected, &mut dropped);
        assert_eq!(rejected, 1, "an out-of-range tile must be counted and rejected");
        assert!(s.pixels.iter().all(|&b| b == 0), "a rejected tile must not be clamped into view");
    }

    #[test]
    fn empty_rect_intersection_writes_nothing() {
        let mut s = Surface::new(256, 256);
        let (mut rejected, mut dropped) = (0, 0);
        composite_region_rects(&[tile(0, 0, 55)], &[(128, 128, 64, 64)], &mut s, (0, 0), &mut rejected, &mut dropped);
        assert_eq!(rejected, 0);
        assert!(s.pixels.iter().all(|&b| b == 0), "a tile that no rect covers must not be drawn");
    }

    #[test]
    fn srl_decodes_an_adaptive_zero_run_then_a_value() {
        // Bits: 0 | 1 | 0 | 0
        //   '0'  -> zero-run header: nz = 1 << k, with the initial kp = 8 giving k = 1,
        //           so two zeros, and kp adapts up to 12.
        //   '1'  -> next run is explicit: read k = 1 bit ...
        //   '0'  -> ... a run of 0, so a value follows immediately; kp adapts down to 6.
        //   '0'  -> sign bit, positive. With numBits == 1 the magnitude is implicitly 1.
        let mut st = UpgradeState::new(&[0b0100_0000], &[]);
        assert_eq!(st.srl_read(1), 0, "first zero of the run");
        assert_eq!(st.srl_read(1), 0, "second zero of the run");
        assert_eq!(st.kp, 12, "a '0' header adapts kp up by 4");
        assert_eq!(st.srl_read(1), 1, "then a positive unit value");
        assert_eq!(st.kp, 6, "emitting a value adapts kp down by 6");
    }

    #[test]
    fn srl_reads_a_unary_magnitude_when_more_than_one_bit_is_added() {
        // Bits: 1 | 0 | 1 | 0 1
        //   '1'   explicit-run header, so read k = 1 run bit ...
        //   '0'   ... an empty run, so a value follows.
        //   '1'   sign bit: negative.
        //   '0''1' unary magnitude: one '0' before the terminating '1' gives magnitude 2,
        //          which is under the (1 << numBits) - 1 == 3 cap.
        let mut st = UpgradeState::new(&[0b1010_1000], &[]);
        assert_eq!(st.srl_read(2), -2);
    }

    #[test]
    fn srl_magnitude_saturates_at_the_numbits_maximum() {
        // Same shape, but the unary run never terminates within the stream: the magnitude
        // must stop at (1 << numBits) - 1 rather than running away.
        let mut st = UpgradeState::new(&[0b1000_0000, 0x00], &[]);
        assert_eq!(st.srl_read(2), 3, "capped at (1 << 2) - 1");
    }

    #[test]
    fn upgrade_refines_significant_coefficients_from_the_raw_stream() {
        // Two coefficients already significant (one positive, one negative) and one still
        // zero. numBits = 2, shift = 3.
        let raw = [0b1101_0000u8]; // first coefficient reads 0b11 = 3, second 0b01 = 1
        let mut st = UpgradeState::new(&[], &raw);
        let mut current = [100i16, 100, 100];
        let mut sign = [5i16, -5, 0];
        // Only the first two consult the raw stream; the third would consult SRL, which is
        // empty here, so restrict the block to the first two.
        st.upgrade_block(&mut current[..2], &mut sign[..2], 3, 2, true);
        assert_eq!(current[0], 100 + (3 << 3), "positive coefficient refined upward");
        assert_eq!(current[1], 100 - (1 << 3), "negative coefficient refined downward");
    }

    #[test]
    fn an_upgrade_pass_carrying_no_new_bits_consumes_nothing() {
        let mut st = UpgradeState::new(&[0xFF], &[0xFF]);
        let mut current = [7i16; 4];
        let mut sign = [1i16; 4];
        st.upgrade_block(&mut current, &mut sign, 3, 0, true);
        assert_eq!(current, [7i16; 4], "numBits == 0 must leave coefficients untouched");
        assert_eq!(st.raw.bit_pos, 0, "and must not consume any raw bits");
        assert_eq!(st.srl.bit_pos, 0, "or any SRL bits");
    }

    #[test]
    fn subband_layouts_tile_the_whole_coefficient_buffer() {
        for extrapolate in [false, true] {
            let layout = subband_layout(extrapolate);
            let mut next = 0usize;
            for (off, len) in layout {
                assert_eq!(off, next, "subbands must be contiguous (extrapolate={extrapolate})");
                next += len;
            }
            assert_eq!(next, 4096, "subbands must cover all 4096 coefficients");
        }
    }

    #[test]
    fn a_run_of_difference_tiles_accumulates_rather_than_restarting() {
        let mut baseline = [0i16; 4096];
        baseline[0] = 10;

        let mut d1 = [0i16; 4096];
        d1[0] = 3;
        accumulate_diff(&mut d1, &mut baseline);
        assert_eq!(d1[0], 13, "first delta renders base+d1");
        assert_eq!(baseline[0], 13, "and the baseline must advance to the sum");

        let mut d2 = [0i16; 4096];
        d2[0] = 5;
        accumulate_diff(&mut d2, &mut baseline);
        assert_eq!(d2[0], 18, "second delta renders base+d1+d2, NOT base+d2 (=15)");
        assert_eq!(baseline[0], 18);
    }

    #[test]
    fn difference_accumulation_wraps_like_the_reference() {
        // FreeRDP casts the INT32 sum back to INT16; matching that keeps us bit-identical
        // on the pathological tiles rather than saturating differently.
        let mut baseline = [0i16; 4096];
        baseline[7] = i16::MAX;
        let mut delta = [0i16; 4096];
        delta[7] = 1;
        accumulate_diff(&mut delta, &mut baseline);
        assert_eq!(delta[7], i16::MIN);
        assert_eq!(baseline[7], i16::MIN);
    }

    #[test]
    fn per_surface_reset_leaves_other_surfaces_alone() {
        let mut ctx = ProgressiveContext::new();
        for id in [1u16, 2] {
            ctx.tile_baseline.insert(
                (id, 0, 0),
                TileState { current: [[0i16; 4096]; 3], sign: [[0i16; 4096]; 3], bit_pos: [ComponentQuant::default(); 3] },
            );
        }
        assert_eq!(ctx.baseline_count(), 2);
        ctx.reset_surface(1);
        assert_eq!(ctx.baseline_count(), 1, "only the named surface's baselines may be dropped");
        assert!(ctx.tile_baseline.contains_key(&(2, 0, 0)));
        ctx.reset_all();
        assert_eq!(ctx.baseline_count(), 0);
    }
}
