// Copyright (C) 2008-2017 Fuzhou Rockchip Electronics Co., Ltd.
// Original author: Seth Liu (2017-03-01).
// Rust translation, checked parsing and I/O: 2026-10-26.
// SPDX-License-Identifier: GPL-2.0-or-later
//! Rockchip BOOT/LDR loaders, RKFW containers, and the boot merger format.

use crate::crypto::{crc16, rc4, rockchip_crc32};
use anyhow::{Context, Result, ensure};
use chrono::{Datelike, Local, Timelike};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

const HEADER_SIZE: usize = 102;
const ENTRY_SIZE: usize = 57;
const SECTOR: usize = 512;
const ALIGNMENT: usize = 2048;

/// Default configuration generated when `pack` cannot find `config.ini`.
pub const DEFAULT_PACK_CONFIG: &str = "[CHIP_NAME]\nNAME=RK320A\n[VERSION]\nMAJOR=2\nMINOR=30\n[CODE471_OPTION]\nNUM=1\nPath1=rk3288_ddr_400MHZ_v1.01.bin\n[CODE472_OPTION]\nNUM=1\nPath1=rk3288_usbplug_v2.32.bin\n[LOADER_OPTION]\nNUM=2\nLOADER1=FlashData\nLOADER2=FlashBoot\nFlashData=rk3288_ddr_400MHZ_v1.01.bin\nFlashBoot=u-boot.bin\n[OUTPUT]\nPATH=rk3288_bootloader_test.bin\n";

/// Entry group in a BOOT/LDR file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum EntryKind {
    Entry471 = 1,
    Entry472 = 2,
    Loader = 4,
}

/// Raw on-disk entry bytes. Maskrom download uses `data` without decryption.
#[derive(Clone, Debug)]
pub struct BootEntry {
    pub kind: EntryKind,
    pub name: String,
    pub data: Vec<u8>,
    pub delay_ms: u32,
}

impl BootEntry {
    /// Decode the boot-merger stream transform. Loader streams restart every
    /// 512 bytes; USB download streams restart only at entry boundaries.
    pub fn decoded_data(&self) -> Vec<u8> {
        let mut bytes = self.data.clone();
        if self.kind == EntryKind::Loader {
            for sector in bytes.chunks_mut(SECTOR) {
                rc4(sector);
            }
        } else {
            // Upstream P_RC4 accepts a uint16_t length. Preserve this on-disk
            // behavior so large code entries interoperate with its merger.
            let length = bytes.len() & 0xffff;
            rc4(&mut bytes[..length]);
        }
        bytes
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReleaseTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl ReleaseTime {
    fn now() -> Self {
        let now = Local::now();
        Self {
            year: now.year() as u16,
            month: now.month() as u8,
            day: now.day() as u8,
            hour: now.hour() as u8,
            minute: now.minute() as u8,
            second: now.second() as u8,
        }
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            year: u16_at(bytes, 0)?,
            month: bytes[2],
            day: bytes[3],
            hour: bytes[4],
            minute: bytes[5],
            second: bytes[6],
        })
    }

    fn encode(self, bytes: &mut [u8]) {
        bytes[..2].copy_from_slice(&self.year.to_le_bytes());
        bytes[2..7].copy_from_slice(&[self.month, self.day, self.hour, self.minute, self.second]);
    }
}

/// A checked BOOT or LDR image, including raw entry payloads.
#[derive(Clone, Debug)]
pub struct BootImage {
    pub version: u32,
    pub merge_version: u32,
    pub release_time: ReleaseTime,
    pub chip_type: u32,
    pub rc4_disabled: bool,
    pub signed: bool,
    pub entries: Vec<BootEntry>,
}

