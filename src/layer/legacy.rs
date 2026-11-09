//! Non-base layer import through vmcompute's legacy transport format.
//!
//! Port of hcsshim `internal/wclayer/legacy.go` (legacyLayerWriter) and the
//! `legacyLayerWriterWrapper.Close` in `importlayer.go`. Entries are staged in
//! a temporary directory in the format `ImportLayer` consumes:
//!   - `Files/...`: a plain file holding the attributes (u32 LE) followed by
//!     the raw Win32 backup stream; a directory additionally gets a
//!     `<name>.$wcidirs$` file in that format
//!   - `Hives/...`: the file restored with BackupWrite, without security
//!
//! Whiteouts, hard links and directory times are applied to the imported
//! layer afterwards, because `ImportLayer` cannot express them.

use super::import::{self, Entry, GENERIC_READ, GENERIC_WRITE, FILE_SHARE_READ, Sink};
use crate::backup::{self, BackupFile};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::io::{BufWriter, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use windows::Win32::Storage::FileSystem::{CreateHardLinkW, FILE_BASIC_INFO};
use windows::core::HSTRING;

const FILES: &str = "Files";
const HIVES: &str = "Hives";
const UTILITY_VM: &str = "UtilityVM";

pub(super) struct LegacySink {
    staging: PathBuf,
    /// Parent layers, topmost first: where hard link targets are looked up.
    parents: Vec<PathBuf>,
    /// Lower-cased names added by this layer.
    added: HashSet<String>,
    pub tombstones: Vec<String>,
    /// (link name, target name, layer holding the target); `None` = this layer.
    pub links: Vec<(String, String, Option<PathBuf>)>,
    /// Directories added by this layer, parents first, with their metadata.
    pub dirs: Vec<(String, FILE_BASIC_INFO)>,
    /// Directory reparse points: staged without their placeholder directory
    /// so nothing can be imported through them, recreated before import.
    pending_dirs: Vec<String>,
}

impl LegacySink {
    /// `parents` must be given base first.
    pub fn new(staging: &Path, parents: &[PathBuf]) -> Result<Self> {
        // Verbatim (\\?\) paths: layer contents routinely exceed MAX_PATH.
        let parents = parents.iter().rev().map(std::fs::canonicalize).collect::<std::io::Result<_>>()?;
        Ok(Self {
            staging: std::fs::canonicalize(staging)?,
            parents,
            added: HashSet::new(),
            tombstones: Vec::new(),
            links: Vec::new(),
            dirs: Vec::new(),
            pending_dirs: Vec::new(),
        })
    }

    /// Final step before `ImportLayer` (legacyLayerWriter.Close).
    pub fn finish(&self) -> Result<()> {
        for name in &self.pending_dirs {
            std::fs::create_dir(import::join(&self.staging, name))?;
        }
        Ok(())
    }
}

impl Sink for LegacySink {
    fn add(&mut self, entry: &Entry) -> Result<Box<dyn Write>> {
        let name = entry.name;
        if name.eq_ignore_ascii_case(UTILITY_VM) || import::under(name, UTILITY_VM) {
            // Only needed for Hyper-V isolation; base layers carry it already.
            bail!("UtilityVM content in a non-base layer is not supported yet");
        }
        if entry.is_dir() {
            self.dirs.push((name.to_string(), entry.info));
        }

        let mut staged = name.to_string();
        if entry.is_dir() {
            if entry.reparse {
                self.pending_dirs.push(name.to_string());
            } else {
                std::fs::create_dir(import::join(&self.staging, name)).context("create staging directory")?;
            }
            staged.push_str(".$wcidirs$");
        }

        let file = std::fs::OpenOptions::new()
            .access_mode(GENERIC_READ | GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ)
            .write(true)
            .create_new(true)
            .open(import::join(&self.staging, &staged))
            .context("create staging file")?;
        // The staged file carries the times; the attributes go into the stream.
        let stripped = FILE_BASIC_INFO { FileAttributes: 0, ..entry.info };
        backup::set_basic_info(&file, &stripped).context("SetFileBasicInfo")?;
        self.added.insert(name.to_ascii_lowercase());

        if import::under(name, HIVES) {
            return Ok(Box::new(BackupFile::new(file, false)));
        }
        let mut out = BufWriter::with_capacity(1 << 16, file);
        out.write_all(&entry.info.FileAttributes.to_le_bytes())?;
        Ok(Box::new(out))
    }

    fn add_link(&mut self, name: &str, target: &str) -> Result<()> {
        if !import::under(name, FILES) || !import::under(target, FILES) {
            bail!("invalid hard link in layer: {name} -> {target}");
        }
        let root = if self.added.contains(&target.to_ascii_lowercase()) {
            None
        } else {
            let parent = self
                .parents
                .iter()
                .find(|p| import::join(p, target).symlink_metadata().is_ok())
                .with_context(|| format!("failed to find link target for {name} -> {target}"))?;
            Some(parent.clone())
        };
        // Links can only be created once ImportLayer has populated the layer.
        self.links.push((name.to_string(), target.to_string(), root));
        self.added.insert(name.to_ascii_lowercase());
        Ok(())
    }

    fn remove(&mut self, name: &str) -> Result<()> {
        if !import::under(name, FILES) {
            bail!("invalid tombstone {name}");
        }
        self.tombstones.push(name.to_string());
        Ok(())
    }
}

/// After `ImportLayer`: apply whiteouts and hard links to `layer`, then put
/// back the directory times those changes bumped.
pub(super) fn apply(sink: &LegacySink, layer: &Path) -> Result<()> {
    for name in &sink.tombstones {
        remove_if_exists(&import::join(layer, name)).with_context(|| format!("remove {name}"))?;
    }
    for (name, target, root) in &sink.links {
        let link = import::join(layer, name);
        let target = import::join(root.as_deref().unwrap_or(layer), target);
        remove_if_exists(&link).with_context(|| format!("replace {name}"))?;
        unsafe { CreateHardLinkW(&HSTRING::from(link.as_os_str()), &HSTRING::from(target.as_os_str()), None) }
            .with_context(|| format!("link {name}"))?;
    }
    for (name, info) in sink.dirs.iter().rev() {
        let dir = import::open_dir_for_times(&import::join(layer, name)).with_context(|| format!("reopen {name}"))?;
        backup::set_basic_info(&dir, info)?;
    }
    Ok(())
}

/// os.Remove semantics: a file or an empty directory; missing is fine.
fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    let result = match path.symlink_metadata() {
        Ok(m) if m.is_dir() => std::fs::remove_dir(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}
