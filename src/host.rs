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

/// Builds of the Windows Server LTSC releases, oldest first.
const LTSC_BUILDS: [u32; 3] = [17763, 20348, 26100];

/// Whether an image of `image` build can run process-isolated on a host of
/// `host` build. Mirrors hcsshim's osversion.CheckHostAndContainerCompat:
/// before LTSC2022 the builds must match exactly; from LTSC2022 on, a host
/// runs images from the latest LTSC not newer than itself up to its own build.
pub fn process_compatible(host: u32, image: u32) -> bool {
    if host < 20348 {
        return host == image;
    }
    let floor = LTSC_BUILDS.iter().copied().filter(|&b| b <= host).max().unwrap_or(host);
    (floor..=host).contains(&image)
}

#[cfg(test)]
mod tests {
    use super::process_compatible;

    #[test]
    fn compat() {
        assert!(process_compatible(26100, 26100));
        assert!(process_compatible(26320, 26100));
        assert!(!process_compatible(26100, 20348));
        assert!(process_compatible(22631, 20348));
        assert!(!process_compatible(26100, 26320));
        assert!(!process_compatible(17763, 17134));
    }
}
