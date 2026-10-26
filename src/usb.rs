// Copyright (C) 2017 Fuzhou Rockchip Electronics Co., Ltd.
// Original author: Seth Liu (2017-03-01).
// Rust translation and adaptations, 2026.
// SPDX-License-Identifier: GPL-2.0-or-later

//! USB discovery and synchronous transfers for Rockchip devices.

use anyhow::{Context, Result, bail, ensure};
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Bulk, ControlOut, ControlType, In, Out, Recipient};
use nusb::{Endpoint, Interface, MaybeFuture};
use std::fmt;
use std::time::Duration;

/// The mode reported by the device's USB descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceMode {
    Maskrom,
    Loader,
    MassStorage,
}

impl fmt::Display for DeviceMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Maskrom => "Maskrom",
            Self::Loader => "Loader",
            Self::MassStorage => "MSC",
        })
    }
}

/// A discovered device; opening it does not depend on enumeration order.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub vid: u16,
    pub pid: u16,
    pub mode: DeviceMode,
    pub bcd_usb: u16,
    /// Host controller and complete physical port chain.
    pub location: String,
    /// Upstream bus/port identifier, when a numeric bus ID is available.
    pub location_id: u32,
    pub(crate) inner: nusb::DeviceInfo,
}

impl DeviceInfo {
    pub fn open(&self) -> Result<crate::device::Device> {
        crate::device::Device::open(self)
    }
}

/// Enumerate Rockusb devices and the legacy Rockchip MSC identifiers.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let mut result = Vec::new();
    for inner in nusb::list_devices()
        .wait()
        .context("enumerate USB devices")?
    {
        let vid = inner.vendor_id();
        let pid = inner.product_id();
        let msc = matches!(
            (vid, pid),
            (0x071b, 0x3203 | 0x3205) | (0x0bb4, 0x2910) | (0x2207, 0 | 0x10)
        );
        let rockusb = matches!((vid, pid), (0x071b, 0x3201 | 0x3228 | 0x3226))
            || (vid == 0x2207 && pid >> 8 != 0);
        if !msc && !rockusb {
            continue;
        }
        let mode = if msc {
            DeviceMode::MassStorage
        } else if inner.usb_version() & 1 == 0 {
            DeviceMode::Maskrom
        } else {
            DeviceMode::Loader
        };
        let ports = inner
            .port_chain()
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(".");
        let bus = inner
            .bus_id()
            .parse::<u32>()
            .unwrap_or(u32::from(inner.device_address()));
        let port = u32::from(inner.port_chain().last().copied().unwrap_or(0));
        result.push(DeviceInfo {
            vid,
            pid,
            mode,
            bcd_usb: inner.usb_version(),
            location: format!("{}-{ports}", inner.bus_id()),
            location_id: (bus << 8) | port,
            inner,
        });
    }
    Ok(result)
}

pub(crate) struct Transport {
    interface: Interface,
    input: Endpoint<Bulk, In>,
    output: Endpoint<Bulk, Out>,
    _device: nusb::Device,
}

