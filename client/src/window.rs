use crate::input::{self, InputEvent};
use anyhow::{Context, Result};
use softbuffer::{Context as SbContext, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

/// Sent to the event loop the moment a frame is published, so the UI thread wakes on the
/// frame rather than on a timer.
#[derive(Debug, Clone, Copy)]
pub struct FrameReady;

#[derive(Default)]
struct Mailbox {
    slot: Mutex<Option<Vec<BitmapTile>>>,
    closed: AtomicBool,
    /// Set once, after the event loop exists. Publishing before then is fine — the first
    /// timer-driven draw picks it up.
    proxy: OnceLock<EventLoopProxy<FrameReady>>,
}

/// A single-slot, latest-wins hand-off of decoded frames from the network thread to the UI
/// thread.
///
/// This deliberately replaces a `sync_channel(1)`. A bounded channel keeps the *oldest*
/// queued item and rejects the new one, so `try_send` on a full channel discarded the
/// **newest** snapshot and presented the older one. On a delta protocol that is not a
/// dropped frame, it is a permanent artifact: the last frame of every burst — the moment a
/// drag is released, a scroll stops, a menu closes — is exactly the one most likely to be
/// dropped, and once the server goes idle no further END_FRAME arrives to carry that content
/// to the screen. The region stays frozen at its second-to-last state forever.
///
/// Overwriting is safe here precisely because each entry is a whole-surface snapshot rather
/// than a delta, so a newer entry strictly supersedes an unconsumed older one.
#[derive(Clone, Default)]
pub struct FrameMailbox(Arc<Mailbox>);

impl FrameMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes the newest frame, superseding any frame the UI thread has not yet consumed,
    /// and wakes the event loop immediately.
    ///
    /// The wake-up is the point. Without it the UI thread only notices a new frame on its
    /// next timer tick, and then only *presents* it a tick later still, because
    /// `request_redraw` is dispatched at the start of the following iteration — up to two
    /// frame periods of latency added to every single frame, which is what made dragging a
    /// window feel heavy next to a reference client.
    pub fn publish(&self, tiles: Vec<BitmapTile>) {
        *self.0.slot.lock().expect("frame mailbox poisoned") = Some(tiles);
        if let Some(proxy) = self.0.proxy.get() {
            // Fails only once the loop has exited, which `is_closed` already covers.
            let _ = proxy.send_event(FrameReady);
        }
    }

    fn attach_proxy(&self, proxy: EventLoopProxy<FrameReady>) {
        let _ = self.0.proxy.set(proxy);
    }

    /// Takes the newest published frame, if any.
    pub fn take(&self) -> Option<Vec<BitmapTile>> {
        self.0.slot.lock().expect("frame mailbox poisoned").take()
    }

    /// True once the window has gone away and there is nothing left to render for.
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        self.0.closed.store(true, Ordering::Relaxed);
    }
}

/// A single fullscreen-desktop-sized window with a CPU-side pixel buffer we blit bitmap
/// updates into. One instance per real monitor eventually (Phase 3); just one for now.
pub struct RemoteDesktopWindow {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    /// Shadow-framebuffer dimensions — the remote desktop's size, which is not necessarily
    /// the window's.
    width: u32,
    height: u32,
    /// The softbuffer presentation surface's current dimensions, which track the window.
    surface_width: u32,
    surface_height: u32,
    /// 0xAARRGGBB per pixel, row-major, matching softbuffer's expected format. This is the
    /// persistent, full-resolution shadow framebuffer: every update is applied into it and
    /// every present copies all of it out.
    framebuffer: Vec<u32>,
}

