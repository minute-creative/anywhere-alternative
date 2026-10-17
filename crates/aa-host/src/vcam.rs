//! Windows: "Anywhere Camera" for every app (Windows 11).
//!
//! Adds the camera to Windows at start, then waits for an app to use it.
//! While one does, the hub asks the watching computer for its camera, and
//! each picture is decoded here (H.264 → NV12) and handed to the camera
//! DLL through shared memory (see `aa_platform::windows::vcam`).

use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::time::Duration;

use aa_platform::windows::vcam::{create_camera, Feeder};
use aa_platform::VideoDecoder;

use crate::calls::CallHub;

pub fn start(hub: CallHub) {
    let _ = std::thread::Builder::new().name("aa-vcam".into()).spawn(move || {
        if let Err(e) = create_camera() {
            tracing::warn!("{e}. Calls in a browser can still use Anywhere Camera through the add-on.");
            return;
        }
        let mut feeder = Feeder::default();
        loop {
            if !feeder.wanted() {
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
            tracing::info!("an app is using Anywhere Camera");
            let (tx, rx) = sync_channel(32);
            hub.set_vcam_frames(Some(tx));
            let mut decoder = match aa_platform::sw::SwDecoder::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("camera decoder: {e}");
                    hub.set_vcam_frames(None);
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            let mut waiting_for_key = true;
            let mut frame_id = 0u32;
            let mut last_check = std::time::Instant::now();
            loop {
                if last_check.elapsed() > Duration::from_millis(300) {
                    last_check = std::time::Instant::now();
                    if !feeder.wanted() {
                        break;
                    }
                }
                let (data, key) = match rx.recv_timeout(Duration::from_millis(300)) {
                    Ok(f) => f,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                if waiting_for_key && !key {
                    continue;
                }
                waiting_for_key = false;
                frame_id = frame_id.wrapping_add(1);
                match decoder.decode(frame_id, &data) {
                    Ok(Some(pic)) => {
                        if let aa_platform::FrameBuffer::Cpu(nv12) = &pic.buffer {
                            let (w, h) = (pic.resolution.width as usize, pic.resolution.height as usize);
                            if (w, h) == (aa_core::calls::vcam::WIDTH, aa_core::calls::vcam::HEIGHT) {
                                feeder.write(nv12);
                            } else {
                                feeder.write(&aa_platform::camera::fit_nv12(nv12, w, h, w).nv12);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!("camera picture didn't decode: {e}");
                        waiting_for_key = true;
                        hub.keyframe.notify_one();
                    }
                }
            }
            hub.set_vcam_frames(None);
            tracing::info!("no app is using Anywhere Camera now");
        }
    });
}
