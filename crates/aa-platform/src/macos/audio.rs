//! The Mac's system sound, through `ScreenCaptureKit` (macOS 13+).
//!
//! macOS has no "loopback" device like Windows; since macOS 13 the screen
//! capture service can also deliver everything the Mac plays (minus our
//! own process, so the viewer's microphone isn't echoed back). We run a
//! separate, audio-only stream so sound never waits on video or vice versa.
//! Covered by the same Screen Recording permission as the picture.

#![allow(unsafe_code, clippy::pedantic)]

use std::collections::VecDeque;
use std::ptr::NonNull;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{SCContentFilter, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType};

use super::capture::{check_permission, main_display, ns_err, wait};
use crate::audio::{AudioCapture, FRAME_LEN_I16};
use crate::{PlatformError, Result};

/// Stereo float samples (interleaved) from one callback.
type Chunk = Vec<f32>;

struct AudioIvars {
    chunks: SyncSender<Chunk>,
}

/// kAudioFormatFlagIsFloat / kAudioFormatFlagIsNonInterleaved.
const FLAG_FLOAT: u32 = 1;
const FLAG_NON_INTERLEAVED: u32 = 0x20;

/// Turn one audio sample buffer into interleaved stereo f32.
fn to_stereo(sample: &CMSampleBuffer) -> Option<Chunk> {
    // SAFETY: reads on a sample buffer valid for this callback.
    unsafe {
        let fmt = sample.format_description()?;
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&fmt).as_ref()?;
        if asbd.mFormatFlags & FLAG_FLOAT == 0 || asbd.mBitsPerChannel != 32 {
            return None; // ScreenCaptureKit always sends float; anything else is unexpected
        }
        let frames = usize::try_from(sample.num_samples()).ok()?;
        let channels = asbd.mChannelsPerFrame.max(1) as usize;
        let block = sample.data_buffer()?;
        let len = block.data_length();
        if len < frames * channels * 4 {
            return None;
        }
        let mut raw = vec![0f32; len / 4];
        if block.copy_data_bytes(0, raw.len() * 4, NonNull::new(raw.as_mut_ptr().cast())?) != 0 {
            return None;
        }
        let planar = asbd.mFormatFlags & FLAG_NON_INTERLEAVED != 0;
        let at = |frame: usize, ch: usize| -> f32 {
            let ch = ch.min(channels - 1);
            if planar {
                raw[ch * frames + frame]
            } else {
                raw[frame * channels + ch]
            }
        };
        Some((0..frames).flat_map(|f| [at(f, 0), at(f, 1)]).collect())
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and we don't impl Drop.
    #[unsafe(super(NSObject))]
    #[name = "AAAudioOutput"]
    #[ivars = AudioIvars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Audio {
                return;
            }
            if let Some(chunk) = to_stereo(sample) {
                let _ = self.ivars().chunks.try_send(chunk);
            }
        }
    }
);

impl AudioOutput {
    fn new(chunks: SyncSender<Chunk>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(AudioIvars { chunks });
        // SAFETY: NSObject's plain init.
        unsafe { msg_send![super(this), init] }
    }
}

pub struct SckAudio {
    stream: Retained<SCStream>,
    _output: Retained<AudioOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    chunks: Receiver<Chunk>,
    /// Interleaved stereo waiting to be cut into 10 ms frames.
    pending: VecDeque<f32>,
}

// SAFETY: as for the screen capture: thread-safe Objective-C objects, used
// by one audio thread.
unsafe impl Send for SckAudio {}

impl std::fmt::Debug for SckAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SckAudio").finish_non_exhaustive()
    }
}

impl SckAudio {
    pub fn new() -> Result<Self> {
        check_permission()?;
        let display = main_display()?;
        // SAFETY: standard ScreenCaptureKit setup on live objects.
        let stream = unsafe {
            let filter =
                SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new());
            let config = SCStreamConfiguration::new();
            config.setCapturesAudio(true);
            config.setSampleRate(48_000);
            config.setChannelCount(2);
            config.setExcludesCurrentProcessAudio(true);
            // Video is required by the API; make it as cheap as possible.
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None)
        };
        let (tx, chunks) = sync_channel(64);
        let output = AudioOutput::new(tx);
        let queue = DispatchQueue::new("aa.audio-capture", None);
        // SAFETY: output and queue are kept alive in Self.
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Audio,
                Some(&queue),
            )
        }
        .map_err(|e| {
            PlatformError::Backend(anyhow::anyhow!("adding the audio output: {}", e.localizedDescription()))
        })?;
        let started = wait(|tx| {
            let block = RcBlock::new(move |error: *mut NSError| {
                let _ = tx.send(if error.is_null() { Ok(()) } else { Err(ns_err("starting audio capture", error)) });
            });
            // SAFETY: the block is copied by the call.
            unsafe { stream.startCaptureWithCompletionHandler(Some(&block)) };
        })
        .ok_or_else(|| PlatformError::Unavailable("audio capture did not start".into()))?;
        started?;
        tracing::info!("system audio: ScreenCaptureKit (48 kHz stereo)");
        Ok(Self { stream, _output: output, _queue: queue, chunks, pending: VecDeque::new() })
    }
}

impl Drop for SckAudio {
    fn drop(&mut self) {
        // SAFETY: stopping our own stream.
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

impl AudioCapture for SckAudio {
    fn next_frame(&mut self, pcm: &mut [i16]) -> Result<bool> {
        while self.pending.len() < FRAME_LEN_I16 {
            match self.chunks.recv_timeout(Duration::from_millis(100)) {
                Ok(c) => self.pending.extend(c),
                // Nothing playing: ScreenCaptureKit sends nothing at all.
                Err(RecvTimeoutError::Timeout) => return Ok(false),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(PlatformError::DeviceLost("audio capture stopped".into()));
                }
            }
        }
        let mut loud = false;
        for (out, s) in pcm.iter_mut().zip(self.pending.drain(..FRAME_LEN_I16)) {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            loud |= v.unsigned_abs() > 2;
            *out = v;
        }
        // Too much backlog (the sender stalled): drop to keep sound in sync.
        if self.pending.len() > FRAME_LEN_I16 * 10 {
            let excess = (self.pending.len() - FRAME_LEN_I16 * 2) & !1;
            self.pending.drain(..excess);
        }
        Ok(loud)
    }
}
