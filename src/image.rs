// SPDX-License-Identifier: GPL-2.0-or-later
// Derived from rkdeveloptool, Copyright 2017 Fuzhou Rockchip Electronics Co., Ltd.
// Original author: Seth Liu (2017-03-01).
// Rust translation and checked streaming I/O, 2026.

//! Raw, Android sparse and UBI images with bounded-memory flash I/O.

use crate::device::Device;
use anyhow::{Context, Result, bail, ensure};
use crc::{CRC_32_ISO_HDLC, Crc};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

const SECTOR: u64 = 512;
const TRANSFER: usize = 128 * 512;
const SPARSE_MAGIC: u32 = 0xed26_ff3a;
const CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Raw,
    Sparse,
    Ubi,
}

/// An image's stored size and expanded size, in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    pub kind: ImageKind,
    pub file_size: u64,
    pub expanded_size: u64,
}

impl ImageInfo {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut file = File::open(path.as_ref())
            .with_context(|| format!("cannot open {}", path.as_ref().display()))?;
        let file_size = file.metadata()?.len();
        let kind = detect_kind(&mut file)?;
        let expanded_size = match kind {
            ImageKind::Sparse => SparseImage::parse(&mut file)?.expanded_size(),
            _ => file_size,
        };
        Ok(Self {
            kind,
            file_size,
            expanded_size,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SparseChunkKind {
    Raw,
    Fill([u8; 4]),
    DontCare,
    Crc32(u32),
}

/// A sparse chunk's source and expanded-output extents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseChunk {
    pub kind: SparseChunkKind,
    pub file_offset: u64,
    pub output_offset: u64,
    pub output_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseImage {
    pub block_size: u32,
    pub blocks: u32,
    pub checksum: u32,
    pub chunks: Vec<SparseChunk>,
}

impl SparseImage {
    /// Validate sparse dimensions, all chunk extents and supplied CRCs before I/O.
    pub fn parse(source: &mut (impl Read + Seek)) -> Result<Self> {
        let file_size = source.seek(SeekFrom::End(0))?;
        source.seek(SeekFrom::Start(0))?;
        let mut header = [0; 28];
        source
            .read_exact(&mut header)
            .context("truncated sparse header")?;
        ensure!(read_u32(&header, 0) == SPARSE_MAGIC, "invalid sparse magic");
        ensure!(
            read_u16(&header, 4) == 1,
            "unsupported sparse major version"
        );
        let header_size = read_u16(&header, 8) as u64;
        let chunk_header_size = read_u16(&header, 10) as u64;
        let block_size = read_u32(&header, 12);
        let blocks = read_u32(&header, 16);
        let chunk_count = read_u32(&header, 20);
        let checksum = read_u32(&header, 24);
        ensure!(
            header_size >= 28 && header_size <= file_size,
            "invalid sparse header size"
        );
        ensure!(chunk_header_size >= 12, "invalid sparse chunk header size");
        ensure!(
            block_size > 0 && (block_size as u64).is_multiple_of(SECTOR),
            "sparse block size must be a nonzero sector multiple"
        );
        ensure!(
            (chunk_count as u64) <= (file_size - header_size) / chunk_header_size,
            "sparse chunk count exceeds file size"
        );
        let expanded_size = (block_size as u64) * (blocks as u64);
        let mut chunks = Vec::new();
        let mut file_offset = header_size;
        let mut output_offset = 0u64;
        for _ in 0..chunk_count {
            source.seek(SeekFrom::Start(file_offset))?;
            let mut header = [0; 12];
            source
                .read_exact(&mut header)
                .context("truncated sparse chunk")?;
            let chunk_type = read_u16(&header, 0);
            let output_blocks = read_u32(&header, 4);
            let total_size = read_u32(&header, 8) as u64;
            ensure!(
                total_size >= chunk_header_size,
                "sparse chunk is smaller than its header"
            );
            let data_offset = file_offset
                .checked_add(chunk_header_size)
                .context("sparse input offset overflow")?;
            let next_offset = file_offset
                .checked_add(total_size)
                .context("sparse input extent overflow")?;
            ensure!(next_offset <= file_size, "truncated sparse chunk data");
            let payload_size = total_size - chunk_header_size;
            let output_size = (output_blocks as u64) * (block_size as u64);
            let kind = match chunk_type {
                0xcac1 => {
                    ensure!(
                        payload_size == output_size,
                        "sparse RAW chunk size mismatch"
                    );
                    SparseChunkKind::Raw
                }
                0xcac2 => {
                    ensure!(payload_size == 4, "sparse FILL chunk size mismatch");
                    source.seek(SeekFrom::Start(data_offset))?;
                    let mut fill = [0; 4];
                    source.read_exact(&mut fill)?;
                    SparseChunkKind::Fill(fill)
                }
                0xcac3 => {
                    ensure!(payload_size == 0, "sparse DONT_CARE chunk has data");
                    SparseChunkKind::DontCare
                }
                0xcac4 => {
                    ensure!(
                        payload_size == 4 && output_blocks == 0,
                        "invalid sparse CRC32 chunk"
                    );
                    source.seek(SeekFrom::Start(data_offset))?;
                    let mut bytes = [0; 4];
                    source.read_exact(&mut bytes)?;
                    SparseChunkKind::Crc32(u32::from_le_bytes(bytes))
                }
                _ => bail!("unsupported sparse chunk type 0x{chunk_type:04x}"),
            };
            chunks.push(SparseChunk {
                kind,
                file_offset: data_offset,
                output_offset,
                output_size,
            });
            output_offset = output_offset
                .checked_add(output_size)
                .context("sparse expanded size overflow")?;
            ensure!(
                output_offset <= expanded_size,
                "sparse chunks exceed declared output size"
            );
            file_offset = next_offset;
        }
        ensure!(
            output_offset == expanded_size,
            "sparse chunks do not fill declared output size"
        );
        let image = Self {
            block_size,
            blocks,
            checksum,
            chunks,
        };
        if checksum != 0
            || image
                .chunks
                .iter()
                .any(|chunk| matches!(chunk.kind, SparseChunkKind::Crc32(_)))
        {
            image.validate_checksums(source)?;
        }
        Ok(image)
    }

    pub fn expanded_size(&self) -> u64 {
        (self.block_size as u64) * (self.blocks as u64)
    }

    fn validate_checksums(&self, source: &mut (impl Read + Seek)) -> Result<()> {
        let mut digest = CRC.digest();
        let mut buffer = [0u8; TRANSFER];
        for chunk in &self.chunks {
            match chunk.kind {
                SparseChunkKind::Crc32(expected) => {
                    ensure!(
                        digest.clone().finalize() == expected,
                        "sparse chunk CRC32 mismatch"
                    );
                    continue;
                }
                SparseChunkKind::Raw => {
                    source.seek(SeekFrom::Start(chunk.file_offset))?;
                }
                SparseChunkKind::Fill(fill) => {
                    for bytes in buffer.as_chunks_mut::<4>().0 {
                        bytes.copy_from_slice(&fill);
                    }
                }
                SparseChunkKind::DontCare => buffer.fill(0),
            }
            let mut remaining = chunk.output_size;
            while remaining != 0 {
                let count = remaining.min(TRANSFER as u64) as usize;
                if chunk.kind == SparseChunkKind::Raw {
                    source.read_exact(&mut buffer[..count])?;
                }
                digest.update(&buffer[..count]);
                remaining -= count as u64;
            }
        }
        if self.checksum != 0 {
            ensure!(
                digest.finalize() == self.checksum,
                "sparse image checksum mismatch"
            );
        }
        Ok(())
    }
}

fn detect_kind(source: &mut (impl Read + Seek)) -> Result<ImageKind> {
    source.seek(SeekFrom::Start(0))?;
    let mut magic = [0; 4];
    let mut count = 0;
    while count < magic.len() {
        let read = source.read(&mut magic[count..])?;
        if read == 0 {
            break;
        }
        count += read;
    }
    source.seek(SeekFrom::Start(0))?;
    Ok(if count == 4 && u32::from_le_bytes(magic) == SPARSE_MAGIC {
        ImageKind::Sparse
    } else if &magic == b"UBI#" {
        ImageKind::Ubi
    } else {
        ImageKind::Raw
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

impl Device {
    /// Write raw, UBI or sparse data, checking expanded size before changing flash.
    pub fn write_image(
        &mut self,
        start: u32,
        path: impl AsRef<Path>,
        partition_sectors: Option<u32>,
    ) -> Result<()> {
        let mut file = File::open(path.as_ref())
            .with_context(|| format!("cannot open {}", path.as_ref().display()))?;
        let kind = detect_kind(&mut file)?;
        let sparse = if kind == ImageKind::Sparse {
            Some(SparseImage::parse(&mut file)?)
        } else {
            None
        };
        let size = sparse
            .as_ref()
            .map_or(file.metadata()?.len(), SparseImage::expanded_size);
        let sectors = size.div_ceil(SECTOR);
        if let Some(limit) = partition_sectors {
            ensure!(sectors <= limit as u64, "image exceeds partition size");
        }
        let disk_sectors = self.read_flash_info()?.sectors;
        let end = (start as u64)
            .checked_add(sectors)
            .context("flash range overflow")?;
        ensure!(end <= disk_sectors as u64, "image exceeds flash capacity");
        ensure!(
            end <= (u32::MAX as u64) + 1,
            "image exceeds Rockusb address range"
        );
        if let Some(sparse) = sparse {
            let erase_count = partition_sectors.map_or(sectors, u64::from);
            ensure!(
                (start as u64) + erase_count <= disk_sectors as u64,
                "erase range exceeds flash capacity"
            );
            self.erase_image_range(
                start,
                u32::try_from(erase_count).context("erase range too large")?,
            )?;
            self.write_sparse(start, &mut file, &sparse)?;
        } else {
            if kind == ImageKind::Ubi {
                self.erase_ubi_range(start, partition_sectors, disk_sectors)?;
            }
            file.seek(SeekFrom::Start(0))?;
            let mut remaining = size;
            let mut current = start as u64;
            let mut buffer = [0u8; TRANSFER];
            self.report_progress("Write LBA from file", 0, size);
            while remaining > 0 {
                let count = remaining.min(TRANSFER as u64) as usize;
                let padded = count.div_ceil(SECTOR as usize) * SECTOR as usize;
                buffer[..padded].fill(0);
                file.read_exact(&mut buffer[..count])
                    .context("image changed or was truncated during writing")?;
                self.write_lba(u32::try_from(current)?, &buffer[..padded])?;
                current += (padded as u64) / SECTOR;
                remaining -= count as u64;
                self.report_progress("Write LBA from file", size - remaining, size);
            }
        }
        Ok(())
    }

    fn erase_image_range(&mut self, mut start: u32, mut count: u32) -> Result<()> {
        while count > 0 {
            let chunk = count.min(32 * 1024);
            self.erase_lba(start, chunk)?;
            count -= chunk;
            if count > 0 {
                start = start.checked_add(chunk).context("erase address overflow")?;
            }
        }
        Ok(())
    }

    fn erase_ubi_range(&mut self, start: u32, count: Option<u32>, disk_sectors: u32) -> Result<()> {
        if self.read_flash_id()?.starts_with(b"EMMC") {
            return Ok(());
        }
        let block_size = self.read_flash_info()?.block_size as u32;
        ensure!(block_size > 0, "NAND reports a zero block size");
        ensure!(
            start.is_multiple_of(block_size),
            "UBI offset must be aligned to a NAND erase block"
        );
        if let Some(count) = count {
            ensure!(
                count.is_multiple_of(block_size),
                "UBI partition size must be aligned to a NAND erase block"
            );
        }
        let count = count.unwrap_or(disk_sectors - start);
        let blocks = count.div_ceil(block_size);
        ensure!(
            (start as u64) + (blocks as u64) * (block_size as u64) <= disk_sectors as u64,
            "NAND erase range exceeds flash capacity"
        );
        let mut block = start / block_size;
        let mut remaining = blocks;
        while remaining > 0 {
            let chunk = remaining.min(16);
            self.erase_block(0, block, chunk as _, true)?;
            remaining -= chunk;
            block = block
                .checked_add(chunk)
                .context("NAND erase address overflow")?;
            self.report_progress(
                "Erase UBI blocks",
                (blocks - remaining) as u64,
                blocks as u64,
            );
        }
        Ok(())
    }

    fn write_sparse(&mut self, start: u32, file: &mut File, image: &SparseImage) -> Result<()> {
        let total = image.expanded_size();
        let mut buffer = [0u8; TRANSFER];
        self.report_progress("Write LBA from file", 0, total);
        for chunk in &image.chunks {
            match chunk.kind {
                SparseChunkKind::DontCare | SparseChunkKind::Crc32(_) => {
                    self.report_progress(
                        "Write LBA from file",
                        chunk.output_offset + chunk.output_size,
                        total,
                    );
                    continue;
                }
                SparseChunkKind::Raw => {
                    file.seek(SeekFrom::Start(chunk.file_offset))?;
                }
                SparseChunkKind::Fill(fill) => {
                    for bytes in buffer.as_chunks_mut::<4>().0 {
                        bytes.copy_from_slice(&fill);
                    }
                }
            }
            let mut done = 0u64;
            while done < chunk.output_size {
                let count = (chunk.output_size - done).min(TRANSFER as u64) as usize;
                if chunk.kind == SparseChunkKind::Raw {
                    file.read_exact(&mut buffer[..count])?;
                }
                let address = (start as u64) + (chunk.output_offset + done) / SECTOR;
                self.write_lba(
                    u32::try_from(address).context("sparse address exceeds Rockusb range")?,
                    &buffer[..count],
                )?;
                done += count as u64;
                self.report_progress("Write LBA from file", chunk.output_offset + done, total);
            }
        }
        Ok(())
    }

    /// Stream logical sectors into a writer without allocating the whole image.
    pub fn read_to_writer(
        &mut self,
        start: u32,
        sectors: u32,
        output: &mut impl Write,
    ) -> Result<()> {
        let end = (start as u64) + (sectors as u64);
        ensure!(
            end <= (u32::MAX as u64) + 1,
            "read exceeds Rockusb address range"
        );
        ensure!(
            end <= self.read_flash_info()?.sectors as u64,
            "read exceeds flash capacity"
        );
        let mut done = 0u32;
        self.report_progress("Read LBA to file", 0, sectors as u64);
        while done < sectors {
            let count = (sectors - done).min(128);
            let bytes = self.read_lba(
                start.checked_add(done).context("read address overflow")?,
                count,
            )?;
            output.write_all(&bytes)?;
            done += count;
            self.report_progress("Read LBA to file", done as u64, sectors as u64);
        }
        output.flush()?;
        Ok(())
    }

    pub fn read_to_file(&mut self, start: u32, sectors: u32, path: impl AsRef<Path>) -> Result<()> {
        // Validate before truncating an existing output file.
        ensure!(
            (start as u64) + (sectors as u64) <= self.read_flash_info()?.sectors as u64,
            "read exceeds flash capacity"
        );
        let mut file = File::create(path.as_ref())
            .with_context(|| format!("cannot create {}", path.as_ref().display()))?;
        self.read_to_writer(start, sectors, &mut file)
    }
}
