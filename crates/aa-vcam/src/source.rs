//! The media source Windows' camera service loads (see lib.rs).
//!
//! COM objects, as Windows expects them:
//! - `Factory`: what `DllGetClassObject` hands out; makes the other two.
//! - `Activator` (IMFActivate): the camera service may ask for this first and
//!   call `ActivateObject` to get the source.
//! - `Source` (IMFMediaSourceEx + IKsControl + IMFGetService): the camera.
//! - `Stream` (IMFMediaStream2): its one picture stream, 1280x720 at 30 fps,
//!   offered as NV12 and YUY2.

#![allow(unsafe_code, non_snake_case, clippy::pedantic)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aa_core::calls::vcam;
use windows::core::{implement, Interface, Ref, Result, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, ERROR_SET_NOT_FOUND, E_NOINTERFACE, E_NOTIMPL,
    E_POINTER, HANDLE, INVALID_HANDLE_VALUE, S_FALSE, S_OK,
};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows_core::{IUnknown, IUnknownImpl};

const W: u32 = vcam::WIDTH as u32;
const H: u32 = vcam::HEIGHT as u32;
const FPS: u32 = 30;
/// 100-nanosecond units per frame.
const FRAME_TIME: i64 = 10_000_000 / FPS as i64;
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
const FOURCC_YUY2: u32 = u32::from_le_bytes(*b"YUY2");

fn clsid() -> GUID {
    GUID::from_u128(vcam::CLSID)
}

// ---------------------------------------------------------------------------
// DLL entry points
// ---------------------------------------------------------------------------

/// COM asks the DLL for its class factory.
///
/// # Safety
/// Called by COM with valid pointers.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null above; COM owns the memory.
    unsafe {
        *ppv = std::ptr::null_mut();
        if *rclsid != clsid() {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = Factory.into();
        factory.query(riid, ppv)
    }
}

/// How many of our objects are alive; at zero Windows may unload the DLL
/// (so an update of Anywhere can replace it without a restart).
static LIVE: AtomicU32 = AtomicU32::new(0);

struct Alive;

