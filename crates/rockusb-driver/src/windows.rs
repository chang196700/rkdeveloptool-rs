// Copyright (C) 2026 Feng Chang
// SPDX-License-Identifier: MIT OR Apache-2.0 OR GPL-2.0-or-later

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_MapCrToWin32Err, CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows_sys::core::{BOOL, GUID};

const USB_DEVICE_INTERFACE: GUID = GUID::from_u128(0xa5dcbf10_6530_11d2_901f_00c04fb951ed);
const GET_CONFIGURATION: u32 = 0x0022_0000;
const DOWNLOAD_471: u32 = 0x8000_a000;
const DOWNLOAD_472: u32 = 0x8000_a004;
const MAX_BULK_TRANSFER: usize = 1024 * 1024;

/// A Windows device bound to the original Rockusb driver.
///
/// Operations preserve message boundaries; perform them in protocol order.
pub struct Device {
    control: OwnedHandle,
    input: OwnedHandle,
    output: OwnedHandle,
    input_packet_size: usize,
}

impl Device {
    /// Open the present USB device with this Windows PnP instance ID.
    ///
    /// The device must expose the Rockusb interface and be bound to Rockusb.
    /// Opening reads the cached configuration descriptor, without sending a
    /// command to the device or changing its driver.
    pub fn open(instance_id: &OsStr) -> io::Result<Self> {
        let path = device_path(instance_id)?;
        let control = open_handle(
            &path,
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )?;
        let mut configuration = vec![0; u16::MAX as usize];
        let actual = perform(
            &control,
            Some(Duration::from_secs(60)),
            |overlapped, actual| unsafe {
                DeviceIoControl(
                    control.as_raw_handle(),
                    GET_CONFIGURATION,
                    null(),
                    0,
                    configuration.as_mut_ptr().cast(),
                    configuration.len() as u32,
                    actual,
                    overlapped,
                )
            },
        )?;
        configuration.truncate(actual as usize);
        let input_packet_size = packet_size(&configuration)?;
        let mut input_path = path.clone();
        input_path.push("\\PIPE00");
        let input = open_handle(&input_path, GENERIC_READ, FILE_SHARE_READ)?;
        let mut output_path = path;
        output_path.push("\\PIPE01");
        let output = open_handle(&output_path, GENERIC_WRITE, FILE_SHARE_WRITE)?;
        Ok(Self {
            control,
            input,
            output,
            input_packet_size,
        })
    }

    /// Send one complete bulk OUT message within the given timeout.
    ///
    /// Large messages use driver calls of at most 1 MiB. The timeout covers
    /// the entire message, and every call must transfer its complete chunk.
    pub fn write(&mut self, data: &[u8], timeout: Duration) -> io::Result<()> {
        u32::try_from(data.len()).map_err(|_| invalid("Rockusb bulk transfer is too large"))?;
        let started = Instant::now();
        for chunk in data.chunks(MAX_BULK_TRANSFER) {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or_else(timed_out)?;
            let actual = perform(&self.output, Some(remaining), |overlapped, actual| unsafe {
                WriteFile(
                    self.output.as_raw_handle(),
                    chunk.as_ptr(),
                    chunk.len() as u32,
                    actual,
                    overlapped,
                )
            })?;
            if actual as usize != chunk.len() {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "short Rockusb bulk write",
                ));
            }
        }
        Ok(())
    }

    /// Receive a bulk IN message, rounding capacity up to a whole USB packet.
    ///
    /// A short USB packet terminates the message. The result can exceed
    /// `length` by the alignment padding, which allows callers to discard
    /// stale packets. Large reads use driver calls of at most 1 MiB and stop
    /// at the first short read. The timeout covers the entire message;
    /// `None` waits without a timeout.
    pub fn read_packet(&mut self, length: usize, timeout: Option<Duration>) -> io::Result<Vec<u8>> {
        if length == 0 {
            return Err(invalid("zero-length Rockusb bulk read"));
        }
        let capacity = length
            .checked_add(self.input_packet_size - 1)
            .map(|length| length / self.input_packet_size * self.input_packet_size)
            .filter(|length| u32::try_from(*length).is_ok())
            .ok_or_else(|| invalid("Rockusb bulk transfer is too large"))?;
        let mut data = vec![0; capacity];
        let started = Instant::now();
        let mut total = 0;
        for chunk in data.chunks_mut(MAX_BULK_TRANSFER) {
            let remaining = timeout
                .map(|timeout| timeout.checked_sub(started.elapsed()).ok_or_else(timed_out))
                .transpose()?;
            let actual = perform(&self.input, remaining, |overlapped, actual| unsafe {
                ReadFile(
                    self.input.as_raw_handle(),
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                    actual,
                    overlapped,
                )
            })? as usize;
            if actual > chunk.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized Rockusb bulk read",
                ));
            }
            total += actual;
            if actual < chunk.len() {
                break;
            }
        }
        data.truncate(total);
        Ok(data)
    }

    /// Send an already-framed Maskrom block using request 0x471 or 0x472.
    ///
    /// `data` is at most 4096 bytes. The caller supplies all protocol framing,
    /// including encryption and checksums when required by its Loader.
    pub fn control_download(&self, index: u16, data: &[u8], timeout: Duration) -> io::Result<()> {
        let code = match index {
            0x471 => DOWNLOAD_471,
            0x472 => DOWNLOAD_472,
            _ => return Err(invalid("unsupported Rockusb Maskrom request")),
        };
        if data.len() > 4096 {
            return Err(invalid("Rockusb Maskrom blocks cannot exceed 4096 bytes"));
        }
        let actual = perform(&self.control, Some(timeout), |overlapped, actual| unsafe {
            DeviceIoControl(
                self.control.as_raw_handle(),
                code,
                data.as_ptr().cast(),
                data.len() as u32,
                null_mut(),
                0,
                actual,
                overlapped,
            )
        })?;
        if actual as usize != data.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short Rockusb Maskrom download",
            ));
        }
        Ok(())
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "Rockusb transfer timed out")
}