impl BootImage {
    /// Open a standalone loader or extract the loader from an RKFW container.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut file =
            File::open(path).with_context(|| format!("open loader {}", path.display()))?;
        let mut magic = [0; 4];
        file.read_exact(&mut magic).context("read loader magic")?;
        if &magic == b"RKFW" {
            return FirmwareImage::open(path)?.boot();
        }
        file.rewind()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Self::from_bytes(&bytes)
    }

    /// Parse bytes after checking the loader checksum, tables and payload ranges.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.starts_with(b"RKFW") {
            let header = FirmwareHeader::parse(bytes, bytes.len() as u64)?;
            let start = usize::try_from(header.boot_offset)?;
            let end = start
                .checked_add(usize::try_from(header.boot_size)?)
                .context("loader range overflow")?;
            let boot = bytes.get(start..end).context("truncated RKFW loader")?;
            ensure!(
                boot.starts_with(b"BOOT") || boot.starts_with(b"LDR "),
                "RKFW boot payload is not a BOOT/LDR loader"
            );
            return Self::from_bytes(boot);
        }
        ensure!(bytes.len() >= HEADER_SIZE + 4, "truncated boot header");
        ensure!(
            bytes.starts_with(b"BOOT") || bytes.starts_with(b"LDR "),
            "invalid BOOT/LDR magic"
        );
        let body_len = bytes.len() - 4;
        ensure!(
            rockchip_crc32(&bytes[..body_len]) == u32_at(bytes, body_len)?,
            "boot CRC32 mismatch"
        );
        let header_size = usize::from(u16_at(bytes, 4)?);
        ensure!(
            (HEADER_SIZE..=body_len).contains(&header_size),
            "invalid boot header size"
        );
        let mut entries = Vec::new();
        let mut table_end = header_size;
        let mut ranges = Vec::new();
        for (kind, field) in [
            (EntryKind::Entry471, 25),
            (EntryKind::Entry472, 31),
            (EntryKind::Loader, 37),
        ] {
            let count = usize::from(bytes[field]);
            if count == 0 {
                continue;
            }
            let offset = usize::try_from(u32_at(bytes, field + 1)?)?;
            let stride = usize::from(bytes[field + 5]);
            ensure!(
                stride >= ENTRY_SIZE && offset >= header_size,
                "invalid boot entry table"
            );
            let end = offset
                .checked_add(count.checked_mul(stride).context("entry table overflow")?)
                .context("entry table overflow")?;
            ensure!(end <= body_len, "truncated boot entry table");
            table_end = table_end.max(end);
            for index in 0..count {
                let entry = &bytes[offset + index * stride..offset + index * stride + stride];
                ensure!(
                    usize::from(entry[0]) >= ENTRY_SIZE && usize::from(entry[0]) <= stride,
                    "invalid boot entry size"
                );
                ensure!(
                    u32_at(entry, 1)? == kind as u32,
                    "boot entry type does not match table"
                );
                let mut name = Vec::new();
                for pair in entry[5..45].as_chunks::<2>().0 {
                    let ch = u16::from_le_bytes([pair[0], pair[1]]);
                    if ch == 0 {
                        break;
                    }
                    name.push(ch);
                }
                let name = String::from_utf16(&name).context("invalid UTF-16 boot entry name")?;
                let start = usize::try_from(u32_at(entry, 45)?)?;
                let end = start
                    .checked_add(usize::try_from(u32_at(entry, 49)?)?)
                    .context("entry data overflow")?;
                ensure!(end <= body_len, "boot entry payload outside file");
                ranges.push((start, end));
                entries.push(BootEntry {
                    kind,
                    name,
                    data: bytes[start..end].to_vec(),
                    delay_ms: u32_at(entry, 53)?,
                });
            }
        }
        ensure!(
            ranges.iter().all(|(start, _)| *start >= table_end),
            "boot payload overlaps header or entry table"
        );
        Ok(Self {
            version: u32_at(bytes, 6)?,
            merge_version: u32_at(bytes, 10)?,
            release_time: ReleaseTime::parse(&bytes[14..21])?,
            chip_type: u32_at(bytes, 21)?,
            rc4_disabled: bytes[44] != 0,
            signed: bytes[43] == b'S',
            entries,
        })
    }

    /// Whether the loader uses the newer FlashHead IDB layout.
    pub fn has_flash_head(&self) -> bool {
        self.loader_entry("FlashHead").is_some()
    }

    fn loader_entry(&self, name: &str) -> Option<&BootEntry> {
        self.entries
            .iter()
            .find(|e| e.kind == EntryKind::Loader && e.name.eq_ignore_ascii_case(name))
    }

    /// Build bytes to write at LBA 64 for `ul`. Before writing a FlashHead image,
    /// callers must check the device's new-IDB capability (`capability[1] & 1`).
    pub fn idb(&self) -> Result<Vec<u8>> {
        let data_entry = self
            .loader_entry("FlashData")
            .context("loader has no FlashData entry")?;
        let boot_entry = self
            .loader_entry("FlashBoot")
            .context("loader has no FlashBoot entry")?;
        let mut data = data_entry.data.clone();
        let mut boot = boot_entry.data.clone();
        if self.rc4_disabled {
            for chunk in data.as_chunks_mut::<SECTOR>().0 {
                rc4(chunk);
            }
            for chunk in boot.as_chunks_mut::<SECTOR>().0 {
                rc4(chunk);
            }
        }
        let data_size = aligned(data.len(), ALIGNMENT)?;
        let boot_size = aligned(boot.len(), ALIGNMENT)?;
        let data_sectors =
            u16::try_from(data_size / SECTOR).context("FlashData too large for IDB")?;
        let boot_sectors =
            u16::try_from(boot_size / SECTOR).context("FlashBoot too large for IDB")?;
        let code_sectors = data_sectors
            .checked_add(boot_sectors)
            .context("IDB sector count overflow")?;
        if let Some(head_entry) = self.loader_entry("FlashHead") {
            let mut head = head_entry.data.clone();
            if self.rc4_disabled {
                for chunk in head.as_chunks_mut::<SECTOR>().0 {
                    rc4(chunk);
                }
            }
            let head_size = aligned(head.len(), ALIGNMENT)?;
            u16::try_from(head_size / SECTOR).context("FlashHead too large for IDB")?;
            let size = head_size
                .checked_add(data_size)
                .and_then(|s| s.checked_add(boot_size))
                .context("IDB size overflow")?;
            let mut image = vec![0; size];
            image[..head.len()].copy_from_slice(&head);
            image[head_size..head_size + data.len()].copy_from_slice(&data);
            image[head_size + data_size..head_size + data_size + boot.len()].copy_from_slice(&boot);
            return Ok(image);
        }
        let size = (4 + usize::from(code_sectors))
            .checked_mul(SECTOR)
            .context("IDB size overflow")?;
        let mut image = vec![0; size];
        put_u32(&mut image, 0, 0x0ff0_aa55);
        put_u32(&mut image, 8, u32::from(self.rc4_disabled));
        put_u16(&mut image, 12, 4);
        put_u16(&mut image, 14, 4);
        put_u16(&mut image, 506, data_sectors);
        put_u16(&mut image, 508, code_sectors);
        put_u16(&mut image, SECTOR, 12);
        put_u16(&mut image, SECTOR + 2, 0xffff);
        put_u32(&mut image, SECTOR + 10, 0x3832_4b52);
        let sec0_crc = crc16(&image[..SECTOR]);
        let sec1_crc = crc16(&image[SECTOR..2 * SECTOR]);
        let sec3_crc = crc16(&image[3 * SECTOR..4 * SECTOR]);
        image[4 * SECTOR..4 * SECTOR + data.len()].copy_from_slice(&data);
        let boot_offset = 4 * SECTOR + data_size;
        image[boot_offset..boot_offset + boot.len()].copy_from_slice(&boot);
        let boot_crc = rockchip_crc32(&image[4 * SECTOR..]);
        let sector2 = &mut image[2 * SECTOR..3 * SECTOR];
        sector2[491..494].copy_from_slice(b"VC\0");
        put_u16(sector2, 494, sec0_crc);
        put_u16(sector2, 496, sec1_crc);
        put_u32(sector2, 498, boot_crc);
        sector2[506..510].copy_from_slice(b"CRC\0");
        put_u16(sector2, 510, sec3_crc);
        for index in [0, 2, 3] {
            rc4(&mut image[index * SECTOR..(index + 1) * SECTOR]);
        }
        Ok(image)
    }
}