impl RemoteDesktopWindow {
    fn new(event_loop: &ActiveEventLoop, width: u32, height: u32) -> Result<Self> {
        let attrs = Window::default_attributes()
            .with_title("linux-rdp-client")
            .with_inner_size(winit::dpi::PhysicalSize::new(width, height))
            .with_resizable(false);
        let window = Rc::new(event_loop.create_window(attrs).context("creating window")?);
        let context = SbContext::new(window.clone()).map_err(|e| anyhow::anyhow!("creating softbuffer context: {e}"))?;
        let mut surface =
            Surface::new(&context, window.clone()).map_err(|e| anyhow::anyhow!("creating softbuffer surface: {e}"))?;
        surface
            .resize(
                NonZeroU32::new(width).context("zero width")?,
                NonZeroU32::new(height).context("zero height")?,
            )
            .map_err(|e| anyhow::anyhow!("sizing softbuffer surface: {e}"))?;

        Ok(Self {
            window,
            surface,
            width,
            height,
            surface_width: width,
            surface_height: height,
            framebuffer: vec![0xFF00_0000; (width * height) as usize],
        })
    }

    /// Blits a BGRX32 (as delivered by decoded RDP bitmap tiles) rectangle into the
    /// framebuffer at (x, y) and requests a redraw. `stride` is bytes per source row.
    pub fn blit_bgrx(&mut self, x: u32, y: u32, w: u32, h: u32, src: &[u8], stride: usize) {
        self.ensure_framebuffer(x + w, y + h);
        blit_bgrx_rows(&mut self.framebuffer, self.width, self.height, x, y, w, h, src, stride);
    }

    /// Resizes the softbuffer surface to match the window. `with_resizable(false)` is only a
    /// hint and several Linux window managers ignore it, so the presented buffer's dimensions
    /// have to track the window's actual size rather than the size we asked for.
    fn resize_surface(&mut self, width: u32, height: u32) {
        let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else { return };
        if (width, height) == (self.surface_width, self.surface_height) {
            return;
        }
        match self.surface.resize(w, h) {
            Ok(()) => {
                self.surface_width = width;
                self.surface_height = height;
                // The freshly-sized buffer holds undefined contents until something presents
                // into it, so ask for a repaint rather than leaving the window garbled until
                // the server happens to send another frame.
                self.window.request_redraw();
            }
            Err(e) => eprintln!("[window] resizing softbuffer surface to {width}x{height} failed: {e}"),
        }
    }

    /// Grows the shadow framebuffer if the remote desktop turns out to be bigger than the
    /// size we asked for. Existing content is preserved at its current coordinates; the new
    /// area starts black until the server paints it.
    fn ensure_framebuffer(&mut self, width: u32, height: u32) {
        if width <= self.width && height <= self.height {
            return;
        }
        let (nw, nh) = (width.max(self.width), height.max(self.height));
        eprintln!("[window] growing framebuffer {}x{} -> {nw}x{nh}", self.width, self.height);
        let mut grown = vec![0xFF00_0000u32; (nw * nh) as usize];
        for y in 0..self.height {
            let src = (y * self.width) as usize;
            let dst = (y * nw) as usize;
            grown[dst..dst + self.width as usize].copy_from_slice(&self.framebuffer[src..src + self.width as usize]);
        }
        self.framebuffer = grown;
        self.width = nw;
        self.height = nh;
    }

    /// Copies the *entire* shadow framebuffer into the presentation buffer.
    ///
    /// This is the property that makes the presentation path correct: the framebuffer is a
    /// persistent, full-resolution source of truth that every update has been applied into,
    /// and each present rewrites the whole presented image from it. Uploading only dirty
    /// rects into an acquired buffer would leave the untouched regions holding whatever the
    /// previous *presentation* buffer had — frame N-2 under double buffering — which is
    /// exactly the ghosting this client was reported to have.
    ///
    /// The copy is row-wise with clipping rather than `copy_from_slice` so that a framebuffer
    /// and a window of different sizes cannot panic.
    fn present(&mut self) -> Result<()> {
        let (sw, sh) = (self.surface_width, self.surface_height);
        let (fw, fh) = (self.width, self.height);
        let mut buf = self
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("locking softbuffer: {e}"))?;
        let cols = fw.min(sw) as usize;
        for y in 0..fh.min(sh) {
            let src = (y * fw) as usize;
            let dst = (y * sw) as usize;
            buf[dst..dst + cols].copy_from_slice(&self.framebuffer[src..src + cols]);
        }
        buf.present().map_err(|e| anyhow::anyhow!("presenting softbuffer: {e}"))?;
        Ok(())
    }
}

