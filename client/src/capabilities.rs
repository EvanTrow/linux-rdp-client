use anyhow::{bail, Context, Result};

const PDU_TYPE_DEMAND_ACTIVE: u16 = 0x1;
const PDU_TYPE_CONFIRM_ACTIVE: u16 = 0x3;
const PDU_TYPE_DATA: u16 = 0x7;

const PDUTYPE2_SYNCHRONIZE: u8 = 31;
const PDUTYPE2_CONTROL: u8 = 20;
const PDUTYPE2_FONTLIST: u8 = 39;
const PDUTYPE2_FONTMAP: u8 = 40;
const PDUTYPE2_REFRESH_RECT: u8 = 0x21;
const PDUTYPE2_SUPPRESS_OUTPUT: u8 = 0x23;

/// The server's fixed global MCS channel (MCS_BASE_CHANNEL_ID(1001) + 1). Used as the
/// Confirm Active PDU's `originatorID` and the Synchronize PDU's `targetUser` — NOT the
/// same thing as the client's own user channel (`user_id`), which is what goes in
/// `pduSource`/`initiator` fields for client-sent PDUs.
pub const MCS_GLOBAL_CHANNEL_ID: u16 = 1002;

fn share_control_header(payload_len: usize, pdu_type: u16, channel_id: u16) -> Vec<u8> {
    let total_length = 6 + payload_len;
    let mut out = Vec::with_capacity(6);
    out.extend_from_slice(&(total_length as u16).to_le_bytes());
    out.extend_from_slice(&(pdu_type | 0x10).to_le_bytes());
    out.extend_from_slice(&channel_id.to_le_bytes());
    out
}

fn share_data_header(payload_len: usize, pdu_type2: u8, share_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&share_id.to_le_bytes());
    out.push(0); // pad1
    out.push(1); // streamId: STREAM_LOW
    out.extend_from_slice(&((12 + payload_len) as u16).to_le_bytes()); // uncompressedLength
    out.push(pdu_type2);
    out.push(0); // compressedType
    out.extend_from_slice(&0u16.to_le_bytes()); // compressedLength
    out
}

/// Wraps a Data PDU (share control header + share data header + payload).
fn build_data_pdu(pdu_type2: u8, share_id: u32, channel_id: u16, payload: &[u8]) -> Vec<u8> {
    let mut inner = share_data_header(payload.len(), pdu_type2, share_id);
    inner.extend_from_slice(payload);
    let mut out = share_control_header(inner.len(), PDU_TYPE_DATA, channel_id);
    out.extend_from_slice(&inner);
    out
}

