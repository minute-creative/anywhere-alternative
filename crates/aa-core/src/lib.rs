//! `aa-core`: everything that is true on every operating system.
//!
//! This crate has **no** OS dependencies and **no** async runtime. It owns:
//!
//! * the data model of a stream ([`video`], [`input`]),
//! * what each side can do and how the two sides agree ([`capability`]),
//! * the bytes that go over the wire ([`wire`]),
//! * the numbers we watch to keep latency down ([`stats`]).
//!
//! Because nothing here touches hardware, all of it is unit-tested and runs
//! in CI on Linux even though the product only ships on macOS and Windows.
//!
//! Design rule: anything that could ever differ between macOS and Windows
//! lives in `aa-platform`, behind a trait. If you find yourself writing
//! `#[cfg(target_os = ...)]` in this crate, stop and move the code.

#![forbid(unsafe_code)]

pub mod capability;
pub mod config;
pub mod control;
pub mod control_flow;
pub mod input;
pub mod stats;
pub mod video;
pub mod wire;

/// Protocol version. Bump on any wire-incompatible change; both peers must
/// match, and the handshake refuses otherwise. Pre-1.0 we don't promise
/// compatibility between versions at all.
pub const PROTOCOL_VERSION: u16 = 1;