#[derive(Clone, Debug)]
struct FirmwareHeader {
    boot_offset: u64,
    boot_size: u64,
    firmware_offset: u64,
    firmware_size: u64,
    payload_end: u64,
}

impl FirmwareHeader {
    fn parse(bytes: &[u8], size: u64) -> Result<Self> {
        ensure!(
            bytes.len() >= HEADER_SIZE && bytes.starts_with(b"RKFW"),
            "invalid or truncated RKFW header"
        );
        let header_size = u64::from(u16_at(bytes, 4)?);
        ensure!(
            header_size >= HEADER_SIZE as u64 && header_size <= size,
            "invalid RKFW header size"
        );
        let boot_offset = u64::from(u32_at(bytes, 25)?);
        let boot_size = u64::from(u32_at(bytes, 29)?);
        let firmware_offset = u64::from(u32_at(bytes, 33)?);
        let low_size = u64::from(u32_at(bytes, 37)?);
        let high_size = if &bytes[55..57] == b"HI" {
            u64::from(u32_at(bytes, 57)?) << 32
        } else {
            0
        };
        let firmware_size = high_size
            .checked_add(low_size)
            .context("RKFW size overflow")?;
        let payload_end = firmware_offset
            .checked_add(firmware_size)
            .context("RKFW firmware range overflow")?;
        let boot_end = boot_offset
            .checked_add(boot_size)
            .context("RKFW loader range overflow")?;
        ensure!(
            boot_offset >= header_size && boot_size > 0 && boot_end <= size,
            "RKFW loader outside file"
        );
        ensure!(
            firmware_offset >= header_size && payload_end <= size && size - payload_end >= 32,
            "RKFW firmware or MD5 trailer outside file"
        );
        Ok(Self {
            boot_offset,
            boot_size,
            firmware_offset,
            firmware_size,
            payload_end,
        })
    }
}

