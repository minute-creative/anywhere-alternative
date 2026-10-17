//! "Anywhere Camera" and "Anywhere Microphone" on this computer.
//!
//! The watching computer sends its camera (H.264) and microphone (Opus,
//! decoded here) only while something here uses them. That "something" is
//! a browser tab with our add-on (connected over a WebSocket on 127.0.0.1)
//! or, on Windows, the virtual camera. [`CallHub`] keeps track of who
//! wants what, tells the session when that changes (so it can ask the
//! viewer to switch its camera and mic on or off), and hands pictures and
//! sound to everyone listening.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aa_core::calls::{encode_media, origin_allowed, BridgeMsg, BRIDGE_PORT, MSG_AUDIO, MSG_VIDEO};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch, Notify};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;

/// Camera pictures (H.264, keyframe?) on their way to the virtual camera.
pub type FrameSender = std::sync::mpsc::SyncSender<(Bytes, bool)>;

/// Numbers browser connections, for the log.
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

/// What the watching computer should send right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Wanted {
    pub camera: bool,
    pub mic: bool,
}

#[derive(Debug)]
struct Client {
    tx: mpsc::Sender<Message>,
    video: bool,
    audio: bool,
}

#[derive(Debug, Default)]
struct State {
    clients: HashMap<u64, Client>,
    /// The Windows virtual camera has an app watching it.
    vcam: bool,
    viewer: Option<String>,
    camera: Option<String>,
    problem: Option<String>,
}

impl State {
    fn wanted(&self) -> Wanted {
        Wanted {
            camera: self.vcam || self.clients.values().any(|c| c.video),
            mic: self.clients.values().any(|c| c.audio),
        }
    }

    fn status(&self) -> Message {
        let m = BridgeMsg::Status {
            viewer: self.viewer.clone(),
            camera: self.camera.clone(),
            problem: self.problem.clone(),
        };
        Message::Text(serde_json::to_string(&m).unwrap_or_default().into())
    }
}

/// Shared between the session, the WebSocket server and (on Windows) the
/// virtual camera.
#[derive(Debug, Clone)]
pub struct CallHub {
    state: Arc<Mutex<State>>,
    wanted_tx: Arc<watch::Sender<Wanted>>,
    /// Someone new is watching: ask the viewer for a keyframe.
    pub keyframe: Arc<Notify>,
    started: Instant,
    /// Where camera pictures go for the Windows virtual camera, while an
    /// app uses it.
    vcam_frames: Arc<Mutex<Option<FrameSender>>>,
}

impl CallHub {
    pub fn new() -> (Self, watch::Receiver<Wanted>) {
        let (tx, rx) = watch::channel(Wanted::default());
        let hub = Self {
            state: Arc::default(),
            wanted_tx: Arc::new(tx),
            keyframe: Arc::new(Notify::new()),
            started: Instant::now(),
            vcam_frames: Arc::default(),
        };
        (hub, rx)
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        let (wanted, status, clients) = {
            let mut s = self.state.lock().expect("calls");
            f(&mut s);
            (s.wanted(), s.status(), s.clients.values().map(|c| c.tx.clone()).collect::<Vec<_>>())
        };
        self.wanted_tx.send_if_modified(|w| {
            let changed = *w != wanted;
            *w = wanted;
            changed
        });
        for tx in clients {
            let _ = tx.try_send(status.clone());
        }
    }

    pub fn wanted(&self) -> Wanted {
        *self.wanted_tx.borrow()
    }

    /// The watching computer changed (`None`: nobody watches now).
    pub fn set_viewer(&self, name: Option<String>) {
        self.update(|s| {
            if name.is_none() {
                s.camera = None;
                s.problem = None;
            }
            s.viewer = name;
        });
    }

    /// The viewer told us about its camera.
    pub fn set_camera(&self, name: Option<String>, problem: Option<String>) {
        self.update(|s| {
            s.camera = name;
            s.problem = problem;
        });
    }

    /// The Windows virtual camera started (pictures go to `frames`) or
    /// stopped (`None`) being used by an app.
    #[allow(dead_code)] // used on Windows only
    pub fn set_vcam_frames(&self, frames: Option<FrameSender>) {
        let on = frames.is_some();
        if let Ok(mut f) = self.vcam_frames.lock() {
            *f = frames;
        }
        self.set_vcam(on);
    }

    fn set_vcam(&self, on: bool) {
        if on && !self.state.lock().expect("calls").vcam {
            self.keyframe.notify_one();
        }
        self.update(|s| s.vcam = on);
    }

