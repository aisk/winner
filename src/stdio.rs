//! Forward stdio between the host console and a container process.
//!
//! HCS hands back plain pipe handles that are not opened for overlapped IO,
//! so each stream gets a blocking thread.

use crate::hcs::Stdio;
use std::fs::File;
use std::io::{self, Write};
use std::thread::JoinHandle;
use windows::Win32::System::Console::*;

/// Start forwarding. Returns the output threads, which finish once the
/// container closes its ends of the pipes. The stdin thread is detached
/// because a console read cannot be cancelled portably.
pub fn forward(stdio: Stdio) -> Vec<JoinHandle<()>> {
    if let Some(h) = stdio.stdin {
        std::thread::spawn(move || {
            let mut pipe = File::from(h);
            let _ = io::copy(&mut io::stdin().lock(), &mut pipe);
            // Dropping the pipe sends EOF to the container.
        });
    }
    let mut out = Vec::new();
    if let Some(h) = stdio.stdout {
        out.push(std::thread::spawn(move || {
            let _ = copy_flushing(File::from(h), io::stdout());
        }));
    }
    if let Some(h) = stdio.stderr {
        out.push(std::thread::spawn(move || {
            let _ = copy_flushing(File::from(h), io::stderr());
        }));
    }
    out
}

fn copy_flushing(mut from: File, mut to: impl Write) -> io::Result<()> {
    use std::io::Read;
    let mut buf = [0u8; 8192];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
}

/// Puts the host console into raw VT mode for the lifetime of the guard, so
/// keystrokes (including Ctrl+C) are passed through to the container's ConPTY.
pub struct RawConsole {
    input: Option<(windows::Win32::Foundation::HANDLE, CONSOLE_MODE)>,
    output: Option<(windows::Win32::Foundation::HANDLE, CONSOLE_MODE)>,
}

impl RawConsole {
    pub fn enable() -> Self {
        let input = Self::modify(STD_INPUT_HANDLE, |m| {
            (m & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT
        });
        let output = Self::modify(STD_OUTPUT_HANDLE, |m| {
            m | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN
        });
        Self { input, output }
    }

    fn modify(
        which: STD_HANDLE,
        f: impl FnOnce(CONSOLE_MODE) -> CONSOLE_MODE,
    ) -> Option<(windows::Win32::Foundation::HANDLE, CONSOLE_MODE)> {
        unsafe {
            let h = GetStdHandle(which).ok()?;
            let mut mode = CONSOLE_MODE::default();
            GetConsoleMode(h, &mut mode).ok()?;
            SetConsoleMode(h, f(mode)).ok()?;
            Some((h, mode))
        }
    }

    /// Current console size as `[rows, cols]`, the order HCS ConsoleSize expects.
    pub fn size() -> Option<[u16; 2]> {
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE).ok()?;
            let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
            GetConsoleScreenBufferInfo(h, &mut info).ok()?;
            let w = info.srWindow;
            Some([(w.Bottom - w.Top + 1) as u16, (w.Right - w.Left + 1) as u16])
        }
    }
}

impl Drop for RawConsole {
    fn drop(&mut self) {
        for (h, mode) in [self.input, self.output].into_iter().flatten() {
            unsafe {
                let _ = SetConsoleMode(h, mode);
            }
        }
    }
}
