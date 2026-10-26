// SPDX-License-Identifier: GPL-2.0-or-later
// Derived from rkdeveloptool, Copyright 2017 Fuzhou Rockchip Electronics Co., Ltd.
// Original author: Seth Liu (2017-03-01).
// Rust translation and checked parsing, 2026.

//! Rockchip parameter files and primary/backup GUID partition tables.

use crate::crypto::rockchip_crc32;
use crate::device::Device;
use anyhow::{Context, Result, bail, ensure};
use crc::{CRC_32_ISO_HDLC, Crc};
use std::{fs::File, io::Read, path::Path};
use uuid::Uuid;

const SECTOR: usize = 512;
const ENTRY_SIZE: usize = 128;
const ENTRY_COUNT: usize = 128;
const TABLE_SIZE: usize = ENTRY_SIZE * ENTRY_COUNT;
const PRIMARY_SIZE: usize = 34 * SECTOR;
const BACKUP_SIZE: usize = 33 * SECTOR;
const GPT_CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

/// A partition's location in 512-byte logical sectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub name: String,
    pub start: u64,
    pub sectors: u64,
}

/// A parameter entry; `None` denotes a partition extending to the disk's end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterEntry {
    pub name: String,
    pub start: u64,
    pub sectors: Option<u64>,
    pub bootable: bool,
    pub grow: bool,
    pub uuid: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterTable {
    pub entries: Vec<ParameterEntry>,
}

impl ParameterTable {
    /// Parse Rockchip's `mtdparts` and optional `uuid:name=...` lines.
    pub fn parse(text: &str) -> Result<Self> {
        ensure!(text.len() <= 512 * 1024 - 12, "parameter is too large");
        let mut entries = Vec::new();
        let mut uuids = Vec::new();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((_, uuid_line)) = line.split_once("uuid:") {
                let (name, value) = uuid_line
                    .split_once('=')
                    .context("invalid partition UUID line")?;
                let uuid = Uuid::parse_str(value.trim()).context("invalid partition UUID")?;
                uuids.push((name.trim().to_owned(), uuid));
                continue;
            }
            let Some((_, tail)) = line.split_once("mtdparts") else {
                continue;
            };
            let (_, parts) = tail
                .split_once(':')
                .context("mtdparts has no device separator")?;
            for part in parts.split(',') {
                let (length, tail) = part
                    .trim()
                    .split_once('@')
                    .context("partition has no offset")?;
                let (offset, tail) = tail.split_once('(').context("partition has no name")?;
                let (name_flags, _) = tail
                    .split_once(')')
                    .context("unterminated partition name")?;
                let mut fields = name_flags.trim().split(':');
                let name = fields.next().unwrap_or_default().trim().to_owned();
                ensure!(!name.is_empty(), "empty partition name");
                ensure!(
                    !entries
                        .iter()
                        .any(|entry: &ParameterEntry| entry.name == name),
                    "duplicate partition name: {name}"
                );
                let flags: Vec<_> = fields.collect();
                let length = length.trim();
                let sectors = if length == "-" {
                    None
                } else {
                    Some(parse_hex(length)?)
                };
                ensure!(sectors != Some(0), "zero length partition: {name}");
                entries.push(ParameterEntry {
                    name,
                    start: parse_hex(offset.trim())?,
                    sectors,
                    bootable: flags.iter().any(|flag| flag.contains("bootable")),
                    grow: sectors.is_none() || flags.iter().any(|flag| flag.contains("grow")),
                    uuid: None,
                });
            }
        }
        ensure!(!entries.is_empty(), "no partitions found in parameter file");
        ensure!(entries.len() <= ENTRY_COUNT, "too many partitions");
        for entry in &mut entries {
            entry.uuid = uuids
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&entry.name))
                .map(|(_, id)| *id);
        }
        Ok(Self { entries })
    }

    /// Resolve grow partitions using an exclusive end sector.
    pub fn partitions(&self, end_sector: u64) -> Result<Vec<Partition>> {
        self.entries
            .iter()
            .map(|entry| {
                let sectors = if entry.grow {
                    end_sector
                        .checked_sub(entry.start)
                        .context("partition starts beyond disk")?
                } else {
                    entry.sectors.context("partition length is unknown")?
                };
                let end = entry
                    .start
                    .checked_add(sectors)
                    .context("partition extent overflow")?;
                ensure!(
                    sectors > 0 && end <= end_sector,
                    "partition {} exceeds disk",
                    entry.name
                );
                Ok(Partition {
                    name: entry.name.clone(),
                    start: entry.start,
                    sectors,
                })
            })
            .collect()
    }
}

