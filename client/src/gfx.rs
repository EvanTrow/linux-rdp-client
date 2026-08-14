use anyhow::{bail, Context, Result};

pub const CMD_WIRE_TO_SURFACE_1: u16 = 0x0001;
pub const CMD_WIRE_TO_SURFACE_2: u16 = 0x0002;
pub const CMD_SOLID_FILL: u16 = 0x0004;
pub const CMD_SURFACE_TO_SURFACE: u16 = 0x0005;
pub const CMD_SURFACE_TO_CACHE: u16 = 0x0006;
pub const CMD_CACHE_TO_SURFACE: u16 = 0x0007;
pub const CMD_EVICT_CACHE_ENTRY: u16 = 0x0008;
pub const CMD_CREATE_SURFACE: u16 = 0x0009;
pub const CMD_DELETE_SURFACE: u16 = 0x000A;
pub const CMD_START_FRAME: u16 = 0x000B;
pub const CMD_END_FRAME: u16 = 0x000C;
pub const CMD_FRAME_ACKNOWLEDGE: u16 = 0x000D;
pub const CMD_RESET_GRAPHICS: u16 = 0x000E;
pub const CMD_MAP_SURFACE_TO_OUTPUT: u16 = 0x000F;
pub const CMD_DELETE_ENCODING_CONTEXT: u16 = 0x0003;
pub const CMD_CACHE_IMPORT_OFFER: u16 = 0x0010;
pub const CMD_CACHE_IMPORT_REPLY: u16 = 0x0011;
pub const CMD_CAPS_ADVERTISE: u16 = 0x0012;
pub const CMD_CAPS_CONFIRM: u16 = 0x0013;

pub const CODEC_UNCOMPRESSED: u16 = 0x0000;
pub const CODEC_CLEARCODEC: u16 = 0x0008;
pub const CODEC_CAPROGRESSIVE: u16 = 0x0009;

fn header(cmd_id: u16, payload_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload_len);
    out.extend_from_slice(&cmd_id.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flags: always 0
    out.extend_from_slice(&((8 + payload_len) as u32).to_le_bytes()); // pduLength
    out
}

/// RDPGFX_CAPS_ADVERTISE_PDU advertising only RDPGFX_CAPSET_VERSION8 with flags=0 — this
/// forces the server to use RemoteFX Progressive (never AVC/H.264), since AVC support is
/// only ever signaled via flags/versions this client simply never offers.
pub fn build_caps_advertise() -> Vec<u8> {
    const CAPVERSION_8: u32 = 0x0008_0004;
    let mut caps_set = Vec::new();
    caps_set.extend_from_slice(&CAPVERSION_8.to_le_bytes()); // version
    caps_set.extend_from_slice(&4u32.to_le_bytes()); // capsDataLength
    caps_set.extend_from_slice(&0u32.to_le_bytes()); // capsData: flags = 0

    let mut payload = Vec::new();
    payload.extend_from_slice(&1u16.to_le_bytes()); // capsSetCount
    payload.extend_from_slice(&caps_set);

    let mut out = header(CMD_CAPS_ADVERTISE, payload.len());
    out.extend_from_slice(&payload);
    out
}

pub fn build_frame_acknowledge(frame_id: u32, total_frames_decoded: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_le_bytes()); // queueDepth: unavailable
    payload.extend_from_slice(&frame_id.to_le_bytes());
    payload.extend_from_slice(&total_frames_decoded.to_le_bytes());
    let mut out = header(CMD_FRAME_ACKNOWLEDGE, payload.len());
    out.extend_from_slice(&payload);
    out
}

/// `RDPGFX_RECT16` (MS-RDPEGFX §2.2.1.2). `right`/`bottom` are **exclusive**, so the width
/// is `right - left` — not `right - left + 1` as it would be for MS-RDPBCGR's inclusive
/// `TS_BITMAP_DATA.destRight`/`destBottom`. The two conventions coexist in RDP and mixing
/// them up is a classic off-by-one, so the conversion lives here rather than at each use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RectU16 {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl RectU16 {
    /// Width and height, or `None` for an inverted/degenerate rect. Returning an error case
    /// rather than subtracting blind matters: `right - left` on `u16` panics in debug builds
    /// and wraps to ~65535 in release, and a 65535-wide rect then drives a multi-gigabyte
    /// allocation in `extract_rect`.
    pub fn size(&self) -> Option<(u32, u32)> {
        if self.right < self.left || self.bottom < self.top {
            return None;
        }
        Some(((self.right - self.left) as u32, (self.bottom - self.top) as u32))
    }
}