impl Alive {
    fn new() -> Self {
        LIVE.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for Alive {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if LIVE.load(Ordering::Acquire) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if riid.is_null() || ppv.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: checked non-null; COM provides them.
        unsafe {
            *ppv = std::ptr::null_mut();
            if *riid == IMFActivate::IID {
                let a: IMFActivate = Activator::new()?.into();
                return a.query(riid, ppv).ok();
            }
            let s: IMFMediaSourceEx = Source::new()?.into();
            s.query(riid, ppv).ok()
        }
    }

    fn LockServer(&self, _lock: windows::core::BOOL) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// IMFAttributes, passed through to a real attribute store
// ---------------------------------------------------------------------------

macro_rules! attributes_via {
    ($t:ty, $field:ident) => {
        impl IMFAttributes_Impl for $t {
            fn GetItem(&self, k: *const GUID, v: *mut PROPVARIANT) -> Result<()> {
                unsafe { self.$field.GetItem(k, Some(v)) }
            }
            fn GetItemType(&self, k: *const GUID) -> Result<MF_ATTRIBUTE_TYPE> {
                unsafe { self.$field.GetItemType(k) }
            }
            fn CompareItem(&self, k: *const GUID, v: *const PROPVARIANT) -> Result<windows::core::BOOL> {
                unsafe { self.$field.CompareItem(k, v) }
            }
            fn Compare(&self, theirs: Ref<IMFAttributes>, t: MF_ATTRIBUTES_MATCH_TYPE) -> Result<windows::core::BOOL> {
                unsafe { self.$field.Compare(theirs.as_ref(), t) }
            }
            fn GetUINT32(&self, k: *const GUID) -> Result<u32> {
                unsafe { self.$field.GetUINT32(k) }
            }
            fn GetUINT64(&self, k: *const GUID) -> Result<u64> {
                unsafe { self.$field.GetUINT64(k) }
            }
            fn GetDouble(&self, k: *const GUID) -> Result<f64> {
                unsafe { self.$field.GetDouble(k) }
            }
            fn GetGUID(&self, k: *const GUID) -> Result<GUID> {
                unsafe { self.$field.GetGUID(k) }
            }
            fn GetStringLength(&self, k: *const GUID) -> Result<u32> {
                unsafe { self.$field.GetStringLength(k) }
            }
            fn GetString(&self, k: *const GUID, v: PWSTR, size: u32, len: *mut u32) -> Result<()> {
                unsafe { self.$field.GetString(k, std::slice::from_raw_parts_mut(v.0, size as usize), Some(len)) }
            }
            fn GetAllocatedString(&self, k: *const GUID, v: *mut PWSTR, len: *mut u32) -> Result<()> {
                unsafe { self.$field.GetAllocatedString(k, v, len) }
            }
            fn GetBlobSize(&self, k: *const GUID) -> Result<u32> {
                unsafe { self.$field.GetBlobSize(k) }
            }
            fn GetBlob(&self, k: *const GUID, buf: *mut u8, size: u32, len: *mut u32) -> Result<()> {
                unsafe { self.$field.GetBlob(k, std::slice::from_raw_parts_mut(buf, size as usize), Some(len)) }
            }
            fn GetAllocatedBlob(&self, k: *const GUID, buf: *mut *mut u8, size: *mut u32) -> Result<()> {
                unsafe { self.$field.GetAllocatedBlob(k, buf, size) }
            }
            fn GetUnknown(&self, k: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
                unsafe {
                    let u: IUnknown = self.$field.GetUnknown(k)?;
                    u.query(riid, ppv).ok()
                }
            }
            fn SetItem(&self, k: *const GUID, v: *const PROPVARIANT) -> Result<()> {
                unsafe { self.$field.SetItem(k, v) }
            }
            fn DeleteItem(&self, k: *const GUID) -> Result<()> {
                unsafe { self.$field.DeleteItem(k) }
            }
            fn DeleteAllItems(&self) -> Result<()> {
                unsafe { self.$field.DeleteAllItems() }
            }
            fn SetUINT32(&self, k: *const GUID, v: u32) -> Result<()> {
                unsafe { self.$field.SetUINT32(k, v) }
            }
            fn SetUINT64(&self, k: *const GUID, v: u64) -> Result<()> {
                unsafe { self.$field.SetUINT64(k, v) }
            }
            fn SetDouble(&self, k: *const GUID, v: f64) -> Result<()> {
                unsafe { self.$field.SetDouble(k, v) }
            }
            fn SetGUID(&self, k: *const GUID, v: *const GUID) -> Result<()> {
                unsafe { self.$field.SetGUID(k, v) }
            }
            fn SetString(&self, k: *const GUID, v: &PCWSTR) -> Result<()> {
                unsafe { self.$field.SetString(k, *v) }
            }
            fn SetBlob(&self, k: *const GUID, buf: *const u8, size: u32) -> Result<()> {
                unsafe { self.$field.SetBlob(k, std::slice::from_raw_parts(buf, size as usize)) }
            }
            fn SetUnknown(&self, k: *const GUID, u: Ref<IUnknown>) -> Result<()> {
                unsafe { self.$field.SetUnknown(k, u.as_ref()) }
            }
            fn LockStore(&self) -> Result<()> {
                unsafe { self.$field.LockStore() }
            }
            fn UnlockStore(&self) -> Result<()> {
                unsafe { self.$field.UnlockStore() }
            }
            fn GetCount(&self) -> Result<u32> {
                unsafe { self.$field.GetCount() }
            }
            fn GetItemByIndex(&self, i: u32, k: *mut GUID, v: *mut PROPVARIANT) -> Result<()> {
                unsafe { self.$field.GetItemByIndex(i, k, Some(v)) }
            }
            fn CopyAllItems(&self, dest: Ref<IMFAttributes>) -> Result<()> {
                unsafe { self.$field.CopyAllItems(dest.as_ref()) }
            }
        }
    };
}

fn new_attributes() -> Result<IMFAttributes> {
    let mut a = None;
    // SAFETY: out-pointer is a local.
    unsafe { MFCreateAttributes(&mut a, 8)? };
    a.ok_or_else(|| E_POINTER.into())
}

// ---------------------------------------------------------------------------
// Activator
// ---------------------------------------------------------------------------

#[implement(IMFActivate)]
struct Activator {
    attrs: IMFAttributes,
    _alive: Alive,
}

impl Activator {
    fn new() -> Result<Self> {
        Ok(Self { attrs: new_attributes()?, _alive: Alive::new() })
    }
}

attributes_via!(Activator_Impl, attrs);

impl IMFActivate_Impl for Activator_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        let source = Source::new()?;
        // The camera service may have put settings here; the source keeps them.
        // SAFETY: plain COM calls on attribute stores we own.
        unsafe { self.attrs.CopyAllItems(&source.attrs)? };
        let s: IMFMediaSourceEx = source.into();
        // SAFETY: COM provides valid pointers.
        unsafe { s.query(riid, ppv).ok() }
    }

