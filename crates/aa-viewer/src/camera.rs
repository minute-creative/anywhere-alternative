//! This computer's camera, sent to the other one while a call there uses
//! "Anywhere Camera". Runs on its own thread (cameras block while waiting for
//! the next picture); compressed pictures come back over a channel. Dropping
//! the handle stops the thread and closes the camera, so its light goes off.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

/// What the camera thread reports.
#[derive(Debug)]
pub enum CameraEvent {
    /// The camera opened; this is its name.
    Started(String),
    /// One compressed picture.
    Frame { data: Bytes, keyframe: bool },
    /// The camera could not be used; why, in plain words.
    Failed(String),
}

#[derive(Debug)]
pub struct CameraSender {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
}

impl CameraSender {
    /// Start the camera whose name contains `prefer` ("auto": the first),
    /// or the moving test picture when `test` is set.
    pub fn start(prefer: String, test: bool) -> (Self, mpsc::Receiver<CameraEvent>) {
        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::channel(8);
        let (flag, key) = (Arc::clone(&stop), Arc::clone(&keyframe));
        let _ = std::thread::Builder::new().name("aa-camera".into()).spawn(move || {
            let source = if test {
                Ok(Box::new(aa_platform::camera::TestPattern::default()) as Box<dyn aa_platform::camera::CameraSource>)
            } else {
                aa_platform::camera::open(Some(&prefer))
            };
            let mut source = match source {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("camera unavailable: {e}");
                    let _ = tx.blocking_send(CameraEvent::Failed(e.to_string()));
                    return;
                }
            };
            let mut enc = match aa_platform::camera::CameraEncoder::new() {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx.blocking_send(CameraEvent::Failed(e.to_string()));
                    return;
                }
            };
            let _ = tx.blocking_send(CameraEvent::Started(source.name()));
            let mut failures = 0u32;
            while !flag.load(Ordering::Relaxed) {
                let frame = match source.next_frame(Duration::from_millis(200)) {
                    Ok(Some(f)) => f,
                    Ok(None) => continue,
                    Err(e) => {
                        failures += 1;
                        if failures >= 3 {
                            let _ = tx.blocking_send(CameraEvent::Failed(e.to_string()));
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                };
                failures = 0;
                let want_key = key.swap(false, Ordering::Relaxed);
                match enc.encode(&frame, want_key) {
                    Ok((data, keyframe)) if !data.is_empty() => {
                        let ev = CameraEvent::Frame { data: Bytes::from(data), keyframe };
                        // Full channel: the network is behind; drop this one.
                        if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(ev) {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!("camera encode: {e}"),
                }
            }
            tracing::info!("camera thread finished");
        });
        (Self { stop, keyframe }, rx)
    }

    /// The other side lost a picture: make the next one a keyframe.
    pub fn keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }
}

impl Drop for CameraSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
