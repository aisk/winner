//! Extract a Windows OCI layer tarball into a base layer directory.
//!
//! Port of hcsshim `pkg/ociwclayer/import.go` + `internal/wclayer/baselayerwriter.go`
//! and go-winio `backuptar`. Each tar entry becomes a file created with backup
//! semantics, whose contents, security descriptor, EAs, reparse data and
//! alternate data streams are restored through a single `BackupWrite` stream.

use crate::backup::{self, BackupFile};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use windows::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateHardLinkW, FILE_ATTRIBUTE_DIRECTORY, FILE_BASIC_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
};
use windows::core::HSTRING;

const PAX_FILE_ATTRIBUTES: &str = "MSWINDOWS.fileattr";
const PAX_SD: &str = "MSWINDOWS.sd";
const PAX_RAW_SD: &str = "MSWINDOWS.rawsd";
const PAX_MOUNT_POINT: &str = "MSWINDOWS.mountpoint";
const PAX_EA_PREFIX: &str = "MSWINDOWS.xattr.";
const PAX_CREATION_TIME: &str = "LIBARCHIVE.creationtime";
const WHITEOUT_PREFIX: &str = ".wh.";

// Access rights used by hcsshim's baseLayerWriter.
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const ACCESS_SYSTEM_SECURITY: u32 = 0x0100_0000;
const FILE_SHARE_READ: u32 = 1;

/// Summary of an extracted base layer.
pub struct Extracted {
    pub has_utility_vm: bool,
}

/// Extract `blob` (gzip or plain tar) into `root`, which must exist and be empty.
/// The caller must hold SeBackupPrivilege and SeRestorePrivilege.
pub fn extract_base_layer(blob: &Path, root: &Path) -> Result<Extracted> {
    let mut input = BufReader::with_capacity(1 << 20, File::open(blob)?);
    let gzip = input.fill_buf()?.starts_with(&[0x1f, 0x8b]);
    let reader: Box<dyn Read> = if gzip {
        Box::new(flate2::read::GzDecoder::new(input))
    } else {
        Box::new(input)
    };

    let mut writer = BaseLayerWriter::new(root)?;
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = String::from_utf8(entry.path_bytes().into_owned())
            .map_err(|_| anyhow!("non UTF-8 path in layer"))?;
        let name = name.trim_start_matches("./").trim_end_matches('/').to_string();
        if name.is_empty() {
            continue;
        }
        writer.entry(&name, &mut entry).with_context(|| format!("extract {name}"))?;
    }
    writer.finish()
}

struct Current {
    name: String,
    out: BackupFile,
}

struct BaseLayerWriter {
    root: PathBuf,
    current: Option<Current>,
    /// Directories and their metadata, parents before children.
    dirs: Vec<(PathBuf, FILE_BASIC_INFO)>,
    /// Lower-cased names of reparse points, which must never be traversed.
    reparse_points: HashSet<String>,
    has_utility_vm: bool,
}

impl BaseLayerWriter {
    fn new(root: &Path) -> Result<Self> {
        // Verbatim (\\?\) root so long paths work and nothing is normalized.
        let root = std::fs::canonicalize(root)?;
        Ok(Self {
            root,
            current: None,
            dirs: Vec::new(),
            reparse_points: HashSet::new(),
            has_utility_vm: false,
        })
    }