impl Transport {
    pub(crate) fn open(info: &DeviceInfo) -> Result<Self> {
        let device = info.inner.open().wait().with_context(|| {
            format!(
                "open USB device {} (Windows requires WinUSB)",
                info.location
            )
        })?;
        if device.active_configuration().is_err() {
            let configuration = device
                .configurations()
                .next()
                .context("device has no USB configuration")?
                .configuration_value();
            device
                .set_configuration(configuration)
                .wait()
                .context("select USB configuration")?;
        }
        let configuration = device
            .active_configuration()
            .context("read active USB configuration")?;
        let mut selected = None;
        for descriptor in configuration.interface_alt_settings() {
            let expected = if info.mode == DeviceMode::MassStorage {
                (8, 6, 0x50)
            } else {
                (0xff, 6, 5)
            };
            if (
                descriptor.class(),
                descriptor.subclass(),
                descriptor.protocol(),
            ) != expected
            {
                continue;
            }
            let mut input = None;
            let mut output = None;
            for endpoint in descriptor
                .endpoints()
                .filter(|ep| ep.transfer_type() == TransferType::Bulk)
            {
                if endpoint.address() & 0x80 != 0 {
                    input.get_or_insert(endpoint.address());
                } else {
                    output.get_or_insert(endpoint.address());
                }
            }
            if let (Some(input), Some(output)) = (input, output) {
                selected = Some((
                    descriptor.interface_number(),
                    descriptor.alternate_setting(),
                    input,
                    output,
                ));
                break;
            }
        }
        let (number, alternate, input, output) =
            selected.context("no compatible Rockusb bulk interface")?;
        let interface = device
            .detach_and_claim_interface(number)
            .wait()
            .context("claim Rockusb interface")?;
        if interface.get_alt_setting() != alternate {
            interface
                .set_alt_setting(alternate)
                .wait()
                .context("select Rockusb alternate setting")?;
        }
        Ok(Self {
            input: interface.endpoint::<Bulk, In>(input)?,
            output: interface.endpoint::<Bulk, Out>(output)?,
            interface,
            _device: device,
        })
    }

    pub(crate) fn write(&mut self, data: &[u8]) -> Result<()> {
        ensure!(data.len() <= u32::MAX as usize, "USB transfer is too large");
        self.output.submit(data.to_vec().into());
        let completion = loop {
            if let Some(completion) = self.output.wait_next_complete(Duration::from_secs(60)) {
                break completion;
            }
        };
        completion.status.context("Rockusb bulk OUT")?;
        ensure!(
            completion.actual_len == data.len(),
            "short bulk write: expected {}, sent {}",
            data.len(),
            completion.actual_len
        );
        Ok(())
    }

    pub(crate) fn read(&mut self, length: usize, timeout: Option<Duration>) -> Result<Vec<u8>> {
        let data = self.read_packet(length, timeout)?;
        ensure!(
            data.len() <= length,
            "oversized bulk response: expected at most {length}, received {}",
            data.len()
        );
        Ok(data)
    }

    // Status resynchronization must be able to discard stale full packets.
    // Normal data reads still enforce their declared maximum via read().
    pub(crate) fn read_packet(
        &mut self,
        length: usize,
        timeout: Option<Duration>,
    ) -> Result<Vec<u8>> {
        ensure!(length != 0, "zero-length bulk read");
        let packet_size = self.input.max_packet_size();
        ensure!(packet_size != 0, "invalid USB maximum packet size");
        // nusb requires IN buffers to be multiples of wMaxPacketSize. Short
        // packets terminate transfers, including the 13-byte CSW.
        let requested = length
            .checked_add(packet_size - 1)
            .context("USB length overflow")?
            / packet_size
            * packet_size;
        ensure!(requested <= u32::MAX as usize, "USB transfer is too large");
        let buffer = Buffer::new(requested);
        let completion = if let Some(timeout) = timeout {
            self.input.transfer_blocking(buffer, timeout)
        } else {
            self.input.submit(buffer);
            loop {
                if let Some(completion) = self.input.wait_next_complete(Duration::from_secs(60)) {
                    break completion;
                }
            }
        };
        Ok(completion
            .into_result()
            .context("Rockusb bulk IN")?
            .into_vec())
    }

    pub(crate) fn control_download(&self, index: u16, data: &[u8]) -> Result<()> {
        if index != 0x471 && index != 0x472 {
            bail!("unsupported Maskrom download request {index:#x}");
        }
        // Interface handles also work with WinUSB. Recipient stays Device:
        // the index is a ROM request, not an interface number.
        self.interface
            .control_out(
                ControlOut {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: 0x0c,
                    value: 0,
                    index,
                    data,
                },
                Duration::from_secs(60),
            )
            .wait()
            .context("Maskrom control download")
    }
}
