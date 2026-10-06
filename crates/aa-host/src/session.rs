//! The network loop: handshake, then fan frames out and input in.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aa_core::capability::negotiate;
use aa_core::control::ControlMessage;
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
    let socket = crate::udp::bind(listen)?;
    tracing::info!("listening on {}", socket.local_addr()?);

    let HostBackends { capture, encoder, input, gamepad, capabilities } = backends;

    let ctl = Arc::new(PipelineControl::default());
    // A few frames of slack: sending a 250-packet keyframe over Wi-Fi takes
    // longer than one frame interval, and dropping the frames behind it
    // would force yet another keyframe. Latency cost is bounded by the
    // viewer's latest-frame slot, which always shows the newest.
    let (frame_tx, mut frame_rx) = mpsc::channel::<EncodedFrame>(6);
    let (input_tx, input_rx) = mpsc::channel::<InputEvent>(256);

    let cap_ctl = Arc::clone(&ctl);
    std::thread::Builder::new()
        .name("aa-capture".into())
        .spawn(move || pipeline::capture_thread(capture, encoder, &cap_ctl, &frame_tx))?;
    std::thread::Builder::new()
        .name("aa-input".into())
        .spawn(move || pipeline::input_thread(input, gamepad, input_rx))?;

    let mut viewer: Option<Viewer> = None;
    let mut seq = SeqCounter::default();
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
                handle_packet(&socket, &packet, from, &mut viewer, &ctl, &capabilities, &input_tx, &mut seq).await?;
            }

            Some(frame) = frame_rx.recv() => {
                let Some(v) = viewer.as_ref() else { continue };
                let slices = wire::slice_frame(&frame.data, frame.meta.frame_id, frame.meta.is_keyframe, &mut seq)?;
                for s in slices {
                    // One send per slice; batching with sendmmsg is a stage-6 optimisation.
                    if let Err(e) = socket.send_to(&s, v.addr).await {
                        tracing::warn!("send failed: {e}");
                        break;
                    }
                }
            }

            _ = housekeeping.tick() => {
                if let Some(v) = viewer.as_ref().filter(|v| v.last_heard.elapsed() > VIEWER_TIMEOUT) {
                    tracing::info!(addr = %v.addr, "viewer timed out");
                    viewer = None;
                    ctl.streaming.store(false, Ordering::Relaxed);
                }
            }

            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                if let Some(v) = viewer.as_ref() {
                    send_control(&socket, v.addr, &ControlMessage::Bye, &mut seq).await?;
                }
                ctl.shutdown.store(true, Ordering::Relaxed);
                return Ok(());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_packet(
    socket: &UdpSocket,
    packet: &Packet,
    from: SocketAddr,
    viewer: &mut Option<Viewer>,
    ctl: &PipelineControl,
    host_caps: &aa_core::capability::Capabilities,
    input_tx: &mpsc::Sender<InputEvent>,
    seq: &mut SeqCounter,
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
                    ctl.streaming.store(false, Ordering::Relaxed);
                }
                ControlMessage::SetMaxBitrate { kbps } if is_current_viewer => {
                    // Stage 2: plumb to encoder.set_bitrate_kbps via a control channel.
                    tracing::info!(kbps, "bitrate cap requested (not yet applied)");
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

async fn send_control(
    socket: &UdpSocket,
    to: SocketAddr,
    msg: &ControlMessage,
    seq: &mut SeqCounter,
) -> anyhow::Result<()> {
    let payload = msg.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
    Header { kind: Kind::Control, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }
        .write(&mut out);
    out.extend_from_slice(&payload);
    socket.send_to(&out, to).await?;
    Ok(())
}
