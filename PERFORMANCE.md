# Performance notes (2026-08-14)

Investigation into "xfreerdp feels smoother when moving windows around", which turned out to
be four separate defects — only two of them in the rendering path, and the one that mattered
most in the *input* path.

## What was actually wrong

### 1. Input was never sent while the server was streaming (the big one)

`input::DuplexStream` multiplexes outgoing input onto the network thread's receive loop,
because rustls' `ClientConnection` can't be split across threads. It used to flush queued
input **only** from the read-timeout branch:

```rust
match self.inner.read(buf) {
    Err(e) if is_timeout(&e) => self.flush_pending_input()?,  // the ONLY send path
    other => return other,
}
```

Sending input was reachable only when the server fell silent for 30ms. Any sustained
graphics traffic keeps reads succeeding, so that branch never runs and **nothing the user
does reaches the server for as long as the traffic lasts**.

Severity scales with how busy the screen is, which is why it presented as a vague "dragging
feels heavy" (short bursts → lurching) and, in the extreme, total loss of control: playing a
video made the client completely uncontrollable until the video ended, at which point the
whole queued backlog flushed at once.

**Fixed**: flush before *every* read attempt. Regression test
`input_is_sent_even_when_reads_never_time_out` drives a mock transport whose reads never time
out and asserts input still reaches the wire.

Two related changes:
- **`TCP_NODELAY`** was never set. This turned out to matter twice over, and the second was
  not anticipated:
  - A mouse move is ~16 bytes, so Nagle held each one for up to a delayed-ACK interval
    (~40ms).
  - `RDPGFX_FRAME_ACKNOWLEDGE` is also tiny, and it is the server's **flow control**. Holding
    each ack ~40ms paced the server to roughly one frame per delayed-ack interval. See the
    frame-rate section below.
  Every real RDP client disables Nagle.
- **Mouse moves now coalesce**: a fast drag queues motion faster than it is worth sending and
  only the newest position is actionable. Runs collapse to the last position — but never
  across a button or key event, or a click would land where the cursor no longer is.

`READ_TIMEOUT` dropped 30ms → 5ms. It now only bounds input latency while the server is
*idle*, which is exactly the start of a gesture, where dead time is felt.

### 2. The event loop woke on a timer instead of on the frame

`about_to_wait` set `ControlFlow::WaitUntil(now + 16ms)`, and a frame published by the
network thread generated no winit event — so the loop slept through its arrival. Worse,
`blit` then called `request_redraw()`, which winit dispatches at the *start of the next*
iteration, costing another tick before anything reached the screen. Up to two frame periods
of latency on every frame.

**Fixed**: `FrameMailbox` holds an `EventLoopProxy` and `publish()` sends a `FrameReady`
event, so the loop wakes immediately and `draw()` blits **and presents synchronously**.
`ControlFlow::Wait` means no polling when idle.

Measured on the live host: frame-to-screen p50 **32ms → ~600us**, max ~3.4ms.

### 3. A hex dump of every graphics message on the network thread

`dvc.rs` unconditionally `eprintln!`-ed every DVC chunk with `{:02x?}`. A 560-byte chunk
formats to ~2KB of text, synchronously, on the network thread — measured at ~6us to
`/dev/null` (~1.2% CPU at 2000 msg/s), but unbounded when stderr is a terminal, since writes
block until the terminal renders them. Now behind `RDP_DEBUG_TRACE=1`.

Worth noting *why* this survived: the replay harness bypasses the DVC layer entirely, so no
amount of replay profiling could have found it.

### 4. The framebuffer blit did no useful work

The decoders store BGRX bytes; on little-endian those four bytes read as a `u32` are already
`0xXXRRGGBB`, which is exactly softbuffer's format with the top byte ignored. The per-pixel
shift-and-mask was computing a value identical to the bytes already in memory. Each row is
now a copy the compiler vectorises: **385us → 144us** per full frame (2.7x).

## What was measured and ruled out

Both of these were leading hypotheses and both were wrong; the numbers are recorded so nobody
re-optimises them:

| Suspect | Measurement | Verdict |
|---|---|---|
| Presentation CPU | 762us/frame end to end = 1300 fps ceiling | not a bottleneck |
| Decode + composite | p50 90us; 0.2-0.7% of frames over 16ms; ~1.4% of wall clock | not a bottleneck |

Decode's tail is real (p99 10-13ms, max ~31ms) but it runs on its own thread, so it lowers
frame *rate* during heavy content rather than adding latency. If that ever needs attention,
progressive tiles are independent and could be decoded in parallel.

## Frame rate

Before `TCP_NODELAY`, the presented rate was a rock-steady **32 fps** under sustained
interaction. That stability was itself the clue, though it was initially misread as a
server-side governor (Windows defaults to ~30 fps for the graphics pipeline): a *client*
bottleneck would vary with content, and this did not vary at all.

It was not the server. Nagle was delaying our `RDPGFX_FRAME_ACKNOWLEDGE` PDUs, and frame
acknowledgement is the mechanism the server uses to pace itself — so the client was
throttling the server to about one frame per delayed-ack interval, and the constancy came
from the constancy of that timer rather than from any policy.

With Nagle disabled the rate becomes **22-50 fps and varies with content**, which is the
expected shape for an uncapped pipeline. The lesson generalises: a suspiciously *stable*
performance number is more often an artificial cap than a natural limit.

## Measuring it yourself

- `RDP_DEBUG_PERF=1` — presented fps and frame-to-screen latency percentiles, every 120
  frames. Reports rate and latency together because they fail differently: latency is how
  stale the image is (presentation path), rate is how often it changes (server + decoder).
- `cargo test --release -- --ignored --nocapture presentation_path_cost` — per-stage cost of
  the presentation path.
- `cargo test --release -- --ignored --nocapture dvc_hexdump_cost` — what the removed
  per-message logging cost.
- `cargo run --release --bin replay -- <capture>.rdpgfx --quiet` — decode+composite
  percentiles for a recorded session, isolated from network and presentation.