    fn ShutdownObject(&self) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn DetachObject(&self) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

// ---------------------------------------------------------------------------
// Shared memory with the Anywhere app
// ---------------------------------------------------------------------------

struct Shared {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

// SAFETY: the mapping is plain memory used with atomics for the fields that
// two processes write; the handle is a kernel object.
unsafe impl Send for Shared {}

impl Shared {
    /// Create the block (or open it if it is still there) so that any
    /// signed-in user's Anywhere app may fill it.
    fn create() -> Option<Self> {
        let name: Vec<u16> = vcam::SECTION.encode_utf16().chain([0]).collect();
        // Full access for the system, the service account and administrators;
        // read and write for signed-in users (the Anywhere app).
        let sddl: Vec<u16> = "D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;AU)".encode_utf16().chain([0]).collect();
        // SAFETY: Win32 calls with valid, zero-terminated strings and locals.
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None)
                .ok()?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(std::ptr::from_ref(&sa)),
                PAGE_READWRITE,
                0,
                vcam::SIZE as u32,
                PCWSTR(name.as_ptr()),
            )
            .ok()?;
            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, vcam::SIZE);
            if view.Value.is_null() {
                let _ = CloseHandle(handle);
                return None;
            }
            let s = Self { handle, view };
            s.field(vcam::OFF_MAGIC).store(vcam::MAGIC, Ordering::Release);
            Some(s)
        }
    }

    fn field(&self, off: usize) -> &AtomicU32 {
        // SAFETY: `off` is a 4-byte aligned offset inside the mapping.
        unsafe { &*(self.view.Value.cast::<u8>().add(off).cast::<AtomicU32>()) }
    }

    fn set_wanted(&self, on: bool) {
        self.field(vcam::OFF_WANTED).store(u32::from(on), Ordering::Release);
    }

    /// Copy the newest complete picture into `out` (NV12). `false`: the app
    /// has not written one yet, or was writing during the copy.
    fn read(&self, out: &mut [u8], last_seq: &mut u32) -> bool {
        let seq = self.field(vcam::OFF_SEQ);
        let before = seq.load(Ordering::Acquire);
        if before == 0 || before % 2 == 1 {
            return false;
        }
        if before == *last_seq {
            return true; // same picture as last time; `out` still holds it
        }
        // SAFETY: the data area is inside the mapping.
        unsafe {
            let src = self.view.Value.cast::<u8>().add(vcam::OFF_DATA);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), vcam::FRAME_LEN.min(out.len()));
        }
        let ok = seq.load(Ordering::Acquire) == before;
        if ok {
            *last_seq = before;
        }
        ok
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.set_wanted(false);
        // SAFETY: unmapping and closing what we created.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

fn media_type(subtype: &GUID, stride: u32, bytes: u32) -> Result<IMFMediaType> {
    // SAFETY: plain attribute writes on a new media type.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(W) << 32) | u64::from(H))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(FPS) << 32) | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        t.SetUINT32(&MF_MT_DEFAULT_STRIDE, stride)?;
        t.SetUINT32(&MF_MT_SAMPLE_SIZE, bytes)?;
        Ok(t)
    }
}

