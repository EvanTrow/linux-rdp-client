//! Permanent debug instrumentation for the graphics path.
//!
//! These flags are deliberately kept in the codebase rather than being added and removed
//! per investigation: RDP is a pure delta protocol, so every rendering defect in this client
//! is "one update was lost or written to the wrong place, and nothing ever came back to
//! correct it". Diagnosing that requires being able to see *which frame last wrote each
//! region*, and that is not something you can reconstruct after the fact.
//!
//! Everything here is off unless the corresponding environment variable is set, and the
//! per-pixel cost when off is a single already-hot branch on an empty `Vec`.
//!
//! | env var | effect |
//! |---|---|
//! | `RDP_DEBUG_TINT=1` | tints every region by the frame that last wrote it (see below) |
//! | `RDP_DEBUG_RECTS=1` | outlines every applied update rect + draws its frame id |
//! | `RDP_DEBUG_STRICT=1` | turns every swallowed graphics decode failure into a hard error |
//! | `RDP_DEBUG_TRACE=1` | per-PDU trace of every graphics operation applied |
//! | `RDP_RECORD=<path>` | records the raw inbound graphics-channel byte stream to `<path>` |
//!
//! ## Reading the tint
//!
//! The hue of a region tells you which frame last wrote it. That separates the two failure
//! modes that look identical to the naked eye:
//!
//! * **Stale hue in the right place** — the update never arrived or never got applied.
//!   Investigate the receive/decode path.
//! * **Correct (current) hue in the wrong place** — the update arrived and decoded fine but
//!   the destination coordinates are wrong. Investigate the blit math.
//!
//! The tint is stored out-of-band, in a parallel per-pixel tag plane
//! (`surface::Surface::tags`) written by *every* surface write path — ClearCodec,
//! RemoteFX Progressive, SolidFill, SurfaceToSurface and CacheToSurface all funnel through
//! `Surface`'s primitives — and is only blended into pixels at presentation time. Baking the
//! tint into the surface itself would compound on every cache-to-surface copy (a region
//! copied 5 times would be tinted 5 times) and would poison the bitmap cache with tinted
//! source pixels, so it is applied non-destructively at the very end instead.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Number of distinct tint hues before the cycle repeats. 16 is comfortably more than the
/// number of frames anyone can distinguish by eye in a single glance, so a stale region is
/// unambiguous rather than "maybe it wrapped around".
pub const TINT_PERIOD: u32 = 16;

/// How much of the tint hue is mixed into the underlying content, in percent. Light enough
/// that text stays readable, strong enough that two adjacent frames are clearly different.
const TINT_STRENGTH_PCT: u32 = 15;

/// How many presented frames an update-rect outline survives before it disappears.
pub const OVERLAY_LIFETIME_FRAMES: u32 = 30;

#[derive(Debug)]
pub struct DebugFlags {
    pub tint: bool,
    pub rects: bool,
    pub strict: bool,
    pub trace: bool,
    pub record: Option<PathBuf>,
}

fn env_on(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(v.as_str(), "" | "0" | "false" | "no"),
        Err(_) => false,
    }
}

pub fn flags() -> &'static DebugFlags {
    static FLAGS: OnceLock<DebugFlags> = OnceLock::new();
    FLAGS.get_or_init(|| {
        let f = DebugFlags {
            tint: env_on("RDP_DEBUG_TINT"),
            rects: env_on("RDP_DEBUG_RECTS"),
            strict: env_on("RDP_DEBUG_STRICT"),
            trace: env_on("RDP_DEBUG_TRACE"),
            record: std::env::var("RDP_RECORD").ok().filter(|s| !s.is_empty()).map(PathBuf::from),
        };
        if f.tint || f.rects || f.strict || f.trace || f.record.is_some() {
            eprintln!("[debug] graphics instrumentation active: {f:?}");
        }
        f
    })
}

/// True when any mode needs the per-pixel "which frame wrote this" tag plane.
pub fn needs_tag_plane() -> bool {
    flags().tint
}

/// True when any mode needs per-update destination-rect tracking.
pub fn needs_update_rects() -> bool {
    flags().rects
}

pub fn strict() -> bool {
    flags().strict
}

pub fn trace() -> bool {
    flags().trace
}

