// Copyright (C) 2017 Fuzhou Rockchip Electronics Co., Ltd
// Original author: Seth Liu (2017-03-01).
// Rust port, 2026. SPDX-License-Identifier: GPL-2.0-or-later
use anyhow::{Context, Result, bail, ensure};
use rkdeveloptool::{BootImage, Device, DeviceInfo, DeviceMode, boot, list_devices};
use std::{env, ffi::OsString, fs, path::PathBuf, process::ExitCode};

const USAGE: &str = "\
---------------------Tool Usage ---------------------
Help:                   -h or --help
Version:                -v or --version
ListDevice:             ld
DownloadBoot:           db [Loader]
UpgradeLoader:          ul <Loader>
ReadLBA:                rl <BeginSec> <SectorLen> <File>
WriteLBA:               wl <BeginSec> <File>
WriteLBA:               wlx <PartitionName> <File>
WriteGPT:               gpt <gpt image or parameter>
WriteParameter:         prm <parameter>
PrintPartition:         ppt
EraseFlash:             ef
TestDevice:             td
ResetDevice:            rd [subcode: 0..5]
ChangeStorage:          cs <storage: 1=EMMC, 2=SD, 9=SPINOR>
ReadFlashID:            rid
ReadFlashInfo:          rfi
ReadChipInfo:           rci
ReadCapability:         rcb
PackBootLoader:         pack
UnpackBootLoader:       unpack <boot loader>
TagSPL:                 tagspl <tag> <U-Boot SPL>
-------------------------------------------------------";

enum Command {
    Help,
    Version,
    List,
    Pack,
    Unpack(PathBuf),
    Tag(String, PathBuf),
    Download(Option<PathBuf>),
    Upgrade(PathBuf),
    Read(u32, u32, PathBuf),
    Write(u32, PathBuf),
    WritePartition(String, PathBuf),
    Gpt(PathBuf),
    Parameter(PathBuf),
    Partitions,
    Erase,
    Test,
    Reset(u8),
    Storage(u8),
    FlashId,
    FlashInfo,
    ChipInfo,
    Capability,
}

fn text(arg: &OsString) -> Result<&str> {
    arg.to_str()
        .context("command and numeric arguments must be valid Unicode")
}

fn number(arg: &OsString) -> Result<u32> {
    let original = text(arg)?;
    let value = original.strip_prefix('+').unwrap_or(original);
    let (digits, radix) = if let Some(digits) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (digits, 16)
    } else if value.len() > 1 && value.starts_with('0') {
        (&value[1..], 8)
    } else {
        (value, 10)
    };
    u32::from_str_radix(digits, radix)
        .with_context(|| format!("invalid unsigned 32-bit number: {original}"))
}

