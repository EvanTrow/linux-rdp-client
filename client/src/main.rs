mod aad;
mod aad_auto;
mod bitmap;
mod capabilities;
mod client_info;
mod dvc;
mod errinfo;
mod gcc;
mod gfx;
mod license;
mod mcs;
mod rds_aad;
mod tls;
mod vchannel;
mod window;
mod x224;
mod zgfx;

use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;

/// Routes incoming MCS Send Data Indications to whichever of the base I/O channel or the
/// "drdynvc" dynamic-virtual-channel machinery actually wants them. Needed because traffic
/// for both can arrive interleaved on the wire — a naive "read one channel's messages
/// until done, then move on to the next channel" sequence silently drops whichever
/// channel's messages arrive during the other's turn (confirmed against a real host: it
/// starts drdynvc capability negotiation before this client's own I/O-channel finalization
/// sequence had finished draining).
struct ChannelRouter {
    io_channel_id: u16,
    demux: vchannel::ChannelDemux,
    dvc: dvc::DvcManager,
    pending_dvc_data: VecDeque<(u32, Vec<u8>)>,
}

impl ChannelRouter {
    fn new(io_channel_id: u16, drdynvc_channel_id: u16) -> Self {
        Self {
            io_channel_id,
            demux: vchannel::ChannelDemux::new(),
            dvc: dvc::DvcManager::new(drdynvc_channel_id),
            pending_dvc_data: VecDeque::new(),
        }
    }

    /// Feeds one raw static-channel-framed drdynvc message through the DVC manager,
    /// queuing any resulting data events for `recv_dvc_data`/`open_dvc_channel` to consume.
    fn handle_drdynvc_chunk<S: Read + Write>(&mut self, stream: &mut S, user_id: u16, payload: &[u8]) -> Result<Vec<dvc::DvcEvent>> {
        let mut opened = Vec::new();
        if let Some(msg) = self.demux.feed(self.dvc.static_channel_id(), payload)? {
            for ev in self.dvc.handle_message(stream, user_id, &msg)? {
                match ev {
                    dvc::DvcEvent::Data { channel_id, data } => self.pending_dvc_data.push_back((channel_id, data)),
                    other => opened.push(other),
                }
            }
        }
        Ok(opened)
    }

    /// Blocks until a message for the base I/O channel is available, transparently
    /// processing (and queuing) any interleaved drdynvc traffic seen along the way.
    fn recv_io<S: Read + Write>(&mut self, stream: &mut S, user_id: u16) -> Result<Vec<u8>> {
        loop {
            let (ch, payload) = mcs::recv_data_indication(stream)?;
            if ch == self.io_channel_id {
                return Ok(payload);
            }
            if ch == self.dvc.static_channel_id() {
                self.handle_drdynvc_chunk(stream, user_id, &payload)?;
            }
        }
    }

    /// Blocks until the named dynamic channel opens (registering interest in it first),
    /// returning its assigned dynamic ChannelId.
    fn open_dvc_channel<S: Read + Write>(&mut self, stream: &mut S, user_id: u16, name: &str) -> Result<u32> {
        self.dvc.want_channel(name);
        if let Some(id) = self.dvc.channel_id_for(name) {
            return Ok(id);
        }
        loop {
            let (ch, payload) = mcs::recv_data_indication(stream)?;
            eprintln!(
                "[debug] recv_data_indication: channel={ch} (drdynvc={}) bytes={} first8={:02x?}",
                self.dvc.static_channel_id(),
                payload.len(),
                &payload[..payload.len().min(8)]
            );
            if ch != self.dvc.static_channel_id() {
                continue;
            }
            for ev in self.handle_drdynvc_chunk(stream, user_id, &payload)? {
                if let dvc::DvcEvent::ChannelOpened { name: n, channel_id } = ev {
                    if n == name {
                        return Ok(channel_id);
                    }
                }
            }
        }
    }

