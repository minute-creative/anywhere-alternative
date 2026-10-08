//! The network loop: handshake, then fan frames out and input in.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aa_core::capability::negotiate;
use aa_core::config::StreamConfig;
use aa_core::control::ControlMessage;
use aa_core::control_flow::{BitrateController, Pacer, ReceiverReport};
use aa_core::input::InputEvent;
use aa_core::wire::{self, Header, Kind, Packet, SeqCounter};
use aa_core::PROTOCOL_VERSION;
use aa_platform::{EncodedFrame, HostBackends};
use bytes::{Buf, Bytes, BytesMut};
use tokio::sync::mpsc;

use crate::net::Net;
use crate::pipeline::{self, PipelineControl};

static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ask the running session to end cleanly (says goodbye to the viewer,
/// restores the speakers).
pub fn request_stop() {
    STOP.store(true, Ordering::Relaxed);
}

async fn stop_requested() {
    while !STOP.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A viewer is considered gone if we hear nothing for this long.
const VIEWER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct Viewer {
    addr: SocketAddr,
    last_heard: Instant,
    /// When its Hello was accepted (repeats right after are just re-answered).
    welcomed: Instant,
}

/// A viewer silent this long may be replaced by a new one from the same
/// computer (it crashed or lost the network and came back). A *live*
/// viewer is never replaced: two viewer apps on one Mac used to take the
/// stream from each other every few seconds.
const TAKEOVER_AFTER_SILENCE: Duration = Duration::from_millis(1500);

#[allow(clippy::too_many_lines)] // one select! loop; splitting it would hide the flow
pub async fn run(listen: SocketAddr, backends: HostBackends) -> anyhow::Result<()> {
    let socket = Arc::new(crate::net::Net::bind(listen)?);
    tracing::info!("listening on {}", socket.local_addr()?);
    // The addresses to type on the viewer if automatic discovery is blocked.
    let port = socket.local_addr()?.port();
    let addrs: Vec<String> = aa_platform::lan::ipv4_interfaces().iter().map(|i| format!("{}:{port}", i.ip)).collect();
    if !addrs.is_empty() {
        tracing::info!("viewers can connect to: {}", addrs.join("  or  "));
    }

    let HostBackends {
        capture,
        encoder,
        encoder_factory,
        rebuild,
        input,
        gamepad,
        audio,
        mut speaker,
        clipboard,
        pad_attach,
        capabilities,
    } = backends;
    // Shared copy/paste: a thread watches this PC's clipboard; `clip` turns
    // copies into datagrams and datagrams back into pastes.
    let clip_link = clipboard.and_then(|c| aa_platform::clipboard::spawn_worker(c).ok());
    let mut clip = aa_core::clipboard::ClipSync::default();
    let mut clip_tick = tokio::time::interval(Duration::from_millis(100));
    let mut mic = MicSink::default();
    // What we last told the viewer about the screen, and when.
    let mut told: (Option<&'static str>, Option<SocketAddr>, Instant) = (None, None, Instant::now());

    let ctl = Arc::new(PipelineControl::default());
    // A few frames of slack: sending a 250-packet keyframe over Wi-Fi takes
    // longer than one frame interval, and dropping the frames behind it
    // would force yet another keyframe. Latency cost is bounded by the
    // viewer's latest-frame slot, which always shows the newest.
    let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(6);
    let (input_tx, input_rx) = mpsc::channel::<InputEvent>(256);
    // DualSense pass-through: messages for the viewer (from the virtual
    // controller's threads) and rumble from generic fallback pads.
    let (pad_out_tx, mut pad_out_rx) = mpsc::channel::<aa_core::ds5::PadMsg>(64);
    let (rumble_tx, mut rumble_rx) = mpsc::channel::<aa_core::input::Rumble>(64);
    let mut pads = {
        let fallback_tx = input_tx.clone();
        aa_platform::padhub::PadHub::new(aa_platform::padhub::PadHooks {
            attach: pad_attach,
            to_viewer: Arc::new(move |m| {
                let _ = pad_out_tx.try_send(m);
            }),
            fallback: Box::new(move |ev| {
                let _ = fallback_tx.try_send(ev);
            }),
        })
    };
    let mut pad_owner: Option<SocketAddr> = None;
    let mut pad_tick = tokio::time::interval(Duration::from_millis(20));
    pad_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Audio: ~100 packets/s; 32 deep is a third of a second of slack.
    let (audio_tx, audio_rx) = mpsc::channel::<pipeline::AudioPacket>(32);
    // Who to send video to. Written by the receive loop, read by the sender
    // task. Pacing a frame takes most of a frame interval, so sending must
    // never run inside the receive loop or incoming packets starve (which
    // is exactly how the first Wi-Fi session froze).
    let video_dest: Arc<std::sync::Mutex<Option<SocketAddr>>> = Arc::new(std::sync::Mutex::new(None));

    let cap_ctl = Arc::clone(&ctl);
    let initial_codec = capabilities.codecs.first().copied().unwrap_or(aa_core::video::Codec::H264);
    std::thread::Builder::new().name("aa-capture".into()).spawn(move || {
        let p = pipeline::Pipeline { capture, encoder, codec: initial_codec, factory: encoder_factory, rebuild };
        pipeline::capture_thread(p, &cap_ctl, &frame_tx);
    })?;
    std::thread::Builder::new()
        .name("aa-input".into())
        .spawn(move || pipeline::input_thread(input, gamepad, input_rx, &rumble_tx))?;
    if let Some(audio) = audio {
        let audio_ctl = Arc::clone(&ctl);
        std::thread::Builder::new()
            .name("aa-audio".into())
            .spawn(move || pipeline::audio_thread(audio, &audio_ctl, &audio_tx))?;
    } else {
        drop(audio_tx);
    }

    start_beacon(listen.port(), socket.public_key_hex());
    set_connected(false);

    let mut viewer: Option<Viewer> = None;
    let seq = Arc::new(SeqCounter::default());
    let fps = capabilities.max_fps;
    let start_kbps = StreamConfig::suggested_bitrate_kbps(capabilities.max_resolution, fps);
    // Floor: still legible for desktop work. Ceiling: the user's cap.
    let mut bitrate = BitrateController::new(start_kbps, 2_000, StreamConfig::default().max_bitrate_kbps);
    let pacer = Pacer::new();

    // --- sender task: owns pacing, shares the socket --------------------
    {
        let socket = Arc::clone(&socket);
        let video_dest = Arc::clone(&video_dest);
        tokio::spawn(sender_task(socket, frame_rx, audio_rx, video_dest, pacer, fps, Arc::clone(&seq)));
    }
    let mut buf = vec![0u8; wire::MAX_DATAGRAM * 2];
    let mut housekeeping = tokio::time::interval(Duration::from_secs(1));
    let mut ticks = 0u64;

    loop {
        tokio::select! {
            recv = socket.recv_from(&mut buf) => {
                let (n, from) = match recv {
                    Ok(x) => x,
                    // Windows quirk: when the viewer quits, our next datagram to
                    // it bounces ("port unreachable") and Windows reports that
                    // as an error on the *next receive*. It means nothing for
                    // us; it used to end the whole host. Any other receive
                    // error is logged and survived too: a host must keep
                    // running through Wi-Fi drops and sleep/resume.
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                    Err(e) => {
                        tracing::warn!("network receive error (continuing): {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let datagram = Bytes::copy_from_slice(&buf[..n]);
                let packet = match Packet::parse(datagram) {
                    Ok(p) => p,
                    Err(e) => { tracing::debug!(%from, "bad datagram: {e}"); continue; }
                };
                if let Some(v) = viewer.as_mut().filter(|v| v.addr == from) {
                    v.last_heard = Instant::now();
                }
                if packet.header.kind == Kind::Mic {
                    if viewer.as_ref().is_some_and(|v| v.addr == from) {
                        mic.handle(packet.payload.clone());
                    }
                    continue;
                }
                if packet.header.kind == Kind::Pad {
                    if viewer.as_ref().is_some_and(|v| v.addr == from) {
                        if pad_owner != Some(from) {
                            pads.clear();
                            pad_owner = Some(from);
                        }
                        if let Some(m) = aa_core::ds5::PadMsg::decode(&packet.payload) {
                            pads.handle(&m);
                        }
                    }
                    continue;
                }
                if matches!(packet.header.kind, Kind::Clipboard | Kind::ClipboardAck) {
                    if viewer.as_ref().is_some_and(|v| v.addr == from) {
                        let (item, ack) = clip.received(&packet, &seq);
                        if let (Some(item), Some(link)) = (item, clip_link.as_ref()) {
                            let _ = link.paste_here.send(item);
                        }
                        if let Some(ack) = ack {
                            let _ = socket.send_to(&ack, from).await;
                        }
                    }
                    continue;
                }
                if let Err(e) = handle_packet(&socket, &packet, from, &mut viewer, &ctl, &capabilities, &input_tx, &seq, &mut bitrate, &video_dest, &mut speaker).await {
                    tracing::warn!("network send error (continuing): {e:#}");
                }
            }

            _ = clip_tick.tick() => {
                // Screen status (locked, no display) to the viewer: on every
                // change, to a newly connected viewer, and every 3 s while set.
                let status = *ctl.screen_status.lock().expect("status");
                let dest = viewer.as_ref().map(|v| v.addr);
                if let Some(d) = dest {
                    let due = status.is_some() && told.2.elapsed() > Duration::from_secs(3);
                    if status != told.0 || dest != told.1 || due {
                        let msg = ControlMessage::HostStatus { message: status.map(str::to_owned) };
                        let _ = send_control(&socket, d, &msg, &seq).await;
                        told = (status, dest, Instant::now());
                    }
                }
                if let Err(e) = clip_pump(&socket, clip_link.as_ref(), &mut clip, viewer.as_ref().map(|v| v.addr), &seq).await {
                    tracing::debug!("clipboard send error (continuing): {e:#}");
                }
            }

            Some(m) = pad_out_rx.recv() => {
                if let Some(v) = viewer.as_ref().filter(|v| Some(v.addr) == pad_owner) {
                    let body = m.encode();
                    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + body.len());
                    Header { kind: Kind::Pad, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
                    out.extend_from_slice(&body);
                    let _ = socket.send_to(&out, v.addr).await;
                }
            }

            Some(r) = rumble_rx.recv() => pads.rumble(r),

            _ = pad_tick.tick(), if pads.active() > 0 => {
                // A different viewer took over: its controllers are new ones.
                if viewer.as_ref().map(|v| v.addr) != pad_owner {
                    pads.clear();
                    pad_owner = None;
                }
                pads.tick();
            }

            _ = housekeeping.tick() => {
                // Keep the "someone is connected" note fresh (a stale one,
                // left by a crash, stops counting after two minutes).
                ticks += 1;
                if viewer.is_some() && ticks % 30 == 0 {
                    set_connected(true);
                }
                if let Some(v) = viewer.as_ref().filter(|v| v.last_heard.elapsed() > VIEWER_TIMEOUT) {
                    tracing::info!(addr = %v.addr, "viewer timed out");
                    set_connected(false);
                    viewer = None;
                    *video_dest.lock().expect("dest") = None;
                    ctl.streaming.store(false, Ordering::Relaxed);
                    let _ = input_tx.try_send(InputEvent::ReleaseAll);
                    restore_speakers(&mut speaker);
                }
            }

            () = stop_requested() => {
                tracing::info!("asked to stop");
                set_connected(false);
                if let Some(v) = viewer.as_ref() {
                    let _ = send_control(&socket, v.addr, &ControlMessage::Bye, &seq).await;
                }
                restore_speakers(&mut speaker);
                ctl.shutdown.store(true, Ordering::Relaxed);
                return Ok(());
            }

            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                set_connected(false);
                if let Some(v) = viewer.as_ref() {
                    let _ = send_control(&socket, v.addr, &ControlMessage::Bye, &seq).await;
                }
                restore_speakers(&mut speaker);
                ctl.shutdown.store(true, Ordering::Relaxed);
                return Ok(());
            }
        }
    }
}

/// The viewer's microphone, played into a virtual microphone cable so apps
/// on this PC can pick it as their mic.
///
/// Windows has no built-in way for a program to *be* a microphone; that
/// takes a signed audio driver. The standard free ones are VB-Audio's
/// "CABLE" and Steam's "Streaming Microphone": each adds a pretend speaker
/// whose sound comes out of a pretend microphone. We play into the speaker
/// side; the user picks the mic side in Discord, the game or Windows.
#[derive(Debug, Default)]
struct MicSink {
    depack: Option<aa_platform::audio::Depacketizer>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    player: Option<aa_platform::audio::Player>,
    /// We looked for a virtual cable (only once per session).
    looked: bool,
    last_report: Option<Instant>,
}

impl MicSink {
    /// Names of the "speaker" side of known virtual microphone cables.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    /// On a Mac the free `BlackHole` driver plays the same role.
    const CABLES: [&'static str; 4] = ["CABLE Input", "Steam Streaming Microphone", "VB-Audio Virtual", "BlackHole"];

    fn handle(&mut self, payload: Bytes) {
        if !self.looked {
            self.looked = true;
            self.depack = aa_platform::audio::Depacketizer::new().ok();
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                if let Ok(p) = aa_platform::audio::Player::on_device_named(&Self::CABLES) {
                    tracing::info!(device = p.device(), "viewer microphone → virtual mic");
                    self.player = Some(p);
                } else if cfg!(target_os = "macos") {
                    tracing::warn!(
                        "the viewer is sending its microphone, but this Mac has no virtual microphone to play \
                         it into. Install BlackHole 2ch (free: existential.audio/blackhole, or `brew install \
                         blackhole-2ch`), restart aa-host, then choose \"BlackHole 2ch\" as the microphone \
                         in the app (or System Settings → Sound → Input)"
                    );
                } else {
                    tracing::warn!(
                        "the viewer is sending its microphone, but this PC has no virtual microphone to play \
                         it into. Install VB-CABLE (free, vb-audio.com/Cable), restart aa-host, then choose \
                         \"CABLE Output\" as the microphone in Discord, the game or Windows sound settings"
                    );
                }
            }
        }
        let Some(depack) = self.depack.as_mut() else { return };
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(p) = self.player.as_mut() {
            p.follow();
            depack.handle(payload, |pcm| p.push(pcm));
        } else {
            depack.handle(payload, |_| {});
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        depack.handle(payload, |_| {});
        if self.last_report.map_or(true, |t| t.elapsed() > Duration::from_secs(5)) {
            self.last_report = Some(Instant::now());
            tracing::info!(frames = depack.frames, concealed = depack.concealed, "viewer microphone");
        }
    }
}

/// Announce this PC on every network adapter once a second so viewers can
/// find it without anyone typing an address (see `aa_platform::lan`).
fn start_beacon(port: u16, key: String) {
    // AA_SIMULATE_NO_BEACON: test switch, as if the router filtered them.
    if std::env::var_os("AA_SIMULATE_NO_BEACON").is_some() {
        return;
    }
    let name = aa_platform::trust::computer_name();
    let payload = ControlMessage::Beacon { name, port, key }.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
    Header { kind: Kind::Control, flags: 0, seq: 0, frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
    out.extend_from_slice(&payload);
    let _ = std::thread::Builder::new().name("aa-beacon".into()).spawn(move || {
        let mut warned = false;
        loop {
            let errors = aa_platform::lan::send_everywhere(&out, aa_platform::lan::BEACON_PORT, true);
            if !errors.is_empty() && !warned {
                tracing::warn!("discovery beacon could not go out everywhere: {}", errors.join("; "));
                warned = true;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    tracing::info!("announcing this computer on the local network");
}

/// Send what was copied on this PC (if a viewer is connected; otherwise the
/// copy just stays here) and repeat any unacknowledged transfer.
async fn clip_pump(
    socket: &Net,
    link: Option<&aa_platform::clipboard::ClipboardLink>,
    clip: &mut aa_core::clipboard::ClipSync,
    dest: Option<SocketAddr>,
    seq: &SeqCounter,
) -> anyhow::Result<()> {
    let Some(link) = link else { return Ok(()) };
    while let Ok(item) = link.copied_here.try_recv() {
        // Nobody connected: the copy just stays on this PC.
        if dest.is_some() && !clip.copied(&item) {
            tracing::warn!(item = item.describe(), "too large to share");
        }
    }
    if let Some(dest) = dest {
        for d in clip.tick(seq) {
            socket.send_to(&d, dest).await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // one dispatch per packet kind
async fn handle_packet(
    socket: &Net,
    packet: &Packet,
    from: SocketAddr,
    viewer: &mut Option<Viewer>,
    ctl: &PipelineControl,
    host_caps: &aa_core::capability::Capabilities,
    input_tx: &mpsc::Sender<InputEvent>,
    seq: &SeqCounter,
    bitrate: &mut BitrateController,
    video_dest: &std::sync::Mutex<Option<SocketAddr>>,
    speaker: &mut Option<Box<dyn aa_platform::audio::SpeakerControl>>,
) -> anyhow::Result<()> {
    let is_current_viewer = viewer.as_ref().is_some_and(|v| v.addr == from);

    match packet.header.kind {
        Kind::Control => {
            let msg = match ControlMessage::decode(&packet.payload) {
                Ok(m) => m,
                Err(e) => {
                    tracing::debug!("bad control message: {e}");
                    return Ok(());
                }
            };
            match msg {
                ControlMessage::Hello { protocol, capabilities } => {
                    if protocol != PROTOCOL_VERSION {
                        let reason = format!("protocol {protocol} != {PROTOCOL_VERSION}");
                        send_control(socket, from, &ControlMessage::Reject { reason }, seq).await?;
                        return Ok(());
                    }
                    // Another machine while one is connected: busy. The *same*
                    // machine on a new port is that viewer reconnecting (it
                    // lost us for a moment): let it take over at once rather
                    // than waiting out the old session's timeout.
                    // The current viewer repeating its Hello (it resends until
                    // answered): answer again, but don't restart the stream.
                    if let Some(v) =
                        viewer.as_ref().filter(|v| v.addr == from && v.welcomed.elapsed() < Duration::from_secs(3))
                    {
                        let _ = v;
                        if let Ok(negotiated) = negotiate(host_caps, &capabilities) {
                            send_control(
                                socket,
                                from,
                                &ControlMessage::Welcome {
                                    protocol: PROTOCOL_VERSION,
                                    negotiated,
                                    host_os: std::env::consts::OS.to_owned(),
                                },
                                seq,
                            )
                            .await?;
                        }
                        return Ok(());
                    }
                    if let Some(v) = viewer.as_ref().filter(|v| v.addr != from) {
                        if v.addr.ip() == from.ip() && v.last_heard.elapsed() > TAKEOVER_AFTER_SILENCE {
                            tracing::info!(old = %v.addr, new = %from, "viewer reconnected");
                        } else {
                            send_control(socket, from, &ControlMessage::Reject { reason: "host busy".into() }, seq)
                                .await?;
                            return Ok(());
                        }
                    }
                    match negotiate(host_caps, &capabilities) {
                        Ok(negotiated) => {
                            let name = socket.peer_name(from).unwrap_or_default();
                            tracing::info!(%from, name, ?negotiated, "viewer connected");
                            let codec = negotiated.codec;
                            send_control(
                                socket,
                                from,
                                &ControlMessage::Welcome {
                                    protocol: PROTOCOL_VERSION,
                                    negotiated,
                                    host_os: std::env::consts::OS.to_owned(),
                                },
                                seq,
                            )
                            .await?;
                            *viewer = Some(Viewer { addr: from, last_heard: Instant::now(), welcomed: Instant::now() });
                            set_connected(true);
                            *video_dest.lock().expect("dest") = Some(from);
                            *bitrate = BitrateController::new(
                                StreamConfig::suggested_bitrate_kbps(host_caps.max_resolution, host_caps.max_fps),
                                2_000,
                                StreamConfig::default().max_bitrate_kbps,
                            );
                            ctl.target_kbps.store(bitrate.current_kbps(), Ordering::Relaxed);
                            ctl.codec.store(codec as u8, Ordering::Relaxed);
                            ctl.force_keyframe.store(true, Ordering::Relaxed);
                            ctl.streaming.store(true, Ordering::Relaxed);
                        }
                        Err(e) => {
                            send_control(socket, from, &ControlMessage::Reject { reason: e.to_string() }, seq).await?;
                        }
                    }
                }
                ControlMessage::Bye if is_current_viewer => {
                    tracing::info!(%from, "viewer left");
                    set_connected(false);
                    *viewer = None;
                    *video_dest.lock().expect("dest") = None;
                    ctl.streaming.store(false, Ordering::Relaxed);
                    let _ = input_tx.try_send(InputEvent::ReleaseAll);
                    restore_speakers(speaker);
                }
                ControlMessage::SetHostMute { muted } if is_current_viewer => {
                    if let Some(s) = speaker {
                        if let Err(e) = s.set_muted(muted) {
                            tracing::warn!("host mute failed: {e}");
                        }
                    } else {
                        tracing::info!("host mute requested but not supported on this host");
                    }
                }
                // Discovery answers come from any state, even mid-session:
                // a second Mac asking "who's there" should still learn our
                // name (it will be told we're busy when it says Hello).
                // AA_SIMULATE_FIREWALL: test switch that drops discovery
                // questions like a PC firewall would (beacons still go out).
                ControlMessage::Discover if std::env::var_os("AA_SIMULATE_FIREWALL").is_some() => {}
                ControlMessage::Discover => {
                    let name = aa_platform::trust::computer_name();
                    let port = socket.local_addr().map_or(aa_platform::discover::DEFAULT_PORT, |a| a.port());
                    let addrs =
                        aa_platform::lan::ipv4_interfaces().iter().map(|i| format!("{}:{port}", i.ip)).collect();
                    send_control(
                        socket,
                        from,
                        &ControlMessage::Here { name, key: socket.public_key_hex(), addrs },
                        seq,
                    )
                    .await?;
                }
                ControlMessage::SetMaxBitrate { kbps } if is_current_viewer => {
                    bitrate.set_max_kbps(kbps);
                    ctl.target_kbps.store(bitrate.current_kbps(), Ordering::Relaxed);
                    tracing::info!(kbps, "bitrate cap set");
                }
                other => tracing::debug!(?other, "ignored control message"),
            }
        }

        Kind::Input if is_current_viewer => {
            let mut payload = packet.payload.clone();
            while payload.has_remaining() {
                match InputEvent::decode(&mut payload) {
                    Ok(ev) => {
                        if input_tx.try_send(ev).is_err() {
                            tracing::warn!("input queue full, dropping event");
                        }
                    }
                    Err(e) => {
                        tracing::debug!("bad input event: {e}");
                        break;
                    }
                }
            }
        }

        Kind::Nack if is_current_viewer => {
            ctl.intra_refresh.store(true, Ordering::Relaxed);
        }

        Kind::Ack if is_current_viewer => {
            let mut payload = packet.payload.clone();
            if let Some(rep) = ReceiverReport::decode(&mut payload) {
                if let Some(kbps) = bitrate.on_report(&rep) {
                    ctl.target_kbps.store(kbps, Ordering::Relaxed);
                    tracing::info!(
                        kbps,
                        loss = format_args!("{:.1}%", rep.loss_ratio() * 100.0),
                        abandoned = rep.frames_abandoned,
                        rtt_ms = format_args!("{:.1}", f64::from(rep.rtt_tenths_ms) / 10.0),
                        "bitrate adapted"
                    );
                }
            }
        }

        Kind::Ping if is_current_viewer => {
            let mut out = BytesMut::with_capacity(wire::HEADER_LEN + packet.payload.len());
            Header { kind: Kind::Pong, seq: seq.take(), ..packet.header }.write(&mut out);
            out.extend_from_slice(&packet.payload);
            socket.send_to(&out, from).await?;
        }

        kind => tracing::trace!(?kind, %from, "ignored packet"),
    }
    Ok(())
}

/// Pulls encoded frames and sends them to the current viewer, paced across
/// the frame interval. Runs independently of the receive loop.
async fn sender_task(
    socket: Arc<Net>,
    mut frame_rx: mpsc::Receiver<EncodedFrame>,
    mut audio_rx: mpsc::Receiver<pipeline::AudioPacket>,
    video_dest: Arc<std::sync::Mutex<Option<SocketAddr>>>,
    pacer: Pacer,
    fps: u16,
    seq: Arc<SeqCounter>,
) {
    let mut audio_open = true;
    loop {
        let frame = tokio::select! {
            f = frame_rx.recv() => match f { Some(f) => f, None => break },
            a = audio_rx.recv(), if audio_open => {
                match a {
                    Some(pkt) => {
                        let dest = *video_dest.lock().expect("dest");
                        if let Some(dest) = dest {
                            let mut out = BytesMut::with_capacity(wire::HEADER_LEN + pkt.data.len());
                            Header { kind: Kind::Audio, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
                            out.extend_from_slice(&pkt.data);
                            if let Err(e) = socket.send_to(&out, dest).await {
                                tracing::warn!("audio send failed: {e}");
                            }
                        }
                    }
                    None => audio_open = false,
                }
                continue;
            }
        };
        let Some(dest) = *video_dest.lock().expect("dest") else { continue };
        let slices = match wire::slice_frame_with_fec(&frame.data, frame.meta.frame_id, frame.meta.is_keyframe, &seq) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("frame too large to slice: {e}");
                continue;
            }
        };
        // Burst send. Pacing a frame across the interval was tried and made
        // Wi-Fi worse: consecutive frames overlapped on the air and collided.
        // The real fix for bursts is a lower bitrate (the controller), and
        // the real fix for the earlier freeze was moving sends off the
        // receive loop, which this task is.
        let _ = (&pacer, fps);
        for s in slices {
            if let Err(e) = socket.send_to(&s, dest).await {
                tracing::warn!("send failed: {e}");
                break;
            }
        }
    }
    tracing::info!("sender task exiting");
}

async fn send_control(socket: &Net, to: SocketAddr, msg: &ControlMessage, seq: &SeqCounter) -> anyhow::Result<()> {
    let payload = msg.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
    Header { kind: Kind::Control, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }
        .write(&mut out);
    out.extend_from_slice(&payload);
    socket.send_to(&out, to).await?;
    Ok(())
}

/// Leave a note while someone is connected, so automatic updates wait.
fn set_connected(on: bool) {
    let path = aa_platform::update::marker::connected();
    if on {
        let _ = std::fs::write(path, b"");
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Put the host's speakers back how we found them. Called on every way a
/// viewer can leave, so a dropped Wi-Fi link never leaves the PC muted.
fn restore_speakers(speaker: &mut Option<Box<dyn aa_platform::audio::SpeakerControl>>) {
    if let Some(s) = speaker {
        if let Err(e) = s.restore() {
            tracing::warn!("restoring host speakers failed: {e}");
        }
    }
}