fn parse_hex(value: &str) -> Result<u64> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .context("parameter sector values must use 0x notation")?;
    u64::from_str_radix(digits, 16).context("invalid parameter sector value")
}

/// Wrap parameter bytes in the on-device PARM container and zero-pad to sectors.
pub fn encode_parameter(text: &[u8]) -> Result<Vec<u8>> {
    let size = u32::try_from(text.len()).context("parameter is too large")?;
    let length = text
        .len()
        .checked_add(12)
        .context("parameter size overflow")?;
    ensure!(length <= 512 * 1024, "parameter is too large");
    let mut bytes = vec![0; length.div_ceil(SECTOR) * SECTOR];
    bytes[..4].copy_from_slice(b"PARM");
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    bytes[8..8 + text.len()].copy_from_slice(text);
    bytes[8 + text.len()..12 + text.len()].copy_from_slice(&rockchip_crc32(text).to_le_bytes());
    Ok(bytes)
}

/// Extract and validate a PARM container, ignoring sector padding.
pub fn decode_parameter(bytes: &[u8]) -> Result<&[u8]> {
    ensure!(bytes.get(..4) == Some(b"PARM"), "invalid parameter magic");
    let length = get_u32(bytes, 4)? as usize;
    let end = 8usize
        .checked_add(length)
        .context("parameter size overflow")?;
    let text = bytes.get(8..end).context("truncated parameter data")?;
    ensure!(
        get_u32(bytes, end)? == rockchip_crc32(text),
        "parameter CRC mismatch"
    );
    Ok(text)
}

/// A Rockchip GPT image: 34 primary sectors followed by 33 backup sectors.
#[derive(Debug, Clone)]
pub struct GptImage {
    primary: Vec<u8>,
    backup: Vec<u8>,
}