/// RKFW metadata and streaming extraction. Signature bytes are exposed, not
/// authenticated: the upstream format parser does not verify a public-key signature.
#[derive(Clone, Debug)]
pub struct FirmwareImage {
    path: PathBuf,
    pub boot_offset: u64,
    pub boot_size: u64,
    pub firmware_offset: u64,
    pub firmware_size: u64,
    pub md5_text: [u8; 32],
    pub signature: Vec<u8>,
}

impl FirmwareImage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut file = File::open(path)?;
        let size = file.metadata()?.len();
        let mut bytes = [0; HEADER_SIZE];
        file.read_exact(&mut bytes).context("read RKFW header")?;
        let header = FirmwareHeader::parse(&bytes, size)?;
        let trailer_size = size - header.payload_end;
        let signed = trailer_size >= 160;
        let md5_offset = if signed {
            header.payload_end
        } else {
            size - 32
        };
        file.seek(SeekFrom::Start(md5_offset))?;
        let mut md5_text = [0; 32];
        file.read_exact(&mut md5_text)?;
        let mut signature = Vec::new();
        if signed {
            ensure!(
                trailer_size - 32 <= 256,
                "RKFW signature trailer exceeds 256 bytes"
            );
            file.read_to_end(&mut signature)?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            boot_offset: header.boot_offset,
            boot_size: header.boot_size,
            firmware_offset: header.firmware_offset,
            firmware_size: header.firmware_size,
            md5_text,
            signature,
        })
    }

    pub fn boot(&self) -> Result<BootImage> {
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.boot_offset))?;
        let mut bytes = Vec::new();
        file.take(self.boot_size).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == self.boot_size,
            "truncated RKFW loader"
        );
        ensure!(
            bytes.starts_with(b"BOOT") || bytes.starts_with(b"LDR "),
            "RKFW boot payload is not a BOOT/LDR loader"
        );
        BootImage::from_bytes(&bytes)
    }

    pub fn save_boot(&self, output: impl AsRef<Path>) -> Result<()> {
        self.copy_range(self.boot_offset, self.boot_size, output.as_ref())
    }

    pub fn save_firmware(&self, output: impl AsRef<Path>) -> Result<()> {
        self.copy_range(self.firmware_offset, self.firmware_size, output.as_ref())
    }

    fn copy_range(&self, offset: u64, size: u64, output: &Path) -> Result<()> {
        if let Ok(output_path) = fs::canonicalize(output) {
            ensure!(
                output_path != fs::canonicalize(&self.path)?,
                "output must differ from source firmware"
            );
        }
        let mut input = File::open(&self.path)?;
        input.seek(SeekFrom::Start(offset))?;
        let mut output = File::create(output)?;
        ensure!(
            io::copy(&mut input.take(size), &mut output)? == size,
            "truncated RKFW payload"
        );
        output.flush()?;
        Ok(())
    }
}

