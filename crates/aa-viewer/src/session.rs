//! Viewer network loop: handshake, receive video, decode, send input.
//!
//! In plain words: this is the watching computer's side of one connection.
//! 1. Handshake: say hello (repeated until the host answers), learn the
//!    agreed codec, size and frame rate.
//! 2. Then one loop waits for whichever happens first: a packet from the
//!    host, a command from the window (mouse, keys, settings), a microphone
//!    or camera chunk to send, or one of several timers (resend check every
//!    2 ms, ping every 0.5 s, report every second).
//! 3. Video packets are put back together into frames (`wire::Reassembler`),
//!    missing pieces are asked for again, and finished frames go to a
//!    separate decode thread so slow decoding never delays the network.
//! 4. Once a second a report tells the host how much was lost, so it can
//!    raise or lower the bitrate.
//!
//! Debugging: the `stream` log line written every second is the best
//! summary (see docs/CODE-GUIDE.md section 5 for what each number means).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::net::Link;
use aa_core::control::ControlMessage;
use aa_core::control_flow::ReceiverReport;
use aa_core::input::InputEvent;
use aa_core::stats::StreamStats;
use aa_core::wire::{self, CompleteFrame, Header, Kind, Packet, Reassembler, Reassembly, SeqCounter};
use aa_platform::{VideoDecoder, ViewerBackends};
use bytes::{Bytes, BytesMut};
use tokio::sync::mpsc;

use crate::link::{FrameSlot, ViewerCommand};

/// What calls on the other computer may use from this one (Settings →
/// "Camera and microphone for calls"). `None`: switched off.
#[derive(Debug, Clone, Default)]
pub struct CallPrefs {
    /// Camera name to prefer ("auto": the first one), or `None` for off.
    pub camera: Option<String>,
    /// Send the moving test picture instead of a camera (`--test-camera`).
    pub test_camera: bool,
    /// Microphone name to prefer ("auto": the system default), or `None`.
    pub mic: Option<String>,
}

/// Whether the session starts with the microphone on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicStart {
    Off,
    /// This machine's default microphone.
    Real,
    /// A test tone (mock runs).
    Tone,
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(6);
const KEEPALIVE: Duration = Duration::from_millis(500);
/// Don't spam the host with keyframe requests; one in flight at a time.
const NACK_INTERVAL: Duration = Duration::from_millis(150);

/// The host went quiet mid-session (PC asleep, crashed, Wi-Fi dropped).
/// The caller reconnects on this error; others (refused, bad version) are final.
#[derive(Debug)]
pub struct HostLost;

impl std::fmt::Display for HostLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the host stopped answering")
    }
}

impl std::error::Error for HostLost {}

/// No packet from the host for this long means it is gone. Video alone
/// arrives many times a second and pongs twice a second, so 4 s of
/// silence is never just a slow moment.
const HOST_SILENT: Duration = Duration::from_secs(4);

/// Everything a session needs besides the platform backends.
pub struct Options {
    pub host: SocketAddr,
    pub bind: SocketAddr,
    /// Where decoded pictures go (`None` in headless mode: decode, print stats).
    pub frames: Option<FrameSlot>,
    pub test_input: bool,
    pub stats_tx: Option<std::sync::mpsc::Sender<crate::overlay::LiveStats>>,
    pub mic: MicStart,
    /// Where the host's `DualSense` output reports go (rumble, triggers…).
    pub pad_out: Option<std::sync::mpsc::Sender<aa_core::ds5::PadMsg>>,
    /// Called once the host has accepted us (used to re-apply settings
    /// after a reconnect).
    pub on_connected: Option<Box<dyn Fn() + Send>>,
    /// Called with the host's explanation when the picture pauses (PC
    /// locked, screen off), and `None` when it is back.
    pub on_status: Option<Box<dyn Fn(Option<String>) + Send>>,
    /// Filled in with the host's key once the secure handshake succeeds
    /// (lets a reconnect find the same computer at a new address).
    pub host_key_out: Option<Arc<std::sync::Mutex<Option<aa_core::secure::PublicKeyBytes>>>>,
    /// Camera and microphone for calls on the host.
    pub calls: CallPrefs,
}

