// Copyright (C) 2026 Feng Chang
// SPDX-License-Identifier: MIT OR Apache-2.0 OR GPL-2.0-or-later

//! Native access to the Windows Rockchip Rockusb driver.
//!
//! This crate implements the driver's user-mode I/O interface independently,
//! without a vendor DLL or USB library. It transfers caller-provided bytes;
//! Loader parsing, checksums and Rockusb command framing belong to the caller.
//! The API is available on Windows only.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::Device;
