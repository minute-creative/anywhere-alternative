//! The system clipboard on each OS, plus the small worker thread that
//! watches it.
//!
//! How changes are noticed: both OSes keep a counter that goes up every
//! time anything is copied (`GetClipboardSequenceNumber` on Windows,
//! `NSPasteboard.changeCount` on macOS). We poll that number four times a
//! second, which costs nothing, and only read the clipboard when it moves.
//! Reading a big screenshot on every poll would waste real CPU.
//!
//! Echo prevention: after we paste something that came from the other
//! machine, the counter moves too. We note the counter right after our own
//! write and skip that change, or the item would bounce back and forth.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

pub use aa_core::clipboard::ClipItem;

use crate::Result;

/// One machine's clipboard.
pub trait SystemClipboard: Send {
    /// A number that changes whenever the clipboard content changes.
    fn change_count(&mut self) -> u64;
    /// Current content: text if there is any, else an image, else `None`.
    fn read(&mut self) -> Option<ClipItem>;
    fn write(&mut self, item: &ClipItem) -> Result<()>;
}

/// In-memory clipboard for tests and the mock pipeline. Clones share state,
/// so a test can hold one end and hand the other to the worker.
#[derive(Debug, Clone, Default)]
pub struct MemoryClipboard(std::sync::Arc<std::sync::Mutex<(u64, Option<ClipItem>)>>);

impl MemoryClipboard {
    /// Simulate the user copying something.
    pub fn copy(&self, item: ClipItem) {
        let mut g = self.0.lock().expect("clipboard");
        g.0 += 1;
        g.1 = Some(item);
    }

    pub fn current(&self) -> Option<ClipItem> {
        self.0.lock().expect("clipboard").1.clone()
    }
}

impl SystemClipboard for MemoryClipboard {
    fn change_count(&mut self) -> u64 {
        self.0.lock().expect("clipboard").0
    }
    fn read(&mut self) -> Option<ClipItem> {
        self.current()
    }
    fn write(&mut self, item: &ClipItem) -> Result<()> {
        self.copy(item.clone());
        Ok(())
    }
}

/// Encode raw RGBA pixels as PNG (fast compression: clipboard latency
/// matters more than a few extra KB).
pub fn rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header().ok()?;
        w.write_image_data(rgba).ok()?;
    }
    Some(out)
}

/// Decode a PNG to (width, height, RGBA pixels).
pub fn png_to_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None, // EXPAND turns this into RGB(A)
    };
    Some((w, h, rgba))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod os {
    use super::{png_to_rgba, rgba_to_png, ClipItem, SystemClipboard};
    use crate::{PlatformError, Result};

    pub struct OsClipboard {
        cb: arboard::Clipboard,
    }

    impl OsClipboard {
        pub fn new() -> Result<Self> {
            let cb =
                arboard::Clipboard::new().map_err(|e| PlatformError::Unavailable(format!("system clipboard: {e}")))?;
            Ok(Self { cb })
        }
    }

    impl SystemClipboard for OsClipboard {
        #[allow(unsafe_code, unused_unsafe)]
        fn change_count(&mut self) -> u64 {
            #[cfg(target_os = "windows")]
            // SAFETY: argument-less Win32 call.
            let n = u64::from(unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() });
            #[cfg(target_os = "macos")]
            // SAFETY: reading a counter on the shared general pasteboard.
            let n = unsafe { objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() } as u64;
            n
        }

        fn read(&mut self) -> Option<ClipItem> {
            // Text first: apps like Word put both text and a picture of
            // the text on the clipboard, and the text is what you want.
            if let Ok(t) = self.cb.get_text() {
                if !t.is_empty() {
                    return Some(ClipItem::Text(t));
                }
            }
            let img = self.cb.get_image().ok()?;
            let png = rgba_to_png(img.width as u32, img.height as u32, &img.bytes)?;
            Some(ClipItem::Png(png.into()))
        }

        fn write(&mut self, item: &ClipItem) -> Result<()> {
            let r = match item {
                ClipItem::Text(t) => self.cb.set_text(t.clone()),
                ClipItem::Png(p) => {
                    let (w, h, rgba) = png_to_rgba(p)
                        .ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("clipboard image is not a valid PNG")))?;
                    self.cb.set_image(arboard::ImageData { width: w as usize, height: h as usize, bytes: rgba.into() })
                }
            };
            r.map_err(|e| PlatformError::Backend(anyhow::anyhow!("clipboard write: {e}")))
        }
    }
}

/// This machine's real clipboard, or `None` where there isn't one (Linux
/// CI) or it can't be opened.
pub fn system() -> Option<Box<dyn SystemClipboard>> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    match os::OsClipboard::new() {
        Ok(c) => return Some(Box::new(c)),
        Err(e) => tracing::warn!("clipboard sharing off: {e}"),
    }
    None
}

/// Both ends of a running clipboard worker.
#[derive(Debug)]
pub struct ClipboardLink {
    /// Things the user copied on this machine, to send to the other.
    pub copied_here: Receiver<ClipItem>,
    /// Things that arrived from the other machine, to paste here.
    pub paste_here: Sender<ClipItem>,
}

/// Start the watcher thread. It stops when `paste_here` is dropped.
pub fn spawn_worker(mut cb: Box<dyn SystemClipboard>) -> std::io::Result<ClipboardLink> {
    let (out_tx, out_rx) = mpsc::channel();
    let (in_tx, in_rx) = mpsc::channel::<ClipItem>();
    std::thread::Builder::new().name("aa-clipboard".into()).spawn(move || {
        // Start from whatever is on the clipboard now: only *new* copies
        // are shared, so connecting doesn't paste something old at the other end.
        let mut seen = cb.change_count();
        let mut last_from_remote: Option<ClipItem> = None;
        loop {
            loop {
                match in_rx.try_recv() {
                    Ok(item) => {
                        match cb.write(&item) {
                            Ok(()) => tracing::info!(item = item.describe(), "pasted from the other machine"),
                            Err(e) => tracing::warn!("{e}"),
                        }
                        seen = cb.change_count();
                        last_from_remote = Some(item);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            let now = cb.change_count();
            if now != seen {
                seen = now;
                if let Some(item) = cb.read() {
                    if last_from_remote.as_ref() != Some(&item) {
                        tracing::info!(item = item.describe(), "copied here; sending");
                        if out_tx.send(item).is_err() {
                            return;
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    })?;
    Ok(ClipboardLink { copied_here: out_rx, paste_here: in_tx })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trips() {
        let rgba: Vec<u8> = (0..4 * 6 * 4).map(|i| i as u8).collect();
        let png = rgba_to_png(4, 6, &rgba).unwrap();
        assert_eq!(png_to_rgba(&png), Some((4, 6, rgba)));
    }

    #[test]
    fn worker_sends_new_copies_and_does_not_echo_pastes() {
        let mem = MemoryClipboard::default();
        mem.copy(ClipItem::Text("old".into()));
        let link = spawn_worker(Box::new(mem.clone())).unwrap();
        // Something already on the clipboard at start is not sent.
        std::thread::sleep(Duration::from_millis(400));
        assert!(link.copied_here.try_recv().is_err());
        // A fresh copy is.
        mem.copy(ClipItem::Text("new".into()));
        let got = link.copied_here.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(got, ClipItem::Text("new".into()));
        // A paste from the other side lands and is not sent back.
        link.paste_here.send(ClipItem::Text("remote".into())).unwrap();
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(mem.current(), Some(ClipItem::Text("remote".into())));
        assert!(link.copied_here.try_recv().is_err());
    }
}