/// Runs the whole session. `commands` carries input and settings from the
/// window; it is borrowed so a reconnect can keep using it.
#[allow(clippy::too_many_lines)] // one select! loop; splitting it would hide the flow
pub async fn run(
    opts: Options,
    backends: ViewerBackends,
    commands: &mut mpsc::Receiver<ViewerCommand>,
) -> anyhow::Result<()> {
    let Options {
        host,
        bind,
        frames,
        test_input,
        stats_tx,
        mic: start_mic,
        pad_out,
        on_connected,
        on_status,
        host_key_out,
        calls,
    } = opts;
    let mut last_status: Option<String> = None;
    tracing::info!("connecting to {host}");
    let (socket, host_key) = Link::connect(bind, host).await?;
    tracing::info!("connected to {host} from {}", socket.local_addr()?);
    if let Some(out) = &host_key_out {
        if let Ok(mut k) = out.lock() {
            *k = Some(host_key);
        }
    }

    let ViewerBackends { decoder: mut decoder_factory, clipboard, capabilities } = backends;
    let clip_link = clipboard.and_then(|c| aa_platform::clipboard::spawn_worker(c).ok());
    let mut clip = aa_core::clipboard::ClipSync::default();
    let mut clip_tick = tokio::time::interval(Duration::from_millis(100));
    let seq = SeqCounter::default();

    // --- handshake ---------------------------------------------------------
    // Hello is repeated every 300 ms until the host answers: one lost packet
    // (either way) used to fail the whole connection on a lossy link. Junk
    // that arrives meanwhile is skipped instead of aborting, and "busy" is
    // retried because the previous viewer may be about to time out.
    let we_decode_hevc = capabilities.codecs.contains(&aa_core::video::Codec::Hevc);
    let hello = ControlMessage::hello(capabilities);
    let mut buf = vec![0u8; wire::MAX_DATAGRAM * 2];
    let mut said_busy = false;
    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let mut resend = tokio::time::interval(Duration::from_millis(300));
        loop {
            tokio::select! {
                _ = resend.tick() => send_control(&socket, &hello, &seq).await?,
                recv = socket.recv(&mut buf) => {
                    let n = recv?;
                    let Ok(packet) = Packet::parse(Bytes::copy_from_slice(&buf[..n])) else { continue };
                    if packet.header.kind != Kind::Control {
                        continue;
                    }
                    match ControlMessage::decode(&packet.payload) {
                        Ok(ControlMessage::Welcome { negotiated, host_os, full_access, .. }) => {
                            crate::link::set_host_os(&host_os);
                            crate::link::update_host_info(|i| i.full_access = full_access);
                            return anyhow::Ok(negotiated);
                        }
                        Ok(ControlMessage::Reject { reason }) if reason.contains("busy") => {
                            if !said_busy {
                                tracing::info!("host is busy with another viewer; waiting");
                                said_busy = true;
                            }
                        }
                        Ok(ControlMessage::Reject { reason }) => anyhow::bail!("host rejected us: {reason}"),
                        _ => {}
                    }
                }
            }
        }
    })
    .await;
    if handshake.is_err() && said_busy {
        anyhow::bail!("the host is busy: another computer is connected to it right now");
    }
    let negotiated = handshake.context_timeout()??;
    tracing::info!(?negotiated, "connected");
    aa_platform::discover::remember(host);
    aa_platform::trust::note_host_addrs(&host_key, &[host.to_string()]);
    aa_platform::trust::mark_host_used(&host_key);
    if let Some(cb) = &on_connected {
        cb();
    }
    let mut last_from_host = Instant::now();
    if !negotiated.gamepad {
        tracing::info!("controllers won't reach the host: it has no virtual controller driver (ViGEmBus on Windows)");
    }
    crate::link::update_host_info(|i| {
        i.codec = Some(negotiated.codec);
        i.notes = quality_notes(negotiated.codec, we_decode_hevc);
    });
    for n in crate::link::host_info().notes {
        tracing::warn!("{n}");
    }
    spawn_route_check(host);
    // Now we know the codec, build the decoder for it.
    let decoder = decoder_factory(negotiated.codec)
        .map_err(|e| anyhow::anyhow!("no decoder for negotiated codec {:?}: {e}", negotiated.codec))?;

    // --- decode thread -----------------------------------------------------
    // Two frames of queue: enough to ride out one slow decode, small enough
    // that a decoder which can't keep up drops frames instead of lagging.
    let (frame_tx, frame_rx) = mpsc::channel::<CompleteFrame>(2);
    // Set by the decode thread when it cannot continue (no reference frame);
    // the network loop turns it into a NACK so the host sends a keyframe, and
    // the decode thread skips non-keyframes until one arrives.
    let need_keyframe = Arc::new(AtomicBool::new(true));
    let decoder_wants_key = Arc::clone(&need_keyframe);
    // The newest frame decoded with nothing missing before it (`u32::MAX`:
    // none yet). Reported to the host so it can repair losses from there.
    let last_good = Arc::new(AtomicU32::new(u32::MAX));
    let decoder_good = Arc::clone(&last_good);
    // Frames that never reached the decoder (counted from gaps in the frame
    // numbers, see `decode_thread`): the "dropped" figure in the log and in
    // the report to the host.
    let missing = Arc::new(AtomicU64::new(0));
    let decoder_missing = Arc::clone(&missing);
    std::thread::Builder::new().name("aa-decode".into()).spawn(move || {
        decode_thread(decoder, frame_rx, frames.as_ref(), &decoder_wants_key, &decoder_good, &decoder_missing);
    })?;

    // --- main loop ---------------------------------------------------------
    let mut reassembler = Reassembler::default();
    let session_t0 = Instant::now();
    // A short tick while a frame waits for a resend: nothing else wakes
    // the loop at the right moment to notice the grace has run out.
    let mut resend_tick = tokio::time::interval(Duration::from_millis(2));
    resend_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stats = StreamStats::default();
    let mut frame_first_slice: Option<(u32, Instant)> = None;
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    let mut report = tokio::time::interval(Duration::from_secs(1));
    let mut frames_this_second = 0u32;
    let mut pacing = crate::pacing::FramePacing::default();
    // Interval deltas for the receiver report.
    let mut last_received = 0u64;
    let mut last_lost = 0u64;
    let mut last_dropped = 0u64;
    let mut last_nack: Option<Instant> = None;
    let mut bytes_this_second = 0usize;
    let mut wiggle = 0u16;
    let mut audio = crate::audio::AudioSink::new();
    // Microphone to the PC, while switched on in the overlay (or --mic).
    let mut mic: Option<(crate::audio::MicSender, mpsc::Receiver<Bytes>)> = match start_mic {
        MicStart::Off => None,
        MicStart::Real => Some(crate::audio::MicSender::start(calls.mic.clone())),
        MicStart::Tone => Some(crate::audio::MicSender::start_tone()),
    };
    if mic.is_some() {
        tracing::info!("sending this machine's microphone to the host");
    }
    // The mic is on while the overlay switch says so, or while a call on the
    // host uses "Anywhere Microphone" (and Settings allow it).
    let mut manual_mic = start_mic != MicStart::Off;
    let mut call_mic = false;
    // The camera, only while a call on the host uses "Anywhere Camera".
    let mut camera: Option<(crate::camera::CameraSender, mpsc::Receiver<crate::camera::CameraEvent>)> = None;
    let mut camera_frame_id: u32 = 0;
    // What we last told the host about the camera, repeated every couple of
    // seconds (control messages are plain datagrams and may be lost).
    let mut camera_state: Option<(ControlMessage, Instant)> = None;
    // A camera that failed is retried at most every 10 seconds.
    let mut camera_failed: Option<Instant> = None;

    // AA_SIMULATE_LOSS=18: test switch that throws away 18% of incoming
    // datagrams at random, like a radio dropping packets at flat delay.
    let simulate_loss: f64 =
        std::env::var("AA_SIMULATE_LOSS").ok().and_then(|v| v.parse::<f64>().ok()).map_or(0.0, |p| p / 100.0);
    let mut noise: u32 = 0x9E37_79B9;

    loop {
        tokio::select! {
            recv = socket.recv(&mut buf) => {
                let n = recv?;
                bytes_this_second += n;
                last_from_host = Instant::now();
                let packet = match Packet::parse(Bytes::copy_from_slice(&buf[..n])) {
                    Ok(p) => p,
                    Err(e) => { tracing::debug!("bad datagram: {e}"); continue; }
                };
                if simulate_loss > 0.0 && matches!(packet.header.kind, Kind::Video | Kind::VideoFec | Kind::Audio) {
                    // xorshift: cheap, deterministic, good enough for a test.
                    noise ^= noise << 13; noise ^= noise >> 17; noise ^= noise << 5;
                    if f64::from(noise % 10_000) / 10_000.0 < simulate_loss {
                        continue;
                    }
                }
                stats.loss.observe(packet.header.seq);

                match packet.header.kind {
                    Kind::Video | Kind::VideoFec => {
                        let fid = packet.header.frame_id;
                        if !frame_first_slice.is_some_and(|(id, _)| id == fid) {
                            frame_first_slice = Some((fid, Instant::now()));
                        }
                        let now_ms = session_t0.elapsed().as_millis() as u64;
                        let mut ready = Vec::new();
                        if let Reassembly::Complete(frame) = reassembler.push(packet, now_ms) {
                            ready.push(frame);
                        }
                        ready.extend(reassembler.poll(now_ms));
                        for frame in ready {
                            if let Some((id, t0)) = frame_first_slice.take().filter(|(id, _)| *id == frame.frame_id) {
                                let _ = id;
                                stats.frame_assembly_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                            }
                            frames_this_second += 1;
                            pacing.frame();
                            queue_frame(&frame_tx, frame, &need_keyframe);
                        }
                        for rq in reassembler.resend_requests(now_ms) {
                            send_resend(&socket, &rq, &seq).await?;
                        }
                        // Counted as missing by the decode thread when the next
                        // frame arrives; here we only ask for a fresh picture.
                        if !reassembler.take_abandoned().is_empty() {
                            need_keyframe.store(true, Ordering::Relaxed);
                        }
                        if need_keyframe.load(Ordering::Relaxed) && !last_nack.is_some_and(|t| t.elapsed() <= NACK_INTERVAL) {
                            send_nack(&socket, 0, &seq).await?;
                            last_nack = Some(Instant::now());
                        }
                    }
                    Kind::Audio => audio.handle(packet.payload),
                    Kind::Pad => {
                        if let (Some(m @ (aa_core::ds5::PadMsg::Output { .. } | aa_core::ds5::PadMsg::Audio { .. })), Some(out)) =
                            (aa_core::ds5::PadMsg::decode(&packet.payload), pad_out.as_ref())
                        {
                            let _ = out.send(m);
                        }
                    }
                    Kind::Clipboard | Kind::ClipboardAck => {
                        let (item, ack) = clip.received(&packet, &seq);
                        if let (Some(item), Some(link)) = (item, clip_link.as_ref()) {
                            let _ = link.paste_here.send(item);
                        }
                        if let Some(ack) = ack {
                            socket.send(&ack).await?;
                        }
                    }
                    Kind::Pong => {
                        if packet.payload.len() >= 8 {
                            let sent = u64::from_be_bytes(packet.payload[..8].try_into().expect("8 bytes"));
                            let now = monotonic_us();
                            stats.rtt_ms.push((now.saturating_sub(sent)) as f64 / 1000.0);
                            if let Some(rtt) = stats.rtt_ms.get() {
                                reassembler.set_rtt_ms(rtt);
                            }
                        }
                    }
                    Kind::Control => match ControlMessage::decode(&packet.payload) {
                        Ok(ControlMessage::Bye) => {
                            tracing::info!("host said bye");
                            return Ok(());
                        }
                        Ok(ControlMessage::StreamInfo { fps, kbps, sharing, warning }) => {
                            crate::link::update_host_info(|i| {
                                i.target_fps = fps;
                                i.target_kbps = kbps;
                                i.sharing = sharing;
                                // The host's input warning, kept as one note that
                                // goes away when the host stops sending it.
                                if i.warning != warning {
                                    if let Some(old) = i.warning.take() {
                                        i.notes.retain(|n| *n != old);
                                    }
                                    if let Some(w) = &warning {
                                        tracing::warn!("host: {w}");
                                        i.notes.push(w.clone());
                                    }
                                    i.warning = warning;
                                }
                            });
                        }
                        Ok(ControlMessage::Shareables { items, current }) => {
                            crate::link::update_host_info(|i| {
                                i.shareables = items;
                                i.sharing = current.label();
                            });
                        }
                        Ok(ControlMessage::CallDevices { camera: want_camera, mic: want_mic }) => {
                            let retry_ok = camera_failed.map_or(true, |t| t.elapsed() > Duration::from_secs(10));
                            if want_camera && camera.is_none() && retry_ok {
                                if let Some(prefer) = calls.camera.clone() {
                                    tracing::info!("a call on the other computer wants this camera");
                                    camera = Some(crate::camera::CameraSender::start(prefer, calls.test_camera));
                                } else {
                                    let msg = ControlMessage::CameraState {
                                        name: None,
                                        sending: false,
                                        problem: Some(format!(
                                            "the camera is switched off for calls in Anywhere's settings on {}",
                                            aa_platform::trust::computer_name()
                                        )),
                                    };
                                    send_control(&socket, &msg, &seq).await?;
                                    camera_state = Some((msg, Instant::now()));
                                }
                            } else if !want_camera && camera.is_some() {
                                camera = None;
                                let msg = ControlMessage::CameraState { name: None, sending: false, problem: None };
                                send_control(&socket, &msg, &seq).await?;
                                camera_state = Some((msg, Instant::now()));
                            }
                            call_mic = want_mic && calls.mic.is_some();
                            apply_mic(&mut mic, manual_mic || call_mic, &calls, start_mic);
                        }
                        Ok(ControlMessage::CameraKeyframe) => {
                            if let Some((c, _)) = camera.as_ref() {
                                c.keyframe();
                            }
                        }
                        Ok(ControlMessage::HostStatus { message }) if message != last_status => {
                            if let Some(m) = &message {
                                tracing::info!("host: {m}");
                            } else {
                                tracing::info!("host: screen is back");
                            }
                            if let Some(cb) = &on_status {
                                cb(message.clone());
                            }
                            last_status = message;
                        }
                        _ => {}
                    },
                    other => tracing::trace!(?other, "ignored packet"),
                }
            }

            cmd = commands.recv() => {
                // Mouse and key events are bundled: everything already queued
                // goes out in one datagram. A non-input command found while
                // bundling (setting, controller report, quit) is handled right
                // after the bundle is sent, so the order is kept.
                let follow_up: Option<Option<ViewerCommand>> = match cmd {
                    Some(ViewerCommand::Input(first)) => {
                        let mut batch = vec![first];
                        let mut after = None;
                        while let Ok(next) = commands.try_recv() {
                            match next {
                                ViewerCommand::Input(ev) => batch.push(ev),
                                other => {
                                    after = Some(other);
                                    break;
                                }
                            }
                            if batch.len() * InputEvent::MAX_ENCODED >= wire::MAX_PAYLOAD {
                                break;
                            }
                        }
                        send_input(&socket, &batch, &seq, negotiated.gamepad).await?;
                        after.map(Some)
                    }
                    other => Some(other),
                };
                if let Some(c) = follow_up {
                    let mut mic_state = MicState { mic: &mut mic, manual: &mut manual_mic, call: call_mic, calls: &calls, start: start_mic };
                    if handle_command(c, &socket, &seq, &mut mic_state).await? {
                        return Ok(());
                    }
                }
            }

            packet = async { mic.as_mut().expect("guarded").1.recv().await }, if mic.is_some() => {
                match packet {
                    Some(payload) => {
                        let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
                        Header { kind: Kind::Mic, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
                        out.extend_from_slice(&payload);
                        socket.send(&out).await?;
                    }
                    // Mic thread ended (no microphone, or permission refused).
                    None => mic = None,
                }
            }

            event = async { camera.as_mut().expect("guarded").1.recv().await }, if camera.is_some() => {
                match event {
                    Some(crate::camera::CameraEvent::Frame { data, keyframe }) => {
                        let flags = if keyframe { wire::flags::KEYFRAME } else { 0 };
                        if let Ok(slices) = wire::slice_frame_as(Kind::Camera, &data, camera_frame_id, flags, &seq) {
                            for s in &slices {
                                socket.send(s).await?;
                            }
                        }
                        camera_frame_id = camera_frame_id.wrapping_add(1);
                    }
                    Some(crate::camera::CameraEvent::Started(name)) => {
                        let msg = ControlMessage::CameraState { name: Some(name), sending: true, problem: None };
                        send_control(&socket, &msg, &seq).await?;
                        camera_state = Some((msg, Instant::now()));
                    }
                    Some(crate::camera::CameraEvent::Failed(why)) => {
                        let msg = ControlMessage::CameraState { name: None, sending: false, problem: Some(why) };
                        send_control(&socket, &msg, &seq).await?;
                        camera_state = Some((msg, Instant::now()));
                        camera_failed = Some(Instant::now());
                        camera = None;
                    }
                    None => camera = None,
                }
            }

            _ = clip_tick.tick() => {
                if let Some(link) = clip_link.as_ref() {
                    while let Ok(item) = link.copied_here.try_recv() {
                        if !clip.copied(&item) {
                            tracing::warn!(item = item.describe(), "too large to share");
                        }
                    }
                }
                for d in clip.tick(&seq) {
                    socket.send(&d).await?;
                }
            }

            _ = resend_tick.tick() => {
                let now_ms = session_t0.elapsed().as_millis() as u64;
                for frame in reassembler.poll(now_ms) {
                    frames_this_second += 1;
                    pacing.frame();
                    queue_frame(&frame_tx, frame, &need_keyframe);
                }
                for rq in reassembler.resend_requests(now_ms) {
                    send_resend(&socket, &rq, &seq).await?;
                }
                if !reassembler.take_abandoned().is_empty() {
                    need_keyframe.store(true, Ordering::Relaxed);
                }
                if need_keyframe.load(Ordering::Relaxed) && !last_nack.is_some_and(|t| t.elapsed() <= NACK_INTERVAL) {
                    send_nack(&socket, 0, &seq).await?;
                    last_nack = Some(Instant::now());
                }
            }

            _ = keepalive.tick() => {
                if last_from_host.elapsed() > HOST_SILENT {
                    tracing::warn!("nothing from the host for {HOST_SILENT:?}");
                    return Err(HostLost.into());
                }
                // Ping doubles as keepalive so the host knows we're alive.
                let mut out = BytesMut::with_capacity(wire::HEADER_LEN + 8);
                Header { kind: Kind::Ping, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
                out.extend_from_slice(&monotonic_us().to_be_bytes());
                socket.send(&out).await?;
            }

            _ = report.tick() => {
                if let Some((msg, at)) = camera_state.as_mut() {
                    if at.elapsed() >= Duration::from_secs(2) {
                        send_control(&socket, msg, &seq).await?;
                        *at = Instant::now();
                    }
                }
                let recv_d = stats.loss.received - last_received;
                // Saturating: a late packet *returns* a loss, so the total can dip
                // below last second's. Plain subtraction wrapped to ~2^64 and the host
                // read it as 100% loss.
                let lost_d = stats.loss.lost.saturating_sub(last_lost);
                let total = recv_d + lost_d;
                let loss_1s = if total == 0 { 0.0 } else { lost_d as f64 / total as f64 };
                let pace = pacing.take(negotiated.fps);
                stats.frames_dropped = missing.load(Ordering::Relaxed);
                tracing::info!(
                    fps = frames_this_second,
                    mbps = format_args!("{:.1}", bytes_this_second as f64 * 8.0 / 1e6),
                    rtt_ms = format_args!("{:.2}", stats.rtt_ms.get().unwrap_or(0.0)),
                    assembly_ms = format_args!("{:.2}", stats.frame_assembly_ms.get().unwrap_or(0.0)),
                    loss = format_args!("{:.2}%", loss_1s * 100.0),
                    dropped = stats.frames_dropped,
                    fec_fixed = reassembler.recovered,
                    resent = reassembler.resent,
                    gap_ms = format_args!("{:.1}/{:.1}/{:.1}", pace.p50_ms, pace.p99_ms, pace.max_ms),
                    stutters = format_args!("{}/{}", pace.stutters, pacing.total_stutters),
                    audio = format_args!("{}f/{}c buf={}ms under={}", audio.frames(), audio.concealed(), audio.buffer_ms(), audio.underruns()),
                    "stream"
                );
                // Receiver report: this interval's loss and abandoned frames, so
                // the host can adapt its bitrate.
                let drop_d = stats.frames_dropped - last_dropped;
                last_received = stats.loss.received;
                last_lost = stats.loss.lost;
                last_dropped = stats.frames_dropped;
                let rep = ReceiverReport {
                    loss_per_10k: (lost_d * 10_000).checked_div(total).map_or(0, |v| v.min(10_000) as u16),
                    frames_abandoned: drop_d.min(u64::from(u16::MAX)) as u16,
                    frames_received: u16::try_from(frames_this_second).unwrap_or(u16::MAX),
                    rtt_tenths_ms: (stats.rtt_ms.get().unwrap_or(0.0) * 10.0).clamp(0.0, 65535.0) as u16,
                    last_good_frame: Some(last_good.load(Ordering::Relaxed)).filter(|&f| f != u32::MAX),
                };
                send_report(&socket, &rep, &seq).await?;
                if let Some(tx) = &stats_tx {
                    let _ = tx.send(crate::overlay::LiveStats {
                        fps: frames_this_second,
                        mbps: (bytes_this_second as f64 * 8.0 / 1e6) as f32,
                        rtt_ms: stats.rtt_ms.get().unwrap_or(0.0) as f32,
                        loss_pct: (rep.loss_ratio() * 100.0) as f32,
                    });
                }
                // For the anonymous usage note at the end (with permission).
                let mbps = (bytes_this_second as f64 * 8.0 / 1e6) as f32;
                let (fps_now, paused, have_mic, have_cam) =
                    (frames_this_second, last_status.is_some(), mic.is_some(), camera.is_some());
                crate::link::update_summary(|s| {
                    s.seconds += 1;
                    s.fps_sum += u64::from(fps_now);
                    s.max_mbps = s.max_mbps.max(mbps);
                    if fps_now == 0 && !paused && s.fps_sum > 0 {
                        s.frozen_seconds += 1;
                    }
                    s.mic |= have_mic;
                    s.camera |= have_cam;
                });
                frames_this_second = 0;
                bytes_this_second = 0;

                if test_input {
                    wiggle = wiggle.wrapping_add(1000);
                    send_input(&socket, &[InputEvent::MouseMoveAbs { x: wiggle, y: wiggle }], &seq, true).await?;
                }
            }

            _ = tokio::signal::ctrl_c() => {
                send_control(&socket, &ControlMessage::Bye, &seq).await?;
                return Ok(());
            }
        }
    }
}

/// Hand a finished frame to the decode thread without waiting. If the
/// decoder is still busy (queue full) the frame is dropped; the decode
/// thread notices the missing number and waits for a keyframe.
fn queue_frame(tx: &mpsc::Sender<CompleteFrame>, frame: CompleteFrame, need_keyframe: &AtomicBool) {
    if tx.try_send(frame).is_err() {
        // Setting the flag here only makes the network loop ask the host for
        // a fresh picture at once; the decode thread finds (and counts) the
        // gap itself.
        need_keyframe.store(true, Ordering::Relaxed);
    }
}

/// Runs on its own thread: decodes frames in order and publishes pictures.
///
/// The host numbers every frame it encodes 0, 1, 2… So a missing number
/// means a frame never reached the decoder, whatever the reason: all its
/// packets lost on the way, given up by the reassembler, dropped here
/// because the decoder was behind, or dropped on the host because its
/// network side was busy. The next frame then builds on a picture the
/// decoder never saw, so we wait for a keyframe or repair frame instead.
///
/// Before 0.5.8 only the drops the network loop saw were counted, through
/// the shared `need_keyframe` flag, which the decoder could also clear a
/// moment later while finishing an older frame. A frame whose packets were
/// all lost, or one the host dropped, went unnoticed: the picture smeared
/// (decoder errors "reference lost" with `dropped=0` in the log) until the
/// next keyframe happened to come.
fn decode_thread(
    mut decoder: Box<dyn VideoDecoder>,
    mut rx: mpsc::Receiver<CompleteFrame>,
    frames: Option<&FrameSlot>,
    need_keyframe: &AtomicBool,
    last_good: &AtomicU32,
    missing: &AtomicU64,
) {
    let mut skipped = 0u32;
    // Number of the last frame that reached this thread (decoded or skipped).
    let mut last_seen: Option<u32> = None;
    while let Some(frame) = rx.blocking_recv() {
        // A P-frame is useless without the frames before it. After any gap,
        // wait for a keyframe or a repair frame (one built only on a frame we
        // confirmed we have); feeding the decoder garbage only makes it
        // report errors for every frame until then anyway.
        let resumes = frame.keyframe || frame.recovery;
        let gap = last_seen.map_or(0, |prev| frame.frame_id.wrapping_sub(prev).wrapping_sub(1));
        last_seen = Some(frame.frame_id);
        // (Bounded: a huge jump is a restart, not thousands of lost frames.)
        missing.fetch_add(u64::from(gap.min(1000)), Ordering::Relaxed);
        let gap = gap > 0;
        if gap && !resumes {
            need_keyframe.store(true, Ordering::Relaxed);
        }
        if need_keyframe.load(Ordering::Relaxed) && !resumes {
            skipped += 1;
            continue;
        }
        if resumes && skipped > 0 {
            let how = if frame.keyframe { "keyframe" } else { "repair frame" };
            tracing::info!(skipped, frame_id = frame.frame_id, "{how} arrived; resuming");
            skipped = 0;
        }
        match decoder.decode(frame.frame_id, &frame.data) {
            Ok(Some(picture)) => {
                need_keyframe.store(false, Ordering::Relaxed);
                last_good.store(frame.frame_id, Ordering::Relaxed);
                tracing::trace!(frame_id = picture.frame_id, ?picture.resolution, "decoded");
                if let Some(slot) = frames {
                    slot.publish(picture);
                }
            }
            Ok(None) => {
                if !need_keyframe.load(Ordering::Relaxed) {
                    last_good.store(frame.frame_id, Ordering::Relaxed);
                }
            }
            Err(e) => {
                tracing::warn!(frame_id = frame.frame_id, "decode failed: {e}; requesting keyframe");
                need_keyframe.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// The microphone switches `handle_command` may flip.
struct MicState<'a> {
    mic: &'a mut Option<(crate::audio::MicSender, mpsc::Receiver<Bytes>)>,
    manual: &'a mut bool,
    call: bool,
    calls: &'a CallPrefs,
    start: MicStart,
}

/// One command from the window other than mouse/keys. `None` means the
/// window is gone. Returns `true` when the session should end (after
/// telling the host goodbye).
async fn handle_command(
    cmd: Option<ViewerCommand>,
    socket: &Link,
    seq: &SeqCounter,
    m: &mut MicState<'_>,
) -> anyhow::Result<bool> {
    match cmd {
        Some(ViewerCommand::Input(events)) => {
            // Normally bundled by the caller; sent alone if one slips through.
            send_input(socket, &[events], seq, true).await?;
        }
        Some(ViewerCommand::SetMaxBitrate(kbps)) => {
            send_control(socket, &ControlMessage::SetMaxBitrate { kbps }, seq).await?;
        }
        Some(ViewerCommand::SetHostMute(muted)) => {
            send_control(socket, &ControlMessage::SetHostMute { muted }, seq).await?;
        }
        Some(ViewerCommand::SetScreenMask(on)) => {
            send_control(socket, &ControlMessage::SetScreenMask { on }, seq).await?;
        }
        Some(ViewerCommand::SetMic(on)) => {
            *m.manual = on;
            apply_mic(m.mic, *m.manual || m.call, m.calls, m.start);
        }
        Some(ViewerCommand::ShareList) => send_control(socket, &ControlMessage::ShareList, seq).await?,
        Some(ViewerCommand::SetShare(target)) => {
            send_control(socket, &ControlMessage::SetShare { target }, seq).await?;
        }
        Some(ViewerCommand::Pad(msg)) => send_pad(socket, &msg, seq).await?,
        Some(ViewerCommand::Quit) | None => {
            tracing::info!("window closed");
            send_control(socket, &ControlMessage::Bye, seq).await?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn monotonic_us() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

async fn send_control(socket: &Link, msg: &ControlMessage, seq: &SeqCounter) -> anyhow::Result<()> {
    let payload = msg.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
    Header { kind: Kind::Control, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }
        .write(&mut out);
    out.extend_from_slice(&payload);
    socket.send(&out).await?;
    Ok(())
}

/// One `DualSense` pass-through message, in its own datagram.
async fn send_pad(socket: &Link, m: &aa_core::ds5::PadMsg, seq: &SeqCounter) -> anyhow::Result<()> {
    let body = m.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + body.len());
    Header { kind: Kind::Pad, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
    out.extend_from_slice(&body);
    socket.send(&out).await?;
    Ok(())
}

/// `pads_ok`: the host has virtual controllers. Without them controller
/// events are dropped here rather than sent for nothing.
async fn send_input(socket: &Link, events: &[InputEvent], seq: &SeqCounter, pads_ok: bool) -> anyhow::Result<()> {
    let is_pad = |e: &&InputEvent| {
        matches!(e, InputEvent::Gamepad { .. } | InputEvent::GamepadAttach { .. } | InputEvent::GamepadDetach { .. })
    };
    if !pads_ok && events.iter().all(|e| is_pad(&e)) {
        return Ok(());
    }
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + events.len() * InputEvent::MAX_ENCODED);
    Header { kind: Kind::Input, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }
        .write(&mut out);
    for ev in events.iter().filter(|e| pads_ok || !is_pad(e)) {
        ev.encode(&mut out);
    }
    socket.send(&out).await?;
    Ok(())
}

async fn send_report(socket: &Link, rep: &ReceiverReport, seq: &SeqCounter) -> anyhow::Result<()> {
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + ReceiverReport::ENCODED_LEN);
    Header { kind: Kind::Ack, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
    rep.encode(&mut out);
    socket.send(&out).await?;
    Ok(())
}

async fn send_resend(socket: &Link, rq: &wire::ResendRequest, seq: &SeqCounter) -> anyhow::Result<()> {
    let payload = rq.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
    Header { kind: Kind::Resend, flags: 0, seq: seq.take(), frame_id: rq.frame_id, slice_index: 0, slice_count: 1 }
        .write(&mut out);
    out.extend_from_slice(&payload);
    socket.send(&out).await?;
    Ok(())
}

async fn send_nack(socket: &Link, frame_id: u32, seq: &SeqCounter) -> anyhow::Result<()> {
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN);
    Header { kind: Kind::Nack, flags: 0, seq: seq.take(), frame_id, slice_index: 0, slice_count: 1 }.write(&mut out);
    socket.send(&out).await?;
    Ok(())
}

/// Small helper so a timeout reads as an error with a useful message.
trait TimeoutContext<T> {
    fn context_timeout(self) -> anyhow::Result<T>;
}

impl<T> TimeoutContext<T> for Result<T, tokio::time::error::Elapsed> {
    fn context_timeout(self) -> anyhow::Result<T> {
        self.map_err(|_| {
            anyhow::anyhow!("no answer from host within {HANDSHAKE_TIMEOUT:?}; is aa-host running and reachable?")
        })
    }
}

/// Switch the microphone on or off (dropping the sender closes the mic).
fn apply_mic(
    mic: &mut Option<(crate::audio::MicSender, mpsc::Receiver<Bytes>)>,
    on: bool,
    calls: &CallPrefs,
    start: MicStart,
) {
    if on && mic.is_none() {
        tracing::info!("sending this machine's microphone to the host");
        *mic = Some(if start == MicStart::Tone {
            crate::audio::MicSender::start_tone()
        } else {
            crate::audio::MicSender::start(calls.mic.clone())
        });
    } else if !on && mic.is_some() {
        tracing::info!("microphone no longer sent");
        *mic = None;
    }
}

/// Things about this connection that cost picture quality, in plain words.
fn quality_notes(codec: aa_core::video::Codec, we_decode_hevc: bool) -> Vec<String> {
    let mut notes = Vec::new();
    if codec == aa_core::video::Codec::H264 {
        if we_decode_hevc {
            notes.push(
                "The other computer can only send H.264, which needs about 40% more data for the same picture.".into(),
            );
        } else if cfg!(target_os = "windows") {
            notes.push(
                "This PC is missing Microsoft's HEVC Video Extensions, so the picture uses H.264 and needs about \
                 40% more data. Install it from the Anywhere app (Add-ons)."
                    .into(),
            );
        }
    }
    notes
}

/// Over Tailscale, check now and then whether the connection goes direct or
/// through Tailscale's relay servers (slow: a few Mbps, extra delay), and
/// say so in the corner display while it does.
fn spawn_route_check(host: SocketAddr) {
    if !aa_platform::tailscale::is_tailscale_ip(host.ip()) {
        return;
    }
    let _ = std::thread::Builder::new().name("aa-route".into()).spawn(move || {
        const NOTE: &str = "Tailscale is relaying this connection through its servers (slow). It usually goes \
                            direct within a minute; if not, a router setting is blocking it: turn on UPnP on the \
                            router at either end.";
        let mut said = None;
        for _ in 0..120 {
            let relayed = aa_platform::tailscale::is_relayed(host.ip());
            if relayed != said {
                match relayed {
                    Some(true) => tracing::warn!("{NOTE}"),
                    Some(false) => tracing::info!("Tailscale: direct connection"),
                    None => {}
                }
                crate::link::update_host_info(|i| {
                    i.notes.retain(|n| n != NOTE);
                    if relayed == Some(true) {
                        i.notes.push(NOTE.to_owned());
                    }
                });
                said = relayed;
            }
            std::thread::sleep(Duration::from_secs(30));
        }
    });
}
