use anyhow::Result;
use std::io::{Read, Write};
use std::sync::mpsc::Receiver;
use std::time::Duration;

// eventCode=FASTPATH_INPUT_EVENT_SCANCODE is 0x0 and, being the low 3 bits of eventHeader
// shifted left by 5, contributes nothing when 0 — keyboard_event() below relies on that
// rather than naming the constant.
const EVENT_MOUSE: u8 = 0x1;

pub const PTR_FLAGS_MOVE: u16 = 0x0800;
pub const PTR_FLAGS_DOWN: u16 = 0x8000;
pub const PTR_FLAGS_BUTTON1: u16 = 0x1000;
pub const PTR_FLAGS_BUTTON2: u16 = 0x2000;
pub const PTR_FLAGS_BUTTON3: u16 = 0x4000;

const KBD_RELEASE: u8 = 0x01;
const KBD_EXTENDED: u8 = 0x02;

/// Wraps one or more already-encoded `TS_FP_INPUT_EVENT` byte blobs in a complete
/// `TS_FP_INPUT_PDU` (MS-RDPBCGR §2.2.8.1.2) — sent raw on the transport, with NO
/// TPKT/X.224/MCS Send-Data-Request framing (that framing is exactly what "fast-path"
/// exists to replace), and no Security Header under Enhanced/TLS security
/// (fipsInformation/dataSignature MUST NOT be present, per spec, when TLS is in effect).
fn build_fastpath_pdu(events: &[Vec<u8>]) -> Vec<u8> {
    let num_events = events.len() as u8;
    debug_assert!(num_events > 0 && num_events <= 15);
    // fpInputHeader: action=FASTPATH_INPUT_ACTION_FASTPATH(0, low 2 bits) |
    // numEvents(4 bits) | flags(0, top 2 bits — FASTPATH_INPUT_ENCRYPTED/SECURE_CHECKSUM
    // only apply to legacy RDP Standard Security, never set under TLS).
    let header = num_events << 2;

    let mut body = Vec::new();
    for e in events {
        body.extend_from_slice(e);
    }

    let mut out = Vec::with_capacity(3 + body.len());
    out.push(header);
    // Always use the 2-byte length form (high bit set on length1, big-endian with length2)
    // — spec-legal for any size and what FreeRDP itself always sends, avoids a branch.
    let total_len = (3 + body.len()) as u16; // header(1) + length(2) + body
    out.extend_from_slice(&(0x8000u16 | total_len).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// `TS_FP_POINTER_EVENT`: pointerFlags(2 LE) + xPos(2 LE) + yPos(2 LE), absolute desktop
/// coordinates. eventHeader's low 5 bits (eventFlags) MUST be 0 for mouse events — all
/// state lives in `pointer_flags`.
fn mouse_event(pointer_flags: u16, x: u16, y: u16) -> Vec<u8> {
    let event_header = EVENT_MOUSE << 5;
    let mut out = vec![event_header];
    out.extend_from_slice(&pointer_flags.to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out
}

/// `TS_FP_KEYBOARD_EVENT`: eventHeader(eventFlags in low 5 bits, eventCode=SCANCODE(0) in
/// high 3) + keyCode(1, raw PS/2-style scancode).
fn keyboard_event(scancode: u8, release: bool, extended: bool) -> Vec<u8> {
    let mut flags = 0u8;
    if release {
        flags |= KBD_RELEASE;
    }
    if extended {
        flags |= KBD_EXTENDED;
    }
    vec![flags, scancode]
}

pub fn send_mouse_move<S: Write>(stream: &mut S, x: u16, y: u16) -> Result<()> {
    stream.write_all(&build_fastpath_pdu(&[mouse_event(PTR_FLAGS_MOVE, x, y)]))?;
    Ok(())
}

pub fn send_mouse_button<S: Write>(stream: &mut S, x: u16, y: u16, button: u16, down: bool) -> Result<()> {
    let flags = if down { PTR_FLAGS_DOWN | button } else { button };
    stream.write_all(&build_fastpath_pdu(&[mouse_event(flags, x, y)]))?;
    Ok(())
}

pub fn send_key<S: Write>(stream: &mut S, scancode: u8, release: bool, extended: bool) -> Result<()> {
    stream.write_all(&build_fastpath_pdu(&[keyboard_event(scancode, release, extended)]))?;
    Ok(())
}

/// One real user input event, captured from the OS window (`window.rs`) and forwarded to
/// the network thread for encoding/sending — decoupled from the wire format so the window
/// side doesn't need to know PDU details.
pub enum InputEvent {
    MouseMove { x: u16, y: u16 },
    MouseButton { x: u16, y: u16, button: u16, down: bool },
    Key { scancode: u8, release: bool, extended: bool },
}

fn send_event<S: Write>(stream: &mut S, ev: &InputEvent) -> Result<()> {
    match *ev {
        InputEvent::MouseMove { x, y } => send_mouse_move(stream, x, y),
        InputEvent::MouseButton { x, y, button, down } => send_mouse_button(stream, x, y, button, down),
        InputEvent::Key { scancode, release, extended } => send_key(stream, scancode, release, extended),
    }
}

/// Wraps an inner `Read + Write` transport (the TLS stream) so the network thread's normal
/// blocking receive loop can also drain and send outgoing input events without needing a
/// second thread to share TLS state with (rustls' `ClientConnection` isn't safely splittable
/// across threads without extra synchronization). The inner stream's socket must have a
/// short read timeout set (see `main.rs`) — every time a read times out with no GFX data
/// available, that's exactly when we check for and flush pending input, then resume
/// waiting. From the caller's point of view this is just an ordinary blocking `Read`/`Write`
/// — timeouts are fully absorbed here, never surfaced as an error.
pub struct DuplexStream<S> {
    inner: S,
    input_rx: Receiver<InputEvent>,
}

impl<S: Read + Write> DuplexStream<S> {
    pub fn new(inner: S, input_rx: Receiver<InputEvent>) -> Self {
        Self { inner, input_rx }
    }

    fn flush_pending_input(&mut self) -> std::io::Result<()> {
        while let Ok(ev) = self.input_rx.try_recv() {
            send_event(&mut self.inner, &ev).map_err(std::io::Error::other)?;
        }
        Ok(())
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

impl<S: Read + Write> Read for DuplexStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Err(e) if is_timeout(&e) => self.flush_pending_input()?,
                other => return other,
            }
        }
    }
}

impl<S: Read + Write> Write for DuplexStream<S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Short enough that input feels responsive (mouse moves flush promptly even while the
/// read loop is otherwise idle waiting on the server) without turning the read loop into a
/// busy-poll.
pub const READ_TIMEOUT: Duration = Duration::from_millis(30);
