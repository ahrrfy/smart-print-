//! Spooler disk space query and pre-flight validation. Layer L7.

use std::path::{Path, PathBuf};
use pdfcraft_print::governor::{evaluate_disk_verdict, SpoolDiskStatus};

/// Resolves the default spooler directory for the current platform:
/// 1. `PDFCRAFT_SPOOL_DIR` environment override, if specified and existing.
/// 2. Windows: `%SystemRoot%\System32\spool\PRINTERS` -> `%SystemDrive%\` -> `temp_dir()`.
/// 3. Unix: `/var/spool/cups` -> `/var/spool` -> `temp_dir()`.
pub fn get_default_spool_directory() -> PathBuf {
    if let Ok(dir) = std::env::var("PDFCRAFT_SPOOL_DIR") {
        let p = PathBuf::from(dir);
        if p.exists() {
            return p;
        }
    }

    #[cfg(windows)]
    {
        if let Ok(sys_root) = std::env::var("SystemRoot").or_else(|_| std::env::var("WINDIR")) {
            let p = PathBuf::from(sys_root).join("System32").join("spool").join("PRINTERS");
            if p.exists() {
                return p;
            }
        }
        if let Ok(sys_drive) = std::env::var("SystemDrive") {
            let p = PathBuf::from(format!("{}\\", sys_drive));
            if p.exists() {
                return p;
            }
        }
        let c_drive = PathBuf::from("C:\\");
        if c_drive.exists() {
            return c_drive;
        }
        std::env::temp_dir()
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        let cups = PathBuf::from("/var/spool/cups");
        if cups.exists() {
            return cups;
        }
        let spool = PathBuf::from("/var/spool");
        if spool.exists() {
            return spool;
        }
        let tmp = PathBuf::from("/tmp");
        if tmp.exists() {
            return tmp;
        }
        std::env::temp_dir()
    }

    #[cfg(target_arch = "wasm32")]
    {
        PathBuf::from("/")
    }
}

/// Queries available and total disk space in bytes for unprivileged callers on the given path.
/// Respects per-user NTFS disk quotas on Windows and filesystem user quotas on Unix.
/// Returns `(available_bytes, total_bytes)`.
pub fn query_disk_space_bytes(path: &str) -> (u64, u64) {
    query_disk_space_bytes_path(Path::new(path)).unwrap_or((0, 0))
}

/// Internal helper querying available and total disk space on `&Path`.
pub fn query_disk_space_bytes_path(path: &Path) -> std::io::Result<(u64, u64)> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut path_u16: Vec<u16> = path.as_os_str().encode_wide().collect();
        path_u16.push(0);

        let mut free_bytes_available: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut total_free_bytes: u64 = 0;

        // SAFETY: Calling GetDiskFreeSpaceExW with valid UTF-16 null-terminated buffer
        // and aligned output pointers for ULARGE_INTEGER (u64).
        #[allow(unsafe_code)]
        let success = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                path_u16.as_ptr(),
                &mut free_bytes_available as *mut u64 as _,
                &mut total_bytes as *mut u64 as _,
                &mut total_free_bytes as *mut u64 as _,
            )
        };

        if success != 0 {
            Ok((free_bytes_available, total_bytes))
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        let stat = rustix::fs::statvfs(path)?;
        let free = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
        let total = (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64);
        Ok((free, total))
    }

    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        Ok((1024 * 1024 * 1024, 10 * 1024 * 1024 * 1024))
    }
}

/// Queries available disk space in bytes for unprivileged callers on the given path.
/// Respects per-user NTFS disk quotas on Windows and filesystem user quotas on Unix.
pub fn query_disk_free_bytes(path: &Path) -> std::io::Result<u64> {
    query_disk_space_bytes_path(path).map(|(avail, _)| avail)
}

/// Validates whether sufficient spool disk space exists for a pending print job.
pub fn verify_spooler_disk_space(
    custom_spool_dir: Option<&Path>,
    estimated_required_bytes: u64,
) -> SpoolDiskStatus {
    let spool_directory = custom_spool_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(get_default_spool_directory);

    let available_bytes = query_disk_free_bytes(&spool_directory)
        .or_else(|_| query_disk_free_bytes(&std::env::temp_dir()))
        .unwrap_or(0);

    let verdict = evaluate_disk_verdict(available_bytes, estimated_required_bytes);

    SpoolDiskStatus {
        spool_directory,
        available_bytes,
        estimated_required_bytes,
        verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_default_spool_directory() {
        let dir = get_default_spool_directory();
        assert!(!dir.as_os_str().is_empty());
    }

    #[test]
    fn test_query_disk_free_bytes_temp_dir() {
        let temp = std::env::temp_dir();
        let free = query_disk_free_bytes(&temp);
        assert!(free.is_ok());
        assert!(free.unwrap() > 0);
    }

    #[test]
    fn test_verify_spooler_disk_space_verdict() {
        let temp = std::env::temp_dir();
        let status = verify_spooler_disk_space(Some(&temp), 1024 * 1024);
        assert!(status.available_bytes > 0);
        assert_eq!(status.estimated_required_bytes, 1024 * 1024);
        // On any standard dev/test machine, temp dir has > 1.5MB available
        assert!(!matches!(
            status.verdict,
            pdfcraft_print::governor::DiskVerdict::CriticalExhaustion { .. }
        ));
    }
}
