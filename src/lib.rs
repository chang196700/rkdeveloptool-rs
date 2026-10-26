// SPDX-License-Identifier: GPL-2.0-or-later
//! Rockchip USB device access and boot image processing.
//!
//! Device operations are synchronous. Library functions do not print to the
//! terminal; use the device progress callback to observe long operations.
//! Protocol and format behavior is derived from Rockchip rkdeveloptool
//! 304f073752fd25c854e1bcf05d8e7f925b1f4e14 (GPL-2.0-or-later).

pub mod boot;
pub mod crypto;
pub mod device;
pub mod image;
pub mod partition;
pub mod usb;

pub use anyhow::{Error, Result};
pub use boot::{BootEntry, BootImage, EntryKind};
pub use device::{Device, FlashInfo, Progress};
pub use partition::Partition;
pub use usb::{DeviceInfo, DeviceMode, list_devices};
