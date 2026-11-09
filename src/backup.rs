//! Win32 backup stream helpers (BackupWrite), privileges and FILETIME math.
//!
//! The encodings mirror go-winio (`backup.go`, `ea.go`, `reparse.go`) so that
//! layers extracted here match what hcsshim/containerd produce.

use anyhow::{Context, Result, bail};
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::io::AsRawHandle;
use windows::Win32::Foundation::{ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    BackupWrite, FILE_BASIC_INFO, FileBasicInfo, SetFileInformationByHandle,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::HSTRING;

// WIN32_STREAM_ID stream ids.
pub const BACKUP_DATA: u32 = 1;
pub const BACKUP_EA_DATA: u32 = 2;
pub const BACKUP_SECURITY_DATA: u32 = 3;
pub const BACKUP_ALTERNATE_DATA: u32 = 4;
pub const BACKUP_REPARSE_DATA: u32 = 8;

/// Encode a WIN32_STREAM_ID header (packed, 20 bytes + UTF-16 name).
pub fn stream_header(id: u32, size: u64, name: &str) -> Vec<u8> {
    let name: Vec<u16> = name.encode_utf16().collect();
    let mut b = Vec::with_capacity(20 + name.len() * 2);
    b.extend_from_slice(&id.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes()); // dwStreamAttributes
    b.extend_from_slice(&size.to_le_bytes());
    b.extend_from_slice(&((name.len() * 2) as u32).to_le_bytes());
    for c in name {
        b.extend_from_slice(&c.to_le_bytes());
    }
    b
}

/// Encode FILE_FULL_EA_INFORMATION entries. Every entry, including the last,
/// is padded to 4 bytes, matching go-winio.
pub fn encode_eas(eas: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    for (i, (name, value)) in eas.iter().enumerate() {
        let (Ok(name_len), Ok(value_len)) = (u8::try_from(name.len()), u16::try_from(value.len())) else {
            bail!("extended attribute {name} is too large");
        };
        let entry = 8 + name.len() + 1 + value.len();
        let padded = (entry + 3) & !3;
        let next = if i + 1 == eas.len() { 0 } else { padded as u32 };
        b.extend_from_slice(&next.to_le_bytes());
        b.push(0); // Flags
        b.push(name_len);
        b.extend_from_slice(&value_len.to_le_bytes());
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        b.extend_from_slice(value);
        b.resize(b.len() + padded - entry, 0);
    }
    Ok(b)
}

/// Encode a REPARSE_DATA_BUFFER for a symlink or mount point (junction).
pub fn encode_reparse_point(target: &str, mount_point: bool) -> Vec<u8> {
    const TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const TAG_SYMLINK: u32 = 0xA000_000C;
    const SYMLINK_FLAG_RELATIVE: u32 = 1;

    let is_drive = |s: &str| {
        let b = s.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    };
    let (nt_target, relative) = if let Some(rest) = target.strip_prefix(r"\\?\") {
        (format!(r"\??\{rest}"), false)
    } else if let Some(rest) = target.strip_prefix(r"\\") {
        (format!(r"\??\UNC\{rest}"), false)
    } else if is_drive(target) {
        (format!(r"\??\{target}"), false)
    } else {
        (target.to_string(), true)
    };
    // Both names are NUL-terminated even though they are counted strings.
    let print: Vec<u16> = target.encode_utf16().chain([0]).collect();
    let subst: Vec<u16> = nt_target.encode_utf16().chain([0]).collect();

    let mut data_len = 8 + subst.len() * 2 + print.len() * 2;
    let tag = if mount_point {
        TAG_MOUNT_POINT
    } else {
        data_len += 4; // symlink flags
        TAG_SYMLINK
    };

    let mut b = Vec::with_capacity(8 + data_len);
    b.extend_from_slice(&tag.to_le_bytes());
    b.extend_from_slice(&(data_len as u16).to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    b.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    b.extend_from_slice(&(((subst.len() - 1) * 2) as u16).to_le_bytes());
    b.extend_from_slice(&((subst.len() * 2) as u16).to_le_bytes()); // PrintNameOffset
    b.extend_from_slice(&(((print.len() - 1) * 2) as u16).to_le_bytes());
    if !mount_point {
        let flags = if relative { SYMLINK_FLAG_RELATIVE } else { 0 };
        b.extend_from_slice(&flags.to_le_bytes());
    }
    for c in subst.iter().chain(&print) {
        b.extend_from_slice(&c.to_le_bytes());
    }
    b
}

/// A file being restored through `BackupWrite`.
pub struct BackupFile {
    file: File,
    ctx: *mut c_void,
    process_security: bool,
}

impl BackupFile {
    /// `process_security` restores security descriptors from the stream
    /// (go-winio's NewBackupFileWriter `includeSecurity`).
    pub fn new(file: File, process_security: bool) -> Self {
        Self { file, ctx: std::ptr::null_mut(), process_security }
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.file.as_raw_handle())
    }
}

impl Write for BackupFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut n = 0u32;
        unsafe { BackupWrite(self.handle(), buf, &mut n, false, self.process_security, &mut self.ctx) }
            .map_err(io::Error::from)?;
        Ok(n as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for BackupFile {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            // An abort call releases the BackupWrite context.
            let mut n = 0u32;
            let _ = unsafe { BackupWrite(self.handle(), &[], &mut n, true, false, &mut self.ctx) };
        }
    }
}