impl GptImage {
    pub fn from_parameter(table: &ParameterTable, disk_sectors: u64) -> Result<Self> {
        ensure!(disk_sectors >= 68, "disk is too small for GPT");
        ensure!(
            !table.entries.is_empty() && table.entries.len() <= ENTRY_COUNT,
            "GPT requires between 1 and 128 partitions"
        );
        let partitions = table.partitions(disk_sectors - 33)?;
        validate_partitions(&partitions, 34, disk_sectors - 34)?;
        let mut primary = vec![0u8; PRIMARY_SIZE];
        primary[450] = 0xee;
        put_u32(&mut primary, 454, 1);
        put_u32(&mut primary, 458, u32::MAX);
        primary[510..512].copy_from_slice(&[0x55, 0xaa]);
        {
            let header = &mut primary[SECTOR..2 * SECTOR];
            header[..8].copy_from_slice(b"EFI PART");
            put_u32(header, 8, 0x0001_0000);
            put_u32(header, 12, 92);
            put_u64(header, 24, 1);
            put_u64(header, 32, disk_sectors - 1);
            put_u64(header, 40, 34);
            put_u64(header, 48, disk_sectors - 34);
            header[56..72].copy_from_slice(&Uuid::new_v4().to_bytes_le());
            put_u64(header, 72, 2);
            put_u32(header, 80, ENTRY_COUNT as u32);
            put_u32(header, 84, ENTRY_SIZE as u32);
        }
        for (index, (entry, partition)) in table.entries.iter().zip(&partitions).enumerate() {
            let start = 2 * SECTOR + index * ENTRY_SIZE;
            let bytes = &mut primary[start..start + ENTRY_SIZE];
            bytes[..16].copy_from_slice(&Uuid::new_v4().to_bytes_le());
            bytes[16..32].copy_from_slice(&entry.uuid.unwrap_or_else(Uuid::new_v4).to_bytes_le());
            put_u64(bytes, 32, partition.start);
            put_u64(bytes, 40, partition.start + partition.sectors - 1);
            put_u64(bytes, 48, if entry.bootable { 4 } else { 0 });
            let name: Vec<_> = entry.name.encode_utf16().collect();
            ensure!(
                name.len() <= 36,
                "GPT partition name is too long: {}",
                entry.name
            );
            for (index, unit) in name.into_iter().enumerate() {
                bytes[56 + index * 2..58 + index * 2].copy_from_slice(&unit.to_le_bytes());
            }
        }
        let mut image = Self {
            primary,
            backup: vec![0; BACKUP_SIZE],
        };
        image.refresh()?;
        Ok(image)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == PRIMARY_SIZE + BACKUP_SIZE,
            "GPT image must contain exactly 67 sectors"
        );
        let primary = bytes[..PRIMARY_SIZE].to_vec();
        let backup = bytes[PRIMARY_SIZE..].to_vec();
        parse_gpt(&primary)?;
        ensure!(
            get_u32(&primary, SECTOR + 80)? == ENTRY_COUNT as u32
                && get_u32(&primary, SECTOR + 84)? == ENTRY_SIZE as u32,
            "GPT image must contain 128 entries of 128 bytes"
        );
        ensure!(
            get_u64(&primary, SECTOR + 72)? == 2,
            "unexpected primary GPT entries location"
        );
        validate_header(&backup[TABLE_SIZE..], &backup[..TABLE_SIZE])?;
        ensure!(
            backup[..TABLE_SIZE] == primary[2 * SECTOR..],
            "primary and backup GPT entries differ"
        );
        ensure!(
            get_u64(&backup, TABLE_SIZE + 24)? == get_u64(&primary, SECTOR + 32)?
                && get_u64(&backup, TABLE_SIZE + 32)? == 1,
            "GPT backup locations do not match"
        );
        Ok(Self { primary, backup })
    }

    /// Move the backup table and extend a final partition that reached the old end.
    pub fn resize(&mut self, disk_sectors: u64) -> Result<()> {
        ensure!(disk_sectors >= 68, "disk is too small for GPT");
        let mut candidate = self.clone();
        let old_last = get_u64(&candidate.primary, SECTOR + 48)?;
        let last_index = (0..ENTRY_COUNT).rev().find(|index| {
            let start = 2 * SECTOR + index * ENTRY_SIZE;
            candidate.primary[start..start + 16]
                .iter()
                .any(|byte| *byte != 0)
        });
        if let Some(index) = last_index {
            let offset = 2 * SECTOR + index * ENTRY_SIZE + 40;
            if get_u64(&candidate.primary, offset)? == old_last {
                put_u64(&mut candidate.primary, offset, disk_sectors - 34);
            }
        }
        put_u64(&mut candidate.primary, SECTOR + 32, disk_sectors - 1);
        put_u64(&mut candidate.primary, SECTOR + 48, disk_sectors - 34);
        candidate.refresh()?;
        candidate.partitions()?;
        *self = candidate;
        Ok(())
    }

    pub fn partitions(&self) -> Result<Vec<Partition>> {
        parse_gpt(&self.primary)
    }
    pub fn primary(&self) -> &[u8] {
        &self.primary
    }
    pub fn backup(&self) -> &[u8] {
        &self.backup
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        [self.primary.as_slice(), self.backup.as_slice()].concat()
    }

    fn refresh(&mut self) -> Result<()> {
        let entries_crc = GPT_CRC.checksum(&self.primary[2 * SECTOR..]);
        put_u32(&mut self.primary, SECTOR + 88, entries_crc);
        update_header_crc(&mut self.primary[SECTOR..2 * SECTOR])?;
        self.backup[..TABLE_SIZE].copy_from_slice(&self.primary[2 * SECTOR..]);
        self.backup[TABLE_SIZE..].copy_from_slice(&self.primary[SECTOR..2 * SECTOR]);
        let last = get_u64(&self.primary, SECTOR + 32)?;
        let header = &mut self.backup[TABLE_SIZE..];
        put_u64(header, 24, last);
        put_u64(header, 32, 1);
        put_u64(header, 72, last - 32);
        update_header_crc(header)
    }
}

