// Copyright (C) 2017 Fuzhou Rockchip Electronics Co., Ltd.
// Original author: Seth Liu (2017-03-01).
// Rust translation and adaptations, 2026.
// SPDX-License-Identifier: GPL-2.0-or-later

//! Rockusb commands and device operations.

use crate::boot::{BootImage, EntryKind};
use crate::crypto::crc_ccitt;
use crate::usb::{DeviceInfo, DeviceMode, Transport};
use anyhow::{Context, Result, bail, ensure};
use std::{thread, time::Duration};

const SECTOR_SIZE: usize = 512;

/// Progress events are emitted synchronously on the calling thread.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub stage: &'static str,
    pub current: u64,
    pub total: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadyStatus {
    pub ready: bool,
    pub current: u16,
    pub total: u16,
}

/// Flash geometry. Sizes are in 512-byte sectors, matching the wire format.
#[derive(Clone, Debug)]
pub struct FlashInfo {
    pub sectors: u32,
    pub block_size: u16,
    pub page_size: u8,
    pub ecc_bits: u8,
    pub access_time: u8,
    pub manufacturer: u8,
    pub chip_select: u8,
    pub raw: Vec<u8>,
}

impl FlashInfo {
    pub fn manufacturer_name(&self) -> &'static str {
        [
            "SAMSUNG", "TOSHIBA", "HYNIX", "INFINEON", "MICRON", "RENESAS", "ST", "INTEL",
        ]
        .get(usize::from(self.manufacturer))
        .copied()
        .unwrap_or("UNKNOWN")
    }
}

struct Status {
    residue: [u8; 4],
    status: u8,
}

/// An exclusively claimed USB interface. Operations on one device are serial.
pub struct Device {
    pub info: DeviceInfo,
    transport: Transport,
    tag: u32,
    progress: Option<Box<dyn FnMut(Progress)>>,
}

impl Device {
    pub fn open(info: &DeviceInfo) -> Result<Self> {
        Ok(Self {
            info: info.clone(),
            transport: Transport::open(info)?,
            tag: 0,
            progress: None,
        })
    }

    pub fn set_progress_callback(&mut self, callback: impl FnMut(Progress) + 'static) {
        self.progress = Some(Box::new(callback));
    }

    pub fn clear_progress_callback(&mut self) {
        self.progress = None;
    }

    pub(crate) fn report_progress(&mut self, stage: &'static str, current: u64, total: u64) {
        if let Some(callback) = &mut self.progress {
            callback(Progress {
                stage,
                current,
                total,
            });
        }
    }

    fn send(
        &mut self,
        opcode: u8,
        subcode: u8,
        address: u32,
        count: u16,
        length: u32,
        lun: u8,
    ) -> Result<u32> {
        ensure!(
            self.info.mode != DeviceMode::MassStorage,
            "operation requires Maskrom or Loader mode"
        );
        self.tag = self.tag.wrapping_add(1);
        let mut cbw = [0u8; 31];
        cbw[0..4].copy_from_slice(b"USBC");
        cbw[4..8].copy_from_slice(&self.tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&length.to_le_bytes());
        cbw[12] = if matches!(opcode, 0 | 1 | 0x14 | 0x1a | 0x1b | 0x2b | 0xaa) {
            0x80
        } else {
            0
        };
        cbw[13] = lun;
        cbw[14] = if matches!(opcode, 0 | 1 | 0x1a | 0x1b | 0x2a | 0x2b | 0xaa | 0xff) {
            6
        } else {
            10
        };
        cbw[15] = opcode;
        cbw[16] = subcode;
        cbw[17..21].copy_from_slice(&address.to_be_bytes());
        cbw[22..24].copy_from_slice(&count.to_be_bytes());
        self.transport.write(&cbw)?;
        Ok(self.tag)
    }

