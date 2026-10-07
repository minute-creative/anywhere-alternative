#![allow(clippy::doc_markdown)] // product names read better plain
//! A tiny USB/IP server: presents virtual USB devices to this same PC.
//!
//! Why: Windows only lets a *driver* create a new USB device, and writing a
//! signed driver is out of reach. usbip-win2 is a free, signed driver that
//! plugs in whatever USB device a USB/IP server describes. So we run the
//! server ourselves, on localhost, describing a perfect DualSense, and ask
//! usbip-win2 to attach it. To Windows and to games it is then a real USB
//! controller.
//!
//! Protocol (Linux kernel `usbip_protocol.rst`): the client opens TCP,
//! sends `OP_REQ_IMPORT` with a bus id, we reply with the device summary,
//! then the same connection carries URBs: `CMD_SUBMIT` (a transfer on an
//! endpoint) answered by `RET_SUBMIT`, and `CMD_UNLINK` (cancel) answered by
//! `RET_UNLINK`. All integers are big-endian.
//!
//! Interrupt-IN transfers (the controller's reports) are *held* until a new
//! report exists, like real hardware, so the game sees each report once and
//! as soon as it arrives. If none arrives for a while we answer with the
//! last one, so the host never sees a dead device.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const VERSION: u16 = 0x0111;
const OP_REQ_DEVLIST: u16 = 0x8005;
const OP_REP_DEVLIST: u16 = 0x0005;
const OP_REQ_IMPORT: u16 = 0x8003;
const OP_REP_IMPORT: u16 = 0x0003;
const CMD_SUBMIT: u32 = 1;
const CMD_UNLINK: u32 = 2;
const RET_SUBMIT: u32 = 3;
const RET_UNLINK: u32 = 4;
const DIR_IN: u32 = 1;
/// Linux errno values the protocol carries (negated).
const EPIPE: i32 = -32; // "stall": request not supported
const ECONNRESET: i32 = -104; // unlinked before completion

/// Longest a held interrupt-IN transfer waits before we answer with the
/// last report anyway.
const HOLD_IN: Duration = Duration::from_millis(8);
/// Largest transfer we accept from a client (sanity limit against garbage).
const MAX_TRANSFER: u32 = 1 << 20;

/// A USB control request (the 8-byte SETUP packet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    fn parse(b: [u8; 8]) -> Self {
        Self {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }
}

/// What a virtual device must provide. Called from the server's threads.
pub trait UsbDevice: Send + Sync {
    /// The 18-byte device descriptor.
    fn device_descriptor(&self) -> Vec<u8>;
    /// The full configuration descriptor (config + interfaces + endpoints).
    fn config_descriptor(&self) -> Vec<u8>;
    /// String descriptor `index` (0 = language list), already encoded.
    fn string_descriptor(&self, index: u8) -> Option<Vec<u8>>;
    /// Class/vendor control requests and interface descriptors (e.g. the
    /// HID report descriptor). `Some(data)` answers (empty for OUT
    /// requests), `None` stalls.
    fn control(&self, setup: Setup, data: &[u8]) -> Option<Vec<u8>>;
    /// Data for an IN transfer on `ep`, if new data is ready. With `stale`
    /// set, return the latest data even if already sent (keep-alive).
    fn poll_in(&self, ep: u8, stale: bool) -> Option<Vec<u8>>;
    /// An OUT transfer on `ep` (e.g. an output report from a game).
    fn out(&self, ep: u8, data: &[u8]);
    /// An isochronous OUT transfer (e.g. audio to the device's speaker).
    fn iso_out(&self, _ep: u8, _data: &[u8], _packets: &[IsoPacket]) {}
    /// Data for an isochronous IN transfer, one buffer per packet of at
    /// most the given lengths (e.g. microphone audio). Default: silence.
    fn iso_in(&self, _ep: u8, lengths: &[usize]) -> Vec<Vec<u8>> {
        lengths.iter().map(|&n| vec![0; n]).collect()
    }
    /// Speed for the import reply: 1 low, 2 full, 3 high.
    fn speed(&self) -> u32 {
        2
    }
}