#[implement(IMFMediaSourceEx, IMFGetService, IKsControl)]
struct Source {
    queue: IMFMediaEventQueue,
    attrs: IMFAttributes,
    pd: IMFPresentationDescriptor,
    sd: IMFStreamDescriptor,
    state: Mutex<SourceState>,
    _alive: Alive,
}

#[derive(Default)]
struct SourceState {
    started: bool,
    shutdown: bool,
    stream: Option<IMFMediaStream2>,
    stream_obj: Option<windows_core::ComObject<Stream>>,
}

impl Source {
    fn new() -> Result<Self> {
        // SAFETY: Media Foundation set-up calls with locals.
        unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
            let nv12 = media_type(&MFVideoFormat_NV12, W, W * H * 3 / 2)?;
            let yuy2 = media_type(&MFVideoFormat_YUY2, W * 2, W * H * 2)?;
            let sd = MFCreateStreamDescriptor(0, &[Some(nv12.clone()), Some(yuy2)])?;
            sd.GetMediaTypeHandler()?.SetCurrentMediaType(&nv12)?;
            sd.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
            sd.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
            sd.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
            sd.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)?;
            let pd = MFCreatePresentationDescriptor(Some(&[Some(sd.clone())]))?;
            pd.SelectStream(0)?;
            Ok(Self {
                queue: MFCreateEventQueue()?,
                attrs: new_attributes()?,
                pd,
                sd,
                state: Mutex::new(SourceState::default()),
                _alive: Alive::new(),
            })
        }
    }
}

fn empty_var() -> PROPVARIANT {
    PROPVARIANT::default()
}

impl IMFMediaEventGenerator_Impl for Source_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        // SAFETY: plain COM call.
        unsafe { self.queue.GetEvent(flags.0) }
    }
    fn BeginGetEvent(&self, cb: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        unsafe { self.queue.BeginGetEvent(cb.as_ref(), state.as_ref()) }
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        unsafe { self.queue.EndGetEvent(result.as_ref()) }
    }
    fn QueueEvent(&self, met: u32, ext: *const GUID, hr: HRESULT, v: *const PROPVARIANT) -> Result<()> {
        unsafe { self.queue.QueueEventParamVar(met, ext, hr, v) }
    }
}

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        if self.state.lock().map_or(true, |s| s.shutdown) {
            return Err(MF_E_SHUTDOWN.into());
        }
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        // SAFETY: plain COM call.
        unsafe { self.pd.Clone() }
    }

    fn Start(&self, pd: Ref<IMFPresentationDescriptor>, _format: *const GUID, start: *const PROPVARIANT) -> Result<()> {
        let mut st = self.state.lock().map_err(|_| windows::core::Error::from(E_POINTER))?;
        if st.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        let empty = empty_var();
        let start_var: *const PROPVARIANT = if start.is_null() { &empty } else { start };
        // Which picture format the app chose (NV12 unless it asked for YUY2).
        let mut yuy2 = false;
        if let Some(pd) = pd.as_ref() {
            // SAFETY: plain COM calls on the descriptor we were given.
            unsafe {
                let mut selected = windows::core::BOOL(0);
                let mut sd = None;
                if pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd).is_ok() {
                    if let Some(sd) = sd {
                        if let Ok(t) = sd.GetMediaTypeHandler().and_then(|h| h.GetCurrentMediaType()) {
                            yuy2 = t.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|g| g == MFVideoFormat_YUY2);
                        }
                    }
                }
            }
        }
        let first = st.stream.is_none();
        if first {
            let ex: IMFMediaSourceEx = self.to_interface();
            let me: IMFMediaSource = ex.cast()?;
            let obj = windows_core::ComObject::new(Stream::new(me, self.sd.clone())?);
            st.stream = Some(obj.to_interface());
            st.stream_obj = Some(obj);
        }
        let stream = st.stream.clone().ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        if let Some(obj) = &st.stream_obj {
            obj.start(yuy2);
        }
        st.started = true;
        drop(st);
        // SAFETY: plain COM calls on our own event queues.
        unsafe {
            self.queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, start_var)?;
            let met = if first { MENewStream } else { MEUpdatedStream };
            let unk: IUnknown = stream.cast()?;
            self.queue.QueueEventParamUnk(met.0 as u32, &GUID::zeroed(), S_OK, &unk)?;
            stream.QueueEvent(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, start_var)?;
        }
        Ok(())
    }

    fn Stop(&self) -> Result<()> {
        let mut st = self.state.lock().map_err(|_| windows::core::Error::from(E_POINTER))?;
        if st.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        st.started = false;
        if let Some(obj) = &st.stream_obj {
            obj.stop();
        }
        let stream = st.stream.clone();
        drop(st);
        let v = empty_var();
        // SAFETY: plain COM calls on our own event queues.
        unsafe {
            self.queue.QueueEventParamVar(MESourceStopped.0 as u32, &GUID::zeroed(), S_OK, &v)?;
            if let Some(s) = stream {
                s.QueueEvent(MEStreamStopped.0 as u32, &GUID::zeroed(), S_OK, &v)?;
            }
        }
        Ok(())
    }

    fn Pause(&self) -> Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }

    fn Shutdown(&self) -> Result<()> {
        let mut st = self.state.lock().map_err(|_| windows::core::Error::from(E_POINTER))?;
        if st.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        st.shutdown = true;
        if let Some(obj) = st.stream_obj.take() {
            obj.shutdown();
        }
        st.stream = None;
        // SAFETY: plain COM call.
        unsafe {
            let _ = self.queue.Shutdown();
        }
        Ok(())
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        Ok(self.attrs.clone())
    }
    fn GetStreamAttributes(&self, id: u32) -> Result<IMFAttributes> {
        if id != 0 {
            return Err(E_NOINTERFACE.into());
        }
        self.sd.cast()
    }
    fn SetD3DManager(&self, _manager: Ref<IUnknown>) -> Result<()> {
        Ok(()) // pictures are in system memory
    }
}

