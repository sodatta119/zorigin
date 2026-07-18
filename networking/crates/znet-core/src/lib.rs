//! Core file-transfer logic for zap, shared across platforms.
//!
//! This crate is deliberately free of any presentation concerns (no terminal
//! output, no QR rendering) so it can back both the desktop CLI and, later, an
//! Android app that hosts the same web server on the phone.
//!
//! - [`transport`] - the host-driven `Transport` trait and its implementations
//!   (currently ADB). Used when the desktop drives the device.
//! - [`web`] - the server-mode web transport: the host runs an HTTP server and
//!   a browser on another device drives transfers. This is the piece that will
//!   run on Android.

pub mod transport;
pub mod web;
