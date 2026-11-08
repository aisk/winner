//! Thin safe wrappers over computecore.dll (HCS v2).
//!
//! Every HCS call is driven synchronously: create an operation, issue the
//! call, then block on `HcsWaitForOperationResult`.

use anyhow::{Context, Result, anyhow};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::System::HostComputeSystem::*;
use windows::core::{HSTRING, PWSTR};

const INFINITE: u32 = u32::MAX;

/// Take ownership of an HCS-allocated result document.
fn take_doc(p: PWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { p.to_string() }.ok();
    unsafe { LocalFree(Some(HLOCAL(p.0.cast()))) };
    s.filter(|s| !s.is_empty())
}

/// Attach the HCS result document (which carries the real error details) to a failure.
fn with_doc(err: windows::core::Error, doc: Option<String>) -> anyhow::Error {
    match doc {
        Some(doc) => anyhow!("{err}: {doc}"),
        None => anyhow!(err),
    }
}

struct Operation(HCS_OPERATION);

impl Operation {
    fn new() -> Self {
        Self(unsafe { HcsCreateOperation(None, None) })
    }

    fn wait(&self) -> Result<Option<String>> {
        let mut doc = PWSTR::null();
        let r = unsafe { HcsWaitForOperationResult(self.0, INFINITE, Some(&mut doc)) };
        let doc = take_doc(doc);
        r.map(|()| doc.clone()).map_err(|e| with_doc(e, doc))
    }

    fn wait_process(&self) -> Result<HCS_PROCESS_INFORMATION> {
        let mut info = HCS_PROCESS_INFORMATION::default();
        let mut doc = PWSTR::null();
        let r = unsafe {
            HcsWaitForOperationResultAndProcessInfo(self.0, INFINITE, Some(&mut info), Some(&mut doc))
        };
        let doc = take_doc(doc);
        r.map(|()| info).map_err(|e| with_doc(e, doc))
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        unsafe { HcsCloseOperation(self.0) }
    }
}

/// A handle to a compute system. Dropping it only closes the handle; the
/// system itself keeps running inside vmcompute until terminated.
pub struct System(HCS_SYSTEM);

// HCS handles may be used from any thread.
unsafe impl Send for System {}
unsafe impl Sync for System {}

impl System {
    pub fn create(id: &str, config: &impl serde::Serialize) -> Result<Self> {
        let json = serde_json::to_string(config)?;
        let op = Operation::new();
        let handle = unsafe { HcsCreateComputeSystem(&HSTRING::from(id), &HSTRING::from(json), op.0, None) }
            .context("HcsCreateComputeSystem")?;
        let system = Self(handle);
        op.wait().context("create compute system")?;
        Ok(system)
    }

    #[allow(dead_code)] // used by ps/rm/shim once implemented
    pub fn open(id: &str) -> Result<Self> {
        const GENERIC_ALL: u32 = 0x1000_0000;
        let handle = unsafe { HcsOpenComputeSystem(&HSTRING::from(id), GENERIC_ALL) }
            .with_context(|| format!("HcsOpenComputeSystem {id}"))?;
        Ok(Self(handle))
    }

    pub fn start(&self) -> Result<()> {
        let op = Operation::new();
        unsafe { HcsStartComputeSystem(self.0, op.0, None) }.context("HcsStartComputeSystem")?;
        op.wait().context("start compute system")?;
        Ok(())
    }

    pub fn terminate(&self) -> Result<()> {
        let op = Operation::new();
        unsafe { HcsTerminateComputeSystem(self.0, op.0, None) }.context("HcsTerminateComputeSystem")?;
        op.wait().context("terminate compute system")?;
        Ok(())
    }

    pub fn create_process(&self, params: &crate::spec::ProcessParameters) -> Result<(Process, Stdio)> {
        let json = serde_json::to_string(params)?;
        let op = Operation::new();
        let handle = unsafe { HcsCreateProcess(self.0, &HSTRING::from(json), op.0, None) }
            .context("HcsCreateProcess")?;
        let process = Process(handle);
        let info = op.wait_process().context("create process")?;
        let own = |h: windows::Win32::Foundation::HANDLE| {
            (!h.is_invalid() && !h.0.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(h.0) })
        };
        let stdio = Stdio {
            pid: info.ProcessId,
            stdin: own(info.StdInput),
            stdout: own(info.StdOutput),
            stderr: own(info.StdError),
        };
        Ok((process, stdio))
    }
}

impl Drop for System {
    fn drop(&mut self) {
        unsafe { HcsCloseComputeSystem(self.0) }
    }
}

pub struct Stdio {
    #[allow(dead_code)]
    pub pid: u32,
    pub stdin: Option<OwnedHandle>,
    pub stdout: Option<OwnedHandle>,
    pub stderr: Option<OwnedHandle>,
}

pub struct Process(HCS_PROCESS);

unsafe impl Send for Process {}
unsafe impl Sync for Process {}

impl Process {
    /// Block until the process exits and return its exit code.
    pub fn wait(&self) -> Result<u32> {
        let mut doc = PWSTR::null();
        let r = unsafe { HcsWaitForProcessExit(self.0, INFINITE, Some(&mut doc)) };
        let doc = take_doc(doc);
        r.map_err(|e| with_doc(e, doc.clone()))?;
        // The result document is a ProcessStatus: { "ProcessId", "Exited", "ExitCode", ... }
        let code = doc
            .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
            .and_then(|v| v.get("ExitCode")?.as_u64())
            .unwrap_or(0);
        Ok(code as u32)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe { HcsCloseProcess(self.0) }
    }
}