/// Merge a boot loader using the upstream sectioned INI configuration.
/// Relative input and output paths follow the current working directory.
pub fn pack(config_path: impl AsRef<Path>) -> Result<PathBuf> {
    let config_path = config_path.as_ref();
    let text = match fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(error)
            if error.kind() == ErrorKind::NotFound && config_path == Path::new("config.ini") =>
        {
            fs::write(config_path, DEFAULT_PACK_CONFIG)
                .context("create default pack configuration")?;
            return Err(error)
                .context("created default config.ini; configure input paths before packing");
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read pack config {}", config_path.display()));
        }
    };
    let config = parse_ini(&text)?;
    let required = |section: &str, key: &str| -> Result<&str> {
        config
            .get(section)
            .and_then(|s| s.get(key))
            .map(String::as_str)
            .with_context(|| format!("missing [{section}] {key}"))
    };
    let chip_type = chip_type(required("CHIP_NAME", "NAME")?)?;
    let major: u16 = required("VERSION", "MAJOR")?
        .parse()
        .context("invalid major version")?;
    let minor: u16 = required("VERSION", "MINOR")?
        .parse()
        .context("invalid minor version")?;
    let bcd = |n: u16| u32::from((((n / 10) % 10) << 4) | (n % 10));
    let mut entries = Vec::new();
    for (section, kind, default_path) in [
        (
            "CODE471_OPTION",
            EntryKind::Entry471,
            "rk3288_ddr_400MHZ_v1.01.bin",
        ),
        (
            "CODE472_OPTION",
            EntryKind::Entry472,
            "rk3288_usbplug_v2.32.bin",
        ),
    ] {
        let opts = config.get(section);
        let count: u8 = opts
            .and_then(|s| s.get("NUM"))
            .map(String::as_str)
            .unwrap_or("1")
            .parse()
            .context("invalid code entry count")?;
        let delay_ms: u16 = opts
            .and_then(|s| s.get("Sleep"))
            .map(String::as_str)
            .unwrap_or("0")
            .parse()
            .context("invalid code delay")?;
        for index in 1..=count {
            let key = format!("Path{index}");
            let value = match opts {
                Some(_) => required(section, &key)?,
                None => default_path,
            };
            let path = native_config_path(value);
            let name = entry_name(&path)?;
            entries.push(read_pack_entry(&path, kind, name, u32::from(delay_ms))?);
        }
    }
    let opts = config
        .get("LOADER_OPTION")
        .context("missing [LOADER_OPTION]")?;
    let count: u8 = opts
        .get("NUM")
        .or_else(|| opts.get("LOADERCOUNT"))
        .context("missing loader count")?
        .parse()
        .context("invalid loader count")?;
    ensure!(count > 0, "loader count must be positive");
    // Upstream's parser uses zero-based LOADER indices although its generated
    // template uses one-based indices. Accept both, without the C++ overflow.
    let first = if opts.contains_key("LOADER0") {
        0usize
    } else {
        1
    };
    let mut names = HashSet::new();
    for index in first..first + usize::from(count) {
        let key = format!("LOADER{index}");
        let name = required("LOADER_OPTION", &key)?;
        ensure!(
            names.insert(name.to_owned()),
            "duplicate loader name {name}"
        );
        let path = native_config_path(required("LOADER_OPTION", name)?);
        entries.push(read_pack_entry(
            &path,
            EntryKind::Loader,
            entry_name(Path::new(name))?,
            0,
        )?);
    }
    let output = native_config_path(required("OUTPUT", "PATH")?);
    let output = if output.extension().is_some_and(|s| s == "bin") {
        output
    } else {
        let mut name = output.into_os_string();
        name.push(".bin");
        PathBuf::from(name)
    };
    let image = BootImage {
        version: (bcd(major) << 8) | bcd(minor),
        merge_version: 0x0103_0000,
        release_time: ReleaseTime::now(),
        chip_type,
        rc4_disabled: true,
        signed: false,
        entries,
    };
    let bytes = serialize(&image)?;
    fs::write(&output, bytes).with_context(|| format!("write loader {}", output.display()))?;
    Ok(output)
}

