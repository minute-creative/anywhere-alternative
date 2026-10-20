//! The network loop: handshake, then fan frames out and input in.

// In plain words: this is the sharing computer's main loop. It waits for
// packets from the viewer and reacts to each kind: hello (start streaming),
// input (pass to the mouse/keyboard thread), reports (adjust quality),
// resend requests, clipboard, controller, camera and microphone for calls.
// Separate threads capture and encode the screen and sound; a separate task
// sends the finished frames, so receiving never waits on sending.
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aa_core::capability::negotiate;
use aa_core::config::StreamConfig;
use aa_core::control::ControlMessage;
use aa_core::control_flow::{BitrateController, Pacer, ReceiverReport};
use aa_core::input::InputEvent;
use aa_core::quality::FpsLadder;
use aa_core::share::ShareTarget;
use aa_core::wire::{self, Header, Kind, Packet, SeqCounter};
use aa_core::PROTOCOL_VERSION;
use aa_platform::{EncodedFrame, HostBackends};
use bytes::{Buf, Bytes, BytesMut};
use tokio::sync::mpsc;

use crate::net::Net;
use crate::pipeline::{self, PipelineControl};

// Set by the app (through a stop request) to end the session cleanly.
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

/// Milliseconds on a clock that only moves forward (for reassembly).
fn monotonic_ms() -> u64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
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
    // Open the network socket (encrypted, see net.rs) and print the addresses
    // people can type if automatic discovery does not work.
    let socket = Arc::new(crate::net::Net::bind(listen)?);
    tracing::info!("listening on {}", socket.local_addr()?);
    // The addresses to type on the viewer if automatic discovery is blocked.
    let port = socket.local_addr()?.port();
    let addrs: Vec<String> = aa_platform::lan::ipv4_interfaces().iter().map(|i| format!("{}:{port}", i.ip)).collect();
    if !addrs.is_empty() {
        tracing::info!("viewers can connect to: {}", addrs.join("  or  "));
    }

    // Take apart the bundle of system abilities prepared in main.rs.
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
    // "Anywhere Camera" / "Anywhere Microphone": the browser add-on's link
    // and who wants the viewer's camera and mic right now.
    let (hub, mut wants) = crate::calls::CallHub::new();
    hub.serve();
    #[cfg(target_os = "windows")]
    crate::vcam::start(hub.clone());
    let mut camera_frames = aa_core::wire::Reassembler::default();
    let mut camera_key_asked: Option<Instant> = None;
    // The viewer the call devices were last set up for, and when we last
    // told it what to send (control messages are plain datagrams, so the
    // answer is repeated while anything is wanted).
    let mut call_viewer: Option<SocketAddr> = None;
    let mut calls_told = Instant::now();
    // What we last told the viewer about the screen, and when.
    let mut told: (Option<&'static str>, Option<SocketAddr>, Instant) = (None, None, Instant::now());

    // Shared switches between this loop and the capture thread (target
    // bitrate, frame rate, "send a keyframe now", what to share...).
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
    // The last second or so of video packets, so a viewer can ask for the
    // ones its radio dropped (see `Kind::Resend`).
    let recent: Arc<std::sync::Mutex<RecentPackets>> = Arc::default();

    // Start the worker threads: screen capture + encoding, input, sound.
    let cap_ctl = Arc::clone(&ctl);
    let initial_codec = capabilities.codecs.first().copied().unwrap_or(aa_core::video::Codec::H264);
    std::thread::Builder::new().name("aa-capture".into()).spawn(move || {
        let p = pipeline::Pipeline { capture, encoder, codec: initial_codec, factory: encoder_factory, rebuild };
        pipeline::capture_thread(p, &cap_ctl, &frame_tx);
    })?;
    let input_ctl = Arc::clone(&ctl);
    std::thread::Builder::new()
        .name("aa-input".into())
        .spawn(move || pipeline::input_thread(input, gamepad, input_rx, &rumble_tx, &input_ctl))?;
    if let Some(audio) = audio {
        let audio_ctl = Arc::clone(&ctl);
        std::thread::Builder::new()
            .name("aa-audio".into())
            .spawn(move || pipeline::audio_thread(audio, &audio_ctl, &audio_tx))?;
    } else {
        drop(audio_tx);
    }

    // Start announcing ourselves on the network every second.
    start_beacon(listen.port(), socket.public_key_hex());
    // Open a door on the home router so paired computers can reach this one
    // from any network (see aa_platform::portmap).
    let portmap = aa_platform::portmap::PortMap::start(listen.port());
    set_connected(false);

    // No viewer yet; start with a sensible bitrate for this screen.
    let mut viewer: Option<Viewer> = None;
    let seq = Arc::new(SeqCounter::default());
    let fps = capabilities.max_fps;
    let start_kbps = StreamConfig::suggested_bitrate_kbps(capabilities.max_resolution, fps);
    // Floor: still legible for desktop work. Ceiling: the user's cap.
    let mut bitrate = BitrateController::new(start_kbps, 2_000, StreamConfig::default().max_bitrate_kbps);
    let pacer = Pacer::new();
    // What is being shared, who chose it, and how fast to send.
    let mut stream = Stream::new(fps);

    // --- sender task: owns pacing, shares the socket --------------------
    {
        let socket = Arc::clone(&socket);
        let video_dest = Arc::clone(&video_dest);
        tokio::spawn(sender_task(
            socket,
            frame_rx,
            audio_rx,
            video_dest,
            pacer,
            fps,
            Arc::clone(&seq),
            Arc::clone(&recent),
            Arc::clone(&ctl),
        ));
    }
    let mut buf = vec![0u8; wire::MAX_DATAGRAM * 2];
    let mut housekeeping = tokio::time::interval(Duration::from_secs(1));
    let mut ticks = 0u64;

    // The main loop: wait for whichever of these happens first, handle it,
    // repeat. Each "=>" branch below is one kind of event.
    loop {
        tokio::select! {
            // A packet arrived from the network.
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
                // Read its label; skip anything malformed.
                let datagram = Bytes::copy_from_slice(&buf[..n]);
                let packet = match Packet::parse(datagram) {
                    Ok(p) => p,
                    Err(e) => { tracing::debug!(%from, "bad datagram: {e}"); continue; }
                };
                // Any packet from the current viewer proves it is still there.
                if let Some(v) = viewer.as_mut().filter(|v| v.addr == from) {
                    v.last_heard = Instant::now();
                }
                // Kinds handled right here: microphone, camera, camera state, controller,
                // clipboard. Each is accepted only from the connected viewer.
                if packet.header.kind == Kind::Mic {
                    if viewer.as_ref().is_some_and(|v| v.addr == from) {
                        mic.handle(packet.payload.clone(), &hub);
                    }
                    continue;
                }
                if packet.header.kind == Kind::Camera {
                    if viewer.as_ref().is_some_and(|v| v.addr == from) {
                        let now_ms = monotonic_ms();
                        if let aa_core::wire::Reassembly::Complete(f) = camera_frames.push(packet, now_ms) {
                            hub.frame(&f.data, f.keyframe);
                        }
                        for f in camera_frames.poll(now_ms) {
                            hub.frame(&f.data, f.keyframe);
                        }
                        let lost = !camera_frames.take_abandoned().is_empty();
                        if lost && camera_key_asked.map_or(true, |t| t.elapsed() > Duration::from_millis(300)) {
                            camera_key_asked = Some(Instant::now());
                            let _ = send_control(&socket, from, &ControlMessage::CameraKeyframe, &seq).await;
                        }
                    }
                    continue;
                }
                if packet.header.kind == Kind::Control && viewer.as_ref().is_some_and(|v| v.addr == from) {
                    if let Ok(ControlMessage::CameraState { name, sending, problem }) = ControlMessage::decode(&packet.payload) {
                        if let Some(p) = &problem {
                            tracing::warn!("the viewer's camera: {p}");
                        } else if sending {
                            tracing::info!(camera = name.as_deref().unwrap_or("?"), "the viewer's camera is on");
                        }
                        hub.set_camera(name.filter(|_| sending), problem);
                        continue;
                    }
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
                // Everything else (hello, input, reports, resends...): see handle_packet.
                if let Err(e) = handle_packet(&socket, &packet, from, &mut viewer, &ctl, &capabilities, &input_tx, &seq, &mut bitrate, &video_dest, &mut speaker, &recent, &mut stream).await {
                    tracing::warn!("network send error (continuing): {e:#}");
                }
                // The viewer changed (connected, left, replaced): tell the call devices.
                let now_viewer = viewer.as_ref().map(|v| v.addr);
                if now_viewer != call_viewer {
                    call_viewer = now_viewer;
                    camera_frames = aa_core::wire::Reassembler::default();
                    hub.set_viewer(now_viewer.map(|a| socket.peer_name(a).unwrap_or_else(|| a.ip().to_string())));
                    if let Some(a) = now_viewer {
                        let w = hub.wanted();
                        let _ = send_control(&socket, a, &ControlMessage::CallDevices { camera: w.camera, mic: w.mic }, &seq).await;
                        calls_told = Instant::now();
                    }
                }
            }

            // Every 100 ms: screen status messages and clipboard sending.
            _ = clip_tick.tick() => {
                // Screen status (locked, no display) to the viewer: on every
                // change, to a newly connected viewer, and every 3 s while set.
                let status = ctl.screen_status.lock().expect("status").or(stream.share_status);
                let dest = viewer.as_ref().map(|v| v.addr);
                if let Some(d) = dest {
                    let due = status.is_some() && told.2.elapsed() > Duration::from_secs(3);
                    if status != told.0 || dest != told.1 || due {
                        let msg = ControlMessage::HostStatus { message: status.map(str::to_owned) };
                        let _ = send_control(&socket, d, &msg, &seq).await;
                        told = (status, dest, Instant::now());
                    }
                }
                // A text field got (or lost) the keyboard focus: tell the
                // phone at once, and every second while one has it.
                if let Some(d) = dest.filter(|_| stream.text_wanted.load(Ordering::Relaxed)) {
                    let now = stream.text_field.lock().ok().and_then(|f| *f);
                    let due = now.is_some() && stream.text_told.1.elapsed() > Duration::from_secs(1);
                    if now != stream.text_told.0 || due {
                        let msg = ControlMessage::TextFocus { field: now.map(|f| f.0), secure: now.is_some_and(|f| f.1) };
                        let _ = send_control(&socket, d, &msg, &seq).await;
                        if now.is_some() != stream.text_told.0.is_some() {
                            tracing::debug!(field = ?now, "text field focus changed");
                        }
                        stream.text_told = (now, Instant::now());
                    }
                }
                // The launcher list is ready: send its pages, then gather icons.
                while let Ok(apps) = stream.apps_rx.try_recv() {
                    let Some(d) = dest else { continue };
                    let pages = aa_core::apps::pages(&apps);
                    let total = u16::try_from(pages.len()).unwrap_or(u16::MAX);
                    for (i, items) in pages.into_iter().enumerate() {
                        let page = u16::try_from(i).unwrap_or(u16::MAX);
                        let _ = send_control(&socket, d, &ControlMessage::Apps { page, pages: total, items }, &seq).await;
                    }
                    tracing::info!(apps = apps.len(), "sent the launcher list");
                    let ids: Vec<String> = apps.into_iter().map(|a| a.id).collect();
                    let tx = stream.icons_tx.clone();
                    let _ = std::thread::Builder::new().name("aa-icons".into()).spawn(move || {
                        let _ = tx.send(aa_platform::apps::icons(&ids));
                    });
                }
                // An app the phone opened is up: show only that app.
                while let Ok(target) = stream.show_rx.try_recv() {
                    if stream.full_access {
                        tracing::info!(sharing = target.label(), "showing the app the phone opened");
                        stream.viewer_choice = Some(target);
                        stream.follow_choice(&ctl, 0);
                    }
                }
                // Launcher icons ready: one named blob through the reliable transfer.
                while let Ok(pack) = stream.icons_rx.try_recv() {
                    if dest.is_some() {
                        let item = aa_core::clipboard::ClipItem::Blob { name: aa_core::apps::IconPack::BLOB_NAME.into(), data: pack.encode() };
                        if clip.copied(&item) {
                            tracing::info!(icons = pack.icons.len(), "sending launcher icons");
                        } else {
                            tracing::warn!("launcher icons too large to send");
                        }
                    }
                }
                if let Err(e) = clip_pump(&socket, clip_link.as_ref(), &mut clip, viewer.as_ref().map(|v| v.addr), &seq).await {
                    tracing::debug!("clipboard send error (continuing): {e:#}");
                }
            }

            // A DualSense message for the viewer (game rumble, lights, haptics).
            Some(m) = pad_out_rx.recv() => {
                if let Some(v) = viewer.as_ref().filter(|v| Some(v.addr) == pad_owner) {
                    let body = m.encode();
                    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + body.len());
                    Header { kind: Kind::Pad, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut out);
                    out.extend_from_slice(&body);
                    let _ = socket.send_to(&out, v.addr).await;
                }
            }

            // Rumble for a generic (non-DualSense) controller.
            Some(r) = rumble_rx.recv() => pads.rumble(r),

            // Every 20 ms while a DualSense is attached: housekeeping for it.
            _ = pad_tick.tick(), if pads.active() > 0 => {
                // A different viewer took over: its controllers are new ones.
                if viewer.as_ref().map(|v| v.addr) != pad_owner {
                    pads.clear();
                    pad_owner = None;
                }
                pads.tick();
            }

            // A call on this computer started or stopped using the camera or mic.
            Ok(()) = wants.changed() => {
                let w = *wants.borrow_and_update();
                tracing::info!(camera = w.camera, mic = w.mic, "call devices wanted");
                if let Some(v) = viewer.as_ref() {
                    let _ = send_control(&socket, v.addr, &ControlMessage::CallDevices { camera: w.camera, mic: w.mic }, &seq).await;
                    calls_told = Instant::now();
                }
            }

            // The browser add-on lost a camera picture: ask the viewer for a fresh one.
            () = hub.keyframe.notified() => {
                if let Some(v) = viewer.as_ref() {
                    let _ = send_control(&socket, v.addr, &ControlMessage::CameraKeyframe, &seq).await;
                }
            }

            // Once a second: repeat call-device requests, refresh the "connected"
            // note, follow the share choice, send stream info, and drop a viewer that
            // has been silent for 5 seconds.
            _ = housekeeping.tick() => {
                // Keep the "someone is connected" note fresh (a stale one,
                // left by a crash, stops counting after two minutes).
                ticks += 1;
                // Repeat what the viewer should send while anything is wanted
                // (and for a few seconds after, so an "off" isn't lost).
                if let Some(v) = viewer.as_ref() {
                    let w = hub.wanted();
                    if (w.camera || w.mic || calls_told.elapsed() < Duration::from_secs(4)) && ticks % 2 == 0 {
                        let _ = send_control(&socket, v.addr, &ControlMessage::CallDevices { camera: w.camera, mic: w.mic }, &seq).await;
                    }
                }
                if viewer.is_some() && ticks % 30 == 0 {
                    set_connected(true);
                }
                // What the person here wants shared (the app keeps it fresh).
                stream.follow_choice(&ctl, ticks);
                if let Some(v) = viewer.as_ref() {
                    // Every 5 s: is something here swallowing the viewer's input?
                    if ticks % 5 == 1 {
                        let now = aa_platform::input_blocker();
                        if now != stream.input_warning {
                            match now {
                                Some(w) => tracing::warn!("{w}"),
                                None if stream.input_warning.is_some() => tracing::info!("input blocker gone"),
                                None => {}
                            }
                            stream.input_warning = now;
                        }
                    }
                    let info = ControlMessage::StreamInfo {
                        fps: stream.ladder.current(),
                        kbps: bitrate.current_kbps(),
                        sharing: stream.now.label(),
                        warning: stream.input_warning.map(str::to_owned),
                        cursor_captured: aa_platform::cursor_captured(),
                    };
                    let _ = send_control(&socket, v.addr, &info, &seq).await;
                }
                if let Some(v) = viewer.as_ref().filter(|v| v.last_heard.elapsed() > VIEWER_TIMEOUT) {
                    tracing::info!(addr = %v.addr, "viewer timed out");
                    set_connected(false);
                    viewer = None;
                    *video_dest.lock().expect("dest") = None;
                    ctl.streaming.store(false, Ordering::Relaxed);
                    let _ = input_tx.try_send(InputEvent::ReleaseAll);
                    restore_speakers(&mut speaker);
                    stream.viewer_left(&ctl);
                }
                if viewer.is_none() && call_viewer.is_some() {
                    call_viewer = None;
                    hub.set_viewer(None);
                }
            }

            // The app asked us to stop: say goodbye, unmute the speakers, end.
            () = stop_requested() => {
                tracing::info!("asked to stop");
                portmap.close();
                set_connected(false);
                if let Some(v) = viewer.as_ref() {
                    let _ = send_control(&socket, v.addr, &ControlMessage::Bye, &seq).await;
                }
                restore_speakers(&mut speaker);
                ctl.shutdown.store(true, Ordering::Relaxed);
                return Ok(());
            }

            // Ctrl+C in a terminal: same as above.
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                portmap.close();
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

    // First microphone packet of this run: find a virtual microphone cable to
    // play into (VB-CABLE on Windows, BlackHole on a Mac), once.
    fn handle(&mut self, payload: Bytes, hub: &crate::calls::CallHub) {
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
        // Decode each 10 ms piece and play it into the cable, and also hand it to
        // the browser add-on for calls.
        let Some(depack) = self.depack.as_mut() else { return };
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(p) = self.player.as_mut() {
            p.follow();
            depack.handle(payload, |pcm| {
                p.push(pcm);
                hub.sound(pcm);
            });
        } else {
            depack.handle(payload, |pcm| hub.sound(pcm));
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        depack.handle(payload, |pcm| hub.sound(pcm));
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
    // The announcement is the same every time: build it once, then send it to
    // every network adapter once a second, forever, on its own thread.
    let name = aa_platform::trust::computer_name();
    let version = aa_platform::update::current().to_owned();
    let payload = ControlMessage::Beacon { name, port, key, version }.encode();
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
    // Things copied on this computer since last time.
    while let Ok(item) = link.copied_here.try_recv() {
        // Nobody connected: the copy just stays on this PC.
        if dest.is_some() && !clip.copied(&item) {
            tracing::warn!(item = item.describe(), "too large to share");
        }
    }
    // Paced clipboard pieces, repeats and requests for missing pieces.
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
    recent: &std::sync::Mutex<RecentPackets>,
    stream: &mut Stream,
) -> anyhow::Result<()> {
    // Most branches below only act for the connected viewer ("is_current_viewer").
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
                // Hello: a viewer wants to connect.
                ControlMessage::Hello { protocol, capabilities } => {
                    // Different protocol versions cannot understand each other: refuse.
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
                                    full_access: stream.full_access,
                                    version: aa_platform::update::current().to_owned(),
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
                            // Tidy up after the old connection as if it had left:
                            // keys it held go up, speakers come back (the
                            // reconnected viewer sends its mute choice again).
                            let _ = input_tx.try_send(InputEvent::ReleaseAll);
                            restore_speakers(speaker);
                        } else {
                            send_control(socket, from, &ControlMessage::Reject { reason: "host busy".into() }, seq)
                                .await?;
                            return Ok(());
                        }
                    }
                    // Agree on codec, size and frame rate; welcome the viewer; reset the
                    // bitrate; ask the encoder for a fresh keyframe; start streaming.
                    match negotiate(host_caps, &capabilities) {
                        Ok(negotiated) => {
                            let name = socket.peer_name(from).unwrap_or_default();
                            let full_access = socket.peer_full_access(from);
                            tracing::info!(%from, name, full_access, ?negotiated, "viewer connected");
                            let codec = negotiated.codec;
                            send_control(
                                socket,
                                from,
                                &ControlMessage::Welcome {
                                    protocol: PROTOCOL_VERSION,
                                    negotiated,
                                    host_os: std::env::consts::OS.to_owned(),
                                    full_access,
                                    version: aa_platform::update::current().to_owned(),
                                },
                                seq,
                            )
                            .await?;
                            *viewer = Some(Viewer { addr: from, last_heard: Instant::now(), welcomed: Instant::now() });
                            set_connected(true);
                            *video_dest.lock().expect("dest") = Some(from);
                            // Ceiling: what this picture can use, not just the
                            // user's cap (see `useful_max_kbps`).
                            stream.useful_kbps = useful_max_kbps(&negotiated);
                            let cap = StreamConfig::default().max_bitrate_kbps.min(stream.useful_kbps);
                            *bitrate = BitrateController::new(
                                StreamConfig::suggested_bitrate_kbps(host_caps.max_resolution, host_caps.max_fps),
                                2_000,
                                cap,
                            );
                            ctl.target_kbps.store(bitrate.current_kbps(), Ordering::Relaxed);
                            ctl.codec.store(codec as u8, Ordering::Relaxed);
                            stream.start(ctl, &negotiated, host_caps, full_access);
                            stream.text_wanted.store(capabilities.text_focus, Ordering::Relaxed);
                            stream.text_told = (None, Instant::now());
                            // Phone mode: a phone-shaped main screen while it
                            // watches. Rearranging screens takes ~1 s, so not
                            // here; the capture follows the new main screen.
                            if let Some([w, h]) = capabilities.phone_display {
                                stream.phone_display = true;
                                let _ = std::thread::Builder::new().name("aa-phone-display".into()).spawn(move || {
                                    if !aa_platform::phone_display_begin((w, h)) {
                                        tracing::info!("phone mode not available here; showing the normal screen");
                                    }
                                });
                            } else if std::mem::take(&mut stream.phone_display) {
                                // A viewer without it replaced one with it.
                                let _ = std::thread::Builder::new().spawn(aa_platform::phone_display_end);
                            }
                            ctl.force_keyframe.store(true, Ordering::Relaxed);
                            ctl.streaming.store(true, Ordering::Relaxed);
                        }
                        Err(e) => {
                            send_control(socket, from, &ControlMessage::Reject { reason: e.to_string() }, seq).await?;
                        }
                    }
                }
                // Goodbye: stop streaming, release any held keys, unmute speakers.
                ControlMessage::Bye if is_current_viewer => {
                    tracing::info!(%from, "viewer left");
                    set_connected(false);
                    *viewer = None;
                    *video_dest.lock().expect("dest") = None;
                    ctl.streaming.store(false, Ordering::Relaxed);
                    let _ = input_tx.try_send(InputEvent::ReleaseAll);
                    restore_speakers(speaker);
                    stream.viewer_left(ctl);
                }
                // Mute or unmute this computer's own speakers.
                ControlMessage::SetHostMute { muted } if is_current_viewer => {
                    if let Some(s) = speaker {
                        if let Err(e) = s.set_muted(muted) {
                            tracing::warn!("host mute failed: {e}");
                        }
                    } else {
                        tracing::info!("host mute requested but not supported on this host");
                    }
                }
                // Black out (or show again) this computer's own monitor.
                ControlMessage::SetScreenMask { on } if is_current_viewer => {
                    aa_platform::screen_mask(on);
                }
                // Discovery answers come from any state, even mid-session:
                // a second Mac asking "who's there" should still learn our
                // name (it will be told we're busy when it says Hello).
                // AA_SIMULATE_FIREWALL: test switch that drops discovery
                // questions like a PC firewall would (beacons still go out).
                ControlMessage::Discover if std::env::var_os("AA_SIMULATE_FIREWALL").is_some() => {}
                ControlMessage::Discover => {
                    let port = socket.local_addr().map_or(aa_platform::discover::DEFAULT_PORT, |a| a.port());
                    // On a home network or Tailscale: name and every address,
                    // including the outside one the router door gives (so a
                    // paired computer can find us later from anywhere). From
                    // the open internet: only the key, nothing that tells a
                    // stranger who or where this computer is (0.5.8).
                    let local = aa_platform::lan::is_local_network(from.ip());
                    let name = if local { aa_platform::trust::computer_name() } else { String::new() };
                    let mut addrs: Vec<String> = if local {
                        aa_platform::lan::ipv4_interfaces().iter().map(|i| format!("{}:{port}", i.ip)).collect()
                    } else {
                        Vec::new()
                    };
                    if local {
                        if let Some(outside) = aa_platform::portmap::current().and_then(|o| o.addr()) {
                            addrs.push(outside.to_string());
                        }
                        // Its own internet (IPv6) addresses, when it has any.
                        addrs.extend(aa_platform::lan::global_ipv6().into_iter().map(|ip| format!("[{ip}]:{port}")));
                    }
                    send_control(
                        socket,
                        from,
                        &ControlMessage::Here {
                            name,
                            key: socket.public_key_hex(),
                            addrs,
                            version: aa_platform::update::current().to_owned(),
                        },
                        seq,
                    )
                    .await?;
                }
                // The viewer moved its bitrate slider.
                ControlMessage::SetMaxBitrate { kbps } if is_current_viewer => {
                    bitrate.set_max_kbps(kbps.min(stream.useful_kbps));
                    ctl.target_kbps.store(bitrate.current_kbps(), Ordering::Relaxed);
                    tracing::info!(kbps, used = kbps.min(stream.useful_kbps), "bitrate cap set");
                }
                // A full-access viewer asks what could be shared, or picks something.
                ControlMessage::ShareList if is_current_viewer && stream.full_access => {
                    let items = tokio::task::spawn_blocking(aa_platform::share::list).await.unwrap_or_default();
                    let msg = ControlMessage::Shareables { items, current: stream.now.clone() };
                    send_control(socket, from, &msg, seq).await?;
                }
                ControlMessage::SetShare { target } if is_current_viewer && stream.full_access => {
                    tracing::info!(sharing = target.label(), "the viewer chose what to share");
                    stream.viewer_choice = Some(target);
                    stream.follow_choice(ctl, 0);
                }
                ControlMessage::ShareList | ControlMessage::SetShare { .. } if is_current_viewer => {
                    tracing::info!("the viewer asked to choose what is shared, but has no full access here");
                }
                // The launcher (iPhone app): list apps and games, then send
                // their icons once the list is out. Listing reads folders
                // and runs small tools, so never on the network loop.
                ControlMessage::AppList if is_current_viewer => {
                    // Listing reads a hundred Info.plists (a second or two):
                    // on its own thread; the pages go out from the 100 ms
                    // tick once the list is in (never stalling the video).
                    let tx = stream.apps_tx.clone();
                    let _ = std::thread::Builder::new().name("aa-apps".into()).spawn(move || {
                        let _ = tx.send(aa_platform::apps::list());
                    });
                }
                ControlMessage::AppAction { id, action, show } if is_current_viewer => {
                    tracing::info!(id, ?action, show, "launcher");
                    // "Show": the phone wants only that app on its screen.
                    // Wait for it to be running (its windows exist) on a
                    // thread; the 100 ms tick then switches what is shared.
                    let show = show && stream.full_access && matches!(action, aa_core::apps::AppAction::Open);
                    let name = stream_app_name(&id);
                    let tx = stream.show_tx.clone();
                    let _ = std::thread::Builder::new().name("aa-launch".into()).spawn(move || {
                        aa_platform::apps::act(&id, action);
                        if show && aa_platform::apps::wait_running(&id, Duration::from_secs(20)) {
                            // A moment for the first window to appear.
                            std::thread::sleep(Duration::from_millis(1200));
                            let _ = tx.send(ShareTarget::App { id, name });
                        }
                    });
                }
                other => tracing::debug!(?other, "ignored control message"),
            }
        }

        // Input: one packet may hold several events; queue each for the input
        // thread (if the queue is full, the event is dropped rather than delayed).
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

        // NACK: the viewer lost a frame. Send a repair frame or a keyframe.
        Kind::Nack if is_current_viewer => {
            // Repair from a confirmed reference when both sides can (no big
            // fresh picture); otherwise a keyframe.
            if ctl.loss_recovery.load(Ordering::Relaxed) {
                ctl.recovery.store(true, Ordering::Relaxed);
            } else {
                ctl.intra_refresh.store(true, Ordering::Relaxed);
            }
        }

        // Resend: send again the exact packets the viewer names, from the last
        // second of sent packets kept in memory.
        Kind::Resend if is_current_viewer => {
            if let Some(rq) = wire::ResendRequest::decode(&packet.payload) {
                // Copy the datagrams out under the lock, send outside it.
                let again: Vec<Bytes> = recent.lock().expect("recent").lookup(&rq);
                for d in &again {
                    socket.send_to(d, from).await?;
                }
                if !again.is_empty() {
                    tracing::debug!(frame = rq.frame_id, n = again.len(), "resent");
                }
            }
        }

        // Ack: the once-a-second report. Updates bitrate, parity and frame rate.
        Kind::Ack if is_current_viewer => {
            let mut payload = packet.payload.clone();
            if let Some(rep) = ReceiverReport::decode(&mut payload) {
                if let Some(good) = rep.last_good_frame {
                    ctl.last_good.store(good, Ordering::Relaxed);
                }
                let before = bitrate.current_kbps();
                let changed = bitrate.on_report(&rep);
                let fec_before = ctl.fec_permille.load(Ordering::Relaxed);
                let fec_now = (bitrate.fec_ratio() * 1000.0) as u32;
                if fec_now != fec_before {
                    ctl.fec_permille.store(fec_now, Ordering::Relaxed);
                    if fec_now.abs_diff(fec_before) >= 50 {
                        tracing::info!(
                            parity = format_args!("{}%", fec_now / 10),
                            loss = format_args!("{:.1}%", rep.loss_ratio() * 100.0),
                            "video redundancy adjusted"
                        );
                    }
                }
                // Frame rate the connection affords at full sharpness.
                let res = ctl.out_res.lock().expect("out res").unwrap_or(stream.res);
                if let Some(fps) = stream.ladder.on_bitrate(
                    stream.codec,
                    res,
                    bitrate.current_kbps(),
                    bitrate.current_kbps() >= before,
                ) {
                    ctl.target_fps.store(fps, Ordering::Relaxed);
                    tracing::info!(fps, kbps = bitrate.current_kbps(), "frame rate adapted");
                }
                if let Some(kbps) = changed {
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

        // Ping: answer at once with a Pong (the viewer measures the round trip).
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
#[allow(clippy::too_many_arguments)] // one task, one set of channels
async fn sender_task(
    socket: Arc<Net>,
    mut frame_rx: mpsc::Receiver<EncodedFrame>,
    mut audio_rx: mpsc::Receiver<pipeline::AudioPacket>,
    video_dest: Arc<std::sync::Mutex<Option<SocketAddr>>>,
    pacer: Pacer,
    fps: u16,
    seq: Arc<SeqCounter>,
    recent: Arc<std::sync::Mutex<RecentPackets>>,
    ctl: Arc<PipelineControl>,
) {
    let mut audio_open = true;
    // Wait for either a finished video frame or a sound packet.
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
        // No viewer: drop the frame.
        let Some(dest) = *video_dest.lock().expect("dest") else { continue };
        let fec = f64::from(ctl.fec_permille.load(Ordering::Relaxed)) / 1000.0;
        let mut flags = if frame.meta.is_keyframe { wire::flags::KEYFRAME } else { 0 };
        if frame.recovery {
            flags |= wire::flags::RECOVERY;
        }
        // Cut the frame into packets, add parity, and send them all.
        let slices = match wire::slice_frame_with_fec_flagged(&frame.data, frame.meta.frame_id, flags, &seq, fec) {
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
        for s in &slices {
            if let Err(e) = socket.send_to(s, dest).await {
                tracing::warn!("send failed: {e}");
                break;
            }
        }
        // Keep the packets a little while in case the viewer asks for some again.
        recent.lock().expect("recent").remember(frame.meta.frame_id, slices);
    }
    tracing::info!("sender task exiting");
}

/// Video datagrams of the most recent frames, kept for resend requests.
/// Bounded by count and bytes; at 60 fps the count bound is about a second.
#[derive(Debug, Default)]
pub struct RecentPackets {
    frames: std::collections::VecDeque<(u32, Vec<Bytes>)>,
    bytes: usize,
}

impl RecentPackets {
    const MAX_FRAMES: usize = 64;
    /// 12 MB: a second of a 4K keyframe-heavy stream, roughly.
    const MAX_BYTES: usize = 12 * 1024 * 1024;

    // Add the newest frame's packets; forget the oldest beyond 64 frames or 12 MB.
    fn remember(&mut self, frame_id: u32, slices: Vec<Bytes>) {
        self.bytes += slices.iter().map(Bytes::len).sum::<usize>();
        self.frames.push_back((frame_id, slices));
        while self.frames.len() > Self::MAX_FRAMES || self.bytes > Self::MAX_BYTES {
            if let Some((_, old)) = self.frames.pop_front() {
                self.bytes -= old.iter().map(Bytes::len).sum::<usize>();
            }
        }
    }

    /// The datagrams a request names, as they were sent (same headers, so
    /// the viewer's reassembler takes them like any other packet).
    fn lookup(&self, rq: &wire::ResendRequest) -> Vec<Bytes> {
        let Some((_, slices)) = self.frames.iter().find(|(id, _)| *id == rq.frame_id) else { return Vec::new() };
        let mut out = Vec::with_capacity(rq.items.len());
        for &item in rq.items.iter().take(wire::ResendRequest::MAX_ITEMS) {
            if item == wire::ResendRequest::ALL_DATA {
                out.extend(
                    slices
                        .iter()
                        .filter(|d| Packet::parse(Bytes::clone(d)).is_ok_and(|p| p.header.kind == Kind::Video))
                        .cloned(),
                );
                continue;
            }
            let want_parity = item & wire::ResendRequest::PARITY_BIT != 0;
            let index = item & !wire::ResendRequest::PARITY_BIT;
            let hit = slices.iter().find(|d| {
                Packet::parse(Bytes::clone(d))
                    .is_ok_and(|p| (p.header.kind == Kind::VideoFec) == want_parity && p.header.slice_index == index)
            });
            if let Some(d) = hit {
                out.push(d.clone());
            }
        }
        out
    }
}

// Wrap a spoken message in a packet label and send it.
async fn send_control(socket: &Net, to: SocketAddr, msg: &ControlMessage, seq: &SeqCounter) -> anyhow::Result<()> {
    let payload = msg.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
    Header { kind: Kind::Control, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }
        .write(&mut out);
    out.extend_from_slice(&payload);
    socket.send_to(&out, to).await?;
    Ok(())
}

/// How fast to ever send, for this picture: twice what "full quality" costs
/// at its size, codec and highest frame rate (`aa_core::quality`).
///
/// Before 0.5.8 the bitrate controller climbed to the viewer's cap (80 Mbps)
/// whatever the picture needed: a 1080p 60 fps HEVC stream, sharp at about
/// 14 Mbps, went past 100 Mbps with parity in Elden Ring. On Wi-Fi each
/// frame then left as a burst of ~140 packets, the radio queued and dropped
/// them, and the picture froze while the controller backed off and climbed
/// again. Twice the sharp rate keeps fast motion clean and leaves the rest
/// of the air for everything else (a video playing on the same Wi-Fi).
fn useful_max_kbps(n: &aa_core::capability::Negotiated) -> u32 {
    aa_core::quality::sharp_kbps(n.codec, n.resolution, n.fps).saturating_mul(2)
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

/// Put the host's speakers back how we found them and take the screen mask
/// off. Called on every way a viewer can leave, so a dropped Wi-Fi link
/// never leaves the PC muted or its monitor black.
fn restore_speakers(speaker: &mut Option<Box<dyn aa_platform::audio::SpeakerControl>>) {
    aa_platform::screen_mask(false);
    if let Some(s) = speaker {
        if let Err(e) = s.restore() {
            tracing::warn!("restoring host speakers failed: {e}");
        }
    }
}

/// What this session shows and how fast: the share choice (the person here,
/// or a full-access viewer), the frame-rate ladder, and the codec and size
/// the ladder prices.
#[derive(Debug)]
struct Stream {
    /// Being shown now.
    now: ShareTarget,
    /// The local person's last choice (from `share.json`).
    local: ShareTarget,
    /// A full-access viewer's pick; cleared when the person here changes
    /// their choice (the newest decision wins) or the viewer leaves.
    viewer_choice: Option<ShareTarget>,
    full_access: bool,
    /// Why the picture is empty because of sharing ("the shared app was closed").
    share_status: Option<&'static str>,
    ladder: FpsLadder,
    /// The most data a second worth sending for this viewer's picture.
    useful_kbps: u32,
    /// Something here blocking the viewer's input (see `input_blocker`).
    input_warning: Option<&'static str>,
    codec: aa_core::video::Codec,
    res: aa_core::video::Resolution,
    /// Shared with a watcher thread that checks every few seconds whether
    /// the shared app is still open (asking the system can take a moment,
    /// so never on the network loop).
    watched: Arc<std::sync::Mutex<ShareTarget>>,
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// Launcher icons being gathered on another thread (slow: one image
    /// conversion per app the first time) land here; the network loop
    /// hands them to the clipboard transfer (`icons_rx`).
    icons_tx: std::sync::mpsc::Sender<aa_core::apps::IconPack>,
    icons_rx: std::sync::mpsc::Receiver<aa_core::apps::IconPack>,
    /// The launcher list, gathered on another thread the same way.
    apps_tx: std::sync::mpsc::Sender<Vec<aa_core::apps::AppEntry>>,
    apps_rx: std::sync::mpsc::Receiver<Vec<aa_core::apps::AppEntry>>,
    /// An app the phone opened and wants shown alone, once it is running.
    show_tx: std::sync::mpsc::Sender<ShareTarget>,
    show_rx: std::sync::mpsc::Receiver<ShareTarget>,
    /// The viewer asked to hear about focused text fields (the phone);
    /// a watcher thread asks the system only while this is set.
    text_wanted: Arc<std::sync::atomic::AtomicBool>,
    /// What the watcher last saw.
    text_field: Arc<std::sync::Mutex<Option<TextField>>>,
    /// What the viewer was last told, and when (repeated every second
    /// while a field is focused: control messages can be lost).
    text_told: (Option<TextField>, Instant),
    /// Phone mode is on for this viewer (to turn it off when it leaves).
    phone_display: bool,
}

/// A focused text field: where (fractions of the screen: x, y, width,
/// height) and whether it is a password field.
type TextField = ([f32; 4], bool);

/// A display name for a launcher id, for the share target's label (the
/// phone shows it in its corner); the bundle id's last part will do.
fn stream_app_name(id: &str) -> String {
    id.rsplit('.').next().unwrap_or(id).to_owned()
}

impl Stream {
    fn new(fps: u16) -> Self {
        let (icons_tx, icons_rx) = std::sync::mpsc::channel();
        let (apps_tx, apps_rx) = std::sync::mpsc::channel();
        let (show_tx, show_rx) = std::sync::mpsc::channel();
        let watched = Arc::new(std::sync::Mutex::new(ShareTarget::Screen));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let text_wanted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let text_field = Arc::new(std::sync::Mutex::new(None));
        {
            // Which text field has the focus, ~7 times a second, only while a
            // phone is watching. Asking can take up to 100 ms when an app is
            // busy, so never on the network loop.
            let (wanted, field) = (Arc::clone(&text_wanted), Arc::clone(&text_field));
            let _ = std::thread::Builder::new().name("aa-text-focus".into()).spawn(move || loop {
                std::thread::sleep(Duration::from_millis(150));
                let now = if wanted.load(Ordering::Relaxed) { aa_platform::focused_text_field() } else { None };
                if let Ok(mut f) = field.lock() {
                    *f = now;
                }
            });
        }
        {
            let (watched, closed) = (Arc::clone(&watched), Arc::clone(&closed));
            let _ = std::thread::Builder::new().name("aa-share-watch".into()).spawn(move || loop {
                std::thread::sleep(Duration::from_secs(3));
                let t = watched.lock().map(|t| t.clone()).unwrap_or_default();
                let gone = !t.is_screen() && !aa_platform::share::still_open(&t);
                closed.store(gone, Ordering::Relaxed);
            });
        }
        Self {
            watched,
            closed,
            icons_tx,
            icons_rx,
            apps_tx,
            apps_rx,
            show_tx,
            show_rx,
            text_wanted,
            text_field,
            text_told: (None, Instant::now()),
            phone_display: false,
            now: ShareTarget::Screen,
            local: ShareTarget::Screen,
            viewer_choice: None,
            full_access: false,
            share_status: None,
            ladder: FpsLadder::new(fps, false),
            useful_kbps: u32::MAX,
            input_warning: None,
            codec: aa_core::video::Codec::Hevc,
            res: aa_core::video::Resolution::new(1920, 1080),
        }
    }

    /// A viewer was accepted: set the frame rate, size and sharing up for it.
    fn start(
        &mut self,
        ctl: &PipelineControl,
        n: &aa_core::capability::Negotiated,
        host: &aa_core::capability::Capabilities,
        full_access: bool,
    ) {
        self.full_access = full_access;
        self.viewer_choice = None;
        self.codec = n.codec;
        self.res = n.resolution;
        self.ladder = FpsLadder::new(n.fps, n.fixed_fps);
        ctl.target_fps.store(self.ladder.current(), Ordering::Relaxed);
        ctl.loss_recovery.store(n.loss_recovery, Ordering::Relaxed);
        ctl.last_good.store(u32::MAX, Ordering::Relaxed);
        // Smaller than our screen: the viewer chose a lower Resolution.
        let smaller =
            n.resolution.width < host.max_resolution.width || n.resolution.height < host.max_resolution.height;
        *ctl.max_res.lock().expect("max res") = smaller.then_some(n.resolution);
        ctl.max_res_changed.store(true, Ordering::Relaxed);
        tracing::info!(
            max_fps = n.fps,
            starting_fps = self.ladder.current(),
            fixed = n.fixed_fps,
            res = ?n.resolution,
            codec = ?n.codec,
            repairs = n.loss_recovery,
            "stream set up"
        );
        // Control limits depend on who this is: apply again.
        self.local = aa_platform::trust::share_choice();
        self.now = self.local.clone();
        self.apply(ctl);
    }

    fn viewer_left(&mut self, ctl: &PipelineControl) {
        if self.viewer_choice.take().is_some() {
            self.follow_choice(ctl, 0);
        }
        self.full_access = false;
        if std::mem::take(&mut self.phone_display) {
            let _ = std::thread::Builder::new().name("aa-phone-display".into()).spawn(aa_platform::phone_display_end);
        }
        self.text_wanted.store(false, Ordering::Relaxed);
        self.text_told = (None, Instant::now());
    }

    /// Re-read the local choice; apply whatever should be shown now.
    /// `tick` (seconds) spaces out the "is that app still open?" check.
    fn follow_choice(&mut self, ctl: &PipelineControl, _tick: u64) {
        // The person at this computer changed their choice: that wins.
        let local = aa_platform::trust::share_choice();
        if local != self.local {
            self.local = local;
            self.viewer_choice = None;
        }
        let want = self.viewer_choice.clone().unwrap_or_else(|| self.local.clone());
        if want != self.now {
            self.now = want;
            self.apply(ctl);
        }
        // The shared app was closed: tell the viewer why the picture is empty.
        let gone = !self.now.is_screen() && self.closed.load(Ordering::Relaxed);
        self.share_status = gone.then_some("The shared app or window was closed on the other computer");
    }

    fn apply(&mut self, ctl: &PipelineControl) {
        tracing::info!(sharing = self.now.label(), full_access = self.full_access, "sharing");
        // Tell the capture, sound and input threads what is being shared, and
        // whether input is limited to that app (no full access).
        *ctl.share.lock().expect("share") = Some(self.now.clone());
        *ctl.audio_share.lock().expect("audio share") = Some(self.now.clone());
        let limit = (!self.now.is_screen() && !self.full_access).then(|| self.now.clone());
        *ctl.input_limit.lock().expect("limit") = Some(limit);
        if let Ok(mut w) = self.watched.lock() {
            *w = self.now.clone();
        }
        self.closed.store(false, Ordering::Relaxed);
        self.share_status = None;
    }
}
