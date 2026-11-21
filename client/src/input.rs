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
/// across threads without extra synchronization). From the caller's point of view this is
/// just an ordinary blocking `Read`/`Write` — timeouts are fully absorbed here, never
/// surfaced as an error.
///
/// Input is flushed **before every read attempt**, not only when a read times out. That
/// distinction is the difference between a usable and an unusable drag: dragging a window
/// makes the server stream graphics continuously, so reads keep succeeding, so a
/// flush-on-timeout-only policy never fires for as long as the drag lasts. The mouse
/// positions driving the drag would queue up unsent and go out in a burst during the next
/// lull, and the remote window would follow the cursor in lurches.
pub struct DuplexStream<S> {
    inner: S,
    input_rx: Receiver<InputEvent>,
}

impl<S: Read + Write> DuplexStream<S> {
    pub fn new(inner: S, input_rx: Receiver<InputEvent>) -> Self {
        Self { inner, input_rx }
    }

    /// Sends everything queued, collapsing runs of consecutive mouse moves to their final
    /// position.
    ///
    /// A fast drag can produce mouse motion faster than it is worth transmitting, and only
    /// the newest position is actionable — but a move may not be reordered across a button
    /// or key event, or a click would land at the wrong place. So runs are collapsed, not
    /// the whole batch.
    fn flush_pending_input(&mut self) -> std::io::Result<()> {
        let mut pending_move: Option<InputEvent> = None;
        while let Ok(ev) = self.input_rx.try_recv() {
            match ev {
                InputEvent::MouseMove { .. } => pending_move = Some(ev),
                other => {
                    if let Some(mv) = pending_move.take() {
                        send_event(&mut self.inner, &mv).map_err(std::io::Error::other)?;
                    }
                    send_event(&mut self.inner, &other).map_err(std::io::Error::other)?;
                }
            }
        }
        if let Some(mv) = pending_move.take() {
            send_event(&mut self.inner, &mv).map_err(std::io::Error::other)?;
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
            // Before blocking, and before every retry: see the type-level comment. During
            // heavy server traffic this is the only thing that gets input out at all.
            self.flush_pending_input()?;
            match self.inner.read(buf) {
                Err(e) if is_timeout(&e) => continue,
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

/// How long a read blocks before looping back to flush pending input.
///
/// This now only bounds input latency while the server is **idle** — during traffic, input
/// goes out before every read regardless. It is short because that idle case is exactly when
/// a user starts moving the mouse, and 30ms of dead time at the start of every gesture is
/// felt. The cost is a bounded ~200 wakeups/second of an otherwise-idle socket, each a
/// syscall returning `WouldBlock`.
pub const READ_TIMEOUT: Duration = Duration::from_millis(5);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A transport whose reads always succeed immediately — i.e. a server streaming
    /// continuously, which is what playing a video or dragging a window looks like.
    #[derive(Default)]
    struct AlwaysReadable {
        written: Vec<u8>,
    }

    impl Read for AlwaysReadable {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf[0] = 0xAA;
            Ok(1)
        }
    }

    impl Write for AlwaysReadable {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The regression that made the client uncontrollable during video playback: input used
    /// to be sent *only* from the read-timeout branch, so a server that never goes quiet
    /// meant input was never transmitted at all — for as long as the traffic lasted.
    #[test]
    fn input_is_sent_even_when_reads_never_time_out() {
        let (tx, rx) = mpsc::channel();
        let mut stream = DuplexStream::new(AlwaysReadable::default(), rx);
        tx.send(InputEvent::MouseMove { x: 100, y: 200 }).unwrap();

        let mut buf = [0u8; 8];
        stream.read(&mut buf).expect("read");

        assert!(
            !stream.inner.written.is_empty(),
            "input must reach the server while it is streaming, not only once it falls silent"
        );
    }

    #[test]
    fn a_run_of_mouse_moves_collapses_to_the_latest_position() {
        let (tx, rx) = mpsc::channel();
        let mut stream = DuplexStream::new(AlwaysReadable::default(), rx);
        for x in 0..10u16 {
            tx.send(InputEvent::MouseMove { x, y: x }).unwrap();
        }
        let mut buf = [0u8; 8];
        stream.read(&mut buf).expect("read");

        // One fast-path PDU, not ten: only the newest position is actionable.
        let one_move = build_fastpath_pdu(&[mouse_event(PTR_FLAGS_MOVE, 9, 9)]);
        assert_eq!(stream.inner.written, one_move, "only the final position should be sent");
    }

    #[test]
    fn a_click_is_never_reordered_before_the_move_that_preceded_it() {
        let (tx, rx) = mpsc::channel();
        let mut stream = DuplexStream::new(AlwaysReadable::default(), rx);
        tx.send(InputEvent::MouseMove { x: 5, y: 5 }).unwrap();
        tx.send(InputEvent::MouseMove { x: 40, y: 60 }).unwrap();
        tx.send(InputEvent::MouseButton { x: 40, y: 60, button: PTR_FLAGS_BUTTON1, down: true }).unwrap();
        let mut buf = [0u8; 8];
        stream.read(&mut buf).expect("read");

        let mut expected = build_fastpath_pdu(&[mouse_event(PTR_FLAGS_MOVE, 40, 60)]);
        expected.extend_from_slice(&build_fastpath_pdu(&[mouse_event(PTR_FLAGS_DOWN | PTR_FLAGS_BUTTON1, 40, 60)]));
        assert_eq!(
            stream.inner.written, expected,
            "collapsing moves must not let a click overtake the position it was made at"
        );
    }
}