#[derive(Debug)]
pub struct WireToSurface1 {
    pub surface_id: u16,
    pub codec_id: u16,
    pub pixel_format: u8,
    pub dest_rect: RectU16,
    pub bitmap_data: Vec<u8>,
}

/// `RDPGFX_WIRE_TO_SURFACE_PDU_2` — always RemoteFX Progressive per spec (the only codec
/// legal in this envelope). No `destRect`: the destination region comes entirely from
/// `RFX_PROGRESSIVE_REGION.rects` inside `bitmap_data` itself.
#[derive(Debug)]
pub struct WireToSurface2 {
    pub surface_id: u16,
    pub codec_id: u16,
    pub bitmap_data: Vec<u8>,
}

#[derive(Debug)]
pub struct CreateSurface {
    pub surface_id: u16,
    pub width: u16,
    pub height: u16,
    pub pixel_format: u8,
}

#[derive(Debug)]
pub struct MapSurfaceToOutput {
    pub surface_id: u16,
    pub output_origin_x: u32,
    pub output_origin_y: u32,
}

#[derive(Debug)]
pub struct SolidFill {
    pub surface_id: u16,
    pub color_bgra: [u8; 4],
    pub fill_rects: Vec<RectU16>,
}

#[derive(Debug)]
pub struct SurfaceToSurface {
    pub surface_id_src: u16,
    pub rect_src: RectU16,
    pub surface_id_dst: u16,
    pub dest_pts: Vec<(u16, u16)>,
}

#[derive(Debug)]
pub struct SurfaceToCache {
    pub surface_id: u16,
    pub cache_slot: u16,
    pub rect_src: RectU16,
}

#[derive(Debug)]
pub struct CacheToSurface {
    pub cache_slot: u16,
    pub surface_id: u16,
    pub dest_pts: Vec<(u16, u16)>,
}

/// One decoded RDPGFX PDU we act on. Anything else is structurally skipped by the caller
/// (see `capabilities`-style dispatch pattern) without needing a variant here.
#[derive(Debug)]
pub enum GfxPdu {
    CapsConfirm,
    ResetGraphics { width: u32, height: u32 },
    CreateSurface(CreateSurface),
    DeleteSurface { surface_id: u16 },
    MapSurfaceToOutput(MapSurfaceToOutput),
    StartFrame { frame_id: u32 },
    EndFrame { frame_id: u32 },
    WireToSurface1(WireToSurface1),
    WireToSurface2(WireToSurface2),
    SolidFill(SolidFill),
    SurfaceToSurface(SurfaceToSurface),
    SurfaceToCache(SurfaceToCache),
    CacheToSurface(CacheToSurface),
    EvictCacheEntry { cache_slot: u16 },
    /// `RDPGFX_DELETE_ENCODING_CONTEXT_PDU` (§2.2.2.3). Parsed rather than lumped into
    /// `Other` so the codec state it names can actually be dropped.
    DeleteEncodingContext { surface_id: u16, codec_context_id: u32 },
    Other { cmd_id: u16 },
}

fn read_rect16(data: &[u8]) -> Result<RectU16> {
    if data.len() < 8 {
        bail!("RDPGFX_RECT16 truncated");
    }
    Ok(RectU16 {
        left: u16::from_le_bytes([data[0], data[1]]),
        top: u16::from_le_bytes([data[2], data[3]]),
        right: u16::from_le_bytes([data[4], data[5]]),
        bottom: u16::from_le_bytes([data[6], data[7]]),
    })
}

