//! Record/replay of the inbound graphics-channel byte stream.
//!
//! RDP is a pure delta protocol: a rendering defect only reproduces under the exact update
//! sequence that caused it, and the server will never resend the region that went wrong. A
//! live session is therefore unrepeatable by construction — the only way to turn a rendering
//! artifact into something you can iterate on (and into a regression test) is to capture the
//! bytes and replay them through the same decode + composite pipeline with no network and no
//! timing dependence.
//!
//! What is captured is the raw DVC-reassembled graphics-channel message, i.e. exactly what
//! `zgfx::ZgfxContext::decompress` is fed. That is the earliest point at which the stream is
//! a well-defined sequence of self-describing messages, and it deliberately sits *before*
//! ZGFX decompression so that the ZGFX history-buffer state machine — which is stateful
//! across messages and has been a source of bugs here before — is replayed too.
//!
//! ## File format
//!
//! ```text
//! magic     10 bytes  "RDPGFXREC\0"
//! version    2 bytes  u16 LE, currently 1
//! records    repeated until EOF:
//!   kind     1 byte   1 = graphics-channel message, 2 = UTF-8 note
//!   length   4 bytes  u32 LE
//!   payload  length bytes
//! ```

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 10] = b"RDPGFXREC\0";
const VERSION: u16 = 1;

pub const KIND_GFX_MESSAGE: u8 = 1;
pub const KIND_NOTE: u8 = 2;

pub struct Recorder {
    out: BufWriter<File>,
    messages: u64,
    bytes: u64,
}

impl Recorder {
    pub fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("creating recording {}", path.display()))?;
        let mut out = BufWriter::new(file);
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        Ok(Self { out, messages: 0, bytes: 0 })
    }

    fn record(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        self.out.write_all(&[kind])?;
        self.out.write_all(&(payload.len() as u32).to_le_bytes())?;
        self.out.write_all(payload)?;
        Ok(())
    }

    pub fn gfx_message(&mut self, payload: &[u8]) -> Result<()> {
        self.record(KIND_GFX_MESSAGE, payload)?;
        self.messages += 1;
        self.bytes += payload.len() as u64;
        // Flushed per message on purpose: a recording is most valuable for the session that
        // ends in a crash or a hang, and a buffered tail would lose exactly the messages
        // that matter most.
        self.out.flush()?;
        Ok(())
    }

    pub fn note(&mut self, text: &str) -> Result<()> {
        self.record(KIND_NOTE, text.as_bytes())?;
        self.out.flush()?;
        Ok(())
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.messages, self.bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordEntry {
    GfxMessage(Vec<u8>),
    Note(String),
    /// A record kind written by a newer version of this client. Kept rather than dropped so
    /// replaying an old recording with a new binary is not silently lossy.
    Unknown { kind: u8, payload: Vec<u8> },
}

/// Reads a whole recording into memory. Recordings of real sessions are a few MB, so there
/// is no reason to stream.
pub fn read(path: &Path) -> Result<Vec<RecordEntry>> {
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("opening recording {}", path.display()))?
        .read_to_end(&mut bytes)?;
    parse(&bytes)
}

pub fn parse(bytes: &[u8]) -> Result<Vec<RecordEntry>> {
    if bytes.len() < 12 || &bytes[..10] != MAGIC {
        bail!("not an RDP graphics recording (bad magic)");
    }
    let version = u16::from_le_bytes([bytes[10], bytes[11]]);
    if version != VERSION {
        bail!("unsupported recording version {version} (this build writes {VERSION})");
    }
    let mut out = Vec::new();
    let mut pos = 12usize;
    while pos < bytes.len() {
        if pos + 5 > bytes.len() {
            bail!("recording truncated in a record header at offset {pos}");
        }
        let kind = bytes[pos];
        let len = u32::from_le_bytes([bytes[pos + 1], bytes[pos + 2], bytes[pos + 3], bytes[pos + 4]]) as usize;
        pos += 5;
        if pos + len > bytes.len() {
            bail!("recording truncated in a record payload at offset {pos} (want {len} bytes)");
        }
        let payload = &bytes[pos..pos + len];
        pos += len;
        out.push(match kind {
            KIND_GFX_MESSAGE => RecordEntry::GfxMessage(payload.to_vec()),
            KIND_NOTE => RecordEntry::Note(String::from_utf8_lossy(payload).into_owned()),
            other => RecordEntry::Unknown { kind: other, payload: payload.to_vec() },
        });
    }
    Ok(out)
}

/// Writes a BGRX8888 image as a binary PPM. Used by the replay harness to dump frames for
/// eyeball comparison and by the regression test for its reference image.
pub fn write_ppm(path: &Path, width: u32, height: u32, bgrx: &[u8]) -> Result<()> {
    let mut out = BufWriter::new(File::create(path).with_context(|| format!("creating {}", path.display()))?);
    write!(out, "P6\n{width} {height}\n255\n")?;
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for px in bgrx.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    out.write_all(&rgb)?;
    Ok(())
}

/// FNV-1a over a frame's pixels. Small, dependency-free, and stable across platforms — good
/// enough to pin a replay's output in a regression test.
pub fn image_digest(bgrx: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bgrx {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_messages_and_notes() {
        let dir = std::env::temp_dir().join(format!("rdp-record-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cap.rdpgfx");

        let mut rec = Recorder::create(&path).unwrap();
        rec.note("desktop 1280x800").unwrap();
        rec.gfx_message(&[1, 2, 3, 4]).unwrap();
        rec.gfx_message(&[]).unwrap();
        rec.gfx_message(&[9; 300]).unwrap();
        assert_eq!(rec.stats(), (3, 304));
        drop(rec);

        let entries = read(&path).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0], RecordEntry::Note("desktop 1280x800".into()));
        assert_eq!(entries[1], RecordEntry::GfxMessage(vec![1, 2, 3, 4]));
        assert_eq!(entries[2], RecordEntry::GfxMessage(vec![]));
        assert_eq!(entries[3], RecordEntry::GfxMessage(vec![9; 300]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_a_foreign_file() {
        assert!(parse(b"not a recording at all").is_err());
    }

    #[test]
    fn rejects_a_truncated_payload() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.push(KIND_GFX_MESSAGE);
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 10]);
        assert!(parse(&bytes).is_err(), "a truncated recording must not replay as a short message");
    }

    #[test]
    fn digest_is_sensitive_to_a_single_pixel() {
        let a = vec![0u8; 64];
        let mut b = a.clone();
        b[37] = 1;
        assert_ne!(image_digest(&a), image_digest(&b));
    }
}
