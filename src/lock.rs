//! File locks built from share modes. Windows releases them when the holding
//! process exits, however it exits, so they cannot go stale the way a status
//! written to disk can.
//!
//! Every lock file is opened for read and write; what differs is what the
//! holder lets others do at the same time:
//!
//! | lock | holder shares | effect |
//! |---|---|---|
//! | `containers/<id>/supervisor.lock` | delete | one holder; the directory can still be deleted |
//! | `store.lock`, shared (pull) | read, write | any number of shared holders |
//! | `store.lock`, exclusive (GC) | nothing | excludes everyone |
//! | `import.lock` | nothing | one layer import at a time |

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::ERROR_SHARING_VIOLATION;
pub use windows::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_NONE, FILE_SHARE_READ, FILE_SHARE_WRITE};

/// Take the lock at `path`, creating the file if needed.
pub fn open(path: &Path, share: FILE_SHARE_MODE) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(share.0)
        .open(path)
}

/// Whether `open` failed because someone else holds the lock.
pub fn is_held(e: &io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_SHARING_VIOLATION.0 as i32)
}

/// `open`, retrying while the lock is held, for at most `timeout`.
pub fn wait(path: &Path, share: FILE_SHARE_MODE, timeout: Duration) -> io::Result<File> {
    let deadline = Instant::now() + timeout;
    loop {
        match open(path, share) {
            Err(e) if is_held(&e) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            r => return r,
        }
    }
}

/// Whether someone holds the lock at `path` in a way that excludes readers
/// (a supervisor lock), without taking it. A missing file is not held.
pub fn is_locked(path: &Path) -> bool {
    File::open(path).is_err_and(|e| is_held(&e))
}