    fn ts(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    /// A camera picture arrived from the viewer.
    pub fn frame(&self, data: &Bytes, keyframe: bool) {
        if let Some(tx) = self.vcam_frames.lock().ok().and_then(|f| f.clone()) {
            let _ = tx.try_send((data.clone(), keyframe));
        }
        let msg = Message::Binary(Bytes::from(encode_media(MSG_VIDEO, keyframe, self.ts(), data)));
        let s = self.state.lock().expect("calls");
        for c in s.clients.values().filter(|c| c.video) {
            // Full: that tab is behind; it will catch up at the next keyframe.
            let _ = c.tx.try_send(msg.clone());
        }
    }

    /// 10 ms of the viewer's microphone (48 kHz stereo).
    pub fn sound(&self, pcm: &[i16]) {
        let s = self.state.lock().expect("calls");
        if !s.clients.values().any(|c| c.audio) {
            return;
        }
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for v in pcm {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let msg = Message::Binary(Bytes::from(encode_media(MSG_AUDIO, false, self.ts(), &bytes)));
        for c in s.clients.values().filter(|c| c.audio) {
            let _ = c.tx.try_send(msg.clone());
        }
    }

    /// Start the WebSocket server for the browser add-on (127.0.0.1 only).
    pub fn serve(&self) {
        let hub = self.clone();
        tokio::spawn(async move {
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, BRIDGE_PORT));
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(
                        "browser camera link not available ({addr}: {e}); another copy of Anywhere may be running"
                    );
                    return;
                }
            };
            tracing::info!(%addr, "browser camera link ready");
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
                let hub = hub.clone();
                tokio::spawn(async move {
                    if let Err(e) = hub.client(stream, id).await {
                        tracing::debug!("browser link {id} ended: {e}");
                    }
                    hub.update(|s| {
                        s.clients.remove(&id);
                    });
                });
            }
        });
    }

    async fn client(&self, stream: tokio::net::TcpStream, id: u64) -> anyhow::Result<()> {
        let _ = stream.set_nodelay(true);
        #[allow(clippy::result_large_err)] // the type tungstenite asks for
        let check = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            let origin = req.headers().get("origin").and_then(|o| o.to_str().ok()).unwrap_or("");
            if origin_allowed(origin) {
                Ok(resp)
            } else {
                tracing::warn!(origin, "turned away a browser connection that isn't the Anywhere add-on");
                let mut r = ErrorResponse::new(Some("not the Anywhere add-on".into()));
                *r.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN;
                Err(r)
            }
        };
        let ws = tokio_tungstenite::accept_hdr_async(stream, check).await?;
        let (mut sink, mut source) = ws.split();
        // Two seconds of video at most queued for a slow tab.
        let (tx, mut rx) = mpsc::channel::<Message>(64);
        self.update(|s| {
            s.clients.insert(id, Client { tx: tx.clone(), video: false, audio: false });
        });
        tracing::info!(id, "browser add-on connected");
        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
        });
        while let Some(msg) = source.next().await {
            let Message::Text(text) = msg? else { continue };
            match serde_json::from_str::<BridgeMsg>(text.as_str()) {
                Ok(BridgeMsg::Want { video, audio }) => {
                    let newly = {
                        let s = self.state.lock().expect("calls");
                        s.clients.get(&id).is_some_and(|c| video && !c.video)
                    };
                    self.update(|s| {
                        if let Some(c) = s.clients.get_mut(&id) {
                            c.video = video;
                            c.audio = audio;
                        }
                    });
                    tracing::info!(id, video, audio, "a browser tab wants the camera / microphone");
                    if newly {
                        self.keyframe.notify_one();
                    }
                }
                Ok(BridgeMsg::Keyframe) => self.keyframe.notify_one(),
                _ => {}
            }
        }
        writer.abort();
        tracing::info!(id, "browser add-on disconnected");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wants_follow_clients_and_vcam() {
        let (hub, mut rx) = CallHub::new();
        assert_eq!(hub.wanted(), Wanted::default());
        let (tx, mut client_rx) = mpsc::channel(8);
        hub.update(|s| {
            s.clients.insert(1, Client { tx, video: true, audio: false });
        });
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), Wanted { camera: true, mic: false });
        // The client got a status message.
        assert!(matches!(client_rx.try_recv(), Ok(Message::Text(_))));
        hub.frame(&Bytes::from_static(b"\0\0\0\x01x"), true);
        let Ok(Message::Binary(b)) = client_rx.try_recv() else { panic!("no picture") };
        assert_eq!(b[0], MSG_VIDEO);
        hub.update(|s| {
            s.clients.clear();
        });
        assert_eq!(hub.wanted(), Wanted::default());
        hub.set_vcam(true);
        assert!(hub.wanted().camera);
    }
}
