use anyhow::{bail, Context, Result};

pub const CMD_WIRE_TO_SURFACE_1: u16 = 0x0001;
pub const CMD_SOLID_FILL: u16 = 0x0004;
pub const CMD_SURFACE_TO_SURFACE: u16 = 0x0005;
pub const CMD_CREATE_SURFACE: u16 = 0x0009;
pub const CMD_DELETE_SURFACE: u16 = 0x000A;
pub const CMD_START_FRAME: u16 = 0x000B;
pub const CMD_END_FRAME: u16 = 0x000C;
pub const CMD_FRAME_ACKNOWLEDGE: u16 = 0x000D;
pub const CMD_RESET_GRAPHICS: u16 = 0x000E;
pub const CMD_MAP_SURFACE_TO_OUTPUT: u16 = 0x000F;
pub const CMD_CAPS_ADVERTISE: u16 = 0x0012;
pub const CMD_CAPS_CONFIRM: u16 = 0x0013;

pub const CODEC_UNCOMPRESSED: u16 = 0x0000;
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

pub struct RectU16 {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

pub struct WireToSurface1 {
    pub surface_id: u16,
    pub codec_id: u16,
    pub pixel_format: u8,
    pub dest_rect: RectU16,
    pub bitmap_data: Vec<u8>,
}

pub struct CreateSurface {
    pub surface_id: u16,
    pub width: u16,
    pub height: u16,
    pub pixel_format: u8,
}

pub struct MapSurfaceToOutput {
    pub surface_id: u16,
    pub output_origin_x: u32,
    pub output_origin_y: u32,
}

pub struct SolidFill {
    pub color_bgra: [u8; 4],
    pub fill_rects: Vec<RectU16>,
}

pub struct SurfaceToSurface {
    pub surface_id_src: u16,
    pub rect_src: RectU16,
    pub surface_id_dst: u16,
    pub dest_pts: Vec<(u16, u16)>,
}

/// One decoded RDPGFX PDU we act on. Anything else is structurally skipped by the caller
/// (see `capabilities`-style dispatch pattern) without needing a variant here.
pub enum GfxPdu {
    CapsConfirm,
    ResetGraphics { width: u32, height: u32 },
    CreateSurface(CreateSurface),
    DeleteSurface { surface_id: u16 },
    MapSurfaceToOutput(MapSurfaceToOutput),
    StartFrame { frame_id: u32 },
    EndFrame { frame_id: u32 },
    WireToSurface1(WireToSurface1),
    SolidFill(SolidFill),
    SurfaceToSurface(SurfaceToSurface),
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
        CMD_SOLID_FILL => {
            if body.len() < 6 {
                bail!("SOLID_FILL truncated");
            }
            let color_bgra = [body[0], body[1], body[2], body[3]];
            let rect_count = u16::from_le_bytes([body[4], body[5]]) as usize;
            let mut fill_rects = Vec::with_capacity(rect_count);
            let mut pos = 6;
            for _ in 0..rect_count {
                fill_rects.push(read_rect16(&body[pos..])?);
                pos += 8;
            }
            Ok(GfxPdu::SolidFill(SolidFill { color_bgra, fill_rects }))
        }
        CMD_SURFACE_TO_SURFACE => {
            if body.len() < 12 {
                bail!("SURFACE_TO_SURFACE truncated");
            }
            let surface_id_src = u16::from_le_bytes([body[0], body[1]]);
            let rect_src = read_rect16(&body[2..10])?;
            let surface_id_dst = u16::from_le_bytes([body[10], body[11]]);
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
        other => Ok(GfxPdu::Other { cmd_id: other }),
    }
}

/// A DVC data message can contain more than one RDPGFX PDU back-to-back (each self-
/// describing its own length via pduLength) — split them out.
pub fn split_pdus(mut data: &[u8]) -> Result<Vec<GfxPdu>> {
    let mut out = Vec::new();
    while data.len() >= 8 {
        let pdu_length = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        if pdu_length < 8 || pdu_length > data.len() {
            bail!("RDPGFX PDU length {pdu_length} invalid ({} bytes remain)", data.len());
        }
        out.push(parse_pdu(&data[..pdu_length]).context("parsing RDPGFX PDU")?);
        data = &data[pdu_length..];
    }
    Ok(out)
}
