use anyhow::{Context, Result};
use softbuffer::{Context as SbContext, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
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

pub fn run<D: SessionDriver + 'static>(driver: D) -> Result<()> {
    let event_loop = EventLoop::new().context("creating event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App { driver, win: None };
    event_loop.run_app(&mut app).context("running event loop")?;
    Ok(())
}
