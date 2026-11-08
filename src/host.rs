//! Host OS information.

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

/// Host build number, e.g. 26100. Uses RtlGetVersion, which is not subject
/// to the manifest-based version lie of GetVersionEx.
pub fn build() -> u32 {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // RtlGetVersion always succeeds.
    let _ = unsafe { RtlGetVersion(&mut info) };
    info.dwBuildNumber
}

/// Extract the build number from an image `os.version` such as `10.0.26100.2894`.
pub fn build_of(os_version: &str) -> Option<u32> {
    os_version.split('.').nth(2)?.parse().ok()
}
