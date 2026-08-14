//! Deterministic replay of a recorded RDP graphics stream.
//!
//! Feeds a recording made with `RDP_RECORD=<path>` through the same ZGFX decompressor, PDU
//! parser, codecs and surface compositor the live session uses — with no network, no timing,
//! and no window. Two things this makes possible that a live session cannot:
//!
//! * **Reproducing a delta-protocol artifact on demand.** The server never resends a region,
//!   so an artifact only ever appears under the exact update sequence that produced it. A
//!   recording is that sequence, frozen.
//! * **Regression testing.** `tests/replay_regression.rs` replays a checked-in fixture and
//!   pins the resulting image, so a coordinate-math change that misplaces content fails the
//!   test suite instead of being noticed weeks later on a screenshot.
//!
//! ```text
//! replay <recording.rdpgfx> [--out-dir DIR] [--frames] [--stop-after N] [--quiet]
//!
//!   --out-dir DIR   write final.ppm (and per-frame PPMs with --frames) here
//!   --frames        dump every presented frame, not just the last
//!   --stop-after N  stop after N END_FRAMEs (bisecting when an artifact first appears)
//!   --quiet         only print the summary line and the digest
//! ```
//!
//! The debug environment variables all apply here too — `RDP_DEBUG_TINT=1` is especially
//! useful on a replay, since the dumped PPMs then show which frame last wrote every region.

use anyhow::{bail, Context, Result};
use rdp_client::record::{self, RecordEntry};
use rdp_client::replay;
use std::path::PathBuf;

struct Options {
    recording: PathBuf,
    out_dir: Option<PathBuf>,
    dump_frames: bool,
    stop_after: Option<u32>,
    quiet: bool,
}

const USAGE: &str = "usage: replay <recording.rdpgfx> [--out-dir DIR] [--frames] [--stop-after N] [--quiet]";

fn parse_args() -> Result<Options> {
    let mut args = std::env::args().skip(1);
    let recording = args.next().context(USAGE)?;
    let mut opts =
        Options { recording: PathBuf::from(recording), out_dir: None, dump_frames: false, stop_after: None, quiet: false };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out-dir" => opts.out_dir = Some(PathBuf::from(args.next().context("--out-dir requires a path")?)),
            "--frames" => opts.dump_frames = true,
            "--stop-after" => {
                opts.stop_after =
                    Some(args.next().context("--stop-after requires a count")?.parse().context("--stop-after count")?)
            }
            "--quiet" => opts.quiet = true,
            other => bail!("unrecognized argument {other:?}\n{USAGE}"),
        }
    }
    Ok(opts)
}

fn main() -> Result<()> {
    let opts = parse_args()?;
    let entries = record::read(&opts.recording)?;
    let messages = entries.iter().filter(|e| matches!(e, RecordEntry::GfxMessage(_))).count();
    let desktop = replay::desktop_from_notes(&entries);
    if !opts.quiet {
        println!(
            "replaying {} ({messages} graphics messages, desktop {}x{})",
            opts.recording.display(),
            desktop.0,
            desktop.1
        );
    }

    if let Some(dir) = &opts.out_dir {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let out_dir = opts.out_dir.clone();
    let dump_frames = opts.dump_frames;
    let quiet = opts.quiet;

    let result = replay::replay(&entries, desktop, opts.stop_after, |frame, w, h, pixels| {
        if !quiet && frame % 100 == 0 {
            println!("  frame {frame}");
        }
        if dump_frames {
            if let Some(dir) = &out_dir {
                let path = dir.join(format!("frame-{frame:05}.ppm"));
                if let Err(e) = record::write_ppm(&path, w, h, pixels) {
                    eprintln!("writing {}: {e:#}", path.display());
                }
            }
        }
    })?;

    if let Some(dir) = &opts.out_dir {
        let path = dir.join("final.ppm");
        record::write_ppm(&path, result.width, result.height, &result.pixels)?;
        if !quiet {
            println!("wrote {}", path.display());
        }
    }

    println!("frames={} digest={:016x}", result.frames, result.digest());
    println!("{}", result.summary);
    if result.dropped_updates > 0 {
        // Loud on purpose: on a delta protocol each of these is a region the user will keep
        // seeing wrong until something unrelated happens to repaint it.
        eprintln!(
            "WARNING: {} update(s) were not applied — every one of them is a permanent artifact in the image above",
            result.dropped_updates
        );
    }
    Ok(())
}
