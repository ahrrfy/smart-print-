#![allow(unsafe_code)]

//! Empirical Challenger Stress & Quota Evaluation for Spooler Disk Space (Feature 20).
//!
//! Evaluates:
//! 1. Win32 `GetDiskFreeSpaceExW` caller quota vs total free bytes invariant:
//!    `free_bytes_available_to_caller <= total_free_bytes`.
//! 2. Error handling for non-existent, invalid, or unmounted drive letters.
//! 3. Automatic fallback from invalid spooler path to system `temp_dir()`.
//! 4. Environmental override `PDFCRAFT_SPOOL_DIR` precedence.
//! 5. Extreme and zero estimated byte evaluations on live filesystem.

use std::path::{Path, PathBuf};
use pdfcraft_platform::disk::{
    get_default_spool_directory, query_disk_free_bytes, verify_spooler_disk_space,
};
use pdfcraft_print::governor::DiskVerdict;

#[test]
fn challenge_disk_free_bytes_valid_paths_and_quota_invariant() {
    let spool_dir = get_default_spool_directory();
    assert!(spool_dir.exists(), "Default spool dir must exist: {:?}", spool_dir);

    let free_spool = query_disk_free_bytes(&spool_dir);
    assert!(
        free_spool.is_ok(),
        "query_disk_free_bytes on default spool dir failed: {:?}",
        free_spool
    );
    let free_bytes = free_spool.unwrap();
    assert!(free_bytes > 0, "Free bytes on default spool dir must be > 0");

    let temp_dir = std::env::temp_dir();
    let free_temp = query_disk_free_bytes(&temp_dir);
    assert!(free_temp.is_ok(), "query_disk_free_bytes on temp dir failed");
    assert!(free_temp.unwrap() > 0);

    // On Windows, direct Win32 GetDiskFreeSpaceExW invariant check:
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut path_u16: Vec<u16> = temp_dir.as_os_str().encode_wide().collect();
        path_u16.push(0);

        let mut caller_free: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;

        let ret = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                path_u16.as_ptr(),
                &mut caller_free as *mut u64 as _,
                &mut total as *mut u64 as _,
                &mut total_free as *mut u64 as _,
            )
        };
        assert_ne!(ret, 0, "GetDiskFreeSpaceExW failed on temp_dir");
        assert!(
            caller_free <= total_free,
            "Caller free bytes ({caller_free}) must be <= total volume free bytes ({total_free})"
        );
        assert!(
            total_free <= total,
            "Total free bytes ({total_free}) must be <= total volume size ({total})"
        );
    }
}

#[test]
fn challenge_disk_query_invalid_paths_fail_gracefully() {
    // Non-existent drive
    let bad_drive = Path::new("Z:\\nonexistent_volume_dir_987654321");
    let res = query_disk_free_bytes(bad_drive);
    assert!(
        res.is_err(),
        "query_disk_free_bytes on non-existent drive must return Err"
    );

    // Empty path
    let empty_path = Path::new("");
    let res_empty = query_disk_free_bytes(empty_path);
    // On Windows, empty string is invalid directory path
    #[cfg(windows)]
    assert!(res_empty.is_err());
}

#[test]
fn challenge_verify_spooler_disk_space_fallback_behavior() {
    // 1. Given an invalid spool path, verify_spooler_disk_space must NOT panic
    // and must fall back to temp_dir.
    let bad_path = Path::new("Z:\\definitely_invalid_unmounted_drive\\spool");
    let status = verify_spooler_disk_space(Some(bad_path), 10 * 1024 * 1024);

    assert_eq!(status.spool_directory, PathBuf::from(bad_path));
    // Fallback must have retrieved available bytes from temp_dir
    assert!(
        status.available_bytes > 0,
        "Fallback must provide valid available_bytes from temp_dir"
    );
    assert_eq!(status.estimated_required_bytes, 10 * 1024 * 1024);
    assert!(!matches!(status.verdict, DiskVerdict::CriticalExhaustion { .. }));

    // 2. Astronomical required bytes must yield CriticalExhaustion
    let status_huge = verify_spooler_disk_space(None, u64::MAX / 2);
    assert!(matches!(
        status_huge.verdict,
        DiskVerdict::CriticalExhaustion { .. }
    ));

    // 3. Zero required bytes must yield Sufficient on a standard dev drive
    let status_zero = verify_spooler_disk_space(None, 0);
    assert_eq!(status_zero.verdict, DiskVerdict::Sufficient);
}

#[test]
fn challenge_spool_dir_environment_override() {
    let temp_sub = std::env::temp_dir().join("pdfcraft_spool_test_override");
    std::fs::create_dir_all(&temp_sub).expect("create test spool dir");

    unsafe {
        std::env::set_var("PDFCRAFT_SPOOL_DIR", temp_sub.to_string_lossy().as_ref());
    }
    let resolved = get_default_spool_directory();
    assert_eq!(resolved, temp_sub);

    // Clean up env var and directory
    unsafe {
        std::env::remove_var("PDFCRAFT_SPOOL_DIR");
    }
    let _ = std::fs::remove_dir(&temp_sub);
}