/// Read a primary GPT and validate its header, entry CRC and partition extents.
pub fn parse_gpt(primary: &[u8]) -> Result<Vec<Partition>> {
    let header = primary
        .get(SECTOR..2 * SECTOR)
        .context("truncated GPT header")?;
    ensure!(header.get(..8) == Some(b"EFI PART"), "no GPT signature");
    let count = get_u32(header, 80)? as usize;
    let stride = get_u32(header, 84)? as usize;
    ensure!(
        count > 0 && stride >= ENTRY_SIZE && stride.is_multiple_of(8),
        "invalid GPT entry dimensions"
    );
    let length = count
        .checked_mul(stride)
        .context("GPT array size overflow")?;
    let offset = get_u64(header, 72)?
        .checked_mul(SECTOR as u64)
        .context("GPT array offset overflow")?;
    let offset = usize::try_from(offset).context("GPT array offset exceeds host address space")?;
    let end = offset
        .checked_add(length)
        .context("GPT array extent overflow")?;
    let entries = primary
        .get(offset..end)
        .context("truncated GPT entry array")?;
    validate_header(header, entries)?;
    ensure!(
        get_u64(header, 24)? == 1,
        "expected primary GPT header at LBA 1"
    );
    let first = get_u64(header, 40)?;
    let last = get_u64(header, 48)?;
    ensure!(
        first <= last && last < get_u64(header, 32)?,
        "invalid GPT usable range"
    );
    let mut partitions = Vec::new();
    for entry in entries.chunks_exact(stride) {
        if entry[..16].iter().all(|byte| *byte == 0) {
            continue;
        }
        let start = get_u64(entry, 32)?;
        let end = get_u64(entry, 40)?;
        let sectors = end
            .checked_sub(start)
            .and_then(|value| value.checked_add(1))
            .context("invalid GPT partition extent")?;
        let name_units: Vec<_> = entry[56..128]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_le_bytes(*bytes))
            .take_while(|unit| *unit != 0)
            .collect();
        let name = String::from_utf16(&name_units).context("invalid UTF-16 GPT partition name")?;
        partitions.push(Partition {
            name,
            start,
            sectors,
        });
    }
    validate_partitions(&partitions, first, last)?;
    Ok(partitions)
}

fn validate_partitions(partitions: &[Partition], first: u64, last: u64) -> Result<()> {
    let mut ranges = Vec::with_capacity(partitions.len());
    for partition in partitions {
        let end = partition
            .start
            .checked_add(partition.sectors)
            .context("partition extent overflow")?;
        ensure!(
            partition.sectors > 0 && partition.start >= first && end - 1 <= last,
            "partition {} exceeds usable disk range",
            partition.name
        );
        ranges.push((partition.start, end));
    }
    ranges.sort_unstable();
    ensure!(
        ranges.windows(2).all(|pair| pair[0].1 <= pair[1].0),
        "overlapping partitions"
    );
    Ok(())
}