/// Wakes held IN transfers when the device has news.
#[derive(Debug, Default)]
pub struct Doorbell {
    rung: Mutex<u64>,
    cv: Condvar,
}

impl Doorbell {
    pub fn ring(&self) {
        *self.rung.lock().expect("doorbell") += 1;
        self.cv.notify_all();
    }

    fn wait(&self, seen: u64, timeout: Duration) -> u64 {
        let g = self.rung.lock().expect("doorbell");
        let (g, _) = self.cv.wait_timeout_while(g, timeout, |n| *n == seen).expect("doorbell");
        *g
    }
}

/// One exported device.
pub struct Export {
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub device: Arc<dyn UsbDevice>,
    pub doorbell: Arc<Doorbell>,
    /// Open connections to this device, so removing it unplugs it.
    conns: Mutex<Vec<TcpStream>>,
}

impl std::fmt::Debug for Export {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Export").field("busid", &self.busid).finish_non_exhaustive()
    }
}

impl Export {
    pub fn new(busid: impl Into<String>, devnum: u32, device: Arc<dyn UsbDevice>) -> Self {
        Self { busid: busid.into(), busnum: 1, devnum, device, doorbell: Arc::default(), conns: Mutex::default() }
    }

    /// Close every connection to this device (the PC sees it unplugged).
    pub fn unplug(&self) {
        for c in self.conns.lock().expect("conns").drain(..) {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        self.doorbell.ring();
    }

    /// Whether a client currently has this device attached.
    pub fn attached(&self) -> bool {
        !self.conns.lock().expect("conns").is_empty()
    }
}

/// The server: owns the listener and the devices on offer.
pub struct Server {
    addr: SocketAddr,
    exports: Arc<Mutex<Vec<Arc<Export>>>>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbipServer").field("addr", &self.addr).finish_non_exhaustive()
    }
}

impl Server {
    /// Listen on `addr` (use port 0 for any free port; see [`Self::addr`]).
    pub fn start(addr: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        let exports: Arc<Mutex<Vec<Arc<Export>>>> = Arc::default();
        let ex = Arc::clone(&exports);
        std::thread::Builder::new().name("aa-usbip".into()).spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let ex = Arc::clone(&ex);
                let _ = std::thread::Builder::new().name("aa-usbip-conn".into()).spawn(move || {
                    if let Err(e) = serve(conn, &ex) {
                        tracing::debug!("usbip connection ended: {e}");
                    }
                });
            }
        })?;
        tracing::info!(%addr, "virtual USB server listening");
        Ok(Self { addr, exports })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn add(&self, export: Export) -> Arc<Export> {
        let e = Arc::new(export);
        let mut list = self.exports.lock().expect("exports");
        list.retain(|x| x.busid != e.busid);
        list.push(Arc::clone(&e));
        e
    }

    /// Stop offering a device and unplug it if attached.
    pub fn remove(&self, busid: &str) {
        let mut list = self.exports.lock().expect("exports");
        for e in list.iter().filter(|x| x.busid == busid) {
            e.unplug();
        }
        list.retain(|x| x.busid != busid);
    }
}

