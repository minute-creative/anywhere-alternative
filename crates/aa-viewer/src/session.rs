//! Viewer network loop: handshake, receive video, decode, send input.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aa_core::control::ControlMessage;
use aa_core::control_flow::ReceiverReport;
use aa_core::input::InputEvent;
use aa_core::stats::StreamStats;
use aa_core::wire::{self, CompleteFrame, Header, Kind, Packet, Reassembler, Reassembly, SeqCounter};
use aa_platform::{VideoDecoder, ViewerBackends};
use bytes::{Bytes, BytesMut};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::link::{FrameSlot, ViewerCommand};

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
}

/// Runs the whole session. `commands` carries input and settings from the
/// window; it is borrowed so a reconnect can keep using it.
#[allow(clippy::too_many_lines)] // one select! loop; splitting it would hide the flow
pub async fn run(
    opts: Options,
    backends: ViewerBackends,
    commands: &mut mpsc::Receiver<ViewerCommand>,
) -> anyhow::Result<()> {
    let Options { host, bind, frames, test_input, stats_tx, mic: start_mic, pad_out, on_connected, on_status } = opts;
    let mut last_status: Option<String> = None;
    let socket = crate::udp::bind(bind)?;
    socket.connect(host).await?;
    tracing::info!("connecting to {host} from {}", socket.local_addr()?);

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
                        Ok(ControlMessage::Welcome { negotiated, host_os, .. }) => {
                            crate::link::set_host_os(&host_os);
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
    if let Some(cb) = &on_connected {
        cb();
    }
    let mut last_from_host = Instant::now();
    if !negotiated.gamepad {
        tracing::info!("controllers won't reach the host: it has no virtual controller driver (ViGEmBus on Windows)");
    }
    // Now we know the codec, build the decoder for it.
    let decoder = decoder_factory(negotiated.codec)
        .map_err(|e| anyhow::anyhow!("no decoder for negotiated codec {:?}: {e}", negotiated.codec))?;

    // --- decode thread -----------------------------------------------------
    let (frame_tx, frame_rx) = mpsc::channel::<CompleteFrame>(2);
    // Set by the decode thread when it cannot continue (no reference frame);
    // the network loop turns it into a NACK so the host sends a keyframe, and
    // the decode thread skips non-keyframes until one arrives.
    let need_keyframe = Arc::new(AtomicBool::new(true));
    let decoder_wants_key = Arc::clone(&need_keyframe);
    std::thread::Builder::new()
        .name("aa-decode".into())
        .spawn(move || decode_thread(decoder, frame_rx, frames.as_ref(), &decoder_wants_key))?;

    // --- main loop ---------------------------------------------------------
    let mut reassembler = Reassembler::default();
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
        MicStart::Real => Some(crate::audio::MicSender::start()),
        MicStart::Tone => Some(crate::audio::MicSender::start_tone()),
    };
    if mic.is_some() {
        tracing::info!("sending this machine's microphone to the host");
    }

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
                stats.loss.observe(packet.header.seq);

                match packet.header.kind {
                    Kind::Video | Kind::VideoFec => {
                        let fid = packet.header.frame_id;
                        if !frame_first_slice.is_some_and(|(id, _)| id == fid) {
                            frame_first_slice = Some((fid, Instant::now()));
                        }
                        match reassembler.push(packet) {
                            Reassembly::Complete(frame) => {
                                if let Some((id, t0)) = frame_first_slice.take().filter(|(id, _)| *id == frame.frame_id) {
                                    let _ = id;
                                    stats.frame_assembly_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                                }
                                frames_this_second += 1;
                                pacing.frame();
                                let fid = frame.frame_id;
                                if frame_tx.try_send(frame).is_err() {
                                    // Decoder is behind; whatever it misses breaks the
                                    // reference chain, so a keyframe is needed.
                                    stats.frames_dropped += 1;
                                    need_keyframe.store(true, Ordering::Relaxed);
                                    let _ = fid;
                                }
                            }
                            Reassembly::Pending | Reassembly::Stale => {}
                        }
                        for lost in reassembler.take_abandoned() {
                            stats.frames_dropped += 1;
                            need_keyframe.store(true, Ordering::Relaxed);
                            let _ = lost;
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
                        }
                    }
                    Kind::Control => match ControlMessage::decode(&packet.payload) {
                        Ok(ControlMessage::Bye) => {
                            tracing::info!("host said bye");
                            return Ok(());
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
                match cmd {
                    Some(ViewerCommand::Input(first)) => {
                        // Coalesce everything already queued into one datagram.
                        let mut batch = vec![first];
                        while let Ok(cmd) = commands.try_recv() {
                            let ViewerCommand::Input(ev) = cmd else {
                                // Not an input event; handle it after this batch.
                                match cmd {
                                    ViewerCommand::SetMaxBitrate(kbps) => {
                                        send_control(&socket, &ControlMessage::SetMaxBitrate { kbps }, &seq).await?;
                                    }
                                    ViewerCommand::SetHostMute(muted) => {
                                        send_control(&socket, &ControlMessage::SetHostMute { muted }, &seq).await?;
                                    }
                                    ViewerCommand::SetMic(on) => set_mic(&mut mic, on),
                                    ViewerCommand::Pad(m) => send_pad(&socket, &m, &seq).await?,
                                    ViewerCommand::Quit => {
                                        send_input(&socket, &batch, &seq, negotiated.gamepad).await?;
                                        send_control(&socket, &ControlMessage::Bye, &seq).await?;
                                        return Ok(());
                                    }
                                    ViewerCommand::Input(_) => unreachable!(),
                                }
                                continue;
                            };
                            batch.push(ev);
                            if batch.len() * InputEvent::MAX_ENCODED >= wire::MAX_PAYLOAD {
                                break;
                            }
                        }
                        send_input(&socket, &batch, &seq, negotiated.gamepad).await?;
                    }
                    Some(ViewerCommand::SetMaxBitrate(kbps)) => {
                        send_control(&socket, &ControlMessage::SetMaxBitrate { kbps }, &seq).await?;
                    }
                    Some(ViewerCommand::SetHostMute(muted)) => {
                        send_control(&socket, &ControlMessage::SetHostMute { muted }, &seq).await?;
                    }
                    Some(ViewerCommand::SetMic(on)) => set_mic(&mut mic, on),
                    Some(ViewerCommand::Pad(m)) => send_pad(&socket, &m, &seq).await?,
                    Some(ViewerCommand::Quit) | None => {
                        tracing::info!("window closed");
                        send_control(&socket, &ControlMessage::Bye, &seq).await?;
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
                let recv_d = stats.loss.received - last_received;
                // Saturating: a late packet *returns* a loss, so the total can dip
                // below last second's. Plain subtraction wrapped to ~2^64 and the host
                // read it as 100% loss.
                let lost_d = stats.loss.lost.saturating_sub(last_lost);
                let total = recv_d + lost_d;
                let loss_1s = if total == 0 { 0.0 } else { lost_d as f64 / total as f64 };
                let pace = pacing.take(negotiated.fps);
                tracing::info!(
                    fps = frames_this_second,
                    mbps = format_args!("{:.1}", bytes_this_second as f64 * 8.0 / 1e6),
                    rtt_ms = format_args!("{:.2}", stats.rtt_ms.get().unwrap_or(0.0)),
                    assembly_ms = format_args!("{:.2}", stats.frame_assembly_ms.get().unwrap_or(0.0)),
                    loss = format_args!("{:.2}%", loss_1s * 100.0),
                    dropped = stats.frames_dropped,
                    fec_fixed = reassembler.recovered,
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

fn decode_thread(
    mut decoder: Box<dyn VideoDecoder>,
    mut rx: mpsc::Receiver<CompleteFrame>,
    frames: Option<&FrameSlot>,
    need_keyframe: &AtomicBool,
) {
    let mut skipped = 0u32;
    while let Some(frame) = rx.blocking_recv() {
        // A P-frame is useless without the frames before it. After any gap,
        // wait for a keyframe; feeding the decoder garbage only makes it
        // report errors for every frame until the next keyframe anyway.
        if need_keyframe.load(Ordering::Relaxed) && !frame.keyframe {
            skipped += 1;
            continue;
        }
        if frame.keyframe && skipped > 0 {
            tracing::info!(skipped, frame_id = frame.frame_id, "keyframe arrived; resuming");
            skipped = 0;
        }
        match decoder.decode(frame.frame_id, &frame.data) {
            Ok(Some(picture)) => {
                need_keyframe.store(false, Ordering::Relaxed);
                tracing::trace!(frame_id = picture.frame_id, ?picture.resolution, "decoded");
                if let Some(slot) = frames {
                    slot.publish(picture);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(frame_id = frame.frame_id, "decode failed: {e}; requesting keyframe");
                need_keyframe.store(true, Ordering::Relaxed);
            }
        }
    }
}

fn monotonic_us() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

async fn send_control(socket: &UdpSocket, msg: &ControlMessage, seq: &SeqCounter) -> anyhow::Result<()> {
    let payload = msg.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
    Header { kind: Kind::Control, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }
        .write(&mut out);
    out.extend_from_slice(&payload);
    socket.send(&out).await?;
    Ok(())
}

/// One `DualSense` pass-through message, in its own datagram.
async fn send_pad(socket: &UdpSocket, m: &aa_core::ds5::PadMsg, seq: &SeqCounter) -> anyhow::Result<()> {
    let body = m.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + body.len());
    Header { kind: Kind::Pad, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
    out.extend_from_slice(&body);
    socket.send(&out).await?;
    Ok(())
}

/// `pads_ok`: the host has virtual controllers. Without them controller
/// events are dropped here rather than sent for nothing.
async fn send_input(socket: &UdpSocket, events: &[InputEvent], seq: &SeqCounter, pads_ok: bool) -> anyhow::Result<()> {
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

async fn send_report(socket: &UdpSocket, rep: &ReceiverReport, seq: &SeqCounter) -> anyhow::Result<()> {
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + ReceiverReport::ENCODED_LEN);
    Header { kind: Kind::Ack, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
    rep.encode(&mut out);
    socket.send(&out).await?;
    Ok(())
}

async fn send_nack(socket: &UdpSocket, frame_id: u32, seq: &SeqCounter) -> anyhow::Result<()> {
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
fn set_mic(mic: &mut Option<(crate::audio::MicSender, mpsc::Receiver<Bytes>)>, on: bool) {
    if on && mic.is_none() {
        tracing::info!("sending this machine's microphone to the host");
        *mic = Some(crate::audio::MicSender::start());
    } else if !on {
        *mic = None;
    }
}