    /// Blocks until a complete DVC-layer data message for `channel_id` is available,
    /// draining any already-queued messages first.
    fn recv_dvc_data<S: Read + Write>(&mut self, stream: &mut S, user_id: u16, channel_id: u32) -> Result<Vec<u8>> {
        loop {
            if let Some(pos) = self.pending_dvc_data.iter().position(|(id, _)| *id == channel_id) {
                let (_, data) = self.pending_dvc_data.remove(pos).unwrap();
                return Ok(data);
            }
            let (ch, payload) = mcs::recv_data_indication(stream)?;
            if ch == self.dvc.static_channel_id() {
                if payload.len() >= 8 {
                    let length = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    let flags = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    eprintln!(
                        "[debug] recv_dvc_data: channel={ch} bytes={} channel_pdu_header: length={length} flags={flags:#010x}",
                        payload.len()
                    );
                }
                self.handle_drdynvc_chunk(stream, user_id, &payload)?;
            }
        }
    }
}

/// The mstshash cookie in the X.224 Connection Request just needs to be *a* string
/// (traditionally a username hint) — sign-in happens interactively in the browser, so this
/// isn't tied to any real account.
const MSTSHASH_COOKIE: &str = "rdp-client";
const DEFAULT_PORT: u16 = 3389;
// Phase 1 target: a single fixed-size desktop. Multi-monitor / real resolutions come in
// Phase 3 via MS-RDPEDISP.
const DESKTOP_WIDTH: u16 = 1280;
const DESKTOP_HEIGHT: u16 = 800;

/// Splits a `<host>` or `<host>:<port>` CLI argument into (host:port for TCP, bare hostname
/// for the RDS AAD Auth resource URI — domain suffix stripped, matching how the RDP host
/// itself reports its short hostname via `dsregcmd /status`).
fn parse_target(arg: &str) -> (String, String) {
    let (host_part, port) = match arg.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, p.to_string()),
        _ => (arg, DEFAULT_PORT.to_string()),
    };
    let host_name = host_part.split('.').next().unwrap_or(host_part).to_string();
    (format!("{host_part}:{port}"), host_name)
}

struct CliArgs {
    target: String,
    /// 1Password item UUID to source AAD credentials from. Presence enables the
    /// automated login flow (with local credential caching) instead of the manual
    /// print-URL-and-paste-redirect flow.
    aad_op_item: Option<String>,
    /// Run the AAD login browser headless. Only meaningful with `aad_op_item` set, and
    /// only for the initial automated attempt — a manual-login fallback is always headed,
    /// since a human needs to see and interact with it.
    headless: bool,
}

const USAGE: &str = "usage: rdp-client <host[:port]> [--aad-op-item <1password-item-uuid>] [--headless]";

