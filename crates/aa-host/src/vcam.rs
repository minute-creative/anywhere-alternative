//! Windows: "Anywhere Camera" for every app (Windows 11).
//!
//! Adds the camera to Windows at start, then waits for an app to use it.
//! While one does, the hub asks the watching computer for its camera, and
//! each picture is decoded here (H.264 → NV12) and handed to the camera
//! DLL through shared memory (see `aa_platform::windows::vcam`).

// Plain summary: on Windows, apps like Zoom or Teams can pick "Anywhere
// Camera". This thread feeds that camera with the other computer's webcam
// pictures, but only while some app is actually using it.
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::time::Duration;

use aa_platform::windows::vcam::{create_camera, Feeder};
use aa_platform::VideoDecoder;

use crate::calls::CallHub;

pub fn start(hub: CallHub) {
    // Its own thread (not async) because decoding is steady CPU work.
    let _ = std::thread::Builder::new().name("aa-vcam".into()).spawn(move || {
        // Register the camera with Windows. If that fails (older Windows), the
        // browser add-on route still works.
        if let Err(e) = create_camera() {
            tracing::warn!("{e}. Calls in a browser can still use Anywhere Camera through the add-on.");
            return;
        }
        let mut feeder = Feeder::default();
        // Outer loop: every 300 ms, check whether any app has opened the camera.
        loop {
            if !feeder.wanted() {
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
            tracing::info!("an app is using Anywhere Camera");
            // An app is using it: open a pipe (up to 32 pictures) and tell the hub to
            // send pictures into it; that also asks the viewer to switch its camera on.
            let (tx, rx) = sync_channel(32);
            hub.set_vcam_frames(Some(tx));
            // Software H.264 decoder. If it can't start, give up this attempt and try
            // again in 5 seconds.
            let mut decoder = match aa_platform::sw::SwDecoder::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("camera decoder: {e}");
                    hub.set_vcam_frames(None);
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            // Nothing can be decoded until a keyframe arrives, so skip pictures until then.
            let mut waiting_for_key = true;
            let mut frame_id = 0u32;
            let mut last_check = std::time::Instant::now();
            // Inner loop: decode pictures while the app still uses the camera.
            loop {
                // Every 300 ms, check whether the app has closed the camera.
                if last_check.elapsed() > Duration::from_millis(300) {
                    last_check = std::time::Instant::now();
                    if !feeder.wanted() {
                        break;
                    }
                }
                // Wait up to 300 ms for a picture; on timeout go round again so the check
                // above still runs.
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
                        // Pictures already the camera's size go straight in; others are resized first.
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
                        // A bad picture: wait for the next keyframe and ask the viewer for one now.
                        tracing::debug!("camera picture didn't decode: {e}");
                        waiting_for_key = true;
                        hub.keyframe.notify_one();
                    }
                }
            }
            // The app stopped using the camera: stop sending pictures (the hub then
            // tells the viewer it can switch its camera off).
            hub.set_vcam_frames(None);
            tracing::info!("no app is using Anywhere Camera now");
        }
    });
}