/// Parses one RDPGFX PDU (header + payload) already extracted from the DVC data stream.
pub fn parse_pdu(data: &[u8]) -> Result<GfxPdu> {
    if data.len() < 8 {
        bail!("RDPGFX_HEADER truncated ({} bytes)", data.len());
    }
    let cmd_id = u16::from_le_bytes([data[0], data[1]]);
    let pdu_length = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    if pdu_length > data.len() {
        bail!("RDPGFX PDU length {pdu_length} exceeds available {} bytes", data.len());
    }
    let body = &data[8..pdu_length];

    match cmd_id {
        CMD_CAPS_CONFIRM => Ok(GfxPdu::CapsConfirm),
        CMD_RESET_GRAPHICS => {
            if body.len() < 8 {
                bail!("RESET_GRAPHICS truncated");
            }
            let width = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            let height = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            Ok(GfxPdu::ResetGraphics { width, height })
        }
        CMD_CREATE_SURFACE => {
            if body.len() < 7 {
                bail!("CREATE_SURFACE truncated");
            }
            Ok(GfxPdu::CreateSurface(CreateSurface {
                surface_id: u16::from_le_bytes([body[0], body[1]]),
                width: u16::from_le_bytes([body[2], body[3]]),
                height: u16::from_le_bytes([body[4], body[5]]),
                pixel_format: body[6],
            }))
        }
        CMD_DELETE_SURFACE => {
            if body.len() < 2 {
                bail!("DELETE_SURFACE truncated");
            }
            Ok(GfxPdu::DeleteSurface {
                surface_id: u16::from_le_bytes([body[0], body[1]]),
            })
        }
        CMD_MAP_SURFACE_TO_OUTPUT => {
            if body.len() < 12 {
                bail!("MAP_SURFACE_TO_OUTPUT truncated");
            }
            Ok(GfxPdu::MapSurfaceToOutput(MapSurfaceToOutput {
                surface_id: u16::from_le_bytes([body[0], body[1]]),
                output_origin_x: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                output_origin_y: u32::from_le_bytes([body[8], body[9], body[10], body[11]]),
            }))
        }
        CMD_START_FRAME => {
            if body.len() < 8 {
                bail!("START_FRAME truncated");
            }
            Ok(GfxPdu::StartFrame {
                frame_id: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
            })
        }
        CMD_END_FRAME => {
            if body.len() < 4 {
                bail!("END_FRAME truncated");
            }
            Ok(GfxPdu::EndFrame {
                frame_id: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
            })
        }
        CMD_WIRE_TO_SURFACE_1 => {
            if body.len() < 17 {
                bail!("WIRE_TO_SURFACE_1 truncated");
            }
            let surface_id = u16::from_le_bytes([body[0], body[1]]);
            let codec_id = u16::from_le_bytes([body[2], body[3]]);
            let pixel_format = body[4];
            let dest_rect = read_rect16(&body[5..13])?;
            let bitmap_data_length = u32::from_le_bytes([body[13], body[14], body[15], body[16]]) as usize;
            let start = 17;
            if start + bitmap_data_length > body.len() {
                bail!("WIRE_TO_SURFACE_1 bitmapData truncated");
            }
            Ok(GfxPdu::WireToSurface1(WireToSurface1 {
                surface_id,
                codec_id,
                pixel_format,
                dest_rect,
                bitmap_data: body[start..start + bitmap_data_length].to_vec(),
            }))
        }
        CMD_WIRE_TO_SURFACE_2 => {
            // surfaceId(2) + codecId(2) + codecContextId(4, unused — FreeRDP logs it but
            // never acts on it either) + pixelFormat(1) + bitmapDataLength(4) + bitmapData.
            if body.len() < 13 {
                bail!("WIRE_TO_SURFACE_2 truncated");
            }
            let surface_id = u16::from_le_bytes([body[0], body[1]]);
            let codec_id = u16::from_le_bytes([body[2], body[3]]);
            let bitmap_data_length = u32::from_le_bytes([body[9], body[10], body[11], body[12]]) as usize;
            let start = 13;
            if start + bitmap_data_length > body.len() {
                bail!("WIRE_TO_SURFACE_2 bitmapData truncated");
            }
            Ok(GfxPdu::WireToSurface2(WireToSurface2 {
                surface_id,
                codec_id,
                bitmap_data: body[start..start + bitmap_data_length].to_vec(),
            }))
        }
        CMD_SOLID_FILL => {
            // RDPGFX_SOLID_FILL_PDU (MS-RDPEGFX §2.2.2.2): surfaceId(2) + fillPixel
            // (RDPGFX_COLOR32: blue,green,red,xA reserved — 4 bytes) + fillRectCount(2) +
            // fillRects.
            if body.len() < 8 {
                bail!("SOLID_FILL truncated");
            }
            let surface_id = u16::from_le_bytes([body[0], body[1]]);
            let color_bgra = [body[2], body[3], body[4], body[5]];
            let rect_count = u16::from_le_bytes([body[6], body[7]]) as usize;
            let mut fill_rects = Vec::with_capacity(rect_count.min(1024));
            let mut pos = 8;
            for _ in 0..rect_count {
                // `&body[pos..]` panics rather than reaching read_rect16's own length guard
                // once pos runs past the body, so the bound is checked here first.
                if pos + 8 > body.len() {
                    bail!("SOLID_FILL fillRects truncated ({rect_count} claimed, {pos} bytes consumed of {})", body.len());
                }
                fill_rects.push(read_rect16(&body[pos..pos + 8])?);
                pos += 8;
            }
            Ok(GfxPdu::SolidFill(SolidFill { surface_id, color_bgra, fill_rects }))
        }
        CMD_SURFACE_TO_SURFACE => {
            // RDPGFX_SURFACE_TO_SURFACE_PDU (MS-RDPEGFX §2.2.2.5):
            //   surfaceIdSrc(2) surfaceIdDest(2) rectSrc(8) destPtsCount(2) destPts(4 each)
            // — 14 fixed bytes, matching FreeRDP's own `Stream_CheckAndLogRequiredLength(.., 14)`
            // in `rdpgfx_recv_surface_to_surface_pdu`. This previously read `rectSrc` from
            // offset 2 and `surfaceIdDest` from offset 10, i.e. two bytes early: `rectSrc`
            // came out as (surfaceIdDest, left, top, right) and `surfaceIdDest` as the real
            // `bottom`. Since a real `bottom` is a screen coordinate, it essentially never
            // names a live surface, so every SurfaceToSurface silently became a no-op — and
            // SurfaceToSurface is exactly what the server uses for scroll optimisation and
            // for moving window content, which is why the artifacts were strip- and
            // chrome-shaped.
            if body.len() < 14 {
                bail!("SURFACE_TO_SURFACE truncated ({} bytes, need 14)", body.len());
            }
            let surface_id_src = u16::from_le_bytes([body[0], body[1]]);
            let surface_id_dst = u16::from_le_bytes([body[2], body[3]]);
            let rect_src = read_rect16(&body[4..12])?;
            let pt_count = u16::from_le_bytes([body[12], body[13]]) as usize;
            let mut dest_pts = Vec::with_capacity(pt_count);
            let mut pos = 14;
            for _ in 0..pt_count {
                if pos + 4 > body.len() {
                    bail!("SURFACE_TO_SURFACE destPts truncated");
                }
                dest_pts.push((
                    u16::from_le_bytes([body[pos], body[pos + 1]]),
                    u16::from_le_bytes([body[pos + 2], body[pos + 3]]),
                ));
                pos += 4;
            }
            Ok(GfxPdu::SurfaceToSurface(SurfaceToSurface {
                surface_id_src,
                rect_src,
                surface_id_dst,
                dest_pts,
            }))
        }
        CMD_SURFACE_TO_CACHE => {
            if body.len() < 20 {
                bail!("SURFACE_TO_CACHE truncated");
            }
            let surface_id = u16::from_le_bytes([body[0], body[1]]);
            // bytes 2..10 are cacheKey (u64, opaque) — not needed, we key our own cache by slot.
            let cache_slot = u16::from_le_bytes([body[10], body[11]]);
            let rect_src = read_rect16(&body[12..20])?;
            Ok(GfxPdu::SurfaceToCache(SurfaceToCache { surface_id, cache_slot, rect_src }))
        }
        CMD_CACHE_TO_SURFACE => {
            if body.len() < 6 {
                bail!("CACHE_TO_SURFACE truncated");
            }
            let cache_slot = u16::from_le_bytes([body[0], body[1]]);
            let surface_id = u16::from_le_bytes([body[2], body[3]]);
            let pt_count = u16::from_le_bytes([body[4], body[5]]) as usize;
            let mut dest_pts = Vec::with_capacity(pt_count);
            let mut pos = 6;
            for _ in 0..pt_count {
                if pos + 4 > body.len() {
                    bail!("CACHE_TO_SURFACE destPts truncated");
                }
                dest_pts.push((
                    u16::from_le_bytes([body[pos], body[pos + 1]]),
                    u16::from_le_bytes([body[pos + 2], body[pos + 3]]),
                ));
                pos += 4;
            }
            Ok(GfxPdu::CacheToSurface(CacheToSurface { cache_slot, surface_id, dest_pts }))
        }
        CMD_DELETE_ENCODING_CONTEXT => {
            if body.len() < 6 {
                bail!("DELETE_ENCODING_CONTEXT truncated");
            }
            Ok(GfxPdu::DeleteEncodingContext {
                surface_id: u16::from_le_bytes([body[0], body[1]]),
                codec_context_id: u32::from_le_bytes([body[2], body[3], body[4], body[5]]),
            })
        }
        CMD_EVICT_CACHE_ENTRY => {
            if body.len() < 2 {
                bail!("EVICT_CACHE_ENTRY truncated");
            }
            Ok(GfxPdu::EvictCacheEntry { cache_slot: u16::from_le_bytes([body[0], body[1]]) })
        }
        other => Ok(GfxPdu::Other { cmd_id: other }),
    }
}