fn parse_cli_args() -> Result<CliArgs> {
    let mut args = std::env::args().skip(1);
    let target = args.next().context(USAGE)?;
    let mut aad_op_item = None;
    let mut headless = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--aad-op-item" => {
                aad_op_item = Some(
                    args.next()
                        .context("--aad-op-item requires a 1Password item UUID")?,
                );
            }
            "--headless" => headless = true,
            other => bail!("unrecognized argument {other:?}\n{USAGE}"),
        }
    }
    Ok(CliArgs {
        target,
        aad_op_item,
        headless,
    })
}

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

    let cli = parse_cli_args()?;
    let (host_addr, host_name) = parse_target(&cli.target);

    let scope = format!("ms-device-service://termsrv.wvd.microsoft.com/name/{host_name}/user_impersonation");
    // Per FreeRDP's aad.c (libfreerdp/core/aad.c, aad_create_jws_payload): the assertion's
    // `u` claim is the resource + /name/<hostname> WITHOUT the /user_impersonation suffix —
    // different from the scope string used to acquire the token.
    let resource_uri = format!("ms-device-service://termsrv.wvd.microsoft.com/name/{host_name}");

    println!("== Phase 0: RDS AAD Auth handshake against {host_addr} ==");

    println!("[1/6] generating PoP key + acquiring RDP access token...");
    let http = reqwest::blocking::Client::new();
    let pop_key = aad::PopKey::generate().context("generating PoP key")?;
    let code_source = match &cli.aad_op_item {
        Some(op_item) => aad::AuthCodeSource::Auto {
            op_item,
            headless: cli.headless,
        },
        None => aad::AuthCodeSource::Manual,
    };
    let access_token = aad::acquire_rdp_access_token(&http, &scope, &pop_key, &code_source)
        .context("acquiring RDP access token")?;
    println!("      access token acquired ({} bytes)", access_token.access_token.len());

    println!("[2/6] acquiring AAD nonce...");
    let aad_nonce = aad::acquire_aad_nonce(&http).context("acquiring AAD nonce")?;

    println!("[3/6] connecting to {host_addr}...");
    let mut stream = TcpStream::connect(&host_addr).context("connecting to RDP host")?;

    println!("[4/6] X.224 negotiation (requesting PROTOCOL_RDSAAD)...");
    x224::send_connection_request(&mut stream, MSTSHASH_COOKIE)?;
    let selected = x224::recv_connection_confirm(&mut stream)?;
    if selected != x224::PROTOCOL_RDSAAD {
        bail!("server did not select PROTOCOL_RDSAAD (selectedProtocol={selected:#010x}) — is enablerdsaadauth actually negotiable here?");
    }
    println!("      server selected PROTOCOL_RDSAAD");

    println!("[5/6] TLS handshake...");
    let sni_name = host_addr.split(':').next().unwrap_or(&host_addr);
    let mut tls_stream = tls::upgrade(stream, sni_name).context("TLS handshake")?;

    println!("[6/6] RDS AAD Auth PDU exchange...");
    let server_nonce = rds_aad::recv_server_nonce(&mut tls_stream).context("receiving Server Nonce PDU")?;
    let assertion = aad::build_rdp_assertion(&access_token, &resource_uri, &pop_key, &server_nonce, &aad_nonce)
        .context("building RDP Assertion")?;
    rds_aad::send_authentication_request(&mut tls_stream, &assertion)
        .context("sending Authentication Request PDU")?;
    rds_aad::recv_authentication_result(&mut tls_stream).context("receiving Authentication Result PDU")?;

    println!("\n✅ RDS AAD Auth succeeded — authentication proven end-to-end.");

    println!("\n== Phase 1: MCS connection + capability exchange ==");

    println!("[1/8] MCS Connect Initial/Response...");
    let mut client_data = Vec::new();
    client_data.extend(gcc::client_core_data(DESKTOP_WIDTH, DESKTOP_HEIGHT, x224::PROTOCOL_RDSAAD));
    client_data.extend(gcc::client_security_data());
    client_data.extend(gcc::client_network_data());
    let gcc_request = mcs::gcc_conference_create_request(&client_data);
    mcs::send_connect_initial(&mut tls_stream, &gcc_request)?;
    let gcc_response = mcs::recv_connect_response(&mut tls_stream)?;
    let server_data = mcs::gcc_conference_create_response_user_data(&gcc_response)?;
    let server_gcc = gcc::parse_server_gcc_data(&server_data)?;
    let server_network = server_gcc.network.context("server did not send Server Network Data")?;
    let io_channel_id = server_network.io_channel_id;
    let drdynvc_channel_id = *server_network
        .channel_ids
        .first()
        .context("server did not allocate a channel id for our requested \"drdynvc\" static channel")?;
    println!("      I/O channel id = {io_channel_id}, drdynvc channel id = {drdynvc_channel_id}");

    println!("[2/8] MCS Erect Domain + Attach User...");
    mcs::send_erect_domain_request(&mut tls_stream)?;
    mcs::send_attach_user_request(&mut tls_stream)?;
    let user_id = mcs::recv_attach_user_confirm(&mut tls_stream)?;
    println!("      user id = {user_id}");

    println!("[3/8] MCS Channel Join (user channel + I/O channel + drdynvc)...");
    mcs::send_channel_join_request(&mut tls_stream, user_id, user_id)?;
    mcs::recv_channel_join_confirm(&mut tls_stream)?;
    mcs::send_channel_join_request(&mut tls_stream, user_id, io_channel_id)?;
    mcs::recv_channel_join_confirm(&mut tls_stream)?;
    mcs::send_channel_join_request(&mut tls_stream, user_id, drdynvc_channel_id)?;
    mcs::recv_channel_join_confirm(&mut tls_stream)?;

    // From here on, traffic for the base I/O channel and the "drdynvc" dynamic-virtual-
    // channel machinery can arrive interleaved on the wire — route both through one place
    // so neither starves the other (a real Windows host started sending drdynvc capability
    // negotiation before our own I/O-channel finalization sequence had finished draining).
    let mut router = ChannelRouter::new(io_channel_id, drdynvc_channel_id);

    println!("[4/8] sending Client Info PDU...");
    mcs::send_data_request(&mut tls_stream, user_id, io_channel_id, &client_info::build())?;

    println!("[5/8] awaiting License PDU...");
    let license_payload = router.recv_io(&mut tls_stream, user_id)?;
    license::check_valid_client(&license_payload).context("licensing")?;
    println!("      license: valid client (no full licensing needed)");

    println!("[6/8] awaiting Demand Active PDU...");
    let demand_active = router.recv_io(&mut tls_stream, user_id)?;
    let share_id = capabilities::parse_demand_active(&demand_active)?;
    println!("      share id = {share_id:#010x}");
    capabilities::debug_dump_demand_active(&demand_active);
    let server_order_capability = capabilities::extract_order_capability(&demand_active);
    let (negotiated_width, negotiated_height) =
        capabilities::extract_desktop_size(&demand_active).unwrap_or((DESKTOP_WIDTH, DESKTOP_HEIGHT));
    println!("      server's negotiated desktop size = {negotiated_width}x{negotiated_height}");

    println!("[7/8] sending Confirm Active + Synchronize + Control + Font List...");
    let confirm_active = capabilities::build_confirm_active(
        share_id,
        user_id,
        DESKTOP_WIDTH,
        DESKTOP_HEIGHT,
        server_order_capability,
    );
    mcs::send_data_request(&mut tls_stream, user_id, io_channel_id, &confirm_active)?;
    mcs::send_data_request(
        &mut tls_stream,
        user_id,
        io_channel_id,
        &capabilities::build_synchronize(share_id, user_id),
    )?;
    mcs::send_data_request(
        &mut tls_stream,
        user_id,
        io_channel_id,
        &capabilities::build_control_cooperate(share_id, user_id),
    )?;
    mcs::send_data_request(
        &mut tls_stream,
        user_id,
        io_channel_id,
        &capabilities::build_control_request_control(share_id, user_id),
    )?;
    mcs::send_data_request(
        &mut tls_stream,
        user_id,
        io_channel_id,
        &capabilities::build_font_list(share_id, user_id),
    )?;

    println!("[8/8] draining finalization responses (Synchronize/Control/Font Map)...");
    loop {
        let payload = router.recv_io(&mut tls_stream, user_id)?;
        match capabilities::peek_pdu_type2(&payload) {
            Ok(47) => {
                let body = capabilities::data_pdu_payload(&payload)?;
                if body.len() < 4 {
                    bail!("Set Error Info PDU too short to read errorInfo");
                }
                let code = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                if code != 0 {
                    bail!("server sent Set Error Info: {code:#010x} = {}", errinfo::describe(code));
                }
                // ERRINFO_NONE: "No error has occurred. This code SHOULD be ignored."
                println!("      (Set Error Info: ERRINFO_NONE — not an actual error, continuing)");
            }
            Ok(40) => {
                println!("      (font map received — finalization complete)");
                break;
            }
            Ok(t) => println!("      (data PDU type {t} — skipping)"),
            Err(e) => println!("      (skipping non-Data PDU: {e:#})"),
        }
    }
    // This host never sends legacy slow-path Bitmap Update PDUs — it only produces
    // graphics via MS-RDPEGFX over a Dynamic Virtual Channel. The channel's wire name on
    // this host is the long form "Microsoft::Windows::RDS::Graphics", not the short
    // "rdpgfx" alias (that's apparently just FreeRDP's internal constant name) — confirmed
    // by testing: without RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL set in Client Core Data's
    // earlyCapabilityFlags, this channel wasn't even offered at all; with it set, this is
    // the name that shows up.
    const GFX_CHANNEL_NAME: &str = "Microsoft::Windows::RDS::Graphics";
    println!("\n== Phase 2: Dynamic Virtual Channel + MS-RDPEGFX ==");

    println!("[1/4] drdynvc capability negotiation + opening \"{GFX_CHANNEL_NAME}\"...");
    let gfx_channel_id = router.open_dvc_channel(&mut tls_stream, user_id, GFX_CHANNEL_NAME)?;
    println!("      graphics dynamic channel id = {gfx_channel_id}");

    println!("[2/4] sending RDPGFX_CAPS_ADVERTISE (v8 only, forces RemoteFX Progressive)...");
    router
        .dvc
        .send_data(&mut tls_stream, user_id, gfx_channel_id, &gfx::build_caps_advertise())?;

    println!("[3/4] awaiting handshake (Caps Confirm, Reset Graphics, Create Surface, Map Surface to Output)...");
    let mut frames_decoded = 0u32;
    let mut got_first_wire_to_surface = false;
    // Everything the server sends on this channel is wrapped in a ZGFX (RDP 8.0 bulk
    // compression) container — informal FreeRDP name, see MS-RDPEGFX §2.2.5 — that must be
    // unwrapped before the bytes are valid RDPGFX_HEADER-prefixed PDUs. The client's own
    // outgoing PDUs (e.g. CAPS_ADVERTISE above) are sent raw/unwrapped, matching FreeRDP.
    let mut zgfx = zgfx::ZgfxContext::new();
    loop {
        let msg = router.recv_dvc_data(&mut tls_stream, user_id, gfx_channel_id)?;
        let msg = zgfx.decompress(&msg)?;
        for pdu in gfx::split_pdus(&msg)? {
            match pdu {
                gfx::GfxPdu::CapsConfirm => println!("      caps confirmed"),
                gfx::GfxPdu::ResetGraphics { width, height } => {
                    println!("      reset graphics: {width}x{height}");
                }
                gfx::GfxPdu::CreateSurface(s) => {
                    println!("      create surface {}: {}x{} format={:#04x}", s.surface_id, s.width, s.height, s.pixel_format);
                }
                gfx::GfxPdu::DeleteSurface { surface_id } => println!("      delete surface {surface_id}"),
                gfx::GfxPdu::MapSurfaceToOutput(m) => {
                    println!(
                        "      map surface {} to output at ({}, {})",
                        m.surface_id, m.output_origin_x, m.output_origin_y
                    );
                }
                gfx::GfxPdu::StartFrame { frame_id } => println!("      start frame {frame_id}"),
                gfx::GfxPdu::EndFrame { frame_id } => {
                    frames_decoded += 1;
                    println!("      end frame {frame_id} (total decoded: {frames_decoded})");
                    router.dvc.send_data(
                        &mut tls_stream,
                        user_id,
                        gfx_channel_id,
                        &gfx::build_frame_acknowledge(frame_id, frames_decoded),
                    )?;
                    if got_first_wire_to_surface {
                        println!("\n✅ Phase 2 GFX pipeline is live — receiving real frame data.");
                        return Ok(());
                    }
                }
                gfx::GfxPdu::WireToSurface1(w) => {
                    println!(
                        "      wire-to-surface: surface={} codec={:#06x} rect=({},{})-({},{}) bytes={}",
                        w.surface_id,
                        w.codec_id,
                        w.dest_rect.left,
                        w.dest_rect.top,
                        w.dest_rect.right,
                        w.dest_rect.bottom,
                        w.bitmap_data.len()
                    );
                    got_first_wire_to_surface = true;
                }
                gfx::GfxPdu::SolidFill(_) => println!("      solid fill"),
                gfx::GfxPdu::SurfaceToSurface(_) => println!("      surface-to-surface"),
                gfx::GfxPdu::Other { cmd_id } => println!("      (other RDPGFX PDU, cmdId={cmd_id:#06x})"),
            }
        }
    }
}