impl IMFGetService_Impl for Source_Impl {
    fn GetService(&self, _service: *const GUID, _riid: *const GUID, _ppv: *mut *mut c_void) -> Result<()> {
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IKsControl_Impl for Source_Impl {
    fn KsProperty(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0).into())
    }
    fn KsMethod(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0).into())
    }
    fn KsEvent(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0).into())
    }
}

// ---------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------

#[implement(IMFMediaStream2)]
struct Stream {
    queue: IMFMediaEventQueue,
    sd: IMFStreamDescriptor,
    inner: Mutex<StreamState>,
    _alive: Alive,
}

struct StreamState {
    source: Option<IMFMediaSource>,
    running: bool,
    shutdown: bool,
    yuy2: bool,
    state: MF_STREAM_STATE,
    shared: Option<Shared>,
    /// The picture being shown (NV12), kept when no new one has come.
    picture: Vec<u8>,
    last_seq: u32,
    last_sample: Option<Instant>,
}

impl Stream {
    fn new(source: IMFMediaSource, sd: IMFStreamDescriptor) -> Result<Self> {
        // A dark grey picture until the first real one arrives.
        let mut picture = vec![128u8; vcam::FRAME_LEN];
        picture[..vcam::WIDTH * vcam::HEIGHT].fill(40);
        Ok(Self {
            // SAFETY: plain Media Foundation call.
            queue: unsafe { MFCreateEventQueue()? },
            sd,
            inner: Mutex::new(StreamState {
                source: Some(source),
                running: false,
                shutdown: false,
                yuy2: false,
                state: MF_STREAM_STATE_STOPPED,
                shared: None,
                picture,
                last_seq: 0,
                last_sample: None,
            }),
            _alive: Alive::new(),
        })
    }

    fn start(&self, yuy2: bool) {
        if let Ok(mut s) = self.inner.lock() {
            s.running = true;
            s.yuy2 = yuy2;
            s.state = MF_STREAM_STATE_RUNNING;
            if s.shared.is_none() {
                s.shared = Shared::create();
            }
            if let Some(sh) = &s.shared {
                sh.set_wanted(true);
            }
        }
    }

    fn stop(&self) {
        if let Ok(mut s) = self.inner.lock() {
            s.running = false;
            s.state = MF_STREAM_STATE_STOPPED;
            // Closing the shared memory tells the Anywhere app to stop the
            // camera on the other computer.
            s.shared = None;
        }
    }

