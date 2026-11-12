//! Two-tier concurrency governor and analytical spool footprint calculator. Layer L4.

use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use thiserror::Error;

pub use crate::stream::CancellationToken;

/// Error conditions arising in concurrency governance or disk space evaluation.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum GovernorError {
    #[error("operation cancelled by user or supervisor")]
    Cancelled,
    #[error("concurrency lock poisoned")]
    LockPoisoned,
    #[error("insufficient spool disk space: required {estimated_mb} MB, but only {available_mb} MB available on {spool_dir}")]
    DiskExhaustion {
        spool_dir: PathBuf,
        available_mb: u64,
        estimated_mb: u64,
    },
}

/// Counting semaphore for synchronous thread permit management with cooperative cancellation.
#[derive(Debug)]
pub struct CountingSemaphore {
    permits: Mutex<usize>,
    cvar: Condvar,
    max_permits: usize,
}

impl CountingSemaphore {
    /// Creates a new counting semaphore with the given initial and maximum permit count.
    pub fn new(permits: usize) -> Self {
        Self {
            permits: Mutex::new(permits),
            cvar: Condvar::new(),
            max_permits: permits,
        }
    }

    /// Number of currently available permits.
    pub fn available_permits(&self) -> usize {
        self.permits.lock().map(|g| *g).unwrap_or(0)
    }

    /// Maximum permit capacity configured for this semaphore.
    pub fn max_permits(&self) -> usize {
        self.max_permits
    }

    /// Acquires a permit, blocking up to 50ms per loop iteration while checking cancellation.
    pub fn acquire(&self, token: &CancellationToken) -> Result<(), GovernorError> {
        let mut guard = self.permits.lock().map_err(|_| GovernorError::LockPoisoned)?;
        loop {
            if token.is_cancelled() {
                return Err(GovernorError::Cancelled);
            }
            if *guard > 0 {
                *guard -= 1;
                return Ok(());
            }
            let (g, timeout) = self
                .cvar
                .wait_timeout(guard, Duration::from_millis(50))
                .map_err(|_| GovernorError::LockPoisoned)?;
            guard = g;
            if timeout.timed_out() && token.is_cancelled() {
                return Err(GovernorError::Cancelled);
            }
        }
    }

    /// Releases a permit back to the semaphore and notifies one waiting thread.
    pub fn release(&self) {
        if let Ok(mut guard) = self.permits.lock() {
            *guard = (*guard + 1).min(self.max_permits);
            self.cvar.notify_one();
        }
    }
}

/// RAII guard releasing a CPU permit upon drop.
#[derive(Debug)]
pub struct CpuPermitGuard<'a> {
    semaphore: &'a CountingSemaphore,
}

impl<'a> Drop for CpuPermitGuard<'a> {
    fn drop(&mut self) {
        self.semaphore.release();
    }
}

/// RAII guard releasing a Spool permit upon drop.
#[derive(Debug)]
pub struct SpoolPermitGuard<'a> {
    semaphore: &'a CountingSemaphore,
}

impl<'a> Drop for SpoolPermitGuard<'a> {
    fn drop(&mut self) {
        self.semaphore.release();
    }
}

/// Two-tier resource governor controlling CPU concurrency and spool driver serialization.
pub struct ConcurrencyGovernor {
    cpu_semaphore: CountingSemaphore,
    spool_semaphore: CountingSemaphore,
}

impl ConcurrencyGovernor {
    /// Creates a governor with explicit permit caps.
    pub fn new(cpu_permits: usize, spool_permits: usize) -> Self {
        Self {
            cpu_semaphore: CountingSemaphore::new(cpu_permits),
            spool_semaphore: CountingSemaphore::new(spool_permits),
        }
    }