fn cap_set(cap_type: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&cap_type.to_le_bytes());
    out.extend_from_slice(&((4 + body.len()) as u16).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Parses a Demand Active PDU (received via `mcs::recv_data_indication` on the I/O
/// channel), returning the server's `shareId` to echo back in Confirm Active. We don't
/// currently need to inspect the server's own capability sets in detail.
pub fn parse_demand_active(payload: &[u8]) -> Result<u32> {
    if payload.len() < 10 {
        bail!("Demand Active PDU too short ({} bytes)", payload.len());
    }
    let pdu_type = u16::from_le_bytes([payload[2], payload[3]]) & 0x0F;
    if pdu_type != PDU_TYPE_DEMAND_ACTIVE {
        bail!("expected Demand Active PDU (type {PDU_TYPE_DEMAND_ACTIVE}), got type {pdu_type}");
    }
    let share_id = u32::from_le_bytes(payload[6..10].try_into().unwrap());
    Ok(share_id)
}

/// Walks a Demand Active PDU's capability sets, calling `f(capabilitySetType, body)` for
/// each. Shared by every function below that needs to pull specific fields out of the
/// server's advertised capabilities instead of guessing what it expects.
fn for_each_capability_set<'a>(payload: &'a [u8], mut f: impl FnMut(u16, &'a [u8])) {
    if payload.len() < 18 {
        return;
    }
    let length_source_descriptor = u16::from_le_bytes([payload[10], payload[11]]) as usize;
    let mut pos = 14 + length_source_descriptor;
    let Some(&number_capabilities_lo) = payload.get(pos) else { return };
    let Some(&number_capabilities_hi) = payload.get(pos + 1) else { return };
    let number_capabilities = u16::from_le_bytes([number_capabilities_lo, number_capabilities_hi]);
    pos += 4; // numberCapabilities + pad2Octets
    for _ in 0..number_capabilities {
        if pos + 4 > payload.len() {
            return;
        }
        let cap_type = u16::from_le_bytes([payload[pos], payload[pos + 1]]);
        let cap_len = u16::from_le_bytes([payload[pos + 2], payload[pos + 3]]) as usize;
        if cap_len < 4 || pos + cap_len > payload.len() {
            return;
        }
        f(cap_type, &payload[pos + 4..pos + cap_len]);
        pos += cap_len;
    }
}

/// Extracts the server's own Order Capability Set (orderFlags, orderSupport[32]) from its
/// Demand Active PDU, so we can echo it straight back in our Confirm Active. Real Windows
/// hosts reject an all-zero orderSupport with ERRINFO_BADCAPABILITIES instead of the
/// spec-described graceful bitmap-only fallback — claiming symmetric support is what real
/// clients (mstsc, FreeRDP) do, even though this client doesn't implement decoding the
/// primary/secondary drawing orders that support implies. `bitmap::parse_update` treats any
/// non-bitmap update type as skippable rather than a hard error, as a safety net for
/// whatever the server ends up sending as a result.
pub fn extract_order_capability(payload: &[u8]) -> Option<(u16, [u8; 32])> {
    let mut result = None;
    for_each_capability_set(payload, |cap_type, body| {
        if cap_type == 3 && body.len() >= 64 && result.is_none() {
            let order_flags = u16::from_le_bytes([body[30], body[31]]);
            let mut order_support = [0u8; 32];
            order_support.copy_from_slice(&body[32..64]);
            result = Some((order_flags, order_support));
        }
    });
    result
}

/// Extracts the server's negotiated desktop size from its own Bitmap Capability Set —
/// what it actually agreed to, which may not match what we requested in Client Core Data.
/// Used to bound the Refresh Rect PDU correctly (a rectangle exceeding the server's actual
/// canvas triggers ERRINFO_INVALIDREFRESHRECTPDU).
pub fn extract_desktop_size(payload: &[u8]) -> Option<(u16, u16)> {
    let mut result = None;
    for_each_capability_set(payload, |cap_type, body| {
        if cap_type == 2 && body.len() >= 12 && result.is_none() {
            let width = u16::from_le_bytes([body[8], body[9]]);
            let height = u16::from_le_bytes([body[10], body[11]]);
            result = Some((width, height));
        }
    });
    result
}

/// Debug-only: enumerates the server's own capability sets from a Demand Active PDU and
/// prints a summary — used to calibrate our Confirm Active against what the server
/// actually expects/offers, instead of guessing.
pub fn debug_dump_demand_active(payload: &[u8]) {
    if payload.len() < 18 {
        eprintln!("[debug] Demand Active PDU too short to dump ({} bytes)", payload.len());
        return;
    }
    let length_source_descriptor = u16::from_le_bytes([payload[10], payload[11]]) as usize;
    let mut pos = 14 + length_source_descriptor;
    if pos + 4 > payload.len() {
        eprintln!("[debug] Demand Active PDU truncated before numberCapabilities");
        return;
    }
    let number_capabilities = u16::from_le_bytes([payload[pos], payload[pos + 1]]);
    pos += 4; // numberCapabilities + pad2Octets
    eprintln!("[debug] Demand Active: {number_capabilities} capability set(s)");
    for _ in 0..number_capabilities {
        if pos + 4 > payload.len() {
            eprintln!("[debug]   (truncated capability set list)");
            break;
        }
        let cap_type = u16::from_le_bytes([payload[pos], payload[pos + 1]]);
        let cap_len = u16::from_le_bytes([payload[pos + 2], payload[pos + 3]]) as usize;
        if cap_len < 4 || pos + cap_len > payload.len() {
            eprintln!("[debug]   type={cap_type} length={cap_len} (invalid/truncated)");
            break;
        }
        let body = &payload[pos + 4..pos + cap_len];
        eprint!("[debug]   type={cap_type} length={cap_len}");
        match cap_type {
            1 if body.len() >= 20 => {
                let extra_flags = u16::from_le_bytes([body[10], body[11]]);
                let refresh_rect_support = body[18];
                let suppress_output_support = body[19];
                eprint!(
                    " extraFlags={extra_flags:#06x} refreshRectSupport={refresh_rect_support} suppressOutputSupport={suppress_output_support}"
                );
            }
            2 if body.len() >= 10 => {
                // preferredBitsPerPixel(2)+receive1BPP(2)+receive4BPP(2)+receive8BPP(2)+
                // desktopWidth(2)@offset8+desktopHeight(2)@offset10... wait width is at 8-9.
                let desktop_width = u16::from_le_bytes([body[8], body[9]]);
                let desktop_height = if body.len() >= 12 {
                    u16::from_le_bytes([body[10], body[11]])
                } else {
                    0
                };
                eprint!(" desktopWidth={desktop_width} desktopHeight={desktop_height}");
            }
            3 if body.len() >= 32 => {
                // terminalDescriptor(16) + pad4octetsA(4) + desktopSaveXGranularity(2) +
                // desktopSaveYGranularity(2) + pad2octetsA(2) + maximumOrderLevel(2) +
                // numberFonts(2) + orderFlags(2) @ offset 30, orderSupport(32) @ offset 32.
                let order_flags = u16::from_le_bytes([body[30], body[31]]);
                let order_support = &body[32..(64).min(body.len())];
                eprint!(" orderFlags={order_flags:#06x} orderSupport={order_support:02x?}");
            }
            _ => {}
        }
        eprintln!();
        pos += cap_len;
    }
}

/// Builds the Confirm Active PDU: General, Bitmap, Order (all-zero orderSupport — no
/// drawing orders implemented, server falls back to Bitmap Updates for everything, which
/// is explicitly spec-sanctioned), Pointer, Input, Share, Color Table Cache, Font, and
/// Virtual Channel capability sets — the same 9 sets real clients send.
pub fn build_confirm_active(
    share_id: u32,
    pdu_source: u16,
    desktop_width: u16,
    desktop_height: u16,
    server_order_capability: Option<(u16, [u8; 32])>,
) -> Vec<u8> {
    let mut general = Vec::new();
    general.extend_from_slice(&1u16.to_le_bytes()); // osMajorType: WINDOWS (max compatibility)
    general.extend_from_slice(&3u16.to_le_bytes()); // osMinorType: WINDOWS_NT
    general.extend_from_slice(&0x0200u16.to_le_bytes()); // protocolVersion: TS_CAPS_PROTOCOLVERSION
    general.extend_from_slice(&0u16.to_le_bytes()); // pad2octetsA
    general.extend_from_slice(&0u16.to_le_bytes()); // compressionTypes: MUST be 0
    general.extend_from_slice(&0u16.to_le_bytes()); // extraFlags: none — no fast-path output yet
    general.extend_from_slice(&0u16.to_le_bytes()); // updateCapabilityFlag: MUST be 0
    general.extend_from_slice(&0u16.to_le_bytes()); // remoteUnshareFlag: MUST be 0
    general.extend_from_slice(&0u16.to_le_bytes()); // compressionLevel: MUST be 0
    general.push(0); // refreshRectSupport
    general.push(0); // suppressOutputSupport

    let mut bitmap = Vec::new();
    bitmap.extend_from_slice(&0x0018u16.to_le_bytes()); // preferredBitsPerPixel: 24
    bitmap.extend_from_slice(&1u16.to_le_bytes()); // receive1BitPerPixel (ignored, TRUE)
    bitmap.extend_from_slice(&1u16.to_le_bytes()); // receive4BitsPerPixel (ignored, TRUE)
    bitmap.extend_from_slice(&1u16.to_le_bytes()); // receive8BitsPerPixel (ignored, TRUE)
    bitmap.extend_from_slice(&desktop_width.to_le_bytes());
    bitmap.extend_from_slice(&desktop_height.to_le_bytes());
    bitmap.extend_from_slice(&0u16.to_le_bytes()); // pad2octets
    bitmap.extend_from_slice(&0u16.to_le_bytes()); // desktopResizeFlag: not supported yet
    bitmap.extend_from_slice(&1u16.to_le_bytes()); // bitmapCompressionFlag: MUST be TRUE
    bitmap.push(0); // highColorFlags (ignored)
    bitmap.push(0); // drawingFlags
    bitmap.extend_from_slice(&1u16.to_le_bytes()); // multipleRectangleSupport: MUST be TRUE
    bitmap.extend_from_slice(&0u16.to_le_bytes()); // pad2octetsB

    // Real Windows hosts reject an all-zero orderSupport with ERRINFO_BADCAPABILITIES
    // instead of the spec-described graceful bitmap-only fallback (confirmed against a
    // real Windows 11 24H2 host, 2026-08-12) — echo back exactly what the server itself
    // advertised in its Demand Active PDU. We don't implement decoding the primary/
    // secondary drawing orders this implies; `bitmap::parse_update` skips non-bitmap
    // update types as a safety net rather than erroring.
    let (server_order_flags, server_order_support) =
        server_order_capability.unwrap_or((0x0002, [0u8; 32]));

    let mut order = Vec::new();
    order.resize(16, 0); // terminalDescriptor
    order.extend_from_slice(&0u32.to_le_bytes()); // pad4octetsA
    order.extend_from_slice(&1u16.to_le_bytes()); // desktopSaveXGranularity
    order.extend_from_slice(&20u16.to_le_bytes()); // desktopSaveYGranularity
    order.extend_from_slice(&0u16.to_le_bytes()); // pad2octetsA
    order.extend_from_slice(&1u16.to_le_bytes()); // maximumOrderLevel: ORD_LEVEL_1_ORDERS
    order.extend_from_slice(&0u16.to_le_bytes()); // numberFonts
    order.extend_from_slice(&server_order_flags.to_le_bytes()); // orderFlags: mirror server's
    order.extend_from_slice(&server_order_support); // orderSupport: mirror server's
    order.extend_from_slice(&0u16.to_le_bytes()); // textFlags (ignored)
    order.extend_from_slice(&0u16.to_le_bytes()); // orderSupportExFlags
    order.extend_from_slice(&0u32.to_le_bytes()); // pad4octetsB
    order.extend_from_slice(&0u32.to_le_bytes()); // desktopSaveSize
    order.extend_from_slice(&0u16.to_le_bytes()); // pad2octetsC
    order.extend_from_slice(&0u16.to_le_bytes()); // pad2octetsD
    order.extend_from_slice(&0u16.to_le_bytes()); // textANSICodePage
    order.extend_from_slice(&0u16.to_le_bytes()); // pad2octetsE

    let mut pointer = Vec::new();
    pointer.extend_from_slice(&1u16.to_le_bytes()); // colorPointerFlag (ignored, TRUE)
    pointer.extend_from_slice(&25u16.to_le_bytes()); // colorPointerCacheSize
    pointer.extend_from_slice(&25u16.to_le_bytes()); // pointerCacheSize

    let mut input = Vec::new();
    input.extend_from_slice(&0x0005u16.to_le_bytes()); // inputFlags: SCANCODES | MOUSEX
    input.extend_from_slice(&0u16.to_le_bytes()); // pad2octetsA
    input.extend_from_slice(&0x0000_0409u32.to_le_bytes()); // keyboardLayout: US
    input.extend_from_slice(&4u32.to_le_bytes()); // keyboardType: IBM enhanced 101/102-key
    input.extend_from_slice(&0u32.to_le_bytes()); // keyboardSubType
    input.extend_from_slice(&12u32.to_le_bytes()); // keyboardFunctionKey
    input.resize(input.len() + 64, 0); // imeFileName

    let mut share = Vec::new();
    share.extend_from_slice(&0u16.to_le_bytes()); // nodeID
    share.extend_from_slice(&0u16.to_le_bytes()); // pad2octets

    let mut color_cache = Vec::new();
    color_cache.extend_from_slice(&6u16.to_le_bytes()); // colorTableCacheSize
    color_cache.extend_from_slice(&0u16.to_le_bytes()); // pad2octets

    let mut font = Vec::new();
    font.extend_from_slice(&1u16.to_le_bytes()); // fontSupportFlags: FONTSUPPORT_FONTLIST
    font.extend_from_slice(&0u16.to_le_bytes()); // pad2octets

    let mut virtual_channel = Vec::new();
    virtual_channel.extend_from_slice(&0u32.to_le_bytes()); // flags: VCCAPS_NO_COMPR

    // Real clients (mstsc, FreeRDP) always send these too, even when advertising no drawing
    // orders — some hosts validate their mere presence, not just content, per ERRINFO_
    // BADCAPABILITIES testing against a real Windows 11 host. All disabled/zeroed since we
    // don't implement caching, brushes, glyphs, offscreen bitmaps, or sound.
    let mut bitmap_cache = Vec::new();
    bitmap_cache.resize(24, 0); // pad1..pad6 (4 bytes each)
    bitmap_cache.extend_from_slice(&0u16.to_le_bytes()); // Cache0Entries
    bitmap_cache.extend_from_slice(&0u16.to_le_bytes()); // Cache0MaximumCellSize
    bitmap_cache.extend_from_slice(&0u16.to_le_bytes()); // Cache1Entries
    bitmap_cache.extend_from_slice(&0u16.to_le_bytes()); // Cache1MaximumCellSize
    bitmap_cache.extend_from_slice(&0u16.to_le_bytes()); // Cache2Entries
    bitmap_cache.extend_from_slice(&0u16.to_le_bytes()); // Cache2MaximumCellSize

    let mut glyph_cache = Vec::new();
    glyph_cache.resize(40, 0); // GlyphCache: 10x TS_CACHE_DEFINITION, all zero (no glyph caching)
    glyph_cache.extend_from_slice(&0u32.to_le_bytes()); // FragCache
    glyph_cache.extend_from_slice(&0u16.to_le_bytes()); // GlyphSupportLevel: GLYPH_SUPPORT_NONE
    glyph_cache.extend_from_slice(&0u16.to_le_bytes()); // pad2octets

    let mut sound = Vec::new();
    sound.extend_from_slice(&0u16.to_le_bytes()); // soundFlags: none (no beep support)
    sound.extend_from_slice(&0u16.to_le_bytes()); // pad2octetsA

    let mut brush = Vec::new();
    brush.extend_from_slice(&0u32.to_le_bytes()); // brushSupportLevel: BRUSH_DEFAULT

    let mut offscreen_cache = Vec::new();
    offscreen_cache.extend_from_slice(&0u32.to_le_bytes()); // offscreenSupportLevel: FALSE
    offscreen_cache.extend_from_slice(&0u16.to_le_bytes()); // offscreenCacheSize
    offscreen_cache.extend_from_slice(&0u16.to_le_bytes()); // offscreenCacheEntries

    let sets = [
        cap_set(1, &general),
        cap_set(2, &bitmap),
        cap_set(3, &order),
        cap_set(4, &bitmap_cache),
        cap_set(8, &pointer),
        cap_set(13, &input),
        cap_set(9, &share),
        cap_set(12, &sound),
        cap_set(15, &brush),
        cap_set(16, &glyph_cache),
        cap_set(17, &offscreen_cache),
        cap_set(10, &color_cache),
        cap_set(14, &font),
        cap_set(20, &virtual_channel),
    ];
    let number_capabilities = sets.len() as u16;
    let capability_sets: Vec<u8> = sets.concat();

    let source_descriptor = b"rdp-client\0";

    let mut payload = Vec::new();
    payload.extend_from_slice(&share_id.to_le_bytes());
    payload.extend_from_slice(&MCS_GLOBAL_CHANNEL_ID.to_le_bytes()); // originatorID: fixed server channel
    payload.extend_from_slice(&(source_descriptor.len() as u16).to_le_bytes());
    let combined_len = 2 + 2 + capability_sets.len(); // numberCapabilities + pad2Octets + sets
    payload.extend_from_slice(&(combined_len as u16).to_le_bytes());
    payload.extend_from_slice(source_descriptor);
    payload.extend_from_slice(&number_capabilities.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes()); // pad2Octets
    payload.extend_from_slice(&capability_sets);

    share_control_header(payload.len(), PDU_TYPE_CONFIRM_ACTIVE, pdu_source)
        .into_iter()
        .chain(payload)
        .collect()
}

/// `pdu_source` is the client's own MCS user channel (`user_id`), used as `pduSource` in the
/// share control header for every one of these client-sent Data PDUs.
pub fn build_synchronize(share_id: u32, pdu_source: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u16.to_le_bytes()); // messageType: SYNCMSGTYPE_SYNC
    payload.extend_from_slice(&MCS_GLOBAL_CHANNEL_ID.to_le_bytes()); // targetUser: fixed server channel
    build_data_pdu(PDUTYPE2_SYNCHRONIZE, share_id, pdu_source, &payload)
}

const CTRLACTION_REQUEST_CONTROL: u16 = 0x0001;
const CTRLACTION_COOPERATE: u16 = 0x0004;

fn build_control(share_id: u32, pdu_source: u16, action: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&action.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes()); // grantId
    payload.extend_from_slice(&0u32.to_le_bytes()); // controlId
    build_data_pdu(PDUTYPE2_CONTROL, share_id, pdu_source, &payload)
}