fn parse(args: &[OsString]) -> Result<Command> {
    if args.is_empty() {
        return Ok(Command::Help);
    }
    let name = text(&args[0])?.to_ascii_lowercase();
    let params = &args[1..];
    let arity = |count: usize| -> Result<()> {
        ensure!(
            params.len() == count,
            "parameter of [{name}] command is invalid; use --help"
        );
        Ok(())
    };
    Ok(match name.as_str() {
        "-h" | "--help" => {
            arity(0)?;
            Command::Help
        }
        "-v" | "--version" => {
            arity(0)?;
            Command::Version
        }
        "ld" => {
            arity(0)?;
            Command::List
        }
        "pack" => {
            arity(0)?;
            Command::Pack
        }
        "unpack" => {
            arity(1)?;
            Command::Unpack((&params[0]).into())
        }
        "tagspl" => {
            arity(2)?;
            Command::Tag(text(&params[0])?.to_owned(), (&params[1]).into())
        }
        "db" => {
            ensure!(params.len() <= 1, "db expects at most one Loader path");
            Command::Download(params.first().map(PathBuf::from))
        }
        "ul" => {
            arity(1)?;
            Command::Upgrade((&params[0]).into())
        }
        "rl" => {
            arity(3)?;
            Command::Read(
                number(&params[0])?,
                number(&params[1])?,
                (&params[2]).into(),
            )
        }
        "wl" => {
            arity(2)?;
            Command::Write(number(&params[0])?, (&params[1]).into())
        }
        "wlx" => {
            arity(2)?;
            Command::WritePartition(text(&params[0])?.to_owned(), (&params[1]).into())
        }
        "gpt" => {
            arity(1)?;
            Command::Gpt((&params[0]).into())
        }
        "prm" => {
            arity(1)?;
            Command::Parameter((&params[0]).into())
        }
        "ppt" => {
            arity(0)?;
            Command::Partitions
        }
        "ef" => {
            arity(0)?;
            Command::Erase
        }
        "td" => {
            arity(0)?;
            Command::Test
        }
        "rd" => {
            ensure!(params.len() <= 1, "rd expects at most one subcode");
            let value = params.first().map(number).transpose()?.unwrap_or(0);
            ensure!(value <= 5, "reset subcode must be in 0..=5");
            Command::Reset(value as u8)
        }
        "cs" => {
            arity(1)?;
            Command::Storage(
                u8::try_from(number(&params[0])?).context("storage must fit in one byte")?,
            )
        }
        "rid" => {
            arity(0)?;
            Command::FlashId
        }
        "rfi" => {
            arity(0)?;
            Command::FlashInfo
        }
        "rci" => {
            arity(0)?;
            Command::ChipInfo
        }
        "rcb" => {
            arity(0)?;
            Command::Capability
        }
        _ => bail!("command is invalid: {name}; use --help"),
    })
}

fn default_loader() -> Result<PathBuf> {
    let config = env::current_exe()?.with_file_name("config.ini");
    let contents = fs::read_to_string(&config)
        .with_context(|| format!("read {} for default Loader", config.display()))?;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("loader")
            && !value.trim().is_empty()
        {
            return Ok(value.trim().into());
        }
    }
    bail!("did not find loader item in {}", config.display())
}

fn cli_devices() -> Result<Vec<DeviceInfo>> {
    Ok(list_devices()?
        .into_iter()
        .filter(|info| info.mode != DeviceMode::MassStorage)
        .collect())
}

fn open_device() -> Result<Device> {
    let devices = cli_devices()?;
    ensure!(
        !devices.is_empty(),
        "Did not find any rockusb device, please plug device in!"
    );
    ensure!(
        devices.len() == 1,
        "Found too many rockusb devices, please unplug other devices!"
    );
    let mut device = devices[0].open()?;
    device.set_progress_callback(|progress| {
        if let Some(percent) = progress
            .current
            .saturating_mul(100)
            .checked_div(progress.total)
        {
            eprintln!("{} ({}%)", progress.stage, percent);
        }
    });
    Ok(device)
}