/// Copies a BGRX8888 rectangle into a `0RGB`-format `u32` framebuffer, clipped to it.
///
/// This is the hottest loop in the client: it runs over the whole desktop on every presented
/// frame, so a per-pixel formulation costs ~1M iterations of bounds-checked shifting per
/// frame at 1280x800.
///
/// It doesn't need to. The decoders store BGRX bytes, so on a little-endian machine a pixel's
/// four bytes read as a native `u32` are already `0xXXRRGGBB` — and softbuffer ignores the
/// top byte. The conversion is therefore the identity, and each row reduces to a copy the
/// compiler can vectorise. The big-endian path keeps the explicit shuffle; it is untested in
/// anger but this client only ever runs on little-endian hosts today.
#[allow(clippy::too_many_arguments)]
pub fn blit_bgrx_rows(
    dst: &mut [u32],
    dst_w: u32,
    dst_h: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    src: &[u8],
    src_stride: usize,
) {
    if x >= dst_w || y >= dst_h {
        return;
    }
    // Clip once, up front, instead of testing every pixel inside the loop.
    let cols = w.min(dst_w - x) as usize;
    let rows = h.min(dst_h - y) as usize;
    if cols == 0 {
        return;
    }

    for row in 0..rows {
        let src_start = row * src_stride;
        let Some(src_row) = src.get(src_start..src_start + cols * 4) else {
            // A short source buffer is the caller's bug, but truncating quietly here would
            // leave a half-drawn frame, so stop rather than paper over it.
            break;
        };
        let dst_start = (y as usize + row) * dst_w as usize + x as usize;
        let dst_row = &mut dst[dst_start..dst_start + cols];

        #[cfg(target_endian = "little")]
        for (d, s) in dst_row.iter_mut().zip(src_row.chunks_exact(4)) {
            *d = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        }
        #[cfg(target_endian = "big")]
        for (d, s) in dst_row.iter_mut().zip(src_row.chunks_exact(4)) {
            *d = ((s[2] as u32) << 16) | ((s[1] as u32) << 8) | s[0] as u32;
        }
    }
}

/// Trait for whatever drives the RDP session — polled once per event-loop iteration so the
/// network side can push new frames / consume queued input without blocking window events.
pub trait SessionDriver {
    /// Called once at startup with the desktop size negotiated over RDP.
    fn desktop_size(&self) -> (u32, u32);
    /// Called every loop iteration; return decoded tiles to blit, if any arrived.
    fn poll(&mut self) -> Vec<BitmapTile>;
    /// Called once when the window is going away, so the session can stop decoding rather
    /// than continue burning CPU on frames nobody will ever see.
    fn shutdown(&mut self) {}
}

pub struct BitmapTile {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// BGRX8888, row-major.
    pub pixels: Vec<u8>,
    pub stride: usize,
    /// When the decoder finished this frame, for the `RDP_DEBUG_PERF` measurement of how
    /// long a frame takes to reach the screen.
    pub decoded_at: std::time::Instant,
}

struct App<D: SessionDriver> {
    driver: D,
    win: Option<RemoteDesktopWindow>,
    input_tx: Sender<InputEvent>,
    latency: crate::debug::LatencyStats,
    /// Last known cursor position, in desktop pixel coordinates — `MouseInput` events don't
    /// carry a position of their own, only `CursorMoved` does, so we track it here to build
    /// a complete `TS_FP_POINTER_EVENT` (which always needs x/y) on a click.
    cursor_pos: (u16, u16),
}

impl<D: SessionDriver> App<D> {
    fn send_input(&self, ev: InputEvent) {
        // The receiving end lives on the network thread; if that thread has already exited
        // (session error/disconnect) there's nothing useful to do with a send failure here —
        // the window will just stop reacting, same as any other post-disconnect input.
        let _ = self.input_tx.send(ev);
    }
}