/// One entry from `split_pdus`: either a parsed PDU or the reason that PDU could not be
/// parsed, with the cmdId it claimed to be.
pub enum SplitEntry {
    Pdu(GfxPdu),
    Failed { cmd_id: u16, pdu_length: usize, error: anyhow::Error },
}

/// A DVC data message can contain more than one RDPGFX PDU back-to-back (each self-
/// describing its own length via pduLength) — split them out.
///
/// A PDU whose *body* fails to parse does not abandon the rest of the message. Framing is
/// recoverable because `pduLength` is read from the fixed header before the body is touched,
/// so the next PDU's offset is still known; and on a delta protocol, throwing away the four
/// good updates that shared a message with one bad one turns a single defect into five
/// permanent artifacts. Only a corrupt `pduLength` — where the next offset genuinely is
/// unknown — ends the message, and that is an error the caller must surface.
pub fn split_pdus(mut data: &[u8]) -> Result<Vec<SplitEntry>> {
    let mut out = Vec::new();
    while data.len() >= 8 {
        let cmd_id = u16::from_le_bytes([data[0], data[1]]);
        let pdu_length = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        if pdu_length < 8 || pdu_length > data.len() {
            bail!("RDPGFX PDU length {pdu_length} invalid (cmd_id={cmd_id:#06x}, {} bytes remain)", data.len());
        }
        out.push(match parse_pdu(&data[..pdu_length]).context("parsing RDPGFX PDU") {
            Ok(pdu) => SplitEntry::Pdu(pdu),
            Err(error) => SplitEntry::Failed { cmd_id, pdu_length, error },
        });
        data = &data[pdu_length..];
    }
    if !data.is_empty() {
        bail!("RDPGFX message has {} trailing bytes that are too short to be a PDU header", data.len());
    }
    Ok(out)
}