pub fn build_control_cooperate(share_id: u32, pdu_source: u16) -> Vec<u8> {
    build_control(share_id, pdu_source, CTRLACTION_COOPERATE)
}

pub fn build_control_request_control(share_id: u32, pdu_source: u16) -> Vec<u8> {
    build_control(share_id, pdu_source, CTRLACTION_REQUEST_CONTROL)
}

pub fn build_font_list(share_id: u32, pdu_source: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u16.to_le_bytes()); // numberFonts: SHOULD be 0
    payload.extend_from_slice(&0u16.to_le_bytes()); // totalNumFonts: SHOULD be 0
    payload.extend_from_slice(&0x0003u16.to_le_bytes()); // listFlags: FONTLIST_FIRST | FONTLIST_LAST
    payload.extend_from_slice(&0x0032u16.to_le_bytes()); // entrySize: SHOULD be 50
    build_data_pdu(PDUTYPE2_FONTLIST, share_id, pdu_source, &payload)
}

/// Explicitly requests a repaint of the whole desktop (TS_REFRESH_RECT_PDU, MS-RDPBCGR
/// 2.2.11.2). Some hosts don't automatically push an initial full-screen Bitmap Update
/// once the connection sequence finishes — sending this after Font List is standard
/// practice for a from-scratch client to trigger the first paint.
pub fn build_refresh_rect(share_id: u32, pdu_source: u16, width: u16, height: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(1); // numberOfAreas
    payload.extend_from_slice(&[0, 0, 0]); // pad3Octets
    payload.extend_from_slice(&0u16.to_le_bytes()); // left
    payload.extend_from_slice(&0u16.to_le_bytes()); // top
    payload.extend_from_slice(&(width - 1).to_le_bytes()); // right (inclusive)
    payload.extend_from_slice(&(height - 1).to_le_bytes()); // bottom (inclusive)
    build_data_pdu(PDUTYPE2_REFRESH_RECT, share_id, pdu_source, &payload)
}

