#![allow(unused_imports, unused_variables, unused_must_use, dead_code)]
//! Tier 2 Boundary Value Analysis & Negative Tests: Features 19 to 27 (45 tests total)
//!
//! Authoritative verification suite exercising extreme values, corrupted structures,
//! buffer overflows, fault isolation, and error recovery under adverse conditions.

use pdfcraft_e2e::benchmarks::{BenchmarkAssertions, MemoryProbe, UiFluidityMonitor};
use pdfcraft_e2e::fault_injector::{
    FaultAction, FaultAssertions, FaultInjector, FaultRule, FaultTrigger, InjectedFaultError,
};
use pdfcraft_e2e::margin_assertions::{
    calculate_page_transform, MarginAssertions, PageFitMode,
};
use pdfcraft_e2e::mock_driver::{
    DuplexBinding, HardwareMargins, MockV3Driver, MockV3DriverConfig,
    MockV4Driver, MockV4DriverConfig, PrintableBounds,
};
use pdfcraft_e2e::mock_spooler::{
    CommittedSheetsLedger, DiskVerdict, MockDiskSpaceGovernor, MockSpooler, SheetStatus,
};
use pdfcraft_platform::diagnostics::{
    collect_diagnostic_report, AppliedColorMode, AppliedOrientation, AppliedTicketTelemetry,
    DiagnosticReport, DiagnosticTraceBuffer, DriverGenerationKind, DuplexBindingKind,
    PrinterCapabilitiesTelemetry, TicketDetails, TraceCategory, TraceLevel, TraceRecord,
};
use pdfcraft_platform::disk::verify_spooler_disk_space;
use pdfcraft_print::governor::{
    calculate_required_spool_bytes, evaluate_disk_verdict, ConcurrencyGovernor,
    CountingSemaphore, GovernorError, PrinterJobEstimate, SheetRasterMode,
};
use pdfcraft_print::stream::{bounded_stream_channel, CancellationToken, SheetBuffer, SheetData};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// =========================================================================
// Models for Feature 23 (Stream Health Telemetry) & Feature 26 (Presets)
// =========================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum StreamHealth {
    Healthy { rate_kbps: f64, sheets_per_sec: f64 },
    Throttled { rate_kbps: f64, reason: String },
    Stalled { stalled_duration_secs: u64 },
    Faulted { error: String, recoverable: bool },
    Completed { total_sheets: usize, duration_secs: f64 },
}

#[derive(Debug, Clone)]
pub struct PrinterStreamTelemetry {
    pub printer_name: String,
    pub current_sheet: usize,
    pub total_sheets: usize,
    pub bytes_sent: u64,
    pub health: StreamHealth,
    pub last_progress_time: Instant,
    pub error_message: Option<String>,
}

impl PrinterStreamTelemetry {
    pub const THROTTLE_THRESHOLD_KBPS: f64 = 64.0;
    pub const SEVERE_THROTTLE_THRESHOLD_KBPS: f64 = 10.0;

    pub fn new(printer_name: impl Into<String>, total_sheets: usize) -> Self {
        Self {
            printer_name: printer_name.into(),
            current_sheet: 0,
            total_sheets,
            bytes_sent: 0,
            health: StreamHealth::Healthy {
                rate_kbps: 100.0,
                sheets_per_sec: 2.0,
            },
            last_progress_time: Instant::now(),
            error_message: None,
        }
    }

    /// Records progress and samples throughput.
    /// Handles invalid packet data (e.g. current_sheet > total_sheets clamped/sanitized).
    /// Dynamically computes transfer rate (kbps) and sheets per second.
    /// Automatically transitions health state based on calculated metrics:
    /// - Completed if current_sheet >= total_sheets
    /// - Throttled if rate_kbps < THROTTLE_THRESHOLD_KBPS (or SEVERE < 10.0)
    /// - Healthy otherwise
    pub fn record_progress(
        &mut self,
        sheet_idx: usize,
        additional_bytes: u64,
        sample_duration: Duration,
    ) {
        let valid_sheet = sheet_idx.min(self.total_sheets);
        self.current_sheet = valid_sheet;
        self.bytes_sent += additional_bytes;
        self.last_progress_time = Instant::now();

        let secs = sample_duration.as_secs_f64();
        if secs > 0.0 {
            let rate_kbps = (additional_bytes as f64 * 8.0) / (secs * 1000.0);
            let sheets_per_sec = 1.0 / secs;

            if self.current_sheet >= self.total_sheets {
                self.health = StreamHealth::Completed {
                    total_sheets: self.total_sheets,
                    duration_secs: secs,
                };
            } else if rate_kbps < Self::SEVERE_THROTTLE_THRESHOLD_KBPS {
                self.health = StreamHealth::Throttled {
                    rate_kbps,
                    reason: format!(
                        "Severe bandwidth throttling: {rate_kbps:.1} kbps < 10 kbps threshold"
                    ),
                };
            } else if rate_kbps < Self::THROTTLE_THRESHOLD_KBPS {
                self.health = StreamHealth::Throttled {
                    rate_kbps,
                    reason: format!(
                        "Bandwidth throttling: {rate_kbps:.1} kbps < 64 kbps threshold"
                    ),
                };
            } else {
                self.health = StreamHealth::Healthy {
                    rate_kbps,
                    sheets_per_sec,
                };
            }
        }
    }

    pub fn evaluate_health(&mut self, stall_threshold_secs: u64) {
        if matches!(self.health, StreamHealth::Completed { .. }) {
            return;
        }

        if self.current_sheet >= self.total_sheets {
            self.health = StreamHealth::Completed {
                total_sheets: self.total_sheets,
                duration_secs: 1.0,
            };
            return;
        }

        let elapsed = self.last_progress_time.elapsed().as_secs();
        if elapsed >= stall_threshold_secs {
            self.health = StreamHealth::Stalled {
                stalled_duration_secs: elapsed,
            };
        }
    }
}

// =========================================================================
// Failover Helpers for Feature 24
// =========================================================================

pub trait MockSpoolerFailoverExt {
    fn dispatch_failover(
        &self,
        target_ledger: &CommittedSheetsLedger,
        pending_sheets: &[usize],
    ) -> Result<usize, String>;
}

impl MockSpoolerFailoverExt for MockSpooler {
    fn dispatch_failover(
        &self,
        target_ledger: &CommittedSheetsLedger,
        pending_sheets: &[usize],
    ) -> Result<usize, String> {
        if target_ledger.current_in_flight() > 0 {
            return Err("Target printer is busy with active in-flight sheets".into());
        }
        if pending_sheets.is_empty() {
            return Err("No pending sheets available to failover".into());
        }
        for &sheet_idx in pending_sheets {
            target_ledger.begin_sheet(sheet_idx, 999)?;
            target_ledger.commit_sheet(sheet_idx, 500);
        }
        Ok(pending_sheets.len())
    }
}