/// Unpack and decode all entry payloads, retaining the upstream padded lengths.
pub fn unpack(loader_path: impl AsRef<Path>, output_dir: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    let image = BootImage::open(loader_path)?;
    let output_dir = output_dir.as_ref();
    for entry in &image.entries {
        validate_filename(&entry.name)?;
    }
    fs::create_dir_all(output_dir)?;
    let mut outputs = Vec::new();
    for entry in image.entries {
        let output = output_dir.join(&entry.name);
        fs::write(&output, entry.decoded_data())
            .with_context(|| format!("unpack entry {}", entry.name))?;
        outputs.push(output);
    }
    Ok(outputs)
}

/// Prefix an SPL with the tag bytes and write `<SPL>.tag`.
pub fn tag_spl(tag: &str, spl_path: impl AsRef<Path>) -> Result<PathBuf> {
    let spl_path = spl_path.as_ref();
    let mut input =
        File::open(spl_path).with_context(|| format!("open SPL {}", spl_path.display()))?;
    let mut name = spl_path.as_os_str().to_owned();
    name.push(".tag");
    let output_path = PathBuf::from(name);
    let mut output = File::create(&output_path)?;
    output.write_all(tag.as_bytes())?;
    io::copy(&mut input, &mut output)?;
    output.flush()?;
    Ok(output_path)
}

fn serialize(image: &BootImage) -> Result<Vec<u8>> {
    let total = image
        .entries
        .len()
        .checked_mul(ENTRY_SIZE)
        .and_then(|n| n.checked_add(HEADER_SIZE))
        .context("boot table size overflow")?;
    let mut bytes = vec![0; total];
    bytes[..4].copy_from_slice(b"BOOT");
    put_u16(&mut bytes, 4, HEADER_SIZE as u16);
    put_u32(&mut bytes, 6, image.version);
    put_u32(&mut bytes, 10, image.merge_version);
    image.release_time.encode(&mut bytes[14..21]);
    put_u32(&mut bytes, 21, image.chip_type);
    bytes[43] = if image.signed { b'S' } else { 0 };
    bytes[44] = u8::from(image.rc4_disabled);
    let mut table_offset = HEADER_SIZE;
    for (kind, field) in [
        (EntryKind::Entry471, 25),
        (EntryKind::Entry472, 31),
        (EntryKind::Loader, 37),
    ] {
        let count = image.entries.iter().filter(|e| e.kind == kind).count();
        bytes[field] = u8::try_from(count).context("more than 255 entries in one group")?;
        put_u32(&mut bytes, field + 1, u32::try_from(table_offset)?);
        bytes[field + 5] = ENTRY_SIZE as u8;
        for entry in image.entries.iter().filter(|e| e.kind == kind) {
            let start = u32::try_from(bytes.len()).context("boot payload exceeds 4 GiB")?;
            let size = u32::try_from(entry.data.len()).context("boot entry exceeds 4 GiB")?;
            ensure!(
                entry.name.encode_utf16().count() < 20,
                "boot entry name exceeds 19 UTF-16 code units"
            );
            let record = &mut bytes[table_offset..table_offset + ENTRY_SIZE];
            record[0] = ENTRY_SIZE as u8;
            put_u32(record, 1, kind as u32);
            for (i, ch) in entry.name.encode_utf16().enumerate() {
                put_u16(record, 5 + i * 2, ch);
            }
            put_u32(record, 45, start);
            put_u32(record, 49, size);
            put_u32(record, 53, entry.delay_ms);
            bytes.extend_from_slice(&entry.data);
            table_offset += ENTRY_SIZE;
        }
    }
    let checksum = rockchip_crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    Ok(bytes)
}