/// Requests the server (re)start sending display updates (TS_SUPPRESS_OUTPUT_PDU,
/// MS-RDPBCGR 2.2.11.3, `allowDisplayUpdates=TRUE`). Some hosts default a fresh session to
/// suppressed output; this is the directly-named mechanism to un-suppress it, as distinct
/// from Refresh Rect (which asks for a repaint of specific areas but doesn't address
/// whether updates are flowing at all).
pub fn build_suppress_output_allow(share_id: u32, pdu_source: u16, width: u16, height: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(1); // allowDisplayUpdates: TRUE
    payload.extend_from_slice(&[0, 0, 0]); // pad3Octets
    payload.extend_from_slice(&0u16.to_le_bytes()); // left
    payload.extend_from_slice(&0u16.to_le_bytes()); // top
    payload.extend_from_slice(&(width - 1).to_le_bytes()); // right (inclusive)
    payload.extend_from_slice(&(height - 1).to_le_bytes()); // bottom (inclusive)
    build_data_pdu(PDUTYPE2_SUPPRESS_OUTPUT, share_id, pdu_source, &payload)
}

/// Returns the Data PDU subtype (pduType2) of a share-control/share-data-header-wrapped
/// PDU, without fully parsing it — enough to recognize the Font Map PDU (the last PDU of
/// the connection finalization sequence) or dispatch other Data PDUs (e.g. Bitmap Update).
pub fn peek_pdu_type2(payload: &[u8]) -> Result<u8> {
    if payload.len() < 18 {
        bail!("Data PDU too short to read pduType2 ({} bytes)", payload.len());
    }
    let pdu_type = u16::from_le_bytes([payload[2], payload[3]]) & 0x0F;
    if pdu_type != PDU_TYPE_DATA {
        bail!("expected a Data PDU (type {PDU_TYPE_DATA}), got type {pdu_type}");
    }
    // shareDataHeader = shareId(4) + pad1(1) + streamId(1) + uncompressedLength(2) +
    // pduType2(1) + ..., so pduType2 sits at local offset 8, absolute 6+8=14.
    Ok(payload[6 + 8])
}

/// The bytes of a Data PDU's payload, i.e. everything after the share control + share data
/// headers (18 bytes total).
pub fn data_pdu_payload(payload: &[u8]) -> Result<&[u8]> {
    payload.get(18..).context("Data PDU shorter than its own headers")
}

pub const PDUTYPE2_FONT_MAP: u8 = PDUTYPE2_FONTMAP;