    /// Automatically sizes permits based on system topology:
    /// - CPU permits: $\le \min(\text{CPUs}, 4)$ (leaving at least 1-2 cores free for the UI loop)
    /// - Spool permits: $\le 2$ (preventing winspool.drv contention and disk thrashing)
    pub fn default_for_system() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        let cpu_limit = cpus.clamp(1, 4);
        let spool_limit = 2;
        Self::new(cpu_limit, spool_limit)
    }

    /// Acquires a CPU permit.
    pub fn acquire_cpu_permit<'a>(
        &'a self,
        token: &CancellationToken,
    ) -> Result<CpuPermitGuard<'a>, GovernorError> {
        self.cpu_semaphore.acquire(token)?;
        Ok(CpuPermitGuard {
            semaphore: &self.cpu_semaphore,
        })
    }

    /// Acquires a Spool driver permit.
    pub fn acquire_spool_permit<'a>(
        &'a self,
        token: &CancellationToken,
    ) -> Result<SpoolPermitGuard<'a>, GovernorError> {
        self.spool_semaphore.acquire(token)?;
        Ok(SpoolPermitGuard {
            semaphore: &self.spool_semaphore,
        })
    }

    /// Available CPU permits.
    pub fn available_cpu_permits(&self) -> usize {
        self.cpu_semaphore.available_permits()
    }

    /// Available Spool permits.
    pub fn available_spool_permits(&self) -> usize {
        self.spool_semaphore.available_permits()
    }
}

/// Rasterization / vector mode for analytical spool size calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SheetRasterMode {
    /// Standard vector commands and embedded fonts (~350 KB per sheet).
    Vector,
    /// 300 DPI transparency flattening or raster printing (~18 MB per sheet).
    Raster300Dpi,
    /// Custom byte footprint per sheet.
    CustomBytes(u64),
}

impl SheetRasterMode {
    pub fn estimated_sheet_bytes(&self) -> u64 {
        match self {
            Self::Vector => 350 * 1024,
            Self::Raster300Dpi => 18 * 1024 * 1024,
            Self::CustomBytes(b) => *b,
        }
    }
}

/// Job estimate input for spool footprint calculation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterJobEstimate {
    pub sheets: usize,
    pub copies: usize,
    pub mode: SheetRasterMode,
}

impl PrinterJobEstimate {
    pub fn new(sheets: usize, copies: usize, mode: SheetRasterMode) -> Self {
        Self {
            sheets,
            copies: copies.max(1),
            mode,
        }
    }

    pub fn total_sheets(&self) -> usize {
        self.sheets.saturating_mul(self.copies)
    }

    pub fn estimated_bytes(&self) -> u64 {
        (self.total_sheets() as u64).saturating_mul(self.mode.estimated_sheet_bytes())
    }
}

/// Calculates required spool bytes across a batch of printer jobs.
/// Applies a 1.25x (+25%) safety factor for `.SHD` shadow index files, EMF record headers,
/// and NTFS cluster alignment overhead.
pub fn calculate_required_spool_bytes(jobs: &[PrinterJobEstimate]) -> u64 {
    let raw_sum: u64 = jobs
        .iter()
        .map(|j| j.estimated_bytes())
        .fold(0u64, |acc, b| acc.saturating_add(b));

    // +25% safety overhead: raw_sum * 5 / 4
    raw_sum.saturating_mul(5) / 4
}

/// Disk space sufficiency verdict for print spooling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskVerdict {
    /// Ample disk space available.
    Sufficient,
    /// Disk space is tight; job can proceed but user should be cautioned.
    LowSpaceWarning {
        available_mb: u64,
        estimated_mb: u64,
    },
    /// Critical disk space shortage; dispatch must be halted.
    CriticalExhaustion {
        available_mb: u64,
        estimated_mb: u64,
    },
}

/// Spooler disk space validation summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpoolDiskStatus {
    pub spool_directory: PathBuf,
    pub available_bytes: u64,
    pub estimated_required_bytes: u64,
    pub verdict: DiskVerdict,
}