    fn status(&mut self, tag: u32, resync: bool) -> Result<Status> {
        let mut data = self.transport.read_packet(13, None)?;
        let mut drained = 0;
        let mut failures = 0;
        loop {
            if data.len() == 13 && &data[..4] == b"USBS" && data[4..8] == tag.to_le_bytes() {
                ensure!(data[12] <= 1, "Rockusb phase error: status {}", data[12]);
                return Ok(Status {
                    residue: data[8..12].try_into().expect("fixed CSW field"),
                    status: data[12],
                });
            }
            ensure!(
                resync && drained < 16 * 1024 && failures < 3,
                "invalid or mismatched Rockusb command status"
            );
            match self.transport.read_packet(13, Some(Duration::from_secs(5))) {
                Ok(next) => {
                    drained += next.len();
                    if next.len() != 13 {
                        failures += 1;
                    }
                    data = next;
                }
                Err(_) => {
                    failures += 1;
                }
            }
        }
    }

    fn success(status: Status) -> Result<()> {
        ensure!(status.status == 0, "device rejected Rockusb command");
        ensure!(
            status.residue == [0; 4],
            "Rockusb command left {} bytes untransferred",
            u32::from_le_bytes(status.residue)
        );
        Ok(())
    }

    fn fixed_read(&mut self, opcode: u8, length: usize) -> Result<Vec<u8>> {
        let tag = self.send(opcode, 0, 0, 0, u32::try_from(length)?, 0)?;
        let data = self.transport.read(length, Some(Duration::from_secs(5)))?;
        ensure!(
            data.len() == length,
            "short Rockusb response: expected {length}, received {}",
            data.len()
        );
        // Some Rockchip loaders put the response length, in big-endian order,
        // in the CSW residue for metadata queries. The exact payload length
        // above, matching command tag and successful status validate the read.
        let status = self.status(tag, false)?;
        ensure!(status.status == 0, "device rejected Rockusb command");
        Ok(data)
    }

    pub fn read_chip_info(&mut self) -> Result<[u8; 16]> {
        Ok(self
            .fixed_read(0x1b, 16)?
            .try_into()
            .expect("checked response length"))
    }

    pub fn read_flash_id(&mut self) -> Result<Vec<u8>> {
        self.fixed_read(1, 5)
    }

    pub fn read_capability(&mut self) -> Result<[u8; 8]> {
        Ok(self
            .fixed_read(0xaa, 8)?
            .try_into()
            .expect("checked response length"))
    }

    pub fn read_flash_info(&mut self) -> Result<FlashInfo> {
        let tag = self.send(0x1a, 0, 0, 0, 11, 0)?;
        let raw = self.transport.read(512, Some(Duration::from_secs(5)))?;
        ensure!(raw.len() >= 11, "short flash geometry response");
        let status = self.status(tag, false)?;
        ensure!(status.status == 0, "device rejected flash geometry request");
        Ok(FlashInfo {
            sectors: u32::from_le_bytes(raw[0..4].try_into().expect("checked response length")),
            block_size: u16::from_le_bytes(raw[4..6].try_into().expect("checked response length")),
            page_size: raw[6],
            ecc_bits: raw[7],
            access_time: raw[8],
            manufacturer: raw[9],
            chip_select: raw[10],
            raw,
        })
    }

    pub fn read_storage(&mut self) -> Result<u8> {
        let bits = u32::from_le_bytes(
            self.fixed_read(0x2b, 4)?
                .try_into()
                .expect("checked response length"),
        );
        Ok(if bits == 0 {
            255
        } else {
            bits.trailing_zeros() as u8
        })
    }

    pub fn change_storage(&mut self, storage: u8) -> Result<()> {
        let tag = self.send(0x2a, storage, 0, 0, 0, 0)?;
        Self::success(self.status(tag, true)?)?;
        // Firmware can acknowledge an unavailable storage selection. The
        // readback is required to establish that the switch actually happened.
        let selected = self.read_storage()?;
        ensure!(
            selected == storage,
            "storage {storage} is unavailable; current storage is {selected}"
        );
        Ok(())
    }

    pub fn reset(&mut self, subcode: u8) -> Result<()> {
        ensure!(subcode <= 5, "reset subcode must be between 0 and 5");
        let tag = self.send(0xff, subcode, 0, 0, 0, 0)?;
        Self::success(self.status(tag, true)?)
    }

    fn ready_with_subcode(&mut self, subcode: u8) -> Result<ReadyStatus> {
        let tag = self.send(0, subcode, 0, 0, 0, 0)?;
        let status = self.status(tag, true)?;
        Ok(ReadyStatus {
            ready: status.status == 0,
            total: u16::from_be_bytes([status.residue[0], status.residue[1]]),
            current: u16::from_be_bytes([status.residue[2], status.residue[3]]),
        })
    }