/// Human-readable name for an RDPGFX cmdId, for logging unhandled PDUs by name rather than
/// by a bare number.
pub fn cmd_name(cmd_id: u16) -> &'static str {
    match cmd_id {
        CMD_WIRE_TO_SURFACE_1 => "WIRE_TO_SURFACE_1",
        CMD_WIRE_TO_SURFACE_2 => "WIRE_TO_SURFACE_2",
        CMD_DELETE_ENCODING_CONTEXT => "DELETE_ENCODING_CONTEXT",
        CMD_SOLID_FILL => "SOLID_FILL",
        CMD_SURFACE_TO_SURFACE => "SURFACE_TO_SURFACE",
        CMD_SURFACE_TO_CACHE => "SURFACE_TO_CACHE",
        CMD_CACHE_TO_SURFACE => "CACHE_TO_SURFACE",
        CMD_EVICT_CACHE_ENTRY => "EVICT_CACHE_ENTRY",
        CMD_CREATE_SURFACE => "CREATE_SURFACE",
        CMD_DELETE_SURFACE => "DELETE_SURFACE",
        CMD_START_FRAME => "START_FRAME",
        CMD_END_FRAME => "END_FRAME",
        CMD_FRAME_ACKNOWLEDGE => "FRAME_ACKNOWLEDGE",
        CMD_RESET_GRAPHICS => "RESET_GRAPHICS",
        CMD_MAP_SURFACE_TO_OUTPUT => "MAP_SURFACE_TO_OUTPUT",
        CMD_CACHE_IMPORT_OFFER => "CACHE_IMPORT_OFFER",
        CMD_CACHE_IMPORT_REPLY => "CACHE_IMPORT_REPLY",
        CMD_CAPS_ADVERTISE => "CAPS_ADVERTISE",
        CMD_CAPS_CONFIRM => "CAPS_CONFIRM",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdu(cmd_id: u16, body: &[u8]) -> Vec<u8> {
        let mut v = header(cmd_id, body.len());
        v.extend_from_slice(body);
        v
    }

    /// Byte layout straight from MS-RDPEGFX §2.2.2.5 / FreeRDP's
    /// `rdpgfx_recv_surface_to_surface_pdu`, which requires 14 fixed bytes:
    /// surfaceIdSrc(2) surfaceIdDest(2) rectSrc(8) destPtsCount(2).
    #[test]
    fn surface_to_surface_reads_dest_id_before_the_source_rect() {
        let mut body = Vec::new();
        body.extend_from_slice(&7u16.to_le_bytes()); // surfaceIdSrc
        body.extend_from_slice(&9u16.to_le_bytes()); // surfaceIdDest
        body.extend_from_slice(&10u16.to_le_bytes()); // rectSrc.left
        body.extend_from_slice(&20u16.to_le_bytes()); // rectSrc.top
        body.extend_from_slice(&110u16.to_le_bytes()); // rectSrc.right (exclusive)
        body.extend_from_slice(&70u16.to_le_bytes()); // rectSrc.bottom (exclusive)
        body.extend_from_slice(&2u16.to_le_bytes()); // destPtsCount
        body.extend_from_slice(&1u16.to_le_bytes()); // destPts[0].x
        body.extend_from_slice(&2u16.to_le_bytes()); // destPts[0].y
        body.extend_from_slice(&3u16.to_le_bytes()); // destPts[1].x
        body.extend_from_slice(&4u16.to_le_bytes()); // destPts[1].y

        let GfxPdu::SurfaceToSurface(s) = parse_pdu(&pdu(CMD_SURFACE_TO_SURFACE, &body)).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(s.surface_id_src, 7);
        assert_eq!(s.surface_id_dst, 9, "surfaceIdDest is at offset 2, not offset 10");
        assert_eq!(s.rect_src, RectU16 { left: 10, top: 20, right: 110, bottom: 70 });
        assert_eq!(s.rect_src.size(), Some((100, 50)));
        assert_eq!(s.dest_pts, vec![(1, 2), (3, 4)]);
    }

    #[test]
    fn surface_to_surface_rejects_a_13_byte_body_instead_of_panicking() {
        let err = parse_pdu(&pdu(CMD_SURFACE_TO_SURFACE, &[0u8; 13])).unwrap_err();
        assert!(err.to_string().contains("truncated"), "got: {err}");
    }

    #[test]
    fn solid_fill_rejects_a_lying_rect_count_instead_of_panicking() {
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_le_bytes()); // surfaceId
        body.extend_from_slice(&[0, 0, 0, 0]); // fillPixel
        body.extend_from_slice(&500u16.to_le_bytes()); // fillRectCount, but no rects follow
        let err = parse_pdu(&pdu(CMD_SOLID_FILL, &body)).unwrap_err();
        assert!(err.to_string().contains("truncated"), "got: {err}");
    }

    #[test]
    fn rect_size_rejects_an_inverted_rect_rather_than_wrapping() {
        assert_eq!(RectU16 { left: 100, top: 0, right: 10, bottom: 10 }.size(), None);
        assert_eq!(RectU16 { left: 0, top: 100, right: 10, bottom: 10 }.size(), None);
        // Exclusive right/bottom: a 0..10 rect is 10 wide, not 11.
        assert_eq!(RectU16 { left: 0, top: 0, right: 10, bottom: 4 }.size(), Some((10, 4)));
    }

    #[test]
    fn one_unparseable_pdu_does_not_discard_the_rest_of_the_message() {
        let mut msg = Vec::new();
        msg.extend_from_slice(&pdu(CMD_END_FRAME, &1u32.to_le_bytes()));
        msg.extend_from_slice(&pdu(CMD_SURFACE_TO_SURFACE, &[0u8; 13])); // too short to parse
        msg.extend_from_slice(&pdu(CMD_END_FRAME, &2u32.to_le_bytes()));

        let entries = split_pdus(&msg).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[0], SplitEntry::Pdu(GfxPdu::EndFrame { frame_id: 1 })));
        assert!(matches!(entries[1], SplitEntry::Failed { cmd_id: CMD_SURFACE_TO_SURFACE, .. }));
        assert!(
            matches!(entries[2], SplitEntry::Pdu(GfxPdu::EndFrame { frame_id: 2 })),
            "a bad PDU must not swallow the good PDUs that follow it"
        );
    }

    #[test]
    fn a_corrupt_pdu_length_ends_the_message_loudly() {
        let mut msg = pdu(CMD_END_FRAME, &1u32.to_le_bytes());
        msg[4] = 0xFF; // pduLength far past the buffer
        assert!(split_pdus(&msg).is_err(), "unrecoverable framing must not be silently truncated");
    }

    #[test]
    fn cache_to_surface_parses_every_destination_point() {
        let mut body = Vec::new();
        body.extend_from_slice(&5u16.to_le_bytes()); // cacheSlot
        body.extend_from_slice(&1u16.to_le_bytes()); // surfaceId
        body.extend_from_slice(&3u16.to_le_bytes()); // destPtsCount
        for i in 0..3u16 {
            body.extend_from_slice(&(i * 10).to_le_bytes());
            body.extend_from_slice(&(i * 20).to_le_bytes());
        }
        let GfxPdu::CacheToSurface(c) = parse_pdu(&pdu(CMD_CACHE_TO_SURFACE, &body)).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(c.cache_slot, 5);
        assert_eq!(c.surface_id, 1);
        assert_eq!(c.dest_pts, vec![(0, 0), (10, 20), (20, 40)]);
    }

    #[test]
    fn wire_to_surface_2_reads_bitmap_data_after_the_length_field() {
        // surfaceId(2) codecId(2) codecContextId(4) pixelFormat(1) bitmapDataLength(4) data
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&CODEC_CAPROGRESSIVE.to_le_bytes());
        body.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        body.push(0x20);
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let GfxPdu::WireToSurface2(w) = parse_pdu(&pdu(CMD_WIRE_TO_SURFACE_2, &body)).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(w.surface_id, 1);
        assert_eq!(w.codec_id, CODEC_CAPROGRESSIVE);
        assert_eq!(w.bitmap_data, vec![0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn delete_encoding_context_is_parsed_not_ignored() {
        let mut body = Vec::new();
        body.extend_from_slice(&4u16.to_le_bytes());
        body.extend_from_slice(&77u32.to_le_bytes());
        let parsed = parse_pdu(&pdu(CMD_DELETE_ENCODING_CONTEXT, &body)).unwrap();
        assert!(matches!(parsed, GfxPdu::DeleteEncodingContext { surface_id: 4, codec_context_id: 77 }));
    }
}