/// Evaluates disk space verdict given available and required bytes.
pub fn evaluate_disk_verdict(available_bytes: u64, estimated_required_bytes: u64) -> DiskVerdict {
    let available_mb = available_bytes / (1024 * 1024);
    let estimated_mb = estimated_required_bytes / (1024 * 1024);

    if available_bytes < estimated_required_bytes {
        DiskVerdict::CriticalExhaustion {
            available_mb,
            estimated_mb,
        }
    } else if available_bytes < estimated_required_bytes.saturating_add(100 * 1024 * 1024)
        || available_bytes < (estimated_required_bytes.saturating_mul(3) / 2)
    {
        DiskVerdict::LowSpaceWarning {
            available_mb,
            estimated_mb,
        }
    } else {
        DiskVerdict::Sufficient
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_counting_semaphore_limits_and_raii() {
        let governor = ConcurrencyGovernor::new(2, 1);
        let token = CancellationToken::new();

        assert_eq!(governor.available_cpu_permits(), 2);
        assert_eq!(governor.available_spool_permits(), 1);

        {
            let guard1 = governor.acquire_cpu_permit(&token).unwrap();
            assert_eq!(governor.available_cpu_permits(), 1);

            let guard2 = governor.acquire_cpu_permit(&token).unwrap();
            assert_eq!(governor.available_cpu_permits(), 0);

            drop(guard1);
            assert_eq!(governor.available_cpu_permits(), 1);
            drop(guard2);
            assert_eq!(governor.available_cpu_permits(), 2);
        }

        {
            let spool_guard = governor.acquire_spool_permit(&token).unwrap();
            assert_eq!(governor.available_spool_permits(), 0);
            drop(spool_guard);
            assert_eq!(governor.available_spool_permits(), 1);
        }
    }

    #[test]
    fn test_semaphore_cancellation_abort() {
        let governor = ConcurrencyGovernor::new(1, 1);
        let token = CancellationToken::new();

        // Exhaust the permit
        let _guard = governor.acquire_cpu_permit(&token).unwrap();
        assert_eq!(governor.available_cpu_permits(), 0);

        // Cancel token in another thread after short sleep
        let token_clone = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            token_clone.cancel();
        });

        // This acquire must abort with Cancelled
        let result = governor.acquire_cpu_permit(&token);
        assert_eq!(result.err(), Some(GovernorError::Cancelled));
    }

    #[test]
    fn test_panic_safety_releases_permit() {
        let governor = Arc::new(ConcurrencyGovernor::new(1, 1));
        let token = CancellationToken::new();

        let gov_clone = governor.clone();
        let handle = std::thread::spawn(move || {
            let _guard = gov_clone.acquire_cpu_permit(&CancellationToken::new()).unwrap();
            assert_eq!(gov_clone.available_cpu_permits(), 0);
            panic!("intentional worker panic to test unwind safety");
        });

        let _ = handle.join(); // Expect thread panic
        // Permit must have been restored by RAII Drop!
        assert_eq!(governor.available_cpu_permits(), 1);
        assert!(governor.acquire_cpu_permit(&token).is_ok());
    }

    #[test]
    fn test_spool_estimation_and_verdict() {
        let jobs = vec![
            PrinterJobEstimate::new(10, 2, SheetRasterMode::Vector), // 20 sheets * 350 KB = 7,000 KB = 7,168,000 B
            PrinterJobEstimate::new(2, 1, SheetRasterMode::Raster300Dpi), // 2 sheets * 18 MB = 36 MB = 37,748,736 B
        ];

        let required = calculate_required_spool_bytes(&jobs);
        // raw = 7,168,000 + 37,748,736 = 44,916,736. * 1.25 = 56,145,920 B (~53.5 MB)
        assert!(required > 50 * 1024 * 1024);
        assert!(required < 60 * 1024 * 1024);

        // Critical exhaustion: available < required
        let verdict = evaluate_disk_verdict(30 * 1024 * 1024, required);
        assert!(matches!(verdict, DiskVerdict::CriticalExhaustion { .. }));

        // Low space warning: available >= required, but tight headroom (< required + 100MB)
        let verdict = evaluate_disk_verdict(required + 10 * 1024 * 1024, required);
        assert!(matches!(verdict, DiskVerdict::LowSpaceWarning { .. }));

        // Sufficient: ample headroom (> required + 100MB and > 1.5x)
        let verdict = evaluate_disk_verdict(required + 500 * 1024 * 1024, required);
        assert_eq!(verdict, DiskVerdict::Sufficient);
    }
}
