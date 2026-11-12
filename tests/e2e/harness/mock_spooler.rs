use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Disk space evaluation verdict matching pdfcraft-platform disk module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskVerdict {
    Sufficient,
    LowSpaceWarning { available_mb: u64, estimated_mb: u64 },
    CriticalExhaustion { available_mb: u64, estimated_mb: u64 },
}

/// Spooler disk status report.
#[derive(Debug, Clone)]
pub struct SpoolDiskStatus {
    pub spool_directory: PathBuf,
    pub available_bytes: u64,
    pub estimated_required_bytes: u64,
    pub verdict: DiskVerdict,
}

/// Programmable disk space governor simulating GetDiskFreeSpaceExW.
pub struct MockDiskSpaceGovernor {
    simulated_available_bytes: AtomicU64,
    simulated_total_bytes: AtomicU64,
}

impl MockDiskSpaceGovernor {
    pub fn new(initial_free_bytes: u64) -> Self {
        Self {
            simulated_available_bytes: AtomicU64::new(initial_free_bytes),
            simulated_total_bytes: AtomicU64::new(1024 * 1024 * 1024 * 1024), // 1 TB default
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.simulated_total_bytes.load(Ordering::SeqCst)
    }

    pub fn set_available_bytes(&self, bytes: u64) {
        self.simulated_available_bytes.store(bytes, Ordering::SeqCst);
    }

    pub fn set_sufficient(&self) {
        self.set_available_bytes(50 * 1024 * 1024 * 1024); // 50 GB
    }

    pub fn set_low_space(&self, estimated_bytes: u64) {
        // Between 1.0x and 1.25x estimated bytes or < 100MB
        let low = (estimated_bytes as f64 * 1.1) as u64;
        self.set_available_bytes(low.max(20 * 1024 * 1024));
    }

    pub fn set_critical_exhaustion(&self, estimated_bytes: u64) {
        // Strictly less than estimated bytes
        let critical = (estimated_bytes / 2).min(5 * 1024 * 1024);
        self.set_available_bytes(critical);
    }

    /// Evaluates disk space verdict.
    pub fn evaluate(&self, spool_dir: &Path, estimated_bytes: u64) -> SpoolDiskStatus {
        let avail = self.simulated_available_bytes.load(Ordering::SeqCst);
        let avail_mb = avail / (1024 * 1024);
        let est_mb = estimated_bytes / (1024 * 1024);

        let verdict = if avail < estimated_bytes {
            DiskVerdict::CriticalExhaustion {
                available_mb: avail_mb,
                estimated_mb: est_mb,
            }
        } else if avail < (estimated_bytes as f64 * 1.25) as u64 || avail < 100 * 1024 * 1024 {
            DiskVerdict::LowSpaceWarning {
                available_mb: avail_mb,
                estimated_mb: est_mb,
            }
        } else {
            DiskVerdict::Sufficient
        };

        SpoolDiskStatus {
            spool_directory: spool_dir.to_path_buf(),
            available_bytes: avail,
            estimated_required_bytes: estimated_bytes,
            verdict,
        }
    }
}

/// Lifecycle status of an individual sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SheetStatus {
    Pending,
    InFlight { worker_id: usize, started_at: Instant },
    Committed { bytes_written: usize, committed_at: Instant },
    Failed { error: String, failed_at: Instant },
}

/// Thread-safe ledger tracking every sheet during multi-sheet spooling.
pub struct CommittedSheetsLedger {
    pub total_sheets: usize,
    sheets: Mutex<Vec<SheetStatus>>,
    current_in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    pub max_allowed_in_flight: usize, // Enforces backpressure (e.g. 2)
}

impl CommittedSheetsLedger {
    pub fn new(total_sheets: usize, max_allowed_in_flight: usize) -> Self {
        let initial = vec![SheetStatus::Pending; total_sheets];
        Self {
            total_sheets,
            sheets: Mutex::new(initial),
            current_in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
            max_allowed_in_flight,
        }
    }

    /// Marks sheet as in-flight; enforces bounded backpressure.
    pub fn begin_sheet(&self, sheet_idx: usize, worker_id: usize) -> Result<(), String> {
        if sheet_idx >= self.total_sheets {
            return Err(format!(
                "Sheet index {sheet_idx} out of bounds (total {})",
                self.total_sheets
            ));
        }

        let in_flight = self.current_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let mut peak = self.peak_in_flight.load(Ordering::SeqCst);
        while in_flight > peak {
            match self.peak_in_flight.compare_exchange_weak(
                peak,
                in_flight,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => peak = actual,
            }
        }

        if in_flight > self.max_allowed_in_flight {
            self.current_in_flight.fetch_sub(1, Ordering::SeqCst);
            return Err(format!(
                "Backpressure violation: {in_flight} sheets in-flight simultaneously (max allowed {})",
                self.max_allowed_in_flight
            ));
        }

        let mut lock = self.sheets.lock().unwrap();
        lock[sheet_idx] = SheetStatus::InFlight {
            worker_id,
            started_at: Instant::now(),
        };
        Ok(())
    }