fn read_exact<const N: usize>(s: &mut TcpStream) -> std::io::Result<[u8; N]> {
    let mut b = [0u8; N];
    s.read_exact(&mut b)?;
    Ok(b)
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// The 312-byte device summary used by devlist and import replies.
fn device_summary(e: &Export) -> Vec<u8> {
    let dd = e.device.device_descriptor();
    let cfg = e.device.config_descriptor();
    let mut out = Vec::with_capacity(312);
    let mut path = format!("/sys/devices/aa/{}", e.busid).into_bytes();
    path.resize(256, 0);
    out.extend_from_slice(&path);
    let mut busid = e.busid.clone().into_bytes();
    busid.resize(32, 0);
    out.extend_from_slice(&busid);
    out.extend_from_slice(&e.busnum.to_be_bytes());
    out.extend_from_slice(&e.devnum.to_be_bytes());
    out.extend_from_slice(&e.device.speed().to_be_bytes());
    out.extend_from_slice(&[dd[9], dd[8], dd[11], dd[10], dd[13], dd[12]]); // vid, pid, bcdDevice (BE)
    out.extend_from_slice(&[dd[4], dd[5], dd[6]]); // class, subclass, protocol
    out.push(cfg.get(5).copied().unwrap_or(1)); // bConfigurationValue
    out.push(dd[17]); // bNumConfigurations
    out.push(cfg.get(4).copied().unwrap_or(1)); // bNumInterfaces
    out
}

/// Interfaces of a config descriptor as (class, subclass, protocol).
fn interfaces(cfg: &[u8]) -> Vec<[u8; 3]> {
    let mut v = Vec::new();
    let mut i = 0;
    while i + 1 < cfg.len() {
        let len = usize::from(cfg[i]);
        if len == 0 {
            break;
        }
        if cfg[i + 1] == 4 && i + 8 < cfg.len() && cfg[i + 3] == 0 {
            v.push([cfg[i + 5], cfg[i + 6], cfg[i + 7]]);
        }
        i += len;
    }
    v
}

fn mgmt_header(cmd: u16, status: u32) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[0..2].copy_from_slice(&VERSION.to_be_bytes());
    h[2..4].copy_from_slice(&cmd.to_be_bytes());
    h[4..8].copy_from_slice(&status.to_be_bytes());
    h
}

fn serve(mut conn: TcpStream, exports: &Mutex<Vec<Arc<Export>>>) -> std::io::Result<()> {
    conn.set_nodelay(true)?;
    let op = read_exact::<8>(&mut conn)?;
    match be16(&op[2..4]) {
        OP_REQ_DEVLIST => {
            let list: Vec<Arc<Export>> = exports.lock().expect("exports").clone();
            let mut out = mgmt_header(OP_REP_DEVLIST, 0).to_vec();
            out.extend_from_slice(&u32::try_from(list.len()).unwrap_or(0).to_be_bytes());
            for e in &list {
                out.extend(device_summary(e));
                for [c, s, p] in interfaces(&e.device.config_descriptor()) {
                    out.extend_from_slice(&[c, s, p, 0]);
                }
            }
            conn.write_all(&out)
        }
        OP_REQ_IMPORT => {
            let want = read_exact::<32>(&mut conn)?;
            let want = String::from_utf8_lossy(&want).trim_end_matches('\0').to_owned();
            let found = exports.lock().expect("exports").iter().find(|e| e.busid == want).cloned();
            let Some(export) = found else {
                return conn.write_all(&mgmt_header(OP_REP_IMPORT, 1));
            };
            let mut out = mgmt_header(OP_REP_IMPORT, 0).to_vec();
            out.extend(device_summary(&export));
            conn.write_all(&out)?;
            let peer = conn.peer_addr()?;
            export.conns.lock().expect("conns").push(conn.try_clone()?);
            tracing::info!(busid = export.busid, "virtual device attached");
            let r = urb_loop(conn, &export);
            export.conns.lock().expect("conns").retain(|c| c.peer_addr().ok() != Some(peer));
            tracing::info!(busid = export.busid, "virtual device detached");
            r
        }
        _ => Ok(()),
    }
}

/// A held interrupt-IN transfer.
#[derive(Debug, Clone, Copy)]
struct PendingIn {
    seqnum: u32,
    ep: u8,
    length: u32,
    since: Instant,
}

/// One packet of an isochronous transfer (offset and length in the buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoPacket {
    pub offset: u32,
    pub length: u32,
}