/// Reports a graphics-path failure that the code is about to continue past.
///
/// Every call site of this is a place where a server update is being dropped, which on a
/// delta protocol means a permanent visible artifact — so it is never silent and it always
/// carries full context (operation, surface, rect, frame id). With `RDP_DEBUG_STRICT=1` it
/// is fatal instead of merely loud, which turns "find the first thing that went wrong in a
/// 4,000-PDU session" into a backtrace.
///
/// Strict mode is a runtime check rather than a `debug_assert!` on purpose: the RemoteFX
/// Progressive decoder is numerically heavy enough that debug builds cannot keep up with
/// real traffic (see RENDERING_ISSUES.md Issue 1), so every session that reproduces anything
/// is a `--release` session — and a `debug_assert!` would be compiled out of exactly the
/// builds that matter.
#[macro_export]
macro_rules! gfx_error {
    ($ctx:expr, $($arg:tt)*) => {{
        let ctx: &$crate::debug::GfxErrorContext = $ctx;
        eprintln!("[gfx-error] {ctx} :: {}", format_args!($($arg)*));
        if $crate::debug::strict() {
            panic!("graphics failure with RDP_DEBUG_STRICT=1: {} :: {}", ctx, format_args!($($arg)*));
        }
    }};
}

/// The context every graphics failure is reported with. Kept as a struct rather than a
/// format string so no call site can forget a field.
#[derive(Debug, Clone, Copy)]
pub struct GfxErrorContext {
    pub op: &'static str,
    pub surface_id: u16,
    pub frame_id: u32,
    /// (left, top, right, bottom), exclusive right/bottom, in surface coordinates.
    pub rect: (u32, u32, u32, u32),
}

impl GfxErrorContext {
    pub fn new(op: &'static str, surface_id: u16, frame_id: u32) -> Self {
        Self { op, surface_id, frame_id, rect: (0, 0, 0, 0) }
    }

    pub fn with_rect(mut self, left: u32, top: u32, right: u32, bottom: u32) -> Self {
        self.rect = (left, top, right, bottom);
        self
    }
}

impl std::fmt::Display for GfxErrorContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "op={} surface={} frame={} rect=({},{})-({},{})",
            self.op, self.surface_id, self.frame_id, self.rect.0, self.rect.1, self.rect.2, self.rect.3
        )
    }
}

/// Maps a frame counter onto a tag value stored in the tag plane. Tag 0 is reserved for
/// "never written", so a region that no update has ever touched is visually distinct from
/// one written by frame 0.
pub fn frame_tag(frame_counter: u32) -> u8 {
    (frame_counter % TINT_PERIOD) as u8 + 1
}

/// The BGR tint hue for a tag, evenly spaced around the hue circle at full saturation.
/// Tag 0 ("never written") is black, i.e. no tint.
pub fn tag_hue_bgr(tag: u8) -> [u8; 3] {
    if tag == 0 {
        return [0, 0, 0];
    }
    let step = (tag - 1) as u32 % TINT_PERIOD;
    // Hue in sixths-of-a-circle fixed point (0..=6*255), value and saturation both maximal.
    let h = step * 6 * 255 / TINT_PERIOD;
    let sector = h / 255;
    let frac = h % 255;
    let (r, g, b) = match sector {
        0 => (255, frac, 0),
        1 => (255 - frac, 255, 0),
        2 => (0, 255, frac),
        3 => (0, 255 - frac, 255),
        4 => (frac, 0, 255),
        _ => (255, 0, 255 - frac),
    };
    [b as u8, g as u8, r as u8]
}

/// Blends `TINT_STRENGTH_PCT` of a tag's hue into one BGRX pixel, in place.
pub fn apply_tint_bgrx(px: &mut [u8], tag: u8) {
    if tag == 0 {
        return;
    }
    let hue = tag_hue_bgr(tag);
    for c in 0..3 {
        let src = px[c] as u32;
        let dst = hue[c] as u32;
        px[c] = ((src * (100 - TINT_STRENGTH_PCT) + dst * TINT_STRENGTH_PCT) / 100) as u8;
    }
}

/// One applied update rect, remembered for `OVERLAY_LIFETIME_FRAMES` presented frames so
/// coordinate errors are visible as a persistent outline rather than a one-frame flash.
#[derive(Debug, Clone, Copy)]
pub struct OverlayRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub frame_id: u32,
    /// Presented frames since this rect was applied.
    pub age: u32,
}

