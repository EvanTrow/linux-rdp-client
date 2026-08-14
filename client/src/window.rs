use crate::input::{self, InputEvent};
use anyhow::{Context, Result};
use softbuffer::{Context as SbContext, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Sender;
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
    width: u32,
    height: u32,
    /// 0xAARRGGBB per pixel, row-major, matching softbuffer's expected format.
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
            framebuffer: vec![0xFF00_0000; (width * height) as usize],
        })
    }

    /// Blits a BGRX32 (as delivered by decoded RDP bitmap tiles) rectangle into the
    /// framebuffer at (x, y) and requests a redraw. `stride` is bytes per source row.
    pub fn blit_bgrx(&mut self, x: u32, y: u32, w: u32, h: u32, src: &[u8], stride: usize) {
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

    fn present(&mut self) -> Result<()> {
        let mut buf = self
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("locking softbuffer: {e}"))?;
        buf.copy_from_slice(&self.framebuffer);
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
        for tile in self.driver.poll() {
            if let Some(win) = &mut self.win {
                win.blit_bgrx(tile.x, tile.y, tile.width, tile.height, &tile.pixels, tile.stride);
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
    event_loop.run_app(&mut app).context("running event loop")?;
    Ok(())
}