    fn entry<R: Read>(&mut self, name: &str, entry: &mut tar::Entry<R>) -> Result<()> {
        let kind = entry.header().entry_type();

        // Alternate data stream of the file currently being written.
        if let Some(cur) = &mut self.current {
            if kind.is_file() {
                if let Some(stream) = name.strip_prefix(cur.name.as_str()).filter(|s| s.starts_with(':')) {
                    let size = entry.size();
                    cur.out.write_all(&backup::stream_header(
                        backup::BACKUP_ALTERNATE_DATA,
                        size,
                        &format!("{stream}:$DATA"),
                    ))?;
                    copy_exact(entry, &mut cur.out, size)?;
                    return Ok(());
                }
            }
        }
        self.current = None;

        let base = name.rsplit('/').next().unwrap_or(name);
        if base.starts_with(WHITEOUT_PREFIX) {
            bail!("base layer cannot contain whiteouts");
        }

        if kind.is_hard_link() {
            let target = entry
                .link_name_bytes()
                .ok_or_else(|| anyhow!("hard link without target"))?;
            let target = String::from_utf8(target.into_owned())?;
            let link = self.resolve(name)?;
            let target = self.resolve(target.trim_start_matches("./"))?;
            unsafe { CreateHardLinkW(&HSTRING::from(link.as_os_str()), &HSTRING::from(target.as_os_str()), None) }
                .context("CreateHardLinkW")?;
            return Ok(());
        }

        if name.eq_ignore_ascii_case("UtilityVM/Files") {
            self.has_utility_vm = true;
        }

        let pax = pax_records(entry)?;
        let info = basic_info(entry.header(), &pax)?;
        let path = self.resolve(name)?;
        let is_dir = info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;

        let mut opts = std::fs::OpenOptions::new();
        opts.access_mode(GENERIC_READ | GENERIC_WRITE | WRITE_DAC | WRITE_OWNER | ACCESS_SYSTEM_SECURITY)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0);
        let file = if is_dir {
            unsafe { CreateDirectoryW(&HSTRING::from(path.as_os_str()), None) }.context("CreateDirectoryW")?;
            self.dirs.push((path.clone(), info));
            opts.open(&path)
        } else {
            // std requires write(true) for create_new; access_mode still decides the rights.
            opts.write(true).create_new(true).open(&path)
        }
        .context("open")?;
        backup::set_basic_info(&file, &info).context("SetFileBasicInfo")?;

        let mut out = BackupFile::new(file);
        if let Some(sd) = security_descriptor(&pax)? {
            out.write_all(&backup::stream_header(backup::BACKUP_SECURITY_DATA, sd.len() as u64, ""))?;
            out.write_all(&sd)?;
        }
        let eas = extended_attributes(&pax)?;
        if !eas.is_empty() {
            let data = backup::encode_eas(&eas)?;
            out.write_all(&backup::stream_header(backup::BACKUP_EA_DATA, data.len() as u64, ""))?;
            out.write_all(&data)?;
        }
        if kind.is_symlink() {
            let target = entry
                .link_name_bytes()
                .ok_or_else(|| anyhow!("symlink without target"))?;
            let target = String::from_utf8(target.into_owned())?.replace('/', "\\");
            let data = backup::encode_reparse_point(&target, pax.iter().any(|(k, _)| k == PAX_MOUNT_POINT));
            out.write_all(&backup::stream_header(backup::BACKUP_REPARSE_DATA, data.len() as u64, ""))?;
            out.write_all(&data)?;
            self.reparse_points.insert(name.to_ascii_lowercase());
        }
        if kind.is_file() {
            let size = entry.size();
            out.write_all(&backup::stream_header(backup::BACKUP_DATA, size, ""))?;
            copy_exact(entry, &mut out, size)?;
        }

