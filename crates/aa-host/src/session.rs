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
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::pipeline::{self, PipelineControl};

/// A viewer is considered gone if we hear nothing for this long.
const VIEWER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct Viewer {
    addr: SocketAddr,
    last_heard: Instant,
}

pub async fn run(listen: SocketAddr, backends: HostBackends) -> anyhow::Result<()> {
    let socket = Arc::new(crate::udp::bind(listen)?);
    tracing::info!("listening on {}", socket.local_addr()?);

    let HostBackends { capture, encoder, input, gamepad, audio, mut speaker, capabilities } = backends;

    let ctl = Arc::new(PipelineControl::default());
    // A few frames of slack: sending a 250-packet keyframe over Wi-Fi takes
    // longer than one frame interval, and dropping the frames behind it
    // would force yet another keyframe. Latency cost is bounded by the
    // viewer's latest-frame slot, which always shows the newest.
    let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(6);
    let (input_tx, input_rx) = mpsc::channel::<InputEvent>(256);
    // Audio: ~100 packets/s; 32 deep is a third of a second of slack.
    let (audio_tx, audio_rx) = mpsc::channel::<pipeline::AudioPacket>(32);
    // Who to send video to. Written by the receive loop, read by the sender
    // task. Pacing a frame takes most of a frame interval, so sending must
    // never run inside the receive loop or incoming packets starve (which
    // is exactly how the first Wi-Fi session froze).
    let video_dest: Arc<std::sync::Mutex<Option<SocketAddr>>> = Arc::new(std::sync::Mutex::new(None));

    let cap_ctl = Arc::clone(&ctl);
    std::thread::Builder::new()
        .name("aa-capture".into())
        .spawn(move || pipeline::capture_thread(capture, encoder, &cap_ctl, &frame_tx))?;
    std::thread::Builder::new()
        .name("aa-input".into())
        .spawn(move || pipeline::input_thread(input, gamepad, input_rx))?;
    if let Some(audio) = audio {
        let audio_ctl = Arc::clone(&ctl);
        std::thread::Builder::new()
            .name("aa-audio".into())
            .spawn(move || pipeline::audio_thread(audio, &audio_ctl, &audio_tx))?;
    } else {
        drop(audio_tx);
    }

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

    loop {
        tokio::select! {
            recv = socket.recv_from(&mut buf) => {
                let (n, from) = recv?;
                let datagram = Bytes::copy_from_slice(&buf[..n]);
                let packet = match Packet::parse(datagram) {
                    Ok(p) => p,
                    Err(e) => { tracing::debug!(%from, "bad datagram: {e}"); continue; }
                };
                if let Some(v) = viewer.as_mut().filter(|v| v.addr == from) {
                    v.last_heard = Instant::now();
                }
                handle_packet(&socket, &packet, from, &mut viewer, &ctl, &capabilities, &input_tx, &seq, &mut bitrate, &video_dest, &mut speaker).await?;
            }

            _ = housekeeping.tick() => {
                if let Some(v) = viewer.as_ref().filter(|v| v.last_heard.elapsed() > VIEWER_TIMEOUT) {
                    tracing::info!(addr = %v.addr, "viewer timed out");
                    viewer = None;
                    *video_dest.lock().expect("dest") = None;
                    ctl.streaming.store(false, Ordering::Relaxed);
                    let _ = input_tx.try_send(InputEvent::ReleaseAll);
                    restore_speakers(&mut speaker);
                }
            }

            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                if let Some(v) = viewer.as_ref() {
                    send_control(&socket, v.addr, &ControlMessage::Bye, &seq).await?;
                }
                restore_speakers(&mut speaker);
                ctl.shutdown.store(true, Ordering::Relaxed);
                return Ok(());
            }
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // one dispatch per packet kind
async fn handle_packet(
    socket: &UdpSocket,
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
                    if viewer.is_some() && !is_current_viewer {
                        send_control(socket, from, &ControlMessage::Reject { reason: "host busy".into() }, seq).await?;
                        return Ok(());
                    }
                    match negotiate(host_caps, &capabilities) {
                        Ok(negotiated) => {
                            tracing::info!(%from, ?negotiated, "viewer connected");
                            send_control(
                                socket,
                                from,
                                &ControlMessage::Welcome { protocol: PROTOCOL_VERSION, negotiated },
                                seq,
                            )
                            .await?;
                            *viewer = Some(Viewer { addr: from, last_heard: Instant::now() });
                            *video_dest.lock().expect("dest") = Some(from);
                            *bitrate = BitrateController::new(
                                StreamConfig::suggested_bitrate_kbps(host_caps.max_resolution, host_caps.max_fps),
                                2_000,
                                StreamConfig::default().max_bitrate_kbps,
                            );
                            ctl.target_kbps.store(bitrate.current_kbps(), Ordering::Relaxed);
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
    socket: Arc<UdpSocket>,
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
        let slices = match wire::slice_frame(&frame.data, frame.meta.frame_id, frame.meta.is_keyframe, &seq) {
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

async fn send_control(
    socket: &UdpSocket,
    to: SocketAddr,
    msg: &ControlMessage,
    seq: &SeqCounter,
) -> anyhow::Result<()> {
    let payload = msg.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
    Header { kind: Kind::Control, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }
        .write(&mut out);
    out.extend_from_slice(&payload);
    socket.send_to(&out, to).await?;
    Ok(())
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