/// An isochronous transfer, completed at the pace a real device would
/// (one packet per 1 ms USB frame). Completing them instantly would make
/// the PC's audio engine think the device plays impossibly fast.
#[derive(Debug, Clone)]
struct PendingIso {
    seqnum: u32,
    ep: u8,
    dir_in: bool,
    packets: Vec<IsoPacket>,
    due: Instant,
    start_frame: u32,
}

struct Conn {
    writer: Mutex<TcpStream>,
    pending: Mutex<VecDeque<PendingIn>>,
    iso: Mutex<VecDeque<PendingIso>>,
    /// When each isochronous endpoint's queued packets run out.
    iso_clock: Mutex<[Option<Instant>; 32]>,
    frame: std::sync::atomic::AtomicU32,
    closed: std::sync::atomic::AtomicBool,
}

impl Conn {
    fn send(&self, msg: &[u8]) {
        if self.writer.lock().expect("writer").write_all(msg).is_err() {
            self.closed.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

fn ret_submit(seqnum: u32, status: i32, data: &[u8]) -> Vec<u8> {
    let mut h = vec![0u8; 48];
    h[0..4].copy_from_slice(&RET_SUBMIT.to_be_bytes());
    h[4..8].copy_from_slice(&seqnum.to_be_bytes());
    h[20..24].copy_from_slice(&status.to_be_bytes());
    h[24..28].copy_from_slice(&u32::try_from(data.len()).unwrap_or(0).to_be_bytes());
    h[32..36].copy_from_slice(&0xFFFF_FFFF_u32.to_be_bytes()); // not isochronous
    h.extend_from_slice(data);
    h
}

/// RET_SUBMIT for an isochronous transfer: header, IN data packed back to
/// back, then one descriptor per packet.
fn ret_iso(p: &PendingIso, data: &[Vec<u8>]) -> Vec<u8> {
    let total: usize =
        if p.dir_in { data.iter().map(Vec::len).sum() } else { p.packets.iter().map(|k| k.length as usize).sum() };
    let n = u32::try_from(p.packets.len()).unwrap_or(0);
    let mut h = vec![0u8; 48];
    h[0..4].copy_from_slice(&RET_SUBMIT.to_be_bytes());
    h[4..8].copy_from_slice(&p.seqnum.to_be_bytes());
    h[24..28].copy_from_slice(&u32::try_from(total).unwrap_or(0).to_be_bytes());
    h[28..32].copy_from_slice(&p.start_frame.to_be_bytes());
    h[32..36].copy_from_slice(&n.to_be_bytes());
    if p.dir_in {
        for d in data {
            h.extend_from_slice(d);
        }
    }
    for (i, k) in p.packets.iter().enumerate() {
        let actual = if p.dir_in { data.get(i).map_or(0, Vec::len) } else { k.length as usize };
        h.extend_from_slice(&k.offset.to_be_bytes());
        h.extend_from_slice(&k.length.to_be_bytes());
        h.extend_from_slice(&u32::try_from(actual).unwrap_or(0).to_be_bytes());
        h.extend_from_slice(&0u32.to_be_bytes());
    }
    h
}

fn ret_unlink(seqnum: u32, status: i32) -> [u8; 48] {
    let mut h = [0u8; 48];
    h[0..4].copy_from_slice(&RET_UNLINK.to_be_bytes());
    h[4..8].copy_from_slice(&seqnum.to_be_bytes());
    h[20..24].copy_from_slice(&status.to_be_bytes());
    h
}

/// One pass of the completion thread: answer interrupt-IN transfers that
/// have data (or have waited long enough) and isochronous ones now due.
fn complete_due(c: &Conn, e: &Export) {
    {
        let mut p = c.pending.lock().expect("pending");
        // Oldest first, one report each, as hardware would.
        while let Some(front) = p.front().copied() {
            let stale = front.since.elapsed() >= HOLD_IN;
            let Some(mut data) = e.device.poll_in(front.ep, stale) else { break };
            data.truncate(front.length as usize);
            p.pop_front();
            c.send(&ret_submit(front.seqnum, 0, &data));
        }
    }
    let now = Instant::now();
    loop {
        let next = {
            let mut q = c.iso.lock().expect("iso");
            match q.iter().position(|p| p.due <= now) {
                Some(i) => q.remove(i),
                None => None,
            }
        };
        let Some(p) = next else { break };
        let data: Vec<Vec<u8>> = if p.dir_in {
            let lens: Vec<usize> = p.packets.iter().map(|k| k.length as usize).collect();
            let mut d = e.device.iso_in(p.ep, &lens);
            d.resize(lens.len(), Vec::new());
            for (x, len) in d.iter_mut().zip(&lens) {
                x.truncate(*len);
            }
            d
        } else {
            Vec::new()
        };
        c.send(&ret_iso(&p, &data));
    }
}

fn urb_loop(conn: TcpStream, export: &Arc<Export>) -> std::io::Result<()> {
    let mut reader = conn.try_clone()?;
    let c = Arc::new(Conn {
        writer: Mutex::new(conn),
        pending: Mutex::new(VecDeque::new()),
        iso: Mutex::new(VecDeque::new()),
        iso_clock: Mutex::new([None; 32]),
        frame: std::sync::atomic::AtomicU32::new(0),
        closed: std::sync::atomic::AtomicBool::new(false),
    });

    // Completes held transfers when the device rings, or when due.
    let pump = {
        let c = Arc::clone(&c);
        let e = Arc::clone(export);
        std::thread::Builder::new().name("aa-usbip-in".into()).spawn(move || {
            let mut seen = 0;
            while !c.closed.load(std::sync::atomic::Ordering::Relaxed) {
                seen = e.doorbell.wait(seen, Duration::from_millis(1));
                complete_due(&c, &e);
            }
        })?
    };

    let result = (|| -> std::io::Result<()> {
        loop {
            let h = read_exact::<48>(&mut reader)?;
            let command = be32(&h[0..4]);
            let seqnum = be32(&h[4..8]);
            let direction = be32(&h[12..16]);
            let ep = u8::try_from(be32(&h[16..20]) & 0x0F).unwrap_or(0);
            match command {
                CMD_SUBMIT => {
                    let length = be32(&h[24..28]);
                    let packets = be32(&h[32..36]);
                    let iso = packets != 0 && packets != 0xFFFF_FFFF;
                    if length > MAX_TRANSFER || (iso && packets > 1024) {
                        return Err(std::io::Error::other("absurd transfer size"));
                    }
                    let mut out = Vec::new();
                    if direction != DIR_IN {
                        out = vec![0u8; length as usize];
                        reader.read_exact(&mut out)?;
                    }
                    if iso {
                        let mut raw = vec![0u8; packets as usize * 16];
                        reader.read_exact(&mut raw)?;
                        let list: Vec<IsoPacket> = raw
                            .chunks_exact(16)
                            .map(|k| IsoPacket { offset: be32(&k[0..4]), length: be32(&k[4..8]) })
                            .collect();
                        submit_iso(&c, export, seqnum, direction == DIR_IN, ep, list, &out);
                        continue;
                    }
                    let setup = Setup::parse(h[40..48].try_into().expect("8 bytes"));
                    handle_submit(&c, export, seqnum, direction, ep, length, setup, &out)?;
                }
                CMD_UNLINK => {
                    let target = be32(&h[20..24]);
                    let removed = {
                        let mut p = c.pending.lock().expect("pending");
                        let before = p.len();
                        p.retain(|x| x.seqnum != target);
                        let mut q = c.iso.lock().expect("iso");
                        let before_iso = q.len();
                        q.retain(|x| x.seqnum != target);
                        p.len() != before || q.len() != before_iso
                    };
                    let status = if removed { ECONNRESET } else { 0 };
                    c.writer.lock().expect("writer").write_all(&ret_unlink(seqnum, status))?;
                }
                _ => return Err(std::io::Error::other(format!("unknown usbip command {command}"))),
            }
        }
    })();
    c.closed.store(true, std::sync::atomic::Ordering::Relaxed);
    export.doorbell.ring();
    let _ = pump.join();
    result
}

/// Queue an isochronous transfer: OUT data goes to the device at once
/// (sooner is better for audio), the completion follows at real-time pace.
fn submit_iso(c: &Conn, export: &Export, seqnum: u32, dir_in: bool, ep: u8, packets: Vec<IsoPacket>, out: &[u8]) {
    // Packets that point outside the buffer are cut, not trusted.
    let packets: Vec<IsoPacket> = packets
        .into_iter()
        .map(|k| {
            let end = (k.offset as usize).saturating_add(k.length as usize);
            if !dir_in && end > out.len() {
                IsoPacket { offset: k.offset.min(u32::try_from(out.len()).unwrap_or(0)), length: 0 }
            } else {
                k
            }
        })
        .collect();
    if !dir_in {
        export.device.iso_out(ep, out, &packets);
    }
    let now = Instant::now();
    let span = Duration::from_millis(packets.len() as u64);
    let due = {
        let mut clocks = c.iso_clock.lock().expect("iso clock");
        let slot = &mut clocks[usize::from(ep & 0x0F) + if dir_in { 16 } else { 0 }];
        // Behind real time (first transfer, or the PC paused): restart the
        // clock now; otherwise continue where the queued packets end.
        let start = slot.filter(|t| *t > now).unwrap_or(now);
        let due = start + span;
        *slot = Some(due);
        due
    };
    let start_frame =
        c.frame.fetch_add(u32::try_from(packets.len()).unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    c.iso.lock().expect("iso").push_back(PendingIso { seqnum, ep, dir_in, packets, due, start_frame });
}

#[allow(clippy::too_many_arguments)]
fn handle_submit(
    c: &Conn,
    export: &Export,
    seqnum: u32,
    direction: u32,
    ep: u8,
    length: u32,
    setup: Setup,
    out: &[u8],
) -> std::io::Result<()> {
    let dev = &export.device;
    if ep == 0 {
        let reply = control(dev.as_ref(), setup, out);
        let msg = match reply {
            Some(mut data) => {
                data.truncate(usize::from(setup.length).min(length as usize));
                if direction == DIR_IN {
                    ret_submit(seqnum, 0, &data)
                } else {
                    ret_submit(seqnum, 0, &[])
                }
            }
            None => ret_submit(seqnum, EPIPE, &[]),
        };
        // An OUT completion reports how much was accepted.
        let msg = if direction != DIR_IN && reply_ok(&msg) { ret_out(seqnum, out.len()) } else { msg };
        return c.writer.lock().expect("writer").write_all(&msg);
    }
    if direction == DIR_IN {
        c.pending.lock().expect("pending").push_back(PendingIn { seqnum, ep, length, since: Instant::now() });
        export.doorbell.ring();
        Ok(())
    } else {
        dev.out(ep, out);
        c.writer.lock().expect("writer").write_all(&ret_out(seqnum, out.len()))
    }
}

fn reply_ok(msg: &[u8]) -> bool {
    msg[20..24] == [0, 0, 0, 0]
}

/// RET_SUBMIT for an OUT transfer: no data, `actual_length` = bytes taken.
fn ret_out(seqnum: u32, n: usize) -> Vec<u8> {
    let mut m = ret_submit(seqnum, 0, &[]);
    m[24..28].copy_from_slice(&u32::try_from(n).unwrap_or(0).to_be_bytes());
    m
}

/// Standard requests answered here; everything else goes to the device.
fn control(dev: &dyn UsbDevice, s: Setup, data: &[u8]) -> Option<Vec<u8>> {
    const GET_STATUS: u8 = 0;
    const CLEAR_FEATURE: u8 = 1;
    const SET_ADDRESS: u8 = 5;
    const GET_DESCRIPTOR: u8 = 6;
    const GET_CONFIGURATION: u8 = 8;
    const SET_CONFIGURATION: u8 = 9;
    const GET_INTERFACE: u8 = 10;
    const SET_INTERFACE: u8 = 11;
    // bmRequestType: bits 5-6 = type (0 standard), bits 0-4 = recipient (0 device).
    let standard = s.request_type & 0x60 == 0;
    let to_device = s.request_type.trailing_zeros() >= 5;
    if standard {
        match s.request {
            GET_DESCRIPTOR if to_device => {
                let [index, kind] = s.value.to_le_bytes();
                return match kind {
                    1 => Some(dev.device_descriptor()),
                    2 => Some(dev.config_descriptor()),
                    3 => dev.string_descriptor(index),
                    _ => None, // device qualifier etc.: full-speed device, stall
                };
            }
            GET_DESCRIPTOR => return dev.control(s, data), // interface: HID report descriptor
            GET_STATUS => return Some(vec![0, 0]),
            GET_CONFIGURATION => return Some(vec![1]),
            GET_INTERFACE => return Some(vec![0]),
            SET_ADDRESS | SET_CONFIGURATION | CLEAR_FEATURE => return Some(Vec::new()),
            SET_INTERFACE => return dev.control(s, data).or_else(|| Some(Vec::new())),
            _ => {}
        }
    }
    dev.control(s, data)
}

/// Encode a UTF-16 string descriptor.
pub fn string_descriptor(text: &str) -> Vec<u8> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut d = vec![u8::try_from(2 + units.len() * 2).unwrap_or(255), 3];
    for u in units {
        d.extend_from_slice(&u.to_le_bytes());
    }
    d
}

pub mod client {
    //! A minimal USB/IP client, playing the part of usbip-win2 in tests and
    //! in the mock host's end-to-end check.
    use super::{be32, mgmt_header, read_exact, CMD_SUBMIT, CMD_UNLINK, MAX_TRANSFER, OP_REQ_IMPORT, RET_SUBMIT};
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    #[derive(Debug)]
    pub struct Client {
        pub s: TcpStream,
        seq: u32,
    }

    impl Client {
        pub fn import(addr: SocketAddr, busid: &str) -> std::io::Result<(Self, Vec<u8>)> {
            let mut s = TcpStream::connect(addr)?;
            s.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut req = mgmt_header(OP_REQ_IMPORT, 0).to_vec();
            let mut b = busid.as_bytes().to_vec();
            b.resize(32, 0);
            req.extend(b);
            s.write_all(&req)?;
            let mut rep = [0u8; 8];
            s.read_exact(&mut rep)?;
            if be32(&rep[4..8]) != 0 {
                return Err(std::io::Error::other("import refused"));
            }
            let mut dev = vec![0u8; 312];
            s.read_exact(&mut dev)?;
            Ok((Self { s, seq: 0 }, dev))
        }

        pub fn submit(&mut self, dir_in: bool, ep: u32, len: u32, setup: [u8; 8], out: &[u8]) -> std::io::Result<u32> {
            self.seq += 1;
            let mut h = vec![0u8; 48];
            h[0..4].copy_from_slice(&CMD_SUBMIT.to_be_bytes());
            h[4..8].copy_from_slice(&self.seq.to_be_bytes());
            h[12..16].copy_from_slice(&u32::from(dir_in).to_be_bytes());
            h[16..20].copy_from_slice(&ep.to_be_bytes());
            h[24..28].copy_from_slice(&len.to_be_bytes());
            h[40..48].copy_from_slice(&setup);
            h.extend_from_slice(out);
            self.s.write_all(&h)?;
            Ok(self.seq)
        }

        pub fn unlink(&mut self, target: u32) -> std::io::Result<u32> {
            self.seq += 1;
            let mut h = vec![0u8; 48];
            h[0..4].copy_from_slice(&CMD_UNLINK.to_be_bytes());
            h[4..8].copy_from_slice(&self.seq.to_be_bytes());
            h[20..24].copy_from_slice(&target.to_be_bytes());
            self.s.write_all(&h)?;
            Ok(self.seq)
        }

        /// Submit an isochronous transfer of `packets` packets of `each` bytes.
        pub fn submit_iso(
            &mut self,
            dir_in: bool,
            ep: u32,
            each: u32,
            packets: u32,
            out: &[u8],
        ) -> std::io::Result<u32> {
            self.seq += 1;
            let mut h = vec![0u8; 48];
            h[0..4].copy_from_slice(&CMD_SUBMIT.to_be_bytes());
            h[4..8].copy_from_slice(&self.seq.to_be_bytes());
            h[12..16].copy_from_slice(&u32::from(dir_in).to_be_bytes());
            h[16..20].copy_from_slice(&ep.to_be_bytes());
            h[24..28].copy_from_slice(&(each * packets).to_be_bytes());
            h[32..36].copy_from_slice(&packets.to_be_bytes());
            h.extend_from_slice(out);
            for i in 0..packets {
                h.extend_from_slice(&(i * each).to_be_bytes());
                h.extend_from_slice(&each.to_be_bytes());
                h.extend_from_slice(&[0; 8]);
            }
            self.s.write_all(&h)?;
            Ok(self.seq)
        }

        /// The reply to an isochronous transfer: (seqnum, IN data, per-packet actual lengths).
        pub fn reply_iso(&mut self, dir_in: bool) -> std::io::Result<(u32, Vec<u8>, Vec<u32>)> {
            let h = read_exact::<48>(&mut self.s)?;
            let len = be32(&h[24..28]) as usize;
            let n = be32(&h[32..36]) as usize;
            if be32(&h[0..4]) != RET_SUBMIT || len > MAX_TRANSFER as usize || n > 1024 {
                return Err(std::io::Error::other("unexpected reply"));
            }
            let mut data = vec![0u8; if dir_in { len } else { 0 }];
            self.s.read_exact(&mut data)?;
            let mut desc = vec![0u8; n * 16];
            self.s.read_exact(&mut desc)?;
            Ok((be32(&h[4..8]), data, desc.chunks_exact(16).map(|k| be32(&k[8..12])).collect()))
        }

        /// Next reply header: (command, seqnum, status, `actual_length`).
        /// Only IN completions are followed by `actual_length` data bytes.
        pub fn reply(&mut self) -> std::io::Result<(u32, u32, i32, usize)> {
            let h = read_exact::<48>(&mut self.s)?;
            let status = i32::from_be_bytes([h[20], h[21], h[22], h[23]]);
            Ok((be32(&h[0..4]), be32(&h[4..8]), status, be32(&h[24..28]) as usize))
        }

        /// The reply to an IN transfer, with its data.
        pub fn reply_in(&mut self) -> std::io::Result<(u32, i32, Vec<u8>)> {
            let (cmd, seq, status, len) = self.reply()?;
            if cmd != RET_SUBMIT || len > MAX_TRANSFER as usize {
                return Err(std::io::Error::other("unexpected reply"));
            }
            let mut data = vec![0u8; len];
            self.s.read_exact(&mut data)?;
            Ok((seq, status, data))
        }

        /// GET_DESCRIPTOR. `recipient` 0 = device, 1 = interface number `w_index`.
        pub fn get_descriptor(
            &mut self,
            kind: u8,
            index: u8,
            recipient: u8,
            w_index: u8,
            len: u16,
        ) -> std::io::Result<(i32, Vec<u8>)> {
            let [l0, l1] = len.to_le_bytes();
            let setup = [0x80 | recipient, 6, index, kind, w_index, 0, l0, l1];
            self.submit(true, 0, u32::from(len), setup, &[])?;
            let (_, status, data) = self.reply_in()?;
            Ok((status, data))
        }
    }
}