fn read_pack_entry(path: &Path, kind: EntryKind, name: String, delay_ms: u32) -> Result<BootEntry> {
    let mut data = fs::read(path).with_context(|| format!("read entry {}", path.display()))?;
    ensure!(!data.is_empty(), "empty boot entry {}", path.display());
    let size = aligned(data.len(), ALIGNMENT)?;
    u32::try_from(size).context("boot entry exceeds 4 GiB")?;
    data.resize(size, 0);
    if kind == EntryKind::Loader {
        for chunk in data.chunks_mut(SECTOR) {
            rc4(chunk);
        }
    } else {
        let length = data.len() & 0xffff;
        rc4(&mut data[..length]);
    }
    Ok(BootEntry {
        kind,
        name,
        data,
        delay_ms,
    })
}

fn entry_name(path: &Path) -> Result<String> {
    let name = path
        .file_stem()
        .context("entry has no file name")?
        .to_str()
        .context("entry name must be UTF-8")?;
    let mut output = String::new();
    let mut length = 0;
    for ch in name.chars() {
        length += ch.len_utf16();
        if length >= 20 {
            break;
        }
        output.push(ch);
    }
    ensure!(!output.is_empty(), "empty boot entry name");
    Ok(output)
}

fn validate_filename(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    ensure!(
        !name.is_empty()
            && !name.contains(['/', '\\', ':', '\0'])
            && matches!(components.next(), Some(Component::Normal(_)))
            && components.next().is_none(),
        "unsafe boot entry file name {name:?}"
    );
    Ok(())
}

fn native_config_path(value: &str) -> PathBuf {
    PathBuf::from(value.replace('\\', "/"))
}

fn parse_ini(text: &str) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current = String::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with(['#', ';']) || line.starts_with("//") {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current = name.to_owned();
            ensure!(
                matches!(
                    name,
                    "CHIP_NAME"
                        | "VERSION"
                        | "CODE471_OPTION"
                        | "CODE472_OPTION"
                        | "LOADER_OPTION"
                        | "OUTPUT"
                ),
                "unknown pack section {name}"
            );
            sections.entry(current.clone()).or_default();
        } else {
            let (key, value) = line
                .split_once('=')
                .with_context(|| format!("invalid config line {}", index + 1))?;
            ensure!(
                !current.is_empty() && !key.trim().is_empty() && !value.trim().is_empty(),
                "invalid config assignment"
            );
            sections
                .entry(current.clone())
                .or_default()
                .insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    Ok(sections)
}

fn chip_type(name: &str) -> Result<u32> {
    let known = match name {
        "RK27" => Some(0x10),
        "RKCAYMAN" => Some(0x11),
        "RK28" => Some(0x20),
        "RK281X" => Some(0x21),
        "RKPANDA" => Some(0x22),
        "RKNANO" => Some(0x30),
        "RKSMART" => Some(0x31),
        "RKCROWN" => Some(0x40),
        "RK29" => Some(0x50),
        "RK292X" => Some(0x51),
        "RK30" => Some(0x60),
        "RK30B" => Some(0x61),
        "RK31" => Some(0x70),
        "RK32" => Some(0x80),
        _ => None,
    };
    if let Some(value) = known {
        return Ok(value);
    }
    let suffix = name
        .strip_prefix("RK")
        .context("chip name must start with RK")?;
    ensure!(!suffix.is_empty() && suffix.is_ascii(), "invalid chip name");
    let mut tag = [0; 4];
    for (dst, src) in tag.iter_mut().zip(suffix.bytes()) {
        *dst = src;
    }
    Ok(u32::from_be_bytes(tag))
}

fn aligned(size: usize, align: usize) -> Result<usize> {
    size.checked_add(align - 1)
        .map(|n| n & !(align - 1))
        .context("aligned size overflow")
}
fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .context("truncated 16-bit field")?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .context("truncated 32-bit field")?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}
fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