fn device_path(instance_id: &OsStr) -> io::Result<OsString> {
    let mut instance = instance_id.encode_wide().collect::<Vec<_>>();
    if instance.is_empty() || instance.contains(&0) {
        return Err(invalid("invalid Windows PnP instance ID"));
    }
    instance.push(0);
    loop {
        let mut length = 0;
        // The instance ID and interface GUID remain live for both CM calls.
        let result = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut length,
                &USB_DEVICE_INTERFACE,
                instance.as_ptr(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        cm_result(result)?;
        let mut paths = vec![0; length as usize];
        let result = unsafe {
            CM_Get_Device_Interface_ListW(
                &USB_DEVICE_INTERFACE,
                instance.as_ptr(),
                paths.as_mut_ptr(),
                length,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if result == CR_BUFFER_SMALL {
            continue;
        }
        cm_result(result)?;
        let end = paths.iter().position(|unit| *unit == 0).unwrap_or(0);
        if end == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Rockusb device interface not found",
            ));
        }
        return Ok(OsString::from_wide(&paths[..end]));
    }
}

fn cm_result(result: u32) -> io::Result<()> {
    if result == CR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(
            unsafe { CM_MapCrToWin32Err(result, ERROR_GEN_FAILURE) } as i32,
        ))
    }
}

fn open_handle(path: &OsStr, access: u32, sharing: u32) -> io::Result<OwnedHandle> {
    let path = path.encode_wide().chain(Some(0)).collect::<Vec<_>>();
    // The path is NUL-terminated. OwnedHandle takes exclusive ownership of a
    // successful CreateFileW result and closes it exactly once.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            sharing,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            null_mut(),
        )
    };
    owned_handle(handle)
}

fn owned_handle(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

fn perform(
    handle: &OwnedHandle,
    timeout: Option<Duration>,
    operation: impl FnOnce(*mut OVERLAPPED, *mut u32) -> BOOL,
) -> io::Result<u32> {
    let event = owned_handle(unsafe { CreateEventW(null(), 1, 0, null()) })?;
    let mut overlapped = OVERLAPPED {
        hEvent: event.as_raw_handle(),
        ..OVERLAPPED::default()
    };
    let mut actual = 0;
    // Every caller keeps its buffer alive until this function has completed or
    // cancelled and drained the I/O. The OVERLAPPED and event live just as long.
    let immediate = operation(&mut overlapped, &mut actual);
    if immediate == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error);
        }
        let milliseconds = timeout.map_or(INFINITE, |duration| {
            duration
                .as_millis()
                .saturating_add(u128::from(duration.subsec_nanos() % 1_000_000 != 0))
                .min(u128::from(INFINITE - 1)) as u32
        });
        let waited = unsafe { WaitForSingleObject(event.as_raw_handle(), milliseconds) };
        if waited != WAIT_OBJECT_0 {
            let error = if waited == WAIT_TIMEOUT {
                timed_out()
            } else {
                io::Error::last_os_error()
            };
            // Cancellation can race completion. Always drain the request, even
            // if CancelIoEx reports that it is no longer pending, before any
            // buffer, event or OVERLAPPED storage is released.
            unsafe {
                CancelIoEx(handle.as_raw_handle(), &overlapped);
                GetOverlappedResult(handle.as_raw_handle(), &overlapped, &mut actual, 1);
            }
            return Err(error);
        }
    }
    if unsafe { GetOverlappedResult(handle.as_raw_handle(), &overlapped, &mut actual, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(actual)
}

fn packet_size(configuration: &[u8]) -> io::Result<usize> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported Rockusb configuration",
        )
    };
    if configuration.len() < 9
        || configuration[0] != 9
        || configuration[1] != 2
        || configuration[4] != 1
        || usize::from(u16::from_le_bytes([configuration[2], configuration[3]]))
            != configuration.len()
    {
        return Err(invalid());
    }
    let mut offset = 9;
    let mut interface = false;
    let mut endpoints = Vec::new();
    while offset < configuration.len() {
        let remaining = &configuration[offset..];
        let length = usize::from(remaining[0]);
        if length < 2 || length > remaining.len() {
            return Err(invalid());
        }
        let descriptor = &remaining[..length];
        match descriptor[1] {
            4 if descriptor.len() >= 9 => {
                if interface || descriptor[2..8] != [0, 0, 2, 0xff, 6, 5] {
                    return Err(invalid());
                }
                interface = true;
            }
            5 if interface && descriptor.len() >= 7 => {
                if descriptor[3] & 3 != 2 {
                    return Err(invalid());
                }
                endpoints.push((
                    descriptor[2],
                    usize::from(u16::from_le_bytes([descriptor[4], descriptor[5]])) & 0x7ff,
                ));
            }
            _ => {}
        }
        offset += length;
    }
    if endpoints.len() != 2
        || endpoints[0].0 & 0x80 == 0
        || endpoints[1].0 & 0x80 != 0
        || !endpoints[0].1.is_power_of_two()
        || endpoints[0].1 > 1024
    {
        return Err(invalid());
    }
    Ok(endpoints[0].1)
}