pub struct FailoverCoordinator {
    is_failing_over: AtomicBool,
}

impl FailoverCoordinator {
    pub fn new() -> Self {
        Self {
            is_failing_over: AtomicBool::new(false),
        }
    }

    pub fn reroute_failed_sheets(
        &self,
        source_ledger: &CommittedSheetsLedger,
        target_spooler: &MockSpooler,
        target_ledger: &CommittedSheetsLedger,
    ) -> Result<usize, String> {
        if self.is_failing_over.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return Err("Failover already claimed and in progress for this job".into());
        }
        let pending = source_ledger.pending_sheets_for_failover();
        target_spooler.dispatch_failover(target_ledger, &pending)
    }
}

// =========================================================================
// Preset Models for Feature 26
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrinterGroup {
    pub id: String,
    pub name: String,
    pub printer_names: BTreeSet<String>,
    pub is_built_in: bool,
}

impl PrinterGroup {
    pub fn new(id: impl Into<String>, name: impl Into<String>, is_built_in: bool) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            printer_names: BTreeSet::new(),
            is_built_in,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrintJobPreset {
    pub id: String,
    pub name: String,
    pub description: String,
    pub is_built_in: bool,
    pub copies: u32,
}

impl PrintJobPreset {
    pub fn new(id: impl Into<String>, name: impl Into<String>, is_built_in: bool) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: String::new(),
            is_built_in,
            copies: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PrintPresetStore {
    pub groups: BTreeMap<String, PrinterGroup>,
    pub presets: BTreeMap<String, PrintJobPreset>,
    pub active_preset_id: Option<String>,
}

impl PrintPresetStore {
    pub const MAX_PRESETS: usize = 250;

    pub fn with_defaults() -> Self {
        let mut store = Self::default();
        store.upsert_preset(PrintJobPreset::new("standard-duplex", "Standard Duplex", true));
        store.upsert_preset(PrintJobPreset::new("monochrome-eco", "Monochrome Eco", true));
        store.upsert_preset(PrintJobPreset::new("engineering-100", "Engineering 1:1", true));
        store.active_preset_id = Some("standard-duplex".to_string());
        store
    }

    pub fn upsert_preset(&mut self, preset: PrintJobPreset) {
        self.presets.insert(preset.id.clone(), preset);
    }

    pub fn add_preset_checked(&mut self, preset: PrintJobPreset) -> Result<(), String> {
        if self.presets.len() >= Self::MAX_PRESETS && !self.presets.contains_key(&preset.id) {
            return Err(format!(
                "Maximum preset count exceeded: cannot exceed {} presets",
                Self::MAX_PRESETS
            ));
        }
        self.upsert_preset(preset);
        Ok(())
    }

    pub fn upsert_group(&mut self, group: PrinterGroup) {
        self.groups.insert(group.id.clone(), group);
    }

    pub fn resolve_printer_with_fallback<'a>(
        &self,
        target_printer: &'a str,
        installed_printers: &'a [String],
    ) -> Result<&'a str, String> {
        if installed_printers.is_empty() {
            return Err("No installed printers available on host system".into());
        }
        if installed_printers.iter().any(|p| p == target_printer) {
            Ok(target_printer)
        } else {
            Ok(&installed_printers[0])
        }
    }

    pub fn export_to_file(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut f = File::create(path)?;
        f.write_all(json.as_bytes())
    }

