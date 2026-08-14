use anyhow::Result;
use rdp_client::session::{self, CliArgs};
use rdp_client::window::{self, FrameMailbox};

fn main() -> Result<()> {
    // Pulling in chromiumoxide's rustls-backed HTTP stack (for AAD browser automation)
    // brought a second rustls crypto provider (aws-lc-rs) into the dependency tree
    // alongside the "ring" one this crate builds with — Cargo unifies both features onto
    // the single `rustls` version everyone depends on, so both end up compiled in. rustls
    // then refuses to guess which one to use and panics on the first TLS connection
    // (ours, for the RDP host) unless a default is installed explicitly up front.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("a rustls CryptoProvider was already installed"))?;

    let cli: CliArgs = session::parse_cli_args()?;

    // Frames are handed over through a latest-wins mailbox rather than a bounded channel:
    // each entry is a whole-surface snapshot, so a newer one strictly supersedes an
    // unconsumed older one — whereas a bounded channel would keep the older one and drop the
    // newer, stranding the last frame of every burst on screen forever. See
    // `window::FrameMailbox`.
    let frames = FrameMailbox::new();
    let (input_tx, input_rx) = std::sync::mpsc::channel();

    let session_frames = frames.clone();
    std::thread::spawn(move || {
        if let Err(e) = session::run_session(cli, session_frames, input_rx) {
            eprintln!("session error: {e:#}");
        }
    });

    window::run(
        session::NetworkDriver {
            frames,
            desktop: (session::DESKTOP_WIDTH as u32, session::DESKTOP_HEIGHT as u32),
        },
        input_tx,
    )
}
