//! "Anywhere Camera" for every app on a Windows 11 PC.
//!
//! Windows 11 lets a program add a camera that every app sees (Teams, Zoom,
//! the Camera app, browsers) without a driver: the program registers a small
//! DLL, and Windows' camera service ("Frame Server") loads it whenever an app
//! opens the camera. This crate is that DLL. It holds no picture of its own:
//! while an app uses the camera it creates a block of shared memory, the
//! Anywhere app sees it and fills it with the pictures from your other
//! computer's camera, and the DLL hands them to the app 30 times a second.
//!
//! The DLL runs inside Windows' camera service (as the Local Service account,
//! in the background session), so it keeps to what that allows: reading,
//! shared memory, no files. Microsoft's rules for such a "custom media
//! source" are followed: <https://learn.microsoft.com/windows-hardware/drivers/stream/frame-server-custom-media-source>.
//!
//! On other systems the crate is empty.

#[cfg(target_os = "windows")]
mod source;

#[cfg(target_os = "windows")]
pub use source::{DllCanUnloadNow, DllGetClassObject};

/// Convert NV12 (a brightness plane, then interleaved colour pairs) into
/// YUY2 (Y0 U Y1 V per two pixels), which older camera apps ask for.
/// `out` rows are `pitch` bytes apart.
pub fn nv12_to_yuy2(nv12: &[u8], w: usize, h: usize, out: &mut [u8], pitch: usize) {
    let (y, uv) = nv12.split_at(w * h);
    for row in 0..h {
        let yr = &y[row * w..row * w + w];
        let cr = &uv[(row / 2) * w..(row / 2) * w + w];
        let o = &mut out[row * pitch..row * pitch + w * 2];
        for x in (0..w).step_by(2) {
            o[x * 2] = yr[x];
            o[x * 2 + 1] = cr[x];
            o[x * 2 + 2] = yr[x + 1];
            o[x * 2 + 3] = cr[x + 1];
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn yuy2_layout() {
        // 2x2 picture: Y = 1,2 / 3,4; one colour pair U=9, V=8.
        let nv12 = [1, 2, 3, 4, 9, 8];
        let mut out = [0u8; 8];
        super::nv12_to_yuy2(&nv12, 2, 2, &mut out, 4);
        assert_eq!(out, [1, 9, 2, 8, 3, 9, 4, 8]);
    }
}