    fn shutdown(&self) {
        if let Ok(mut s) = self.inner.lock() {
            s.shutdown = true;
            s.running = false;
            s.shared = None;
            s.source = None;
        }
        // SAFETY: plain COM call.
        unsafe {
            let _ = self.queue.Shutdown();
        }
    }

    /// One picture as a Media Foundation sample.
    fn sample(&self, s: &mut StreamState) -> Result<IMFSample> {
        if let Some(sh) = s.shared.as_ref() {
            let _ = sh.read(&mut s.picture, &mut s.last_seq);
        }
        let (w, h) = (vcam::WIDTH, vcam::HEIGHT);
        // SAFETY: Media Foundation buffer calls; we write only inside the
        // locked area (pitch × rows).
        unsafe {
            let fourcc = if s.yuy2 { FOURCC_YUY2 } else { FOURCC_NV12 };
            let buffer = MFCreate2DMediaBuffer(W, H, fourcc, false)?;
            let b2d: IMF2DBuffer = buffer.cast()?;
            let mut scan0 = std::ptr::null_mut();
            let mut pitch = 0i32;
            b2d.Lock2D(&mut scan0, &mut pitch)?;
            let pitch = pitch.unsigned_abs() as usize;
            if s.yuy2 {
                let out = std::slice::from_raw_parts_mut(scan0, pitch * h);
                crate::nv12_to_yuy2(&s.picture, w, h, out, pitch);
            } else {
                for row in 0..h {
                    std::ptr::copy_nonoverlapping(s.picture.as_ptr().add(row * w), scan0.add(row * pitch), w);
                }
                let uv = scan0.add(pitch * h);
                for row in 0..h / 2 {
                    std::ptr::copy_nonoverlapping(s.picture.as_ptr().add(w * h + row * w), uv.add(row * pitch), w);
                }
            }
            b2d.Unlock2D()?;
            let len = buffer.GetMaxLength()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(MFGetSystemTime())?;
            sample.SetSampleDuration(FRAME_TIME)?;
            Ok(sample)
        }
    }
}

impl IMFMediaEventGenerator_Impl for Stream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        unsafe { self.queue.GetEvent(flags.0) }
    }
    fn BeginGetEvent(&self, cb: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        unsafe { self.queue.BeginGetEvent(cb.as_ref(), state.as_ref()) }
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        unsafe { self.queue.EndGetEvent(result.as_ref()) }
    }
    fn QueueEvent(&self, met: u32, ext: *const GUID, hr: HRESULT, v: *const PROPVARIANT) -> Result<()> {
        unsafe { self.queue.QueueEventParamVar(met, ext, hr, v) }
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        let s = self.inner.lock().map_err(|_| windows::core::Error::from(E_POINTER))?;
        s.source.clone().ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        Ok(self.sd.clone())
    }

    fn RequestSample(&self, token: Ref<IUnknown>) -> Result<()> {
        let mut s = self.inner.lock().map_err(|_| windows::core::Error::from(E_POINTER))?;
        if s.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        if !s.running {
            return Err(MF_E_INVALID_STATE_TRANSITION.into());
        }
        // Keep to 30 pictures a second even if asked faster.
        if let Some(t) = s.last_sample {
            let gap = Duration::from_nanos(FRAME_TIME as u64 * 100);
            let since = t.elapsed();
            if since < gap {
                std::thread::sleep(gap - since);
            }
        }
        s.last_sample = Some(Instant::now());
        let sample = self.sample(&mut s)?;
        drop(s);
        // SAFETY: plain COM calls.
        unsafe {
            if let Some(t) = token.as_ref() {
                sample.SetUnknown(&MFSampleExtension_Token, t)?;
            }
            let unk: IUnknown = sample.cast()?;
            self.queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &unk)
        }
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> Result<()> {
        if let Ok(mut s) = self.inner.lock() {
            s.state = value;
        }
        Ok(())
    }
    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        Ok(self.inner.lock().map_or(MF_STREAM_STATE_STOPPED, |s| s.state))
    }
}
