// Copyright (C) 2017 Fuzhou Rockchip Electronics Co., Ltd.
// Original author: Seth Liu (2017-03-01).
// Rust translation and bounds checking: 2026-10-26.
// SPDX-License-Identifier: GPL-2.0-or-later
//! Checksums and the fixed-key stream transform used by Rockchip file formats.
//! RC4 here is a compatibility transform, not a cryptographic API for new data.

use crc::{Algorithm, Crc};

const ROCKCHIP: Algorithm<u32> = Algorithm {
    width: 32,
    poly: 0x04c1_0db7,
    init: 0,
    refin: false,
    refout: false,
    xorout: 0,
    check: 0,
    residue: 0,
};

/// Non-reflected CRC32 with initial value zero, used by BOOT and PARM.
pub fn rockchip_crc32(data: &[u8]) -> u32 {
    Crc::<u32>::new(&ROCKCHIP).checksum(data)
}

/// IEEE CRC32 used by GPT (different from the BOOT checksum).
pub fn crc32_ieee(data: &[u8]) -> u32 {
    Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(data)
}

/// CRC16-CCITT with initial value zero, used in IDB sectors.
pub fn crc16(data: &[u8]) -> u16 {
    Crc::<u16>::new(&crc::CRC_16_XMODEM).checksum(data)
}

/// CRC16-CCITT with initial value 0xffff, used by Maskrom downloads.
pub fn crc_ccitt(data: &[u8]) -> u16 {
    Crc::<u16>::new(&crc::CRC_16_IBM_3740).checksum(data)
}

/// Apply the upstream fixed-key RC4 transform, restarting the stream each call.
pub fn rc4(data: &mut [u8]) {
    const KEY: [u8; 16] = [124, 78, 3, 4, 85, 5, 9, 7, 45, 44, 123, 56, 23, 13, 23, 17];
    let mut state = std::array::from_fn::<_, 256, _>(|i| i as u8);
    let mut j = 0usize;
    for i in 0..256 {
        j = (j + usize::from(state[i]) + usize::from(KEY[i % KEY.len()])) & 255;
        state.swap(i, j);
    }
    let (mut i, mut j) = (0usize, 0usize);
    for byte in data {
        i = (i + 1) & 255;
        j = (j + usize::from(state[i])) & 255;
        state.swap(i, j);
        *byte ^= state[(usize::from(state[i]) + usize::from(state[j])) & 255];
    }
}

/// Encode a legacy 512-byte NAND sector plus its three metadata bytes.
/// The generator words are those produced by upstream generate_gf/gen_poly
/// for m=13, t=8, polynomial 0x25af.
pub fn bch_encode(input: &[u8; 515]) -> [u8; 528] {
    let mut bch = [0u32; 4];
    for byte in input {
        for bit in 0..8 {
            let feedback = (bch[0] & 1) ^ u32::from((byte >> bit) & 1);
            bch[0] = ((bch[0] >> 1) | ((bch[1] & 1) << 31)) ^ (0x4c3d_56ac * feedback);
            bch[1] = ((bch[1] >> 1) | ((bch[2] & 1) << 31)) ^ (0x97dc_b25b * feedback);
            bch[2] = ((bch[2] >> 1) | ((bch[3] & 1) << 31)) ^ (0x6ac1_0939 * feedback);
            bch[3] = ((bch[3] >> 1) ^ (0x0b * feedback)) | (feedback << 7);
        }
    }
    for (word, mask) in bch
        .iter_mut()
        .zip([0xad62_73b1, 0x3483_93d2, 0xe6eb_ed3c, 0xc8])
    {
        *word = !(*word ^ mask);
    }
    let mut output = [0; 528];
    output[..515].copy_from_slice(input);
    for (i, word) in bch[..3].iter().enumerate() {
        output[515 + i * 4..519 + i * 4].copy_from_slice(&word.to_le_bytes());
    }
    output[527] = bch[3] as u8;
    output
}