    /// Confirms sheet committed to spooler disk/driver.
    pub fn commit_sheet(&self, sheet_idx: usize, bytes_written: usize) {
        let mut lock = self.sheets.lock().unwrap();
        lock[sheet_idx] = SheetStatus::Committed {
            bytes_written,
            committed_at: Instant::now(),
        };
        self.current_in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    /// Records failure for a sheet (e.g. paper jam, timeout).
    pub fn fail_sheet(&self, sheet_idx: usize, error: String) {
        let mut lock = self.sheets.lock().unwrap();
        lock[sheet_idx] = SheetStatus::Failed {
            error,
            failed_at: Instant::now(),
        };
        self.current_in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    /// Returns all sheet indices that were NOT successfully committed (for failover slicing).
    pub fn pending_sheets_for_failover(&self) -> Vec<usize> {
        let lock = self.sheets.lock().unwrap();
        lock.iter()
            .enumerate()
            .filter_map(|(idx, status)| match status {
                SheetStatus::Committed { .. } => None,
                _ => Some(idx),
            })
            .collect()
    }

    pub fn committed_count(&self) -> usize {
        let lock = self.sheets.lock().unwrap();
        lock.iter()
            .filter(|s| matches!(s, SheetStatus::Committed { .. }))
            .count()
    }

    pub fn is_all_committed(&self) -> bool {
        self.committed_count() == self.total_sheets
    }

    pub fn peak_in_flight_observed(&self) -> usize {
        self.peak_in_flight.load(Ordering::SeqCst)
    }

    pub fn current_in_flight(&self) -> usize {
        self.current_in_flight.load(Ordering::SeqCst)
    }

    pub fn status_of(&self, sheet_idx: usize) -> Option<SheetStatus> {
        let lock = self.sheets.lock().unwrap();
        lock.get(sheet_idx).cloned()
    }
}

/// The Virtual Spooler combining sandbox filesystem, disk governor, and ledger.
pub struct MockSpooler {
    temp_dir: tempfile::TempDir,
    pub disk_governor: MockDiskSpaceGovernor,
    pub fail_at_sheet: AtomicUsize, // usize::MAX means no failure
}

impl Default for MockSpooler {
    fn default() -> Self {
        Self::new()
    }
}

impl MockSpooler {
    pub fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("pdfcraft_mock_spool_")
            .tempdir()
            .expect("Failed to create temporary spool directory");

        let printers_dir = temp.path().join("PRINTERS");
        fs::create_dir_all(&printers_dir).expect("Failed to create PRINTERS spool folder");

        Self {
            temp_dir: temp,
            disk_governor: MockDiskSpaceGovernor::new(50 * 1024 * 1024 * 1024),
            fail_at_sheet: AtomicUsize::new(usize::MAX),
        }
    }

    pub fn spool_path(&self) -> PathBuf {
        self.temp_dir.path().join("PRINTERS")
    }

    /// Injects a failure at a specific sheet index.
    pub fn inject_failure_at_sheet(&self, sheet_idx: usize) {
        self.fail_at_sheet.store(sheet_idx, Ordering::SeqCst);
    }

    /// Simulates streaming page spooling with backpressure and failure injection.
    pub fn spool_sheet_stream<F>(
        &self,
        job_id: u32,
        ledger: &CommittedSheetsLedger,
        worker_id: usize,
        mut sheet_producer: F,
    ) -> Result<(), String>
    where
        F: FnMut(usize) -> Option<Vec<u8>>,
    {
        for sheet_idx in 0..ledger.total_sheets {
            ledger.begin_sheet(sheet_idx, worker_id)?;

            // Check if failure is injected at this sheet
            if sheet_idx == self.fail_at_sheet.load(Ordering::SeqCst) {
                let err_msg = format!("Paper Jam / I/O Fault at sheet {sheet_idx}");
                ledger.fail_sheet(sheet_idx, err_msg.clone());
                return Err(err_msg);
            }

            let sheet_data = sheet_producer(sheet_idx).ok_or_else(|| {
                let err = format!("Producer ended prematurely at sheet {sheet_idx}");
                ledger.fail_sheet(sheet_idx, err.clone());
                err
            })?;

            // Write simulated spool file (e.g. 00001_0000.SPL)
            let file_name = format!("{:05}_{:04}.SPL", job_id, sheet_idx);
            let target_file = self.spool_path().join(file_name);
            fs::write(&target_file, &sheet_data)
                .map_err(|e| format!("Failed to write spool file: {e}"))?;

            ledger.commit_sheet(sheet_idx, sheet_data.len());
        }

        Ok(())
    }
}
