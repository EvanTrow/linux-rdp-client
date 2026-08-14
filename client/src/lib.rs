//! Library crate for the from-scratch Linux RDP client.
//!
//! Everything lives here rather than in `main.rs` so that the graphics pipeline can be
//! driven from something other than a live TCP connection — specifically the deterministic
//! record/replay harness (`src/bin/replay.rs`) and the integration tests in `tests/`, which
//! feed a recorded inbound PDU byte stream through the exact same decode + composite code
//! the network path uses. RDP is a pure delta protocol, so a lost or misapplied update is a
//! permanent artifact that only ever reproduces under the exact update sequence that caused
//! it; replaying a recording is the only way to turn that into a repeatable test.

pub mod aad;
pub mod aad_auto;
pub mod bitmap;
pub mod capabilities;
pub mod clearcodec;
pub mod client_info;
pub mod debug;
pub mod dvc;
pub mod errinfo;
pub mod gcc;
pub mod gfx;
pub mod gfxstate;
pub mod input;
pub mod license;
pub mod mcs;
pub mod nscodec;
pub mod progressive;
pub mod rds_aad;
pub mod record;
pub mod replay;
pub mod session;
pub mod surface;
pub mod tls;
pub mod vchannel;
pub mod window;
pub mod x224;
pub mod zgfx;