impl<D: SessionDriver> App<D> {
    /// Consumes the newest published frame and puts it on screen, synchronously.
    ///
    /// Presenting here rather than via `request_redraw` matters: a redraw request is
    /// dispatched at the *start of the next* event-loop iteration, so deferring to it adds a
    /// whole extra wake-up of latency to every frame.
    fn draw(&mut self) {
        // One whole-surface snapshot per mapped output region, all from the same frame, so
        // every tile must be blitted. (Collapsing a *burst* of frames to the newest happens
        // upstream in `FrameMailbox`, where superseding an unconsumed full snapshot is safe.)
        let tiles = self.driver.poll();
        if tiles.is_empty() {
            return;
        }
        let Some(win) = &mut self.win else { return };
        for tile in &tiles {
            win.blit_bgrx(tile.x, tile.y, tile.width, tile.height, &tile.pixels, tile.stride);
        }
        if let Err(e) = win.present() {
            eprintln!("present failed: {e:#}");
        }
        if crate::debug::perf() {
            if let Some(t) = tiles.first() {
                self.latency.record(t.decoded_at.elapsed().as_micros() as u64);
            }
        }
    }
}

impl<D: SessionDriver> ApplicationHandler<FrameReady> for App<D> {
    /// A frame was published. This is the whole point of the proxy: the loop is otherwise
    /// parked in `ControlFlow::Wait` and wakes exactly when there is something to show.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: FrameReady) {
        self.draw();
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (w, h) = self.driver.desktop_size();
        match RemoteDesktopWindow::new(event_loop, w, h) {
            Ok(win) => self.win = Some(win),
            Err(e) => {
                eprintln!("failed to create window: {e:#}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(win) = &mut self.win {
                    win.resize_surface(size.width, size.height);
                }
            }
            // The window manager is asking us to repaint (expose, unminimise, resize). New
            // frames do not come through here — they arrive as `FrameReady`.
            WindowEvent::RedrawRequested => {
                if let Some(win) = &mut self.win {
                    if let Err(e) = win.present() {
                        eprintln!("present failed: {e:#}");
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let (w, h) = self.driver.desktop_size();
                let x = (position.x.max(0.0) as u32).min(w.saturating_sub(1)) as u16;
                let y = (position.y.max(0.0) as u32).min(h.saturating_sub(1)) as u16;
                self.cursor_pos = (x, y);
                self.send_input(InputEvent::MouseMove { x, y });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(rdp_button) = mouse_button_flag(button) else { return };
                let (x, y) = self.cursor_pos;
                self.send_input(InputEvent::MouseButton {
                    x,
                    y,
                    button: rdp_button,
                    down: state == ElementState::Pressed,
                });
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let Some((scancode, extended)) = keycode_to_scancode(code) else { return };
                self.send_input(InputEvent::Key {
                    scancode,
                    release: event.state == ElementState::Released,
                    extended,
                });
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Park until something actually happens. Frames wake us through the proxy, input and
        // expose through the usual window events — so there is no reason to spin on a timer,
        // and a timer would only add latency between a frame arriving and being drawn.
        event_loop.set_control_flow(ControlFlow::Wait);
        // A frame published before the proxy was attached (or racing the wake-up) would
        // otherwise sit unclaimed until the next event; this costs nothing when idle.
        self.draw();
    }
}

fn mouse_button_flag(button: MouseButton) -> Option<u16> {
    match button {
        MouseButton::Left => Some(input::PTR_FLAGS_BUTTON1),
        MouseButton::Right => Some(input::PTR_FLAGS_BUTTON2),
        MouseButton::Middle => Some(input::PTR_FLAGS_BUTTON3),
        _ => None, // back/forward/other: no RDP fast-path mouse-button equivalent to map to
    }
}

/// Maps a winit physical `KeyCode` to a PC/XT ("Set 1") scancode + extended flag, as expected
/// by `TS_FP_KEYBOARD_EVENT` (MS-RDPBCGR). Covers the common keys (letters, digits,
/// punctuation, whitespace/editing, function keys, arrows, navigation cluster, modifiers) —
/// not exhaustive (numpad, media keys, IME keys are omitted; unmapped keys are silently
/// ignored rather than sending something wrong).
fn keycode_to_scancode(code: KeyCode) -> Option<(u8, bool)> {
    use KeyCode::*;
    let (sc, ext) = match code {
        Escape => (0x01, false),
        Digit1 => (0x02, false),
        Digit2 => (0x03, false),
        Digit3 => (0x04, false),
        Digit4 => (0x05, false),
        Digit5 => (0x06, false),
        Digit6 => (0x07, false),
        Digit7 => (0x08, false),
        Digit8 => (0x09, false),
        Digit9 => (0x0A, false),
        Digit0 => (0x0B, false),
        Minus => (0x0C, false),
        Equal => (0x0D, false),
        Backspace => (0x0E, false),
        Tab => (0x0F, false),
        KeyQ => (0x10, false),
        KeyW => (0x11, false),
        KeyE => (0x12, false),
        KeyR => (0x13, false),
        KeyT => (0x14, false),
        KeyY => (0x15, false),
        KeyU => (0x16, false),
        KeyI => (0x17, false),
        KeyO => (0x18, false),
        KeyP => (0x19, false),
        BracketLeft => (0x1A, false),
        BracketRight => (0x1B, false),
        Enter => (0x1C, false),
        ControlLeft => (0x1D, false),
        KeyA => (0x1E, false),
        KeyS => (0x1F, false),
        KeyD => (0x20, false),
        KeyF => (0x21, false),
        KeyG => (0x22, false),
        KeyH => (0x23, false),
        KeyJ => (0x24, false),
        KeyK => (0x25, false),
        KeyL => (0x26, false),
        Semicolon => (0x27, false),
        Quote => (0x28, false),
        Backquote => (0x29, false),
        ShiftLeft => (0x2A, false),
        Backslash => (0x2B, false),
        KeyZ => (0x2C, false),
        KeyX => (0x2D, false),
        KeyC => (0x2E, false),
        KeyV => (0x2F, false),
        KeyB => (0x30, false),
        KeyN => (0x31, false),
        KeyM => (0x32, false),
        Comma => (0x33, false),
        Period => (0x34, false),
        Slash => (0x35, false),
        ShiftRight => (0x36, false),
        AltLeft => (0x38, false),
        Space => (0x39, false),
        CapsLock => (0x3A, false),
        F1 => (0x3B, false),
        F2 => (0x3C, false),
        F3 => (0x3D, false),
        F4 => (0x3E, false),
        F5 => (0x3F, false),
        F6 => (0x40, false),
        F7 => (0x41, false),
        F8 => (0x42, false),
        F9 => (0x43, false),
        F10 => (0x44, false),
        NumLock => (0x45, false),
        ScrollLock => (0x46, false),
        F11 => (0x57, false),
        F12 => (0x58, false),

        ControlRight => (0x1D, true),
        AltRight => (0x38, true),
        NumpadEnter => (0x1C, true),
        NumpadDivide => (0x35, true),
        ArrowUp => (0x48, true),
        ArrowLeft => (0x4B, true),
        ArrowRight => (0x4D, true),
        ArrowDown => (0x50, true),
        Insert => (0x52, true),
        Delete => (0x53, true),
        Home => (0x47, true),
        End => (0x4F, true),
        PageUp => (0x49, true),
        PageDown => (0x51, true),
        SuperLeft => (0x5B, true),
        SuperRight => (0x5C, true),
        ContextMenu => (0x5D, true),

        _ => return None,
    };
    Some((sc, ext))
}

pub fn run<D: SessionDriver + 'static>(driver: D, input_tx: Sender<InputEvent>, frames: &FrameMailbox) -> Result<()> {
    let event_loop = EventLoop::<FrameReady>::with_user_event().build().context("creating event loop")?;
    frames.attach_proxy(event_loop.create_proxy());
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App { driver, win: None, input_tx, latency: Default::default(), cursor_pos: (0, 0) };
    let result = event_loop.run_app(&mut app).context("running event loop");
    app.driver.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(marker: u8) -> BitmapTile {
        BitmapTile {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            pixels: vec![marker, marker, marker, 0xFF],
            stride: 4,
            decoded_at: std::time::Instant::now(),
        }
    }

    /// The defect this type exists to fix: a `sync_channel(1)` keeps the *oldest* queued
    /// snapshot and rejects the new one. At the tail of a burst — the instant a drag is
    /// released or a scroll stops — that drops the newest frame, and because RDP never
    /// resends an unchanged region and no further END_FRAME arrives once the server idles,
    /// the screen stays frozen one frame behind forever.
    #[test]
    fn a_bounded_channel_would_strand_the_last_frame_of_a_burst() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<BitmapTile>(1);
        assert!(tx.try_send(tile(1)).is_ok());
        // Frames 2 and 3 are produced before the UI thread's next 16ms tick.
        assert!(tx.try_send(tile(2)).is_err(), "capacity is 1");
        assert!(tx.try_send(tile(3)).is_err());
        let delivered: Vec<u8> = rx.try_iter().map(|t| t.pixels[0]).collect();
        assert_eq!(delivered, vec![1], "the OLDEST frame survives and the newest is lost — this is the bug");
    }

    #[test]
    fn the_mailbox_keeps_the_newest_frame_instead() {
        let mailbox = FrameMailbox::new();
        mailbox.publish(vec![tile(1)]);
        mailbox.publish(vec![tile(2)]);
        mailbox.publish(vec![tile(3)]);
        let got = mailbox.take().expect("a frame must be available");
        assert_eq!(got[0].pixels[0], 3, "the newest whole-surface snapshot must win");
        assert!(mailbox.take().is_none(), "taking must clear the slot");
    }

    #[test]
    fn every_surface_of_one_frame_survives_publication() {
        // Superseding a *frame* is safe; dropping one output region of the current frame is
        // not, because that region would keep its old pixels indefinitely.
        let mailbox = FrameMailbox::new();
        mailbox.publish(vec![tile(1), tile(2), tile(3)]);
        assert_eq!(mailbox.take().unwrap().len(), 3);
    }

    /// The old per-pixel formulation, kept only so the benchmark below can show what the
    /// current one is worth.
    fn blit_bgrx_rows_per_pixel(dst: &mut [u32], dst_w: u32, dst_h: u32, w: u32, h: u32, src: &[u8], stride: usize) {
        for row in 0..h {
            if row >= dst_h {
                break;
            }
            let src_row = &src[(row as usize) * stride..];
            for col in 0..w {
                if col >= dst_w {
                    break;
                }
                let i = (col as usize) * 4;
                if i + 3 >= src_row.len() {
                    break;
                }
                let b = src_row[i] as u32;
                let g = src_row[i + 1] as u32;
                let r = src_row[i + 2] as u32;
                dst[(row * dst_w + col) as usize] = 0xFF00_0000 | (r << 16) | (g << 8) | b;
            }
        }
    }

    #[test]
    fn blit_matches_the_per_pixel_reference() {
        let (w, h) = (67u32, 33u32);
        let src: Vec<u8> = (0..(w * h * 4)).map(|i| (i % 251) as u8).collect();
        let mut a = vec![0u32; (w * h) as usize];
        let mut b = vec![0u32; (w * h) as usize];
        blit_bgrx_rows(&mut a, w, h, 0, 0, w, h, &src, (w * 4) as usize);
        blit_bgrx_rows_per_pixel(&mut b, w, h, w, h, &src, (w * 4) as usize);
        // softbuffer ignores the top byte, so compare only the 24 bits that are displayed.
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(x & 0x00FF_FFFF, y & 0x00FF_FFFF, "pixel {i} differs");
        }
    }