pub fn set_basic_info(file: &File, info: &FILE_BASIC_INFO) -> io::Result<()> {
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileBasicInfo,
            std::ptr::from_ref(info).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    }
    .map_err(io::Error::from)
}

/// Seconds/nanoseconds since the Unix epoch to a FILETIME-style i64.
pub fn unix_to_filetime(secs: i64, nanos: u32) -> i64 {
    const EPOCH_DIFF: i64 = 116_444_736_000_000_000; // 1601-01-01 to 1970-01-01, in 100ns
    secs * 10_000_000 + (nanos / 100) as i64 + EPOCH_DIFF
}

/// Parse a PAX time value such as `1700000000.123456789` or `-1.5`.
pub fn parse_pax_time(s: &str) -> Option<(i64, u32)> {
    let (neg, s) = s.strip_prefix('-').map_or((false, s), |r| (true, r));
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let secs: i64 = int.parse().ok()?;
    let frac: String = frac.chars().chain(std::iter::repeat('0')).take(9).collect();
    let nanos: u32 = frac.parse().ok()?;
    Some(match (neg, nanos) {
        (false, _) => (secs, nanos),
        (true, 0) => (-secs, 0),
        (true, n) => (-secs - 1, 1_000_000_000 - n),
    })
}

struct OwnedToken(HANDLE);

impl Drop for OwnedToken {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Enable privileges on the process token. Fails if the token does not hold
/// them, which in practice means the process is not elevated.
pub fn enable_privileges(names: &[&str]) -> Result<()> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token)
            .context("OpenProcessToken")?;
        let token = OwnedToken(token);
        for name in names {
            let mut luid = LUID::default();
            LookupPrivilegeValueW(None, &HSTRING::from(*name), &mut luid)
                .with_context(|| format!("LookupPrivilegeValue {name}"))?;
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            AdjustTokenPrivileges(token.0, false, Some(&tp), 0, None, None)
                .with_context(|| format!("AdjustTokenPrivileges {name}"))?;
            if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                bail!("{name} is not held by this process; run winner from an elevated terminal (or via `sudo`)");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_time() {
        assert_eq!(parse_pax_time("1700000000"), Some((1_700_000_000, 0)));
        assert_eq!(parse_pax_time("1.5"), Some((1, 500_000_000)));
        assert_eq!(parse_pax_time("-1.25"), Some((-2, 750_000_000)));
        assert_eq!(unix_to_filetime(0, 0), 116_444_736_000_000_000);
    }

    #[test]
    fn header_layout() {
        let h = stream_header(BACKUP_ALTERNATE_DATA, 5, ":s:$DATA");
        assert_eq!(h.len(), 20 + 8 * 2);
        assert_eq!(&h[16..20], &16u32.to_le_bytes());
    }

    #[test]
    fn ea_padding() {
        let b = encode_eas(&[("A".into(), vec![1]), ("BC".into(), vec![])]).unwrap();
        // entry 1: 8 + 1 + 1 + 1 = 11 -> 12; entry 2: 8 + 2 + 1 = 11 -> 12
        assert_eq!(b.len(), 24);
        assert_eq!(&b[0..4], &12u32.to_le_bytes());
        assert_eq!(&b[12..16], &0u32.to_le_bytes());
    }

    #[test]
    fn junction_reparse() {
        let b = encode_reparse_point(r"C:\Users", true);
        let subst_len = r"\??\C:\Users".len() * 2;
        assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), 0xA000_0003);
        assert_eq!(u16::from_le_bytes(b[10..12].try_into().unwrap()) as usize, subst_len);
        assert_eq!(b.len(), 8 + 8 + subst_len + 2 + r"C:\Users".len() * 2 + 2);
    }
}