    pub fn test_ready(&mut self) -> Result<ReadyStatus> {
        self.ready_with_subcode(0)
    }

    pub fn wait_ready(&mut self) -> Result<()> {
        loop {
            let mut last_error = None;
            let mut ready = None;
            for _ in 0..3 {
                match self.test_ready() {
                    Ok(status) => {
                        ready = Some(status);
                        break;
                    }
                    Err(error) => {
                        last_error = Some(error);
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
            let status = ready.ok_or_else(|| last_error.expect("three failed attempts"))?;
            if status.ready {
                return Ok(());
            }
            ensure!(
                status.current <= status.total,
                "invalid device readiness progress"
            );
            ensure!(
                self.info.mode != DeviceMode::Loader || status.total != 0,
                "Loader reported zero readiness total"
            );
            self.report_progress(
                "Test Device",
                u64::from(status.current),
                u64::from(status.total),
            );
            thread::sleep(Duration::from_secs(1));
        }
    }

    fn range(start: u32, count: u32) -> Result<()> {
        ensure!(
            u64::from(start) + u64::from(count) <= u64::from(u32::MAX) + 1,
            "LBA range exceeds the Rockusb address space"
        );
        Ok(())
    }

    /// Read at most 65535 sectors; use `read_to_file` for larger streaming reads.
    pub fn read_lba(&mut self, start: u32, count: u32) -> Result<Vec<u8>> {
        Self::range(start, count)?;
        let count =
            u16::try_from(count).context("one read_lba call supports at most 65535 sectors")?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let length = usize::from(count) * SECTOR_SIZE;
        let tag = self.send(0x14, 0, start, count, u32::try_from(length)?, 0)?;
        let data = self.transport.read(length, None)?;
        ensure!(
            data.len() == length,
            "short LBA read: expected {length}, received {}",
            data.len()
        );
        Self::success(self.status(tag, false)?)?;
        Ok(data)
    }

    /// Write up to 65535 complete 512-byte sectors in one protocol command.
    /// Keeping IDB writes together preserves the firmware's image semantics.
    pub fn write_lba(&mut self, start: u32, data: &[u8]) -> Result<()> {
        ensure!(
            data.len().is_multiple_of(SECTOR_SIZE),
            "write_lba data must contain whole 512-byte sectors"
        );
        let sectors = u16::try_from(data.len() / SECTOR_SIZE)
            .context("one write_lba call supports at most 65535 sectors")?;
        Self::range(start, u32::from(sectors))?;
        if sectors == 0 {
            return Ok(());
        }
        let tag = self.send(0x15, 0, start, sectors, data.len() as u32, 0)?;
        self.transport.write(data)?;
        Self::success(self.status(tag, false)?)
    }

    /// Write legacy NAND sectors, each containing 512 data and 16 spare bytes.
    pub fn write_sector(&mut self, start: u32, data: &[u8]) -> Result<()> {
        ensure!(
            data.len().is_multiple_of(528),
            "NAND write requires complete 528-byte sectors"
        );
        let count = u16::try_from(data.len() / 528)?;
        ensure!(
            count != 0 && count <= 32,
            "NAND write requires between 1 and 32 sectors"
        );
        Self::range(start, u32::from(count))?;
        let tag = self.send(5, 0, start, count, data.len() as u32, 0)?;
        self.transport.write(data)?;
        Self::success(self.status(tag, false)?)
    }

    pub fn erase_lba(&mut self, start: u32, count: u32) -> Result<()> {
        Self::range(start, count)?;
        let mut completed = 0;
        while completed < count {
            let chunk = (count - completed).min(32768) as u16;
            let tag = self.send(
                0x25,
                0,
                start.checked_add(completed).context("LBA overflow")?,
                chunk,
                0,
                0,
            )?;
            Self::success(self.status(tag, false)?)?;
            completed += u32::from(chunk);
        }
        Ok(())
    }

    /// Erase up to 16 NAND blocks. Known bad blocks are skipped by the device.
    pub fn erase_block(&mut self, chip: u8, start: u32, count: u32, force: bool) -> Result<()> {
        ensure!(
            chip < 8 && count != 0 && count <= 16,
            "invalid NAND erase chip or block count"
        );
        Self::range(start, count)?;
        let tag = self.send(
            if force { 0x0b } else { 6 },
            0,
            start,
            count as u16,
            0,
            chip,
        )?;
        let status = self.status(tag, false)?;
        // Status 1 is ERR_FOUND_BAD_BLOCK for this command; the upstream
        // erase workflow deliberately continues past such blocks.
        ensure!(status.residue == [0; 4], "incomplete NAND erase command");
        Ok(())
    }

    pub fn erase_flash(&mut self) -> Result<()> {
        let info = self.read_flash_info()?;
        ensure!(
            info.sectors != 0 && info.block_size != 0 && info.page_size != 0,
            "invalid flash geometry"
        );
        let emmc = self.read_flash_id()?.starts_with(b"EMMC");
        let direct_lba = self
            .read_capability()
            .map(|caps| caps[0] & 1 != 0)
            .unwrap_or(false);
        if emmc || direct_lba {
            let mut completed = 0;
            while completed < info.sectors {
                let count = (info.sectors - completed).min(32768);
                self.erase_lba(completed, count)?;
                completed += count;
                self.report_progress("Erase Flash", u64::from(completed), u64::from(info.sectors));
            }
        } else {
            let blocks = info.sectors / u32::from(info.block_size);
            ensure!(
                blocks != 0 && info.chip_select != 0,
                "invalid NAND block count or chip mask"
            );
            let total = u64::from(blocks) * u64::from(info.chip_select.count_ones());
            let mut completed = 0u64;
            for chip in 0..8 {
                if info.chip_select & (1 << chip) == 0 {
                    continue;
                }
                let mut position = 0;
                while position < blocks {
                    let count = (blocks - position).min(16);
                    self.erase_block(chip, position, count, true)?;
                    position += count;
                    completed += u64::from(count);
                    self.report_progress("Erase Flash", completed, total);
                }
            }
        }
        Ok(())
    }

    pub fn download_boot(&mut self, image: &BootImage) -> Result<()> {
        ensure!(
            self.info.mode == DeviceMode::Maskrom,
            "download_boot requires Maskrom mode"
        );
        let total = image
            .entries
            .iter()
            .filter(|entry| entry.kind != EntryKind::Loader)
            .map(|entry| entry.data.len() as u64)
            .sum();
        let mut completed = 0;
        for kind in [EntryKind::Entry471, EntryKind::Entry472] {
            for entry in image.entries.iter().filter(|entry| entry.kind == kind) {
                if entry.data.is_empty() {
                    continue;
                }
                let mut data = entry.data.clone();
                let remainder = data.len() % 4096;
                if remainder == 4095 {
                    data.push(0);
                }
                let crc = crc_ccitt(&data);
                data.extend_from_slice(&crc.to_be_bytes());
                let request = if kind == EntryKind::Entry471 {
                    0x471
                } else {
                    0x472
                };
                for chunk in data.chunks(4096) {
                    self.transport.control_download(request, chunk)?;
                }
                if remainder == 4094 {
                    self.transport.control_download(request, &[0])?;
                }
                completed += entry.data.len() as u64;
                self.report_progress("Download Boot", completed, total);
                if entry.delay_ms != 0 {
                    thread::sleep(Duration::from_millis(u64::from(entry.delay_ms)));
                }
            }
        }
        ensure!(total != 0, "Loader contains no Maskrom download entries");
        thread::sleep(Duration::from_secs(1));
        Ok(())
    }

    pub fn upgrade_loader(&mut self, image: &BootImage) -> Result<()> {
        ensure!(
            self.info.mode == DeviceMode::Maskrom,
            "upgrade_loader requires Maskrom mode"
        );
        if image.has_flash_head() {
            ensure!(
                self.read_capability()?[1] & 1 != 0,
                "device does not support the new ID block format"
            );
        }
        let idb = image.idb()?;
        if idb.is_empty() {
            bail!("empty ID block image");
        }
        self.write_lba(64, &idb)?;
        self.report_progress("Upgrade Loader", idb.len() as u64, idb.len() as u64);
        Ok(())
    }
}