/// Draws a 1px outline plus the frame id into a BGRX8888 image, fading with age.
pub fn draw_overlay_rect(pixels: &mut [u8], img_w: u32, img_h: u32, r: &OverlayRect) {
    if r.w == 0 || r.h == 0 {
        return;
    }
    let fade = OVERLAY_LIFETIME_FRAMES.saturating_sub(r.age) * 100 / OVERLAY_LIFETIME_FRAMES.max(1);
    if fade == 0 {
        return;
    }
    let color = tag_hue_bgr(frame_tag(r.frame_id));

    let x1 = (r.x + r.w).min(img_w).saturating_sub(1);
    let y1 = (r.y + r.h).min(img_h).saturating_sub(1);
    if r.x >= img_w || r.y >= img_h {
        return;
    }

    let put = |x: u32, y: u32, pixels: &mut [u8]| {
        if x >= img_w || y >= img_h {
            return;
        }
        let i = ((y * img_w + x) * 4) as usize;
        if i + 3 >= pixels.len() {
            return;
        }
        for c in 0..3 {
            let src = pixels[i + c] as u32;
            pixels[i + c] = ((src * (100 - fade) + color[c] as u32 * fade) / 100) as u8;
        }
    };

    for x in r.x..=x1 {
        put(x, r.y, pixels);
        put(x, y1, pixels);
    }
    for y in r.y..=y1 {
        put(r.x, y, pixels);
        put(x1, y, pixels);
    }

    draw_number(pixels, img_w, img_h, r.x + 2, r.y + 2, r.frame_id, color, fade);
}

/// 3x5 bitmap font for the digits 0-9, one bit per pixel, MSB-first, 3 bits per row.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b001, 0b001], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

/// Renders `value` in decimal at (x, y) using the 3x5 font, blended by `fade` percent.
#[allow(clippy::too_many_arguments)]
fn draw_number(pixels: &mut [u8], img_w: u32, img_h: u32, x: u32, y: u32, value: u32, color: [u8; 3], fade: u32) {
    let text = value.to_string();
    for (i, ch) in text.bytes().enumerate() {
        let glyph = &DIGITS[(ch - b'0') as usize];
        let gx = x + i as u32 * 4;
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..3u32 {
                if bits & (1 << (2 - col)) == 0 {
                    continue;
                }
                let px = gx + col;
                let py = y + row as u32;
                if px >= img_w || py >= img_h {
                    continue;
                }
                let idx = ((py * img_w + px) * 4) as usize;
                if idx + 3 >= pixels.len() {
                    continue;
                }
                for c in 0..3 {
                    let src = pixels[idx + c] as u32;
                    pixels[idx + c] = ((src * (100 - fade) + color[c] as u32 * fade) / 100) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_tag_never_returns_the_unwritten_sentinel() {
        for f in 0..1000 {
            assert_ne!(frame_tag(f), 0, "tag 0 is reserved for 'never written'");
        }
    }

    #[test]
    fn tint_hues_are_distinct_across_the_whole_period() {
        let mut seen = Vec::new();
        for f in 0..TINT_PERIOD {
            let hue = tag_hue_bgr(frame_tag(f));
            assert!(!seen.contains(&hue), "hue {hue:?} repeats within one tint period");
            seen.push(hue);
        }
        // ...and repeats only after a full period, so a stale region is unambiguous.
        assert_eq!(tag_hue_bgr(frame_tag(0)), tag_hue_bgr(frame_tag(TINT_PERIOD)));
    }

    #[test]
    fn tint_is_light_enough_to_read_content_through() {
        // A pure-white pixel must stay recognisably white under any tint.
        for f in 0..TINT_PERIOD {
            let mut px = [255u8, 255, 255, 255];
            apply_tint_bgrx(&mut px, frame_tag(f));
            for c in 0..3 {
                assert!(px[c] >= 216, "tint too strong: white -> {px:?}");
            }
        }
    }

    #[test]
    fn untinted_tag_leaves_pixels_untouched() {
        let mut px = [1u8, 2, 3, 255];
        apply_tint_bgrx(&mut px, 0);
        assert_eq!(px, [1, 2, 3, 255]);
    }
}