    #[test]
    fn blit_clips_to_the_framebuffer_without_wrapping() {
        // A tile wider than the destination must clip, not wrap onto the next row.
        let mut dst = vec![0u32; 4 * 2];
        let src: Vec<u8> = [9u8, 9, 9, 0xFF].into_iter().cycle().take(6 * 2 * 4).collect();
        blit_bgrx_rows(&mut dst, 4, 2, 2, 0, 6, 2, &src, 6 * 4);
        assert_eq!(dst[0], 0, "must not paint left of the destination x");
        assert_eq!(dst[2] & 0x00FF_FFFF, 0x0009_0909);
        assert_eq!(dst[3] & 0x00FF_FFFF, 0x0009_0909);
        assert_eq!(dst[6] & 0x00FF_FFFF, 0x0009_0909, "second row lands at the right offset");
    }

    /// Not a correctness test — a measurement, so the presentation path's cost is a number
    /// rather than an opinion. Run with:
    /// `cargo test --release -- --ignored --nocapture presentation_path_cost`
    #[test]
    #[ignore]
    fn presentation_path_cost() {
        use std::time::Instant;
        const W: u32 = 1280;
        const H: u32 = 800;
        const N: u32 = 200;
        let src: Vec<u8> = (0..(W * H * 4)).map(|i| (i % 251) as u8).collect();
        let mut fb = vec![0u32; (W * H) as usize];

        let t = Instant::now();
        for _ in 0..N {
            let _clone = std::hint::black_box(src.clone());
        }
        let clone_us = t.elapsed().as_secs_f64() * 1e6 / N as f64;

        let t = Instant::now();
        for _ in 0..N {
            blit_bgrx_rows_per_pixel(&mut fb, W, H, W, H, &src, (W * 4) as usize);
        }
        let old_us = t.elapsed().as_secs_f64() * 1e6 / N as f64;

        let t = Instant::now();
        for _ in 0..N {
            blit_bgrx_rows(&mut fb, W, H, 0, 0, W, H, &src, (W * 4) as usize);
        }
        let new_us = t.elapsed().as_secs_f64() * 1e6 / N as f64;

        let mut present = vec![0u32; (W * H) as usize];
        let t = Instant::now();
        for _ in 0..N {
            present.copy_from_slice(&fb);
            std::hint::black_box(&present);
        }
        let present_us = t.elapsed().as_secs_f64() * 1e6 / N as f64;

        println!("\n  full-surface clone (4MB): {clone_us:8.0} us");
        println!("  blit, per-pixel (old):   {old_us:8.0} us");
        println!("  blit, row-copy (new):    {new_us:8.0} us   [{:.1}x faster]", old_us / new_us);
        println!("  framebuffer -> present:  {present_us:8.0} us");
        println!("  ---");
        println!("  old per-frame total:     {:8.0} us  ({:.0} fps ceiling)", clone_us + old_us + present_us, 1e6 / (clone_us + old_us + present_us));
        println!("  new per-frame total:     {:8.0} us  ({:.0} fps ceiling)\n", clone_us + new_us + present_us, 1e6 / (clone_us + new_us + present_us));
    }

