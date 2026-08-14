use crate::input::{self, InputEvent};
use anyhow::{Context, Result};
use softbuffer::{Context as SbContext, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

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
#[derive(Default)]
struct Mailbox {
    slot: Mutex<Option<Vec<BitmapTile>>>,
    closed: AtomicBool,
}

#[derive(Clone, Default)]
pub struct FrameMailbox(Arc<Mailbox>);

impl FrameMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes the newest frame, superseding any frame the UI thread has not yet consumed.
    pub fn publish(&self, tiles: Vec<BitmapTile>) {
        *self.0.slot.lock().expect("frame mailbox poisoned") = Some(tiles);
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
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

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
        for row in 0..h {
            let dst_y = y + row;
            if dst_y >= self.height {
                break;
            }
            let src_row = &src[(row as usize) * stride..];
            for col in 0..w {
                let dst_x = x + col;
                if dst_x >= self.width {
                    break;
                }
                let i = (col as usize) * 4;
                if i + 3 >= src_row.len() {
                    break;
                }
                let b = src_row[i] as u32;
                let g = src_row[i + 1] as u32;
                let r = src_row[i + 2] as u32;
                self.framebuffer[(dst_y * self.width + dst_x) as usize] = 0xFF00_0000 | (r << 16) | (g << 8) | b;
            }
        }
        self.window.request_redraw();
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
}

struct App<D: SessionDriver> {
    driver: D,
    win: Option<RemoteDesktopWindow>,
    input_tx: Sender<InputEvent>,
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

impl<D: SessionDriver> ApplicationHandler for App<D> {
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
        // `poll` returns one whole-surface snapshot per mapped output region, all from the
        // same frame — so every tile here must be blitted, not just the last one. (Collapsing
        // a *burst* of frames down to the newest happens upstream, in `FrameMailbox`, where
        // it is safe: superseding an unconsumed full snapshot loses nothing, whereas dropping
        // one of this frame's surfaces would leave that output region permanently stale.)
        let tiles = self.driver.poll();
        if !tiles.is_empty() {
            if let Some(win) = &mut self.win {
                for tile in &tiles {
                    win.blit_bgrx(tile.x, tile.y, tile.width, tile.height, &tile.pixels, tile.stride);
                }
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(16),
        ));
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

pub fn run<D: SessionDriver + 'static>(driver: D, input_tx: Sender<InputEvent>) -> Result<()> {
    let event_loop = EventLoop::new().context("creating event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App { driver, win: None, input_tx, cursor_pos: (0, 0) };
    let result = event_loop.run_app(&mut app).context("running event loop");
    app.driver.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(marker: u8) -> BitmapTile {
        BitmapTile { x: 0, y: 0, width: 1, height: 1, pixels: vec![marker, marker, marker, 0xFF], stride: 4 }
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

    #[test]
    fn closing_is_observable_by_the_session_thread() {
        let mailbox = FrameMailbox::new();
        assert!(!mailbox.is_closed());
        let clone = mailbox.clone();
        clone.close();
        assert!(mailbox.is_closed(), "the session thread must see the window going away");
    }
}