fn hex(data: &[u8]) -> String {
    data.iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn execute(command: Command) -> Result<()> {
    // Offline commands never initialize USB, allowing use on build hosts.
    match command {
        Command::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        Command::Version => {
            println!("rkdeveloptool ver {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Command::Pack => {
            let path = boot::pack("config.ini")?;
            println!("Successfully merged boot to {}", path.display());
            return Ok(());
        }
        Command::Unpack(path) => {
            for output in boot::unpack(path, ".")? {
                println!("Unpacked {}", output.display());
            }
            return Ok(());
        }
        Command::Tag(tag, path) => {
            let output = boot::tag_spl(&tag, path)?;
            println!("Writing tagged spl to {}\ndone", output.display());
            return Ok(());
        }
        Command::List => {
            let devices = cli_devices()?;
            ensure!(!devices.is_empty(), "not found any devices!");
            for (index, device) in devices.iter().enumerate() {
                println!(
                    "DevNo={}\tVid=0x{:x},Pid=0x{:x},LocationID={:x}\t{}",
                    index + 1,
                    device.vid,
                    device.pid,
                    device.location_id,
                    device.mode
                );
            }
            return Ok(());
        }
        _ => {}
    }

    // Parse and check boot inputs before opening a device.
    let boot = match &command {
        Command::Download(path) => Some(BootImage::open(
            path.clone().map(Ok).unwrap_or_else(default_loader)?,
        )?),
        Command::Upgrade(path) => Some(BootImage::open(path)?),
        _ => None,
    };
    let mut device = open_device()?;
    match command {
        Command::Download(_) => {
            device.download_boot(boot.as_ref().context("missing Loader")?)?;
            println!("Downloading bootloader succeeded.");
        }
        Command::Upgrade(_) => {
            device.upgrade_loader(boot.as_ref().context("missing Loader")?)?;
            println!("Upgrading loader succeeded.");
        }
        Command::Read(start, count, path) => {
            device.read_to_file(start, count, path)?;
            println!("Read LBA to file succeeded.");
        }
        Command::Write(start, path) => {
            device.write_image(start, path, None)?;
            println!("Write LBA from file succeeded.");
        }
        Command::WritePartition(name, path) => {
            device.write_partition(&name, path)?;
            println!("Write LBA from file succeeded.");
        }
        Command::Gpt(path) => {
            device.write_gpt(path)?;
            println!("Writing GPT succeeded.");
        }
        Command::Parameter(path) => {
            device.write_parameter(path)?;
            println!("Writing parameter succeeded.");
        }
        Command::Partitions => {
            println!("**********Partition Info**********\nNO  LBA       Name");
            for (index, part) in device.partitions()?.iter().enumerate() {
                println!("{index:02}  {:08X}  {}", part.start, part.name);
            }
        }
        Command::Erase => {
            device.erase_flash()?;
            println!("Erasing flash succeeded.");
        }
        Command::Test => {
            ensure!(
                device.test_ready()?.ready,
                "Test Device failed: device is not ready"
            );
            println!("Test Device OK.");
        }
        Command::Reset(subcode) => {
            device.reset(subcode)?;
            println!("Reset Device OK.");
        }
        Command::Storage(storage) => {
            device.change_storage(storage)?;
            println!("Change Storage OK.");
        }
        Command::FlashId => println!("Flash ID: {}", hex(&device.read_flash_id()?)),
        Command::ChipInfo => println!("Chip Info: {}", hex(&device.read_chip_info()?)),
        Command::FlashInfo => {
            let info = device.read_flash_info()?;
            let names = [
                "SAMSUNG", "TOSHIBA", "HYNIX", "INFINEON", "MICRON", "RENESAS", "ST", "INTEL",
            ];
            let manufacturer = names
                .get(usize::from(info.manufacturer))
                .unwrap_or(&"Unknown");
            println!(
                "Flash Info:\n\tManufacturer: {manufacturer}, value={:02X}\n\tFlash Size: {} MB\n\tFlash Size: {} Sectors\n\tBlock Size: {} KB\n\tPage Size: {} KB\n\tECC Bits: {}\n\tAccess Time: {}",
                info.manufacturer,
                info.sectors / 2048,
                info.sectors,
                info.block_size / 2,
                info.page_size / 2,
                info.ecc_bits,
                info.access_time
            );
            let chips: Vec<String> = (0..8)
                .filter(|bit| info.chip_select & (1 << bit) != 0)
                .map(|bit| format!("Flash<{bit}>"))
                .collect();
            println!("\tFlash CS: {}", chips.join(" "));
        }
        Command::Capability => {
            let caps = device.read_capability()?;
            println!("Capability:{}", hex(&caps));
            for (mask, label) in [
                (1, "Direct LBA"),
                (2, "Vendor Storage"),
                (4, "First 4m Access"),
                (8, "Read LBA"),
                (0x10, "Read Com Log"),
                (0x20, "Read IDB Config"),
                (0x40, "Read Secure Mode"),
            ] {
                if caps[0] & mask != 0 {
                    println!("{label}:\tenabled");
                }
            }
            if caps[1] & 1 != 0 {
                println!("New IDB:\tenabled");
            }
        }
        _ => unreachable!("offline commands returned before opening USB"),
    }
    Ok(())
}

fn main() -> ExitCode {
    let result = parse(&env::args_os().skip(1).collect::<Vec<_>>()).and_then(execute);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