    /// Measures what the per-message DVC hex dump used to cost on the network thread.
    /// Run with: `cargo test --release -- --ignored --nocapture dvc_hexdump_cost`
    #[test]
    #[ignore]
    fn dvc_hexdump_cost() {
        use std::io::Write;
        use std::time::Instant;
        const N: u32 = 2000;
        let chunk: Vec<u8> = (0..560u32).map(|i| i as u8).collect();

        let t = Instant::now();
        for _ in 0..N {
            std::hint::black_box(format!("[debug] DVC data: chunk_len={} chunk={:02x?}", chunk.len(), chunk));
        }
        let fmt_us = t.elapsed().as_secs_f64() * 1e6 / N as f64;

        let mut sink = std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap();
        let t = Instant::now();
        for _ in 0..N {
            let line = format!("[debug] DVC data: chunk_len={} chunk={:02x?}", chunk.len(), chunk);
            writeln!(sink, "{line}").unwrap();
        }
        let fmt_write_us = t.elapsed().as_secs_f64() * 1e6 / N as f64;

        println!("\n  hex-format one 560B chunk:      {fmt_us:7.1} us");
        println!("  format + write to /dev/null:    {fmt_write_us:7.1} us");
        println!("  at 2000 msg/s that is {:.0} ms/s of network-thread time", fmt_write_us * 2000.0 / 1000.0);
        println!("  (a terminal, unlike /dev/null, must then render every line)\n");
    }

    #[test]
    fn closing_is_observable_by_the_session_thread() {
        let mailbox = FrameMailbox::new();
        assert!(!mailbox.is_closed());
        let clone = mailbox.clone();
        clone.close();
        assert!(mailbox.is_closed(), "the session thread must see the window going away");
    }
}
