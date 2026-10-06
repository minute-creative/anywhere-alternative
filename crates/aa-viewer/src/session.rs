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

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const KEEPALIVE: Duration = Duration::from_millis(500);
/// Don't spam the host with keyframe requests; one in flight at a time.
const NACK_INTERVAL: Duration = Duration::from_millis(150);

#[allow(clippy::too_many_lines)] // one select! loop; splitting it would hide the flow
/// Runs the whole session. `frames` receives decoded pictures (`None` in
/// headless mode: decode and discard, print stats), `commands` carries
/// input from the window.
pub async fn run(
    host: SocketAddr,
    bind: SocketAddr,
    backends: ViewerBackends,
    frames: Option<FrameSlot>,
    mut commands: mpsc::Receiver<ViewerCommand>,
    test_input: bool,
    stats_tx: Option<std::sync::mpsc::Sender<crate::overlay::LiveStats>>,
) -> anyhow::Result<()> {
    let socket = crate::udp::bind(bind)?;
    socket.connect(host).await?;
    tracing::info!("connecting to {host} from {}", socket.local_addr()?);

    let ViewerBackends { decoder: mut decoder_factory, capabilities } = backends;
    let seq = SeqCounter::default();

    // --- handshake ---------------------------------------------------------
    send_control(&socket, &ControlMessage::hello(capabilities), &seq).await?;
    let mut buf = vec![0u8; wire::MAX_DATAGRAM * 2];
    let negotiated = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        loop {
            let n = socket.recv(&mut buf).await?;
            let packet = Packet::parse(Bytes::copy_from_slice(&buf[..n]))?;
            if packet.header.kind != Kind::Control {
                continue;
            }
            match ControlMessage::decode(&packet.payload)? {
                ControlMessage::Welcome { negotiated, .. } => return anyhow::Ok(negotiated),
                ControlMessage::Reject { reason } => anyhow::bail!("host rejected us: {reason}"),
                _ => {}
            }
        }
    })
    .await
    .context_timeout()??;
    tracing::info!(?negotiated, "connected");
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
    // Interval deltas for the receiver report.
    let mut last_received = 0u64;
    let mut last_lost = 0u64;
    let mut last_dropped = 0u64;
    let mut last_nack: Option<Instant> = None;
    let mut bytes_this_second = 0usize;
    let mut wiggle = 0u16;
    let mut audio = crate::audio::AudioSink::new();

    loop {
        tokio::select! {
            recv = socket.recv(&mut buf) => {
                let n = recv?;
                bytes_this_second += n;
                let packet = match Packet::parse(Bytes::copy_from_slice(&buf[..n])) {
                    Ok(p) => p,
                    Err(e) => { tracing::debug!("bad datagram: {e}"); continue; }
                };
                stats.loss.observe(packet.header.seq);

                match packet.header.kind {
                    Kind::Video => {
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
                    Kind::Pong => {
                        if packet.payload.len() >= 8 {
                            let sent = u64::from_be_bytes(packet.payload[..8].try_into().expect("8 bytes"));
                            let now = monotonic_us();
                            stats.rtt_ms.push((now.saturating_sub(sent)) as f64 / 1000.0);
                        }
                    }
                    Kind::Control => {
                        if let Ok(ControlMessage::Bye) = ControlMessage::decode(&packet.payload) {
                            tracing::info!("host said bye");
                            return Ok(());
                        }
                    }
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
                                    ViewerCommand::Quit => {
                                        send_input(&socket, &batch, &seq).await?;
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
                        send_input(&socket, &batch, &seq).await?;
                    }
                    Some(ViewerCommand::SetMaxBitrate(kbps)) => {
                        send_control(&socket, &ControlMessage::SetMaxBitrate { kbps }, &seq).await?;
                    }
                    Some(ViewerCommand::SetHostMute(muted)) => {
                        send_control(&socket, &ControlMessage::SetHostMute { muted }, &seq).await?;
                    }
                    Some(ViewerCommand::Quit) | None => {
                        tracing::info!("window closed");
                        send_control(&socket, &ControlMessage::Bye, &seq).await?;
                        return Ok(());
                    }
                }
            }

            _ = keepalive.tick() => {
                // Ping doubles as keepalive so the host knows we're alive.
                let mut out = BytesMut::with_capacity(wire::HEADER_LEN + 8);
                Header { kind: Kind::Ping, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
                out.extend_from_slice(&monotonic_us().to_be_bytes());
                socket.send(&out).await?;
            }

            _ = report.tick() => {
                let recv_d = stats.loss.received - last_received;
                let lost_d = stats.loss.lost - last_lost;
                let total = recv_d + lost_d;
                let loss_1s = if total == 0 { 0.0 } else { lost_d as f64 / total as f64 };
                tracing::info!(
                    fps = frames_this_second,
                    mbps = format_args!("{:.1}", bytes_this_second as f64 * 8.0 / 1e6),
                    rtt_ms = format_args!("{:.2}", stats.rtt_ms.get().unwrap_or(0.0)),
                    assembly_ms = format_args!("{:.2}", stats.frame_assembly_ms.get().unwrap_or(0.0)),
                    loss = format_args!("{:.2}%", loss_1s * 100.0),
                    dropped = stats.frames_dropped,
                    audio = format_args!("{}f/{}c buf={}ms under={}", audio.frames, audio.concealed, audio.buffer_ms(), audio.underruns()),
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
                    send_input(&socket, &[InputEvent::MouseMoveAbs { x: wiggle, y: wiggle }], &seq).await?;
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

async fn send_input(socket: &UdpSocket, events: &[InputEvent], seq: &SeqCounter) -> anyhow::Result<()> {
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + events.len() * InputEvent::MAX_ENCODED);
    Header { kind: Kind::Input, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }
        .write(&mut out);
    for ev in events {
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