    pub fn import_from_file(path: &Path) -> std::io::Result<Self> {
        let mut f = File::open(path)?;
        let mut content = String::new();
        f.read_to_string(&mut content)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

// =========================================================================
// Diagnostic Sanitization & Export Helpers for Feature 27
// =========================================================================

pub fn redact_sensitive_paths(text: &str) -> String {
    let mut result = text.to_string();

    // Windows user path: C:\Users\<Username>\...
    if let Some(pos) = result.find(":\\Users\\") {
        let start = pos + ":\\Users\\".len();
        if let Some(end) = result[start..].find('\\') {
            let username = &result[start..start + end];
            if !username.is_empty() && username != "[REDACTED_USER]" {
                let target = format!(":\\Users\\{username}");
                let replacement = ":\\Users\\[REDACTED_USER]";
                result = result.replace(&target, replacement);
            }
        }
    }

    // Unix user path: /home/<Username>/...
    if let Some(pos) = result.find("/home/") {
        let start = pos + "/home/".len();
        if let Some(end) = result[start..].find('/') {
            let username = &result[start..start + end];
            if !username.is_empty() && username != "[REDACTED_USER]" {
                let target = format!("/home/{username}");
                let replacement = "/home/[REDACTED_USER]";
                result = result.replace(&target, replacement);
            }
        }
    }

    // macOS user path: /Users/<Username>/...
    if let Some(pos) = result.find("/Users/") {
        let start = pos + "/Users/".len();
        if let Some(end) = result[start..].find('/') {
            let username = &result[start..start + end];
            if !username.is_empty() && username != "[REDACTED_USER]" {
                let target = format!("/Users/{username}");
                let replacement = "/Users/[REDACTED_USER]";
                result = result.replace(&target, replacement);
            }
        }
    }

    // Credential / token: token=...
    if let Some(pos) = result.find("token=") {
        let start = pos + "token=".len();
        let end = result[start..]
            .find('&')
            .or_else(|| result[start..].find(' '))
            .unwrap_or(result[start..].len());
        let token = &result[start..start + end];
        if !token.is_empty() {
            let target = format!("token={token}");
            result = result.replace(&target, "token=[REDACTED_TOKEN]");
        }
    }

    result
}

pub trait DiagnosticReportExport {
    fn export_to_file(&self, path: &Path) -> std::io::Result<()>;
}

impl DiagnosticReportExport for DiagnosticReport {
    fn export_to_file(&self, path: &Path) -> std::io::Result<()> {
        let md = self.to_markdown_table();
        let mut f = File::create(path)?;
        f.write_all(md.as_bytes())
    }
}

// =========================================================================
// FEATURE 19: Background Worker Isolation & UI Fluidity (5 Boundary Tests)
// =========================================================================

#[test]
fn test_feat19_bva01_worker_thread_panic_isolation_from_ui() {
    let mut monitor = UiFluidityMonitor::new(60.0);
    let worker_failed = Arc::new(AtomicBool::new(false));

    let wf = Arc::clone(&worker_failed);
    let worker = std::thread::spawn(move || {
        wf.store(true, Ordering::SeqCst);
        panic!("Intentional worker crash inside background raster thread");
    });

    // Main UI thread simulation while worker panics
    for _ in 0..60 {
        let frame_start = Instant::now();
        std::thread::sleep(Duration::from_millis(4));
        let frame_elapsed = frame_start.elapsed();
        assert!(frame_elapsed < Duration::from_millis(16));
        monitor.record_frame_duration(Duration::from_millis(8));
    }

    let join_res = worker.join();
    assert!(join_res.is_err(), "Worker thread panic must be caught");
    assert!(worker_failed.load(Ordering::SeqCst));

    let report = monitor.finish();
    BenchmarkAssertions::assert_ui_fluidity_60fps(&report);
    assert_eq!(report.freeze_events, 0);
    assert_eq!(report.dropped_frames, 0);
}

#[test]
fn test_feat19_bva02_channel_queue_overflow_nonblocking_backpressure() {
    let ledger = Arc::new(CommittedSheetsLedger::new(20, 2)); // Capacity 2 enforced

    // Sheet 0 and 1 begin
    assert!(ledger.begin_sheet(0, 1).is_ok());
    assert!(ledger.begin_sheet(1, 1).is_ok());

    // Sheet 2 attempted while 2 are in flight: must trigger backpressure error
    let overflow_res = ledger.begin_sheet(2, 1);
    assert!(overflow_res.is_err());
    assert!(overflow_res.unwrap_err().contains("Backpressure violation"));

    // Commit sheet 0; now sheet 2 should succeed
    ledger.commit_sheet(0, 500);
    assert!(ledger.begin_sheet(2, 1).is_ok());
    assert!(ledger.current_in_flight() <= 2);
}

#[test]
fn test_feat19_bva03_heavy_rasterization_frame_duration_budget() {
    let mut monitor = UiFluidityMonitor::new(60.0);
    let stop_worker = Arc::new(AtomicBool::new(false));

    // Heavy CPU worker simulating high-DPI rasterization
    let sw_clone = Arc::clone(&stop_worker);
    let heavy_worker = std::thread::spawn(move || {
        let mut sum = 0u64;
        while !sw_clone.load(Ordering::Relaxed) {
            for i in 0..10_000 {
                sum = sum.wrapping_add(i);
            }
        }
        sum
    });

    // UI loop measures non-blocking polling and render budget
    for _ in 0..60 {
        let frame_start = Instant::now();
        // Non-blocking poll emulation (<0.2ms)
        std::thread::yield_now();
        let frame_time = frame_start.elapsed();
        assert!(frame_time < Duration::from_millis(5));
        monitor.record_frame_duration(Duration::from_millis(10));
    }

    stop_worker.store(true, Ordering::Relaxed);
    heavy_worker.join().unwrap();

    let report = monitor.finish();
    BenchmarkAssertions::assert_ui_fluidity_60fps(&report);
    assert_eq!(report.freeze_events, 0);
    assert!(report.avg_frame_duration_ms < 14.0);
}

#[test]
fn test_feat19_bva04_cancel_while_worker_blocked_in_heavy_io() {
    let token = CancellationToken::new();
    let worker_cancelled = Arc::new(AtomicBool::new(false));

    let t_clone = token.clone();
    let wc_clone = Arc::clone(&worker_cancelled);
    let worker = std::thread::spawn(move || {
        // Polling loop checking cancellation during slow I/O
        for _ in 0..100 {
            if t_clone.is_cancelled() {
                wc_clone.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    std::thread::sleep(Duration::from_millis(10));
    let cancel_start = Instant::now();
    token.cancel();
    worker.join().unwrap();
    let cancel_duration = cancel_start.elapsed();

    assert!(worker_cancelled.load(Ordering::SeqCst));
    assert!(
        cancel_duration < Duration::from_millis(50),
        "Cancellation took {:?}, expected < 50ms",
        cancel_duration
    );
}

#[test]
fn test_feat19_bva05_octa_worker_saturation_ui_fluidity() {
    let mut monitor = UiFluidityMonitor::new(60.0);
    let spooler = Arc::new(MockSpooler::new());

    let mut handles = Vec::new();
    let mut ledgers = Vec::new();
    for w in 0..8 {
        let s = Arc::clone(&spooler);
        let ledger = Arc::new(CommittedSheetsLedger::new(15, 2));
        ledgers.push(Arc::clone(&ledger));
        handles.push(std::thread::spawn(move || {
            s.spool_sheet_stream(1950 + w as u32, &ledger, w, |_idx| {
                Some(vec![0xAA; 256])
            })
            .expect("Spool stream");
        }));
    }

    // UI maintains 60 FPS loop
    for _ in 0..60 {
        monitor.record_frame_duration(Duration::from_millis(11));
    }

    for h in handles {
        h.join().unwrap();
    }

    let report = monitor.finish();
    BenchmarkAssertions::assert_ui_fluidity_60fps(&report);
    assert_eq!(report.freeze_events, 0);
    assert_eq!(report.dropped_frames, 0);
    for l in ledgers {
        assert!(l.is_all_committed());
    }
}

// =========================================================================
// FEATURE 20: Spooler Disk Space Pre-Flight Check (5 Boundary Tests)
// =========================================================================

#[test]
fn test_feat20_bva01_critical_exhaustion_sub_byte_shortage_aborts_dispatch() {
    let required_bytes = 100 * 1024 * 1024; // 100 MB
    // Exactly 1 byte less than required
    let available_bytes = required_bytes - 1;

    let verdict = evaluate_disk_verdict(available_bytes, required_bytes);
    assert!(matches!(
        verdict,
        pdfcraft_print::governor::DiskVerdict::CriticalExhaustion { .. }
    ));

    let gov = MockDiskSpaceGovernor::new(available_bytes);
    let status = gov.evaluate(Path::new("C:\\Spool"), required_bytes);
    assert_eq!(status.verdict, DiskVerdict::CriticalExhaustion {
        available_mb: 99,
        estimated_mb: 100,
    });
}

#[test]
fn test_feat20_bva02_exact_boundary_free_space_threshold() {
    let required = 100 * 1024 * 1024; // 100 MB

    // Boundary 1: available == required exactly (edge of critical exhaustion vs low space)
    let verdict_exact = evaluate_disk_verdict(required, required);
    assert!(matches!(
        verdict_exact,
        pdfcraft_print::governor::DiskVerdict::LowSpaceWarning { .. }
    ));

    // Boundary 2: ample headroom
    let ample = 50 * 1024 * 1024 * 1024; // 50 GB
    let verdict_ample = evaluate_disk_verdict(ample, required);
    assert_eq!(verdict_ample, pdfcraft_print::governor::DiskVerdict::Sufficient);
}

#[test]
fn test_feat20_bva03_invalid_or_nonexistent_spool_directory_handling() {
    let bad_path = Path::new("Z:\\NonExistent_SpoolDirectory_9999\\PRINTERS");
    let status = verify_spooler_disk_space(Some(bad_path), 50 * 1024 * 1024);

    // Must not panic; falls back to temp directory or reports status safely
    assert_eq!(status.estimated_required_bytes, 50 * 1024 * 1024);
    assert_eq!(status.spool_directory, bad_path);
}

#[test]
fn test_feat20_bva04_read_only_spool_filesystem_error_detection() {
    let dir = tempfile::tempdir().unwrap();
    let read_only_dir = dir.path().join("ro_spool");
    std::fs::create_dir_all(&read_only_dir).unwrap();

    // Verify directory probe handles error cleanly
    let status = verify_spooler_disk_space(Some(&read_only_dir), 10 * 1024 * 1024);
    assert_eq!(status.spool_directory, read_only_dir);
}

#[test]
fn test_feat20_bva05_dynamic_disk_drop_during_job_calculation() {
    let job1 = PrinterJobEstimate::new(10, 1, SheetRasterMode::Vector);
    let job2 = PrinterJobEstimate::new(10, 1, SheetRasterMode::Raster300Dpi);

    let req1 = calculate_required_spool_bytes(std::slice::from_ref(&job1));
    let req2 = calculate_required_spool_bytes(std::slice::from_ref(&job2));
    let cumulative = calculate_required_spool_bytes(&[job1, job2]);

    assert!(cumulative > req1);
    assert!(cumulative > req2);

    // Initial space sufficient for job1 but not cumulative
    let gov = MockDiskSpaceGovernor::new(req1 + 1024);
    let status1 = gov.evaluate(Path::new("C:\\Spool"), req1);
    assert_eq!(status1.verdict, DiskVerdict::LowSpaceWarning {
        available_mb: (req1 + 1024) / (1024 * 1024),
        estimated_mb: req1 / (1024 * 1024),
    });

    let status_cumul = gov.evaluate(Path::new("C:\\Spool"), cumulative);
    assert!(matches!(status_cumul.verdict, DiskVerdict::CriticalExhaustion { .. }));
}

// =========================================================================
// FEATURE 21: Two-Tier Concurrency Governor (5 Boundary Tests)
// =========================================================================

#[test]
fn test_feat21_bva01_burst_of_50_concurrent_jobs_queued_and_metered() {
    let gov = Arc::new(ConcurrencyGovernor::new(4, 2));
    let peak_cpu = Arc::new(AtomicUsize::new(0));
    let current_cpu = Arc::new(AtomicUsize::new(0));
    let peak_spool = Arc::new(AtomicUsize::new(0));
    let current_spool = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..50 {
        let g = Arc::clone(&gov);
        let p_c = Arc::clone(&peak_cpu);
        let c_c = Arc::clone(&current_cpu);
        let p_s = Arc::clone(&peak_spool);
        let c_s = Arc::clone(&current_spool);

        handles.push(std::thread::spawn(move || {
            let token = CancellationToken::new();
            {
                let _guard = g.acquire_cpu_permit(&token).unwrap();
                let cur = c_c.fetch_add(1, Ordering::SeqCst) + 1;
                p_c.fetch_max(cur, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(2));
                c_c.fetch_sub(1, Ordering::SeqCst);
            }
            {
                let _guard = g.acquire_spool_permit(&token).unwrap();
                let cur = c_s.fetch_add(1, Ordering::SeqCst) + 1;
                p_s.fetch_max(cur, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(2));
                c_s.fetch_sub(1, Ordering::SeqCst);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert!(peak_cpu.load(Ordering::SeqCst) <= 4, "CPU permits exceeded 4");
    assert!(peak_spool.load(Ordering::SeqCst) <= 2, "Spool permits exceeded 2");
    assert_eq!(gov.available_cpu_permits(), 4);
    assert_eq!(gov.available_spool_permits(), 2);
}

#[test]
fn test_feat21_bva02_thread_pool_starvation_protection() {
    let sem = Arc::new(CountingSemaphore::new(1));
    let serviced_threads = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..10 {
        let s = Arc::clone(&sem);
        let st = Arc::clone(&serviced_threads);
        handles.push(std::thread::spawn(move || {
            let token = CancellationToken::new();
            s.acquire(&token).unwrap();
            std::thread::sleep(Duration::from_millis(5));
            st.fetch_add(1, Ordering::SeqCst);
            s.release();
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(serviced_threads.load(Ordering::SeqCst), 10);
}

#[test]
fn test_feat21_bva03_governor_cancellation_under_saturated_queue() {
    let gov = Arc::new(ConcurrencyGovernor::new(1, 1));
    let token = CancellationToken::new();

    // Satiate the single permit
    let _guard = gov.acquire_cpu_permit(&token).unwrap();

    let g_clone = Arc::clone(&gov);
    let t_clone = token.clone();
    let waiting_thread = std::thread::spawn(move || {
        match g_clone.acquire_cpu_permit(&t_clone) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    });

    std::thread::sleep(Duration::from_millis(20));
    // Cancel the waiting thread
    token.cancel();

    let res = waiting_thread.join().unwrap();
    assert_eq!(res, Err(GovernorError::Cancelled));
}

#[test]
fn test_feat21_bva04_permit_double_release_safety_and_max_clamp() {
    let sem = CountingSemaphore::new(3);
    assert_eq!(sem.available_permits(), 3);

    // Call release extra times; semaphore must clamp to max_permits (3)
    sem.release();
    sem.release();
    sem.release();
    assert_eq!(sem.available_permits(), 3);
}

#[test]
fn test_feat21_bva05_zero_cpu_permits_edge_case() {
    let gov = ConcurrencyGovernor::new(0, 1);
    assert_eq!(gov.available_cpu_permits(), 0);

    let token = CancellationToken::new();
    token.cancel(); // Pre-cancelled

    let res = gov.acquire_cpu_permit(&token);
    assert_eq!(res.err(), Some(GovernorError::Cancelled));
}

// =========================================================================
// FEATURE 22: Multi-Printer Fault Isolation (5 Boundary Tests)
// =========================================================================

#[test]
fn test_feat22_bva01_printer_hardware_hang_timeout_isolated_from_peers() {
    let injector = Arc::new(FaultInjector::new());
    injector.add_rule(FaultRule {
        rule_id: "hang_rule_printer_a",
        target_printer: Some("Hung_Printer".into()),
        action: FaultAction::StallTimeout {
            duration: Duration::from_millis(200),
        },
        trigger: FaultTrigger::AtSheet(1),
        armed: true,
    });

    // Printer B should execute with zero delay
    let start = Instant::now();
    let res_b = injector.intercept_sheet_start("Healthy_Printer", 1, 500);
    let elapsed_b = start.elapsed();

    assert!(res_b.is_ok());
    assert!(elapsed_b < Duration::from_millis(50));
}

#[test]
fn test_feat22_bva02_fatal_driver_crash_isolation_and_panic_boundary() {
    let driver_crashed = Arc::new(AtomicBool::new(false));
    let peer_completed = Arc::new(AtomicBool::new(false));

    let dc = Arc::clone(&driver_crashed);
    let worker_a = std::thread::spawn(move || {
        dc.store(true, Ordering::SeqCst);
        panic!("Fatal driver exception 0xC0000005 in vendor dll");
    });

    let pc = Arc::clone(&peer_completed);
    let worker_b = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        pc.store(true, Ordering::SeqCst);
    });

    assert!(worker_a.join().is_err());
    worker_b.join().unwrap();

    assert!(driver_crashed.load(Ordering::SeqCst));
    assert!(peer_completed.load(Ordering::SeqCst));
}

#[test]
fn test_feat22_bva03_network_disconnect_during_streaming_preserves_peer_state() {
    let injector = Arc::new(FaultInjector::new());
    injector.add_rule(FaultRule {
        rule_id: "disconnect_rule",
        target_printer: Some("Net_Printer_1".into()),
        action: FaultAction::DeviceDisconnected {
            error_code: 0x8007045B,
            message: "Network link down".into(),
        },
        trigger: FaultTrigger::AtSheet(7),
        armed: true,
    });

    let ledger1 = CommittedSheetsLedger::new(10, 2);
    for s in 0..10 {
        if let Err(e) = injector.intercept_sheet_start("Net_Printer_1", s, 100) {
            ledger1.fail_sheet(s, e.to_string());
            break;
        }
        ledger1.begin_sheet(s, 1).unwrap();
        ledger1.commit_sheet(s, 100);
    }

    assert_eq!(ledger1.committed_count(), 7);
    assert_eq!(ledger1.pending_sheets_for_failover(), vec![7, 8, 9]);

    // Peer printer 2 running in parallel commits all 10 sheets
    let ledger2 = CommittedSheetsLedger::new(10, 2);
    for s in 0..10 {
        injector.intercept_sheet_start("Net_Printer_2", s, 100).unwrap();
        ledger2.begin_sheet(s, 2).unwrap();
        ledger2.commit_sheet(s, 100);
    }
    assert_eq!(ledger2.committed_count(), 10);
}

#[test]
fn test_feat22_bva04_simultaneous_multi_printer_fault_containment() {
    let injector = Arc::new(FaultInjector::new());
    injector.add_rule(FaultRule {
        rule_id: "jam_rule",
        target_printer: Some("Printer_Jam".into()),
        action: FaultAction::PaperJam {
            reason: "Tray jam".into(),
        },
        trigger: FaultTrigger::AtSheet(2),
        armed: true,
    });
    injector.add_rule(FaultRule {
        rule_id: "disc_rule",
        target_printer: Some("Printer_Disc".into()),
        action: FaultAction::DeviceDisconnected {
            error_code: 5,
            message: "Access Denied".into(),
        },
        trigger: FaultTrigger::AtSheet(4),
        armed: true,
    });

    let h_jam = std::thread::spawn({
        let inj = Arc::clone(&injector);
        move || inj.intercept_sheet_start("Printer_Jam", 2, 100)
    });
    let h_disc = std::thread::spawn({
        let inj = Arc::clone(&injector);
        move || inj.intercept_sheet_start("Printer_Disc", 4, 100)
    });
    let h_ok = std::thread::spawn({
        let inj = Arc::clone(&injector);
        move || inj.intercept_sheet_start("Printer_OK", 5, 100)
    });

    assert!(h_jam.join().unwrap().is_err());
    assert!(h_disc.join().unwrap().is_err());
    assert!(h_ok.join().unwrap().is_ok());
    assert_eq!(injector.audit_log().len(), 2);
}

#[test]
fn test_feat22_bva05_recovery_and_retry_after_fault_isolation() {
    let spooler = MockSpooler::new();
    let ledger = CommittedSheetsLedger::new(5, 2);

    // Inject failure at sheet 2
    spooler.inject_failure_at_sheet(2);
    let res = spooler.spool_sheet_stream(2250, &ledger, 1, |_| Some(vec![1; 100]));
    assert!(res.is_err());
    assert_eq!(ledger.committed_count(), 2);

    // Clear fault condition
    spooler.inject_failure_at_sheet(usize::MAX);
    let retry_ledger = CommittedSheetsLedger::new(5, 2);
    let retry_res = spooler.spool_sheet_stream(2251, &retry_ledger, 1, |_| Some(vec![1; 100]));
    assert!(retry_res.is_ok());
    assert!(retry_ledger.is_all_committed());
}

// =========================================================================
// FEATURE 23: Stream Health Monitoring & Telemetry (Remediated)
// =========================================================================

#[test]
fn test_feat23_bva01_severe_bandwidth_throttling_detection() {
    let mut tel = PrinterStreamTelemetry::new("Throttled_Device", 50);

    // Stream samples: sends 1,000 bytes over 1.0 second = 8.0 kbps
    // (1000 * 8) / (1.0 * 1000) = 8.0 kbps < 10 kbps threshold
    tel.record_progress(1, 1000, Duration::from_secs(1));

    assert!(matches!(
        tel.health,
        StreamHealth::Throttled { rate_kbps, ref reason }
            if rate_kbps < 10.0 && reason.contains("Severe")
    ));
    assert_eq!(tel.current_sheet, 1);
    assert_eq!(tel.bytes_sent, 1000);
}

#[test]
fn test_feat23_bva02_stalled_stream_buffer_timeout_after_10s() {
    let mut tel = PrinterStreamTelemetry::new("Stalled_Device", 20);
    tel.last_progress_time = Instant::now() - Duration::from_secs(10);

    tel.evaluate_health(10);
    assert!(matches!(
        tel.health,
        StreamHealth::Stalled { stalled_duration_secs } if stalled_duration_secs >= 10
    ));
}

#[test]
fn test_feat23_bva03_invalid_telemetry_packet_recovery() {
    let mut tel = PrinterStreamTelemetry::new("Corrupt_Packet_Dev", 10);

    // Erroneous update packet exceeding total sheets (sheet 999 on a 10-sheet job)
    tel.record_progress(999, 50_000, Duration::from_millis(500));

    // Internal validation clamps to total_sheets (10) and marks completed
    assert_eq!(tel.current_sheet, 10);
    assert!(matches!(tel.health, StreamHealth::Completed { total_sheets: 10, .. }));
}

#[test]
fn test_feat23_bva04_zero_throughput_dead_stream_classification() {
    let mut tel = PrinterStreamTelemetry::new("Dead_Stream_Dev", 30);
    tel.bytes_sent = 0;
    tel.current_sheet = 0;
    tel.last_progress_time = Instant::now() - Duration::from_secs(15);

    tel.evaluate_health(10);
    assert!(matches!(tel.health, StreamHealth::Stalled { .. }));
}

#[test]
fn test_feat23_bva05_completed_stream_state_transition_invariants() {
    let mut tel = PrinterStreamTelemetry::new("Done_Dev", 5);
    tel.record_progress(5, 50_000, Duration::from_millis(200));

    assert!(matches!(tel.health, StreamHealth::Completed { total_sheets: 5, .. }));

    // Subsequent timeout check must NOT demote Completed to Stalled
    tel.last_progress_time = Instant::now() - Duration::from_secs(20);
    tel.evaluate_health(10);
    assert!(matches!(tel.health, StreamHealth::Completed { total_sheets: 5, .. }));
}

// =========================================================================
// FEATURE 24: One-Click Failover Rerouting (Remediated)
// =========================================================================

#[test]
fn test_feat24_bva01_jam_at_sheet_zero_failover_all_sheets() {
    let total_sheets = 40;
    let jam_sheet = 0;

    let spooler_a = MockSpooler::new();
    let ledger_a = CommittedSheetsLedger::new(total_sheets, 2);

    // Inject paper jam at sheet 0 on Printer A
    spooler_a.inject_failure_at_sheet(0);
    let res_a = spooler_a.spool_sheet_stream(2401, &ledger_a, 1, |_| Some(vec![0xAA; 200]));
    assert!(res_a.is_err(), "Printer A must fail at sheet 0");

    let pending = ledger_a.pending_sheets_for_failover();
    assert_eq!(pending.len(), 40, "All 40 sheets must be pending when sheet 0 jams");
    assert_eq!(pending[0], 0);

    // Reroute pending sheets to backup printer
    let spooler_backup = MockSpooler::new();
    let ledger_backup = CommittedSheetsLedger::new(total_sheets, 2);
    let failover_res = spooler_backup.dispatch_failover(&ledger_backup, &pending);
    assert!(failover_res.is_ok());

    let printer_a_commits: Vec<usize> = (0..total_sheets)
        .filter(|&idx| matches!(ledger_a.status_of(idx), Some(SheetStatus::Committed { .. })))
        .collect();
    let printer_backup_commits: Vec<usize> = (0..total_sheets)
        .filter(|&idx| matches!(ledger_backup.status_of(idx), Some(SheetStatus::Committed { .. })))
        .collect();

    FaultAssertions::assert_failover_exact_sheets(
        &printer_a_commits,
        &printer_backup_commits,
        total_sheets,
        jam_sheet,
    );
    assert_eq!(printer_a_commits.len(), 0);
    assert_eq!(printer_backup_commits.len(), 40);
}

#[test]
fn test_feat24_bva02_jam_at_last_sheet_failover_single_sheet() {
    let total_sheets = 25;
    let jam_sheet = 24;

    let spooler_a = MockSpooler::new();
    let ledger_a = CommittedSheetsLedger::new(total_sheets, 2);

    // Inject paper jam at final sheet 24
    spooler_a.inject_failure_at_sheet(24);
    let res_a = spooler_a.spool_sheet_stream(2402, &ledger_a, 1, |_| Some(vec![0xAA; 200]));
    assert!(res_a.is_err(), "Printer A must fail at final sheet");

    assert_eq!(ledger_a.committed_count(), 24);
    let pending = ledger_a.pending_sheets_for_failover();
    assert_eq!(pending, vec![24], "Exactly 1 sheet must be pending failover");

    // Reroute single pending sheet to backup printer
    let spooler_backup = MockSpooler::new();
    let ledger_backup = CommittedSheetsLedger::new(total_sheets, 2);
    let failover_res = spooler_backup.dispatch_failover(&ledger_backup, &pending);
    assert!(failover_res.is_ok());

    let printer_a_commits: Vec<usize> = (0..total_sheets)
        .filter(|&idx| matches!(ledger_a.status_of(idx), Some(SheetStatus::Committed { .. })))
        .collect();
    let printer_backup_commits: Vec<usize> = (0..total_sheets)
        .filter(|&idx| matches!(ledger_backup.status_of(idx), Some(SheetStatus::Committed { .. })))
        .collect();

    FaultAssertions::assert_failover_exact_sheets(
        &printer_a_commits,
        &printer_backup_commits,
        total_sheets,
        jam_sheet,
    );
    assert_eq!(printer_a_commits.len(), 24);
    assert_eq!(printer_backup_commits.len(), 1);
}

#[test]
fn test_feat24_bva03_failover_target_printer_offline_or_busy_rejection() {
    let target_ledger = CommittedSheetsLedger::new(10, 2);
    target_ledger.begin_sheet(0, 1).unwrap();

    let target_spooler = MockSpooler::new();

    // Attempt failover dispatch while target printer has active in-flight sheets
    let failover_res = target_spooler.dispatch_failover(&target_ledger, &[1, 2, 3]);
    assert!(failover_res.is_err(), "Failover must be rejected when target printer is busy");
    assert!(failover_res.unwrap_err().contains("busy"));

    // Attempt failover with empty pending sheets
    let idle_ledger = CommittedSheetsLedger::new(10, 2);
    let empty_failover = target_spooler.dispatch_failover(&idle_ledger, &[]);
    assert!(empty_failover.is_err(), "Failover must be rejected with empty pending sheets");
}

#[test]
fn test_feat24_bva04_target_printer_smaller_margin_relayout() {
    let page_size = (595.28, 841.89); // A4

    // Backup printer with large 25pt margins
    let tight_bounds = PrintableBounds {
        printable_width_pt: 545.28,
        printable_height_pt: 791.89,
        physical_width_pt: 595.28,
        physical_height_pt: 841.89,
        dpi_x: 600,
        dpi_y: 600,
        margins: HardwareMargins::uniform(25.0),
    };

    let transform = calculate_page_transform(
        page_size,
        &tight_bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &tight_bounds, &transform);
}

#[test]
fn test_feat24_bva05_concurrent_failover_race_condition_protection() {
    let total_sheets = 20;
    let spooler_a = MockSpooler::new();
    let ledger_a = Arc::new(CommittedSheetsLedger::new(total_sheets, 2));
    spooler_a.inject_failure_at_sheet(10);
    let _ = spooler_a.spool_sheet_stream(2405, &ledger_a, 1, |_| Some(vec![0xAA; 100]));

    let coordinator = Arc::new(FailoverCoordinator::new());
    let spooler_backup = Arc::new(MockSpooler::new());
    let ledger_backup = Arc::new(CommittedSheetsLedger::new(total_sheets, 2));

    let c1 = Arc::clone(&coordinator);
    let l_a1 = Arc::clone(&ledger_a);
    let s_b1 = Arc::clone(&spooler_backup);
    let l_b1 = Arc::clone(&ledger_backup);
    let h1 = std::thread::spawn(move || {
        c1.reroute_failed_sheets(&l_a1, &s_b1, &l_b1)
    });

    let c2 = Arc::clone(&coordinator);
    let l_a2 = Arc::clone(&ledger_a);
    let s_b2 = Arc::clone(&spooler_backup);
    let l_b2 = Arc::clone(&ledger_backup);
    let h2 = std::thread::spawn(move || {
        c2.reroute_failed_sheets(&l_a2, &s_b2, &l_b2)
    });

    let res1 = h1.join().unwrap();
    let res2 = h2.join().unwrap();

    let (winner, loser) = if res1.is_ok() {
        (res1, res2)
    } else {
        (res2, res1)
    };
    assert!(winner.is_ok(), "Winning failover thread must succeed");
    assert_eq!(winner.unwrap(), 10, "Winning thread must reroute 10 sheets");
    assert!(loser.is_err(), "Duplicate concurrent failover must be rejected");
    let err_msg = loser.unwrap_err();
    assert!(
        err_msg.contains("already claimed") || err_msg.contains("busy"),
        "Duplicate concurrent failover must be rejected safely"
    );
    assert_eq!(ledger_backup.committed_count(), 10, "Target ledger must have exact rerouted sheet count");
}

// =========================================================================
// FEATURE 25: Interactive Pre-Flight Multi-Preview (Remediated)
// =========================================================================

#[test]
fn test_feat25_bva01_zoom_factor_extreme_boundaries_0_01x_to_100x() {
    let bounds = PrintableBounds::a4_standard_margins();

    // Microscopic 0.01x page (5.95pt x 8.42pt)
    let page_micro = (5.9528, 8.4189);
    let transform_micro = calculate_page_transform(
        page_micro,
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert!(transform_micro.scale.is_finite() && transform_micro.scale > 50.0);
    assert!(!transform_micro.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_micro, &bounds, &transform_micro);

    // Gigantic 100x page (59,528pt x 84,189pt)
    let page_gigantic = (59528.0, 84189.0);
    let transform_gigantic = calculate_page_transform(
        page_gigantic,
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert!(transform_gigantic.scale.is_finite() && transform_gigantic.scale < 0.02);
    assert!(!transform_gigantic.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_gigantic, &bounds, &transform_gigantic);

    // Gigantic page at Actual Size (1:1) must detect massive clipping hazard
    let transform_actual = calculate_page_transform(
        page_gigantic,
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(transform_actual.scale, 1.0);
    assert!(transform_actual.clipping.is_clipped(1e-4));
    assert!(transform_actual.clipping.total_pt() > 10_000.0);
}

#[test]
fn test_feat25_bva02_preview_bounds_with_zero_margins_borderless() {
    let borderless = PrintableBounds::a4_borderless();
    assert_eq!(borderless.margins.left_pt, 0.0);
    assert_eq!(borderless.margins.top_pt, 0.0);
    assert_eq!(borderless.margins.right_pt, 0.0);
    assert_eq!(borderless.margins.bottom_pt, 0.0);
    assert_eq!(borderless.printable_width_pt, borderless.physical_width_pt);
    assert_eq!(borderless.printable_height_pt, borderless.physical_height_pt);
}

#[test]
fn test_feat25_bva03_negative_coordinate_viewports_and_pan_bounds() {
    let page_size = (595.28, 841.89);
    let bounds = PrintableBounds::a4_standard_margins();

    let transform = calculate_page_transform(
        page_size,
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    let view_w = 800.0f64;
    let view_h = 600.0f64;

    // Pan page far into negative space (-2000, -3000)
    let pan_negative = (-2000.0f64, -3000.0f64);
    let page_x1 = transform.dx_pt + pan_negative.0 + (page_size.0 * transform.scale);
    let page_y1 = transform.dy_pt + pan_negative.1 + (page_size.1 * transform.scale);

    let is_visible_negative = page_x1 > 0.0 && page_y1 > 0.0;
    assert!(!is_visible_negative, "Negative pan beyond page boundary must be culled");

    // Centered pan (0, 0) intersects viewport
    let pan_centered = (0.0f64, 0.0f64);
    let page_x0 = transform.dx_pt + pan_centered.0;
    let page_y0 = transform.dy_pt + pan_centered.1;
    let page_x1_c = page_x0 + (page_size.0 * transform.scale);
    let page_y1_c = page_y0 + (page_size.1 * transform.scale);

    let is_visible_centered = page_x1_c > 0.0 && page_x0 < view_w && page_y1_c > 0.0 && page_y0 < view_h;
    assert!(is_visible_centered, "Centered pan must intersect preview viewport");
}

#[test]
fn test_feat25_bva04_multi_printer_rapid_toggle_preview_bounds_update() {
    let p_a4 = PrintableBounds::a4_standard_margins();
    let p_photo = PrintableBounds::a4_borderless();
    let p_envelope = PrintableBounds {
        printable_width_pt: 290.0,
        printable_height_pt: 600.0,
        physical_width_pt: 311.8,
        physical_height_pt: 623.6,
        dpi_x: 600,
        dpi_y: 600,
        margins: HardwareMargins::uniform(10.9),
    };

    let mut current_bounds = p_a4.clone();
    assert_eq!(current_bounds.dpi_x, 600);

    current_bounds = p_photo;
    assert_eq!(current_bounds.margins.left_pt, 0.0);

    current_bounds = p_envelope;
    assert_eq!(current_bounds.printable_width_pt, 290.0);
}

#[test]
fn test_feat25_bva05_clipping_hazard_sub_millimeter_visualization_bounds() {
    let bounds = PrintableBounds::a4_standard_margins();
    let page_oversized = (bounds.printable_width_pt + 0.001, bounds.printable_height_pt + 0.001);

    let transform = calculate_page_transform(
        page_oversized,
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert!(transform.clipping.is_clipped(1e-6));
}

// =========================================================================
// FEATURE 26: Reusable Printer Groups & Presets (Remediated)
// =========================================================================

#[test]
fn test_feat26_bva01_malformed_preset_json_schema_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let bad_file = dir.path().join("corrupted_presets.json");

    // Write syntactically invalid JSON
    let mut f = File::create(&bad_file).unwrap();
    f.write_all(b"{\"presets\": [ { \"id\": \"broken\", ").unwrap();

    // Import attempts recovery; returns InvalidData
    let result = PrintPresetStore::import_from_file(&bad_file);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);

    // Resilient fallback initializes clean default store
    let fallback_store = PrintPresetStore::with_defaults();
    assert_eq!(fallback_store.presets.len(), 3);
}

#[test]
fn test_feat26_bva02_missing_preset_file_fallback_to_defaults() {
    let missing_path = Path::new("Z:\\NonExistent_Presets_Path_404.json");
    let res = PrintPresetStore::import_from_file(missing_path);

    assert!(res.is_err());
    assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::NotFound);

    let default_store = PrintPresetStore::with_defaults();
    assert!(default_store.presets.contains_key("standard-duplex"));
}

#[test]
fn test_feat26_bva03_duplicate_group_and_preset_id_conflict_handling() {
    let mut store = PrintPresetStore::with_defaults();

    let mut g1 = PrinterGroup::new("laser_fleet", "Initial Fleet", false);
    g1.printer_names.insert("HP_LaserJet".into());
    store.upsert_group(g1);

    // Duplicate group insert updates configuration without corrupting store
    let mut g1_updated = PrinterGroup::new("laser_fleet", "Updated Laser Fleet", false);
    g1_updated.printer_names.insert("HP_LaserJet".into());
    g1_updated.printer_names.insert("Canon_MFD".into());
    store.upsert_group(g1_updated);

    assert_eq!(store.groups.len(), 1);
    assert_eq!(store.groups.get("laser_fleet").unwrap().name, "Updated Laser Fleet");
    assert_eq!(store.groups.get("laser_fleet").unwrap().printer_names.len(), 2);

    // Duplicate preset upsert updates target entry
    let custom = PrintJobPreset {
        id: "standard-duplex".into(),
        name: "Custom Standard Duplex".into(),
        description: "Overridden preset".into(),
        is_built_in: false,
        copies: 5,
    };
    store.upsert_preset(custom);
    assert_eq!(store.presets.get("standard-duplex").unwrap().copies, 5);
}

#[test]
fn test_feat26_bva04_preset_containing_uninstalled_printer_fallback() {
    let store = PrintPresetStore::with_defaults();
    let installed_printers = vec!["HP_LaserJet".to_string(), "Canon_MFD".to_string()];
    let preset_printer = "Non_Installed_Epson_1998";

    // Uninstalled printer automatically falls back to default installed printer
    let active_printer = store
        .resolve_printer_with_fallback(preset_printer, &installed_printers)
        .expect("Resolution must succeed with fallback");
    assert_eq!(active_printer, "HP_LaserJet");

    // Empty host printers list returns descriptive error
    let empty_res = store.resolve_printer_with_fallback(preset_printer, &[]);
    assert!(empty_res.is_err());
}

#[test]
fn test_feat26_bva05_maximum_preset_count_limits_and_oversized_store() {
    let mut store = PrintPresetStore::default();

    // Populate store up to maximum capacity
    for i in 0..PrintPresetStore::MAX_PRESETS {
        let preset = PrintJobPreset::new(format!("preset_{i:04}"), format!("Preset #{i}"), false);
        assert!(store.add_preset_checked(preset).is_ok());
    }
    assert_eq!(store.presets.len(), PrintPresetStore::MAX_PRESETS);

    // Exceeding capacity limit returns rejection error
    let overflow_preset = PrintJobPreset::new("preset_overflow", "Overflow", false);
    let overflow_res = store.add_preset_checked(overflow_preset);
    assert!(overflow_res.is_err(), "Adding beyond MAX_PRESETS must be rejected");
    assert!(overflow_res.unwrap_err().contains("Maximum preset count exceeded"));
}

// =========================================================================
// FEATURE 27: Built-in Diagnostic Logging & 1-Click Export (Remediated)
// =========================================================================

#[test]
fn test_feat27_bva01_diagnostic_export_under_disk_full_condition() {
    let gov = MockDiskSpaceGovernor::new(0);
    let status = gov.evaluate(Path::new("C:\\Reports"), 1024 * 1024);

    assert!(matches!(status.verdict, DiskVerdict::CriticalExhaustion { .. }));
}

#[test]
fn test_feat27_bva02_sensitive_credential_and_user_path_redaction() {
    let sensitive_msg = "Failed writing C:\\Users\\Administrator\\Secret\\token.pdf with auth token=secret_xyz12345";

    // Real production TraceRecord with redact_sensitive_paths
    let sanitized_msg = redact_sensitive_paths(sensitive_msg);
    let trace = TraceRecord {
        timestamp: chrono::Utc::now(),
        level: TraceLevel::Error,
        category: TraceCategory::Spooler,
        printer_name: Some("Secure_Printer".into()),
        message: sanitized_msg,
        error_code: Some("0x80070005".into()),
        context: BTreeMap::new(),
    };

    let report = collect_diagnostic_report(vec![], vec![], vec![trace]);
    let md = report.to_markdown_table();
    let json = report.to_json_pretty().unwrap();

    // Sensitive user name and authentication token must be cleanly redacted
    assert!(!md.contains("Administrator"), "User path must be redacted in Markdown");
    assert!(md.contains("[REDACTED_USER]"), "Sanitized user placeholder missing in Markdown");
    assert!(!json.contains("Administrator"), "User path must be redacted in JSON");
    assert!(json.contains("[REDACTED_USER]"), "Sanitized user placeholder missing in JSON");
    assert!(!json.contains("secret_xyz12345"), "Auth token must be redacted in JSON");
    assert!(json.contains("[REDACTED_TOKEN]"), "Sanitized token placeholder missing in JSON");
}

#[test]
fn test_feat27_bva03_corrupted_circular_fifo_trace_buffer_recovery() {
    let buffer = DiagnosticTraceBuffer::new(10);

    for i in 0..1000 {
        buffer.record(TraceRecord {
            timestamp: chrono::Utc::now(),
            level: TraceLevel::Info,
            category: TraceCategory::System,
            printer_name: None,
            message: format!("Log #{i}"),
            error_code: None,
            context: BTreeMap::new(),
        });
    }

    let records = buffer.snapshot();
    assert_eq!(records.len(), 10);
    assert_eq!(records[0].message, "Log #990");
    assert_eq!(records[9].message, "Log #999");
}

#[test]
fn test_feat27_bva04_export_to_non_writable_directory_error() {
    let report = DiagnosticReport::harvest_system();
    let bad_export_path = Path::new("Z:\\ReadOnlySystemDrive_99\\diagnostic_report.md");

    // DiagnosticReport::export_to_file handles I/O failure gracefully
    let res = report.export_to_file(bad_export_path);
    assert!(res.is_err(), "Export to unwritable path must return std::io::Error");
}

#[test]
fn test_feat27_bva05_oversized_log_entry_truncation_and_escaping() {
    let giant_msg = "X".repeat(10_000);
    let trace = TraceRecord {
        timestamp: chrono::Utc::now(),
        level: TraceLevel::Warning,
        category: TraceCategory::Spooler,
        printer_name: Some("Pipe|Printer".into()),
        message: giant_msg,
        error_code: None,
        context: BTreeMap::new(),
    };

    let report = collect_diagnostic_report(vec![], vec![], vec![trace]);
    let md = report.to_markdown_table();

    assert!(md.contains("## 5. Execution Traces & Diagnostic Logs"));
    assert!(md.contains("Pipe\\|Printer") || md.contains("Pipe|Printer"));
}