fn validate_header(header: &[u8], entries: &[u8]) -> Result<()> {
    ensure!(
        header.get(..8) == Some(b"EFI PART"),
        "invalid GPT signature"
    );
    ensure!(
        get_u32(header, 8)? == 0x0001_0000,
        "unsupported GPT revision"
    );
    let size = get_u32(header, 12)? as usize;
    ensure!(
        (92..=SECTOR).contains(&size) && header.len() >= size,
        "invalid GPT header size"
    );
    let expected = get_u32(header, 16)?;
    let mut check = header[..size].to_vec();
    put_u32(&mut check, 16, 0);
    ensure!(
        expected == GPT_CRC.checksum(&check),
        "GPT header CRC mismatch"
    );
    ensure!(
        get_u32(header, 88)? == GPT_CRC.checksum(entries),
        "GPT entry array CRC mismatch"
    );
    Ok(())
}

fn update_header_crc(header: &mut [u8]) -> Result<()> {
    let size = get_u32(header, 12)? as usize;
    ensure!(
        (92..=SECTOR).contains(&size) && size <= header.len(),
        "invalid GPT header size"
    );
    put_u32(header, 16, 0);
    put_u32(header, 16, GPT_CRC.checksum(&header[..size]));
    Ok(())
}

fn get_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).context("offset overflow")?;
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..end)
            .context("truncated 32-bit field")?
            .try_into()?,
    ))
}
fn get_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset.checked_add(8).context("offset overflow")?;
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..end)
            .context("truncated 64-bit field")?
            .try_into()?,
    ))
}
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_parameter_input(path: &Path) -> Result<Vec<u8>> {
    const MAX: usize = 512 * 1024;
    let file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((MAX + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX, "partition input is too large");
    Ok(bytes)
}

impl Device {
    fn load_partitions(&mut self) -> Result<(Vec<Partition>, bool)> {
        match self.read_lba(0, 34).and_then(|bytes| parse_gpt(&bytes)) {
            Ok(partitions) => Ok((partitions, true)),
            Err(gpt_error) => {
                let bytes = self
                    .read_lba(0x2000, 1024)
                    .context("cannot read parameter partition table")?;
                let text = std::str::from_utf8(decode_parameter(&bytes)?)
                    .context("parameter is not UTF-8")?;
                let table = ParameterTable::parse(text)
                    .with_context(|| format!("no valid GPT ({gpt_error}) or parameter table"))?;
                let disk_sectors = self.read_flash_info()?.sectors as u64;
                Ok((table.partitions(disk_sectors)?, false))
            }
        }
    }

    pub fn partitions(&mut self) -> Result<Vec<Partition>> {
        Ok(self.load_partitions()?.0)
    }

    pub fn write_partition(&mut self, name: &str, path: impl AsRef<Path>) -> Result<()> {
        let (partitions, gpt) = self.load_partitions()?;
        let partition = partitions
            .into_iter()
            .find(|entry| {
                if gpt {
                    entry.name == name
                } else {
                    entry.name.eq_ignore_ascii_case(name)
                }
            })
            .with_context(|| format!("partition not found: {name}"))?;
        let start = u32::try_from(partition.start)
            .context("partition offset exceeds Rockusb address range")?;
        let sectors = u32::try_from(partition.sectors)
            .context("partition size exceeds Rockusb address range")?;
        self.write_image(start, path, Some(sectors))
    }

    pub fn write_parameter(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let text = read_parameter_input(path.as_ref())?;
        let bytes = encode_parameter(&text)?;
        self.write_lba(0x2000, &bytes)
    }

    pub fn write_gpt(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let bytes = read_parameter_input(path.as_ref())?;
        let sectors = self.read_flash_info()?.sectors;
        let mut image = if path.as_ref().to_string_lossy().contains(".img")
            || bytes.get(SECTOR..SECTOR + 8) == Some(b"EFI PART")
        {
            GptImage::from_bytes(&bytes)?
        } else {
            let text = std::str::from_utf8(&bytes).context("parameter is not UTF-8")?;
            GptImage::from_parameter(&ParameterTable::parse(text)?, sectors as u64)?
        };
        image.resize(sectors as u64)?;
        if sectors < 33 {
            bail!("flash is too small for GPT");
        }
        self.write_lba(0, image.primary())?;
        self.write_lba(sectors - 33, image.backup())?;
        Ok(())
    }
}