        // Keep the file open: following entries may be its alternate data streams.
        self.current = Some(Current { name: name.to_string(), out });
        Ok(())
    }

    /// Map a tar path to a host path under the root, refusing anything that
    /// could escape it: `..`, drive or stream syntax, or walking through a
    /// reparse point created earlier in this layer.
    fn resolve(&self, name: &str) -> Result<PathBuf> {
        let mut path = self.root.clone();
        let mut prefix = String::new();
        for comp in name.split('/') {
            if comp.is_empty() || comp == "." || comp == ".." || comp.contains(['\\', ':']) {
                bail!("invalid path {name:?}");
            }
            if self.reparse_points.contains(&prefix) {
                bail!("path {name:?} traverses a reparse point");
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(&comp.to_ascii_lowercase());
            path.push(comp);
        }
        Ok(path)
    }

    fn finish(mut self) -> Result<Extracted> {
        self.current = None;
        // Creating children bumped the directories' times; restore them,
        // children first.
        for (path, info) in self.dirs.iter().rev() {
            let dir = std::fs::OpenOptions::new()
                .access_mode(GENERIC_READ | GENERIC_WRITE)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
                .open(path)
                .with_context(|| format!("reopen {}", path.display()))?;
            backup::set_basic_info(&dir, info)?;
        }
        Ok(Extracted { has_utility_vm: self.has_utility_vm })
    }
}

fn copy_exact(from: &mut impl Read, to: &mut impl Write, size: u64) -> io::Result<()> {
    let n = io::copy(&mut from.take(size), to)?;
    if n != size {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

fn pax_records<R: Read>(entry: &mut tar::Entry<R>) -> Result<Vec<(String, Vec<u8>)>> {
    let Some(exts) = entry.pax_extensions()? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for ext in exts {
        let ext = ext?;
        out.push((ext.key()?.to_string(), ext.value_bytes().to_vec()));
    }
    Ok(out)
}

fn pax_str<'a>(pax: &'a [(String, Vec<u8>)], key: &str) -> Option<&'a str> {
    pax.iter().find(|(k, _)| k == key).and_then(|(_, v)| std::str::from_utf8(v).ok())
}

fn pax_time(pax: &[(String, Vec<u8>)], key: &str) -> Option<i64> {
    let (s, n) = backup::parse_pax_time(pax_str(pax, key)?)?;
    Some(backup::unix_to_filetime(s, n))
}

/// Equivalent of backuptar.FileInfoFromHeader. Missing times are left as 0,
/// which SetFileInformationByHandle treats as "do not change".
fn basic_info(header: &tar::Header, pax: &[(String, Vec<u8>)]) -> Result<FILE_BASIC_INFO> {
    let mtime = pax_time(pax, "mtime").unwrap_or_else(|| backup::unix_to_filetime(header.mtime().unwrap_or(0) as i64, 0));
    let attributes = match pax_str(pax, PAX_FILE_ATTRIBUTES) {
        Some(s) => s.parse().context("invalid MSWINDOWS.fileattr")?,
        None if header.entry_type().is_dir() => FILE_ATTRIBUTE_DIRECTORY.0,
        None => 0,
    };
    Ok(FILE_BASIC_INFO {
        CreationTime: pax_time(pax, PAX_CREATION_TIME).unwrap_or(mtime),
        LastAccessTime: pax_time(pax, "atime").unwrap_or(0),
        LastWriteTime: mtime,
        ChangeTime: pax_time(pax, "ctime").unwrap_or(0),
        FileAttributes: attributes,
    })
}

fn security_descriptor(pax: &[(String, Vec<u8>)]) -> Result<Option<Vec<u8>>> {
    if let Some(raw) = pax_str(pax, PAX_RAW_SD) {
        return Ok(Some(base64::engine::general_purpose::STANDARD.decode(raw)?));
    }
    if pax_str(pax, PAX_SD).is_some() {
        // Legacy SDDL form; modern layers use the raw form.
        // TODO: ConvertStringSecurityDescriptorToSecurityDescriptorW if ever needed.
        bail!("SDDL security descriptors (MSWINDOWS.sd) are not supported");
    }
    Ok(None)
}

fn extended_attributes(pax: &[(String, Vec<u8>)]) -> Result<Vec<(String, Vec<u8>)>> {
    pax.iter()
        .filter_map(|(k, v)| Some((k.strip_prefix(PAX_EA_PREFIX)?, v)))
        .map(|(name, v)| Ok((name.to_string(), base64::engine::general_purpose::STANDARD.decode(v)?)))
        .collect()
}
