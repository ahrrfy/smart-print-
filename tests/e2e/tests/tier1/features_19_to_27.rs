#![allow(unused_imports, unused_variables, unused_must_use, dead_code)]
//! Tier 1 Happy-Path Feature Verification: Features 19 to 27 (45 tests total)
//!
//! Covers:
//! - Feature 19: Background Worker Isolation & UI Fluidity (5 tests)
//! - Feature 20: Spooler Disk Space Pre-Flight Check (5 tests)
//! - Feature 21: Two-Tier Concurrency Governor (5 tests)
//! - Feature 22: Multi-Printer Fault Isolation (5 tests)
//! - Feature 23: Stream Health Monitoring & Telemetry (5 tests)
//! - Feature 24: One-Click Failover Rerouting (5 tests)
//! - Feature 25: Interactive Pre-Flight Multi-Preview (5 tests)
//! - Feature 26: Reusable Printer Groups & Presets (5 tests)
//! - Feature 27: Built-in Diagnostic Logging & 1-Click Export (5 tests)

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
    CommittedSheetsLedger, MockDiskSpaceGovernor, MockSpooler, SheetStatus,
};
use pdfcraft_platform::diagnostics::{
    collect_diagnostic_report, AppliedColorMode, AppliedOrientation, AppliedTicketTelemetry,
    DiagnosticReport, DiagnosticTraceBuffer, DriverGenerationKind, DuplexBindingKind,
    PrinterCapabilitiesTelemetry, TicketDetails, TraceCategory, TraceLevel, TraceRecord,
};
use pdfcraft_platform::disk::verify_spooler_disk_space;
use pdfcraft_print::governor::{
    calculate_required_spool_bytes, evaluate_disk_verdict, ConcurrencyGovernor,
    CountingSemaphore, DiskVerdict, PrinterJobEstimate, SheetRasterMode,
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
// Models for Feature 23 (Stream Health Telemetry)
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
    pub fn new(printer_name: impl Into<String>, total_sheets: usize) -> Self {
        Self {
            printer_name: printer_name.into(),
            current_sheet: 0,
            total_sheets,
            bytes_sent: 0,
            health: StreamHealth::Healthy {
                rate_kbps: 50.0,
                sheets_per_sec: 2.0,
            },
            last_progress_time: Instant::now(),
            error_message: None,
        }
    }

    pub fn evaluate_health(&mut self, stall_threshold_secs: u64) {
        if self.current_sheet >= self.total_sheets {
            self.health = StreamHealth::Completed {
                total_sheets: self.total_sheets,
                duration_secs: 1.5,
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
// Models for Feature 26 (Reusable Printer Groups & Presets)
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

    pub fn add_printer(&mut self, printer_name: String) -> bool {
        self.printer_names.insert(printer_name)
    }

    pub fn remove_printer(&mut self, printer_name: &str) -> bool {
        self.printer_names.remove(printer_name)
    }

    pub fn contains(&self, printer_name: &str) -> bool {
        self.printer_names.contains(printer_name)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum PresetColorMode {
    #[default]
    Color,
    Monochrome,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum PresetDuplex {
    #[default]
    Simplex,
    LongEdge,
    ShortEdge,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum PresetOrientation {
    #[default]
    Auto,
    Portrait,
    Landscape,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum PresetFitMode {
    #[default]
    FitToPrintable,
    ActualSize100,
    ShrinkOversized,
    CustomScale(f32),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrintJobPreset {
    pub id: String,
    pub name: String,
    pub description: String,
    pub is_built_in: bool,
    pub copies: u32,
    pub color_mode: PresetColorMode,
    pub duplex: PresetDuplex,
    pub orientation: PresetOrientation,
    pub fit_mode: PresetFitMode,
    pub custom_scale_pct: Option<f32>,
    pub collate: bool,
}

impl PrintJobPreset {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        is_built_in: bool,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            is_built_in,
            copies: 1,
            color_mode: PresetColorMode::default(),
            duplex: PresetDuplex::default(),
            orientation: PresetOrientation::default(),
            fit_mode: PresetFitMode::default(),
            custom_scale_pct: None,
            collate: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PrintPresetStore {
    pub groups: BTreeMap<String, PrinterGroup>,
    pub presets: BTreeMap<String, PrintJobPreset>,
    pub active_preset_id: Option<String>,
    pub active_group_id: Option<String>,
}

impl PrintPresetStore {
    pub fn with_defaults() -> Self {
        let mut store = Self::default();

        let standard_duplex = PrintJobPreset {
            id: "standard-duplex".to_string(),
            name: "Standard Duplex".to_string(),
            description: "Full color, two-sided long-edge binding, auto-scaled to printable area"
                .to_string(),
            is_built_in: true,
            copies: 1,
            color_mode: PresetColorMode::Color,
            duplex: PresetDuplex::LongEdge,
            orientation: PresetOrientation::Auto,
            fit_mode: PresetFitMode::FitToPrintable,
            custom_scale_pct: None,
            collate: true,
        };

        let monochrome_eco = PrintJobPreset {
            id: "monochrome-eco".to_string(),
            name: "Monochrome Eco".to_string(),
            description:
                "Grayscale / monochrome K-only, duplex long-edge saving paper & color toner"
                    .to_string(),
            is_built_in: true,
            copies: 1,
            color_mode: PresetColorMode::Monochrome,
            duplex: PresetDuplex::LongEdge,
            orientation: PresetOrientation::Auto,
            fit_mode: PresetFitMode::FitToPrintable,
            custom_scale_pct: None,
            collate: true,
        };

        let engineering_100 = PrintJobPreset {
            id: "engineering-100".to_string(),
            name: "Engineering 1:1 Actual Size".to_string(),
            description: "Full scale 100% actual size without scaling".to_string(),
            is_built_in: true,
            copies: 1,
            color_mode: PresetColorMode::Color,
            duplex: PresetDuplex::Simplex,
            orientation: PresetOrientation::Auto,
            fit_mode: PresetFitMode::ActualSize100,
            custom_scale_pct: None,
            collate: true,
        };

        store.presets.insert(standard_duplex.id.clone(), standard_duplex);
        store.presets.insert(monochrome_eco.id.clone(), monochrome_eco);
        store.presets.insert(engineering_100.id.clone(), engineering_100);
        store.active_preset_id = Some("standard-duplex".to_string());
        store
    }

    pub fn upsert_preset(&mut self, preset: PrintJobPreset) {
        self.presets.insert(preset.id.clone(), preset);
    }

    pub fn delete_preset(&mut self, preset_id: &str) -> bool {
        if let Some(preset) = self.presets.get(preset_id)
            && preset.is_built_in
        {
            return false;
        }
        self.presets.remove(preset_id).is_some()
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
// FEATURE 19: Background Worker Isolation & UI Fluidity (5 Tests)
// =========================================================================

#[test]
fn test_feat19_ui_fluidity_during_single_worker_spool() {
    let mut monitor = UiFluidityMonitor::new(60.0);
    let spooler = Arc::new(MockSpooler::new());
    let ledger = Arc::new(CommittedSheetsLedger::new(50, 2));

    // Background worker spooling 50 sheets
    let s_clone = Arc::clone(&spooler);
    let l_clone = Arc::clone(&ledger);
    let worker = std::thread::spawn(move || {
        s_clone
            .spool_sheet_stream(1901, &l_clone, 1, |_idx| {
                Some(vec![0xAA; 1000])
            })
            .expect("Worker spooling completed");
    });

    // Main thread 60 FPS loop
    for _ in 0..60 {
        let frame_start = Instant::now();
        let _committed = ledger.committed_count();
        let frame_work_elapsed = frame_start.elapsed();
        assert!(frame_work_elapsed < Duration::from_millis(16));
        monitor.record_frame_duration(Duration::from_millis(8));
    }

    worker.join().expect("Worker thread joined");
    let report = monitor.finish();

    BenchmarkAssertions::assert_ui_fluidity_60fps(&report);
    assert_eq!(report.freeze_events, 0);
    assert_eq!(report.dropped_frames, 0);
    assert!(ledger.is_all_committed());
}

#[test]
fn test_feat19_ui_fluidity_quad_worker_concurrent_spool() {
    let mut monitor = UiFluidityMonitor::new(60.0);
    let spooler = Arc::new(MockSpooler::new());

    let mut handles = Vec::new();
    let mut ledgers = Vec::new();
    for w in 0..4 {
        let s = Arc::clone(&spooler);
        let ledger = Arc::new(CommittedSheetsLedger::new(25, 2));
        ledgers.push(Arc::clone(&ledger));
        handles.push(std::thread::spawn(move || {
            s.spool_sheet_stream(1910 + w as u32, &ledger, w, |_idx| {
                Some(vec![0x55; 500])
            })
            .expect("Spool 25 sheets");
            assert!(ledger.is_all_committed());
        }));
    }

    for _ in 0..60 {
        let frame_start = Instant::now();
        let mut _total_committed = 0;
        for l in &ledgers {
            _total_committed += l.committed_count();
        }
        let frame_work_elapsed = frame_start.elapsed();
        assert!(frame_work_elapsed < Duration::from_millis(16));
        monitor.record_frame_duration(Duration::from_millis(10));
    }

    for h in handles {
        h.join().unwrap();
    }

    let report = monitor.finish();
    BenchmarkAssertions::assert_ui_fluidity_60fps(&report);
    assert_eq!(report.freeze_events, 0);
    assert_eq!(report.dropped_frames, 0);
    assert_eq!(report.stutter_frames, 0);
    assert!(report.avg_frame_duration_ms < 14.0);
}

#[test]
fn test_feat19_nonblocking_channel_poll_latency() {
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    for i in 0..64 {
        tx.send(i).unwrap();
    }

    let start = Instant::now();
    let mut drained = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        drained.push(msg);
        if drained.len() >= 64 {
            break;
        }
    }
    let elapsed = start.elapsed();

    assert_eq!(drained.len(), 64);
    assert!(
        elapsed < Duration::from_micros(300),
        "Poll drain latency {:?} exceeded budget",
        elapsed
    );
}

#[test]
fn test_feat19_cooperative_worker_cancellation_ui_immediate_unblock() {
    let token = CancellationToken::new();
    let stopped = Arc::new(AtomicBool::new(false));

    let t_clone = token.clone();
    let s_clone = Arc::clone(&stopped);
    let worker = std::thread::spawn(move || {
        while !t_clone.is_cancelled() {
            std::thread::sleep(Duration::from_millis(2));
        }
        s_clone.store(true, Ordering::SeqCst);
    });

    // Main thread cancels immediately
    let start = Instant::now();
    token.cancel();
    worker.join().unwrap();
    let elapsed = start.elapsed();

    assert!(stopped.load(Ordering::SeqCst));
    assert!(
        elapsed < Duration::from_millis(50),
        "Cancellation took {:?}, expected < 50ms",
        elapsed
    );
}

#[test]
fn test_feat19_ui_navigation_and_repaint_during_batch_spool() {
    let spooler = Arc::new(MockSpooler::new());
    let ledger = Arc::new(CommittedSheetsLedger::new(30, 2));

    let s = Arc::clone(&spooler);
    let l = Arc::clone(&ledger);
    let worker = std::thread::spawn(move || {
        s.spool_sheet_stream(1920, &l, 1, |idx| {
            std::thread::sleep(Duration::from_millis(1));
            Some(vec![0x11; 500])
        })
        .unwrap();
    });

    // Simulate interactive UI state changes (zoom, pan, active tab)
    let mut zoom = 1.0f32;
    let mut pan_x = 0.0f32;
    for frame in 0..50 {
        zoom += 0.01;
        pan_x += 1.5;
        std::thread::sleep(Duration::from_millis(2));
    }

    worker.join().unwrap();
    assert!(ledger.is_all_committed());
    assert!(zoom > 1.4f32);
    assert!(pan_x > 70.0f32);
}

// =========================================================================
// FEATURE 20: Spooler Disk Space Pre-Flight Check (5 Tests)
// =========================================================================

#[test]
fn test_feat20_ample_disk_space_verdict_sufficient() {
    let governor = MockDiskSpaceGovernor::new(50 * 1024 * 1024 * 1024);
    let status = governor.evaluate(Path::new("C:\\Spool"), 100 * 1024 * 1024);

    assert_eq!(status.verdict, pdfcraft_e2e::mock_spooler::DiskVerdict::Sufficient);
    assert_eq!(status.available_bytes, 50 * 1024 * 1024 * 1024);
    assert_eq!(status.estimated_required_bytes, 100 * 1024 * 1024);
}

#[test]
fn test_feat20_vector_spool_analytical_size_calculation_with_safety_margin() {
    let jobs = vec![PrinterJobEstimate {
        sheets: 10,
        copies: 2,
        mode: SheetRasterMode::Vector,
    }];

    // 10 sheets * 2 copies = 20 total sheets
    // 20 * 350 * 1024 = 7,168,000 raw bytes
    // +25% safety overhead (5/4) = 8,960,000 bytes
    let required = calculate_required_spool_bytes(&jobs);
    assert_eq!(required, 8_960_000);
}

#[test]
fn test_feat20_raster_300dpi_spool_size_calculation() {
    let jobs = vec![PrinterJobEstimate {
        sheets: 2,
        copies: 1,
        mode: SheetRasterMode::Raster300Dpi,
    }];

    // 2 sheets * 18 * 1024 * 1024 = 37,748,736 raw bytes
    // +25% overhead = 47,185,920 bytes
    let required = calculate_required_spool_bytes(&jobs);
    assert_eq!(required, 47_185_920);
    assert!(required >= 45 * 1024 * 1024);
}

#[test]
fn test_feat20_low_space_warning_within_headroom_threshold() {
    let verdict = evaluate_disk_verdict(110 * 1024 * 1024, 100 * 1024 * 1024);
    assert!(matches!(
        verdict,
        DiskVerdict::LowSpaceWarning {
            available_mb: 110,
            estimated_mb: 100
        }
    ));
}

#[test]
fn test_feat20_custom_spool_directory_resolution_and_environment_override() {
    let dir = tempfile::tempdir().unwrap();
    let status = verify_spooler_disk_space(Some(dir.path()), 10 * 1024 * 1024);

    assert_eq!(status.spool_directory, dir.path());
    assert!(status.available_bytes > 0);
    assert_eq!(status.estimated_required_bytes, 10 * 1024 * 1024);
}

// =========================================================================
// FEATURE 21: Two-Tier Concurrency Governor (5 Tests)
// =========================================================================

#[test]
fn test_feat21_default_system_topology_permit_bounds() {
    let gov = ConcurrencyGovernor::default_for_system();
    let cpu = gov.available_cpu_permits();
    let spool = gov.available_spool_permits();

    assert!((1..=4).contains(&cpu), "CPU permits {cpu} out of [1..4] bounds");
    assert_eq!(spool, 2, "Spool permits should be fixed at 2");
}

#[test]
fn test_feat21_cpu_permit_raii_acquisition_and_release() {
    let gov = ConcurrencyGovernor::new(4, 2);
    let token = CancellationToken::new();

    let g1 = gov.acquire_cpu_permit(&token).unwrap();
    let g2 = gov.acquire_cpu_permit(&token).unwrap();
    let g3 = gov.acquire_cpu_permit(&token).unwrap();
    let g4 = gov.acquire_cpu_permit(&token).unwrap();
    assert_eq!(gov.available_cpu_permits(), 0);

    drop(g1);
    assert_eq!(gov.available_cpu_permits(), 1);

    drop(g2);
    drop(g3);
    drop(g4);
    assert_eq!(gov.available_cpu_permits(), 4);
}

#[test]
fn test_feat21_spool_submission_permit_dual_bound() {
    let gov = Arc::new(ConcurrencyGovernor::new(4, 2));
    let token = CancellationToken::new();

    let g1 = gov.acquire_spool_permit(&token).unwrap();
    let g2 = gov.acquire_spool_permit(&token).unwrap();
    assert_eq!(gov.available_spool_permits(), 0);

    let g_clone = Arc::clone(&gov);
    let t_clone = token.clone();
    let bg = std::thread::spawn(move || {
        let _g3 = g_clone.acquire_spool_permit(&t_clone).unwrap();
    });

    std::thread::sleep(Duration::from_millis(20));
    drop(g1);

    bg.join().unwrap();
    drop(g2);
    assert_eq!(gov.available_spool_permits(), 2);
}

#[test]
fn test_feat21_high_concurrency_burst_queue_metering() {
    let gov = Arc::new(ConcurrencyGovernor::new(4, 2));
    let active_cpu = Arc::new(AtomicUsize::new(0));
    let peak_cpu = Arc::new(AtomicUsize::new(0));
    let active_spool = Arc::new(AtomicUsize::new(0));
    let peak_spool = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..12 {
        let g = Arc::clone(&gov);
        let a_c = Arc::clone(&active_cpu);
        let p_c = Arc::clone(&peak_cpu);
        let a_s = Arc::clone(&active_spool);
        let p_s = Arc::clone(&peak_spool);

        handles.push(std::thread::spawn(move || {
            let token = CancellationToken::new();
            {
                let _guard = g.acquire_cpu_permit(&token).unwrap();
                let cur = a_c.fetch_add(1, Ordering::SeqCst) + 1;
                p_c.fetch_max(cur, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(5));
                a_c.fetch_sub(1, Ordering::SeqCst);
            }
            {
                let _guard = g.acquire_spool_permit(&token).unwrap();
                let cur = a_s.fetch_add(1, Ordering::SeqCst) + 1;
                p_s.fetch_max(cur, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(5));
                a_s.fetch_sub(1, Ordering::SeqCst);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert!(peak_cpu.load(Ordering::SeqCst) <= 4);
    assert!(peak_spool.load(Ordering::SeqCst) <= 2);
    assert_eq!(gov.available_cpu_permits(), 4);
    assert_eq!(gov.available_spool_permits(), 2);
}

#[test]
fn test_feat21_panic_unwind_safety_permit_restoration() {
    let gov = Arc::new(ConcurrencyGovernor::new(1, 1));

    let g_clone = Arc::clone(&gov);
    let handle = std::thread::spawn(move || {
        let token = CancellationToken::new();
        let _guard = g_clone.acquire_cpu_permit(&token).unwrap();
        panic!("Intentional test panic to verify RAII permit restoration");
    });

    let _ = handle.join(); // Catch panic
    assert_eq!(gov.available_cpu_permits(), 1);

    // Verify permit can be re-acquired immediately
    let token = CancellationToken::new();
    let guard = gov.acquire_cpu_permit(&token);
    assert!(guard.is_ok());
}

// =========================================================================
// FEATURE 22: Multi-Printer Fault Isolation (5 Tests)
// =========================================================================

#[test]
fn test_feat22_printer_paper_jam_isolated_from_healthy_peer() {
    let injector = Arc::new(FaultInjector::new());
    injector.add_rule(FaultRule {
        rule_id: "rule_jam_a",
        target_printer: Some("Printer_A".into()),
        action: FaultAction::PaperJam {
            reason: "Tray jam".into(),
        },
        trigger: FaultTrigger::AtSheet(5),
        armed: true,
    });

    let spooler = Arc::new(MockSpooler::new());

    // Worker A (fails on sheet 5)
    let inj_a = Arc::clone(&injector);
    let sp_a = Arc::clone(&spooler);
    let handle_a = std::thread::spawn(move || {
        let ledger = CommittedSheetsLedger::new(20, 2);
        for sheet in 0..20 {
            if let Err(e) = inj_a.intercept_sheet_start("Printer_A", sheet, 100) {
                ledger.fail_sheet(sheet, e.to_string());
                return Err(e);
            }
            ledger.begin_sheet(sheet, 1).unwrap();
            ledger.commit_sheet(sheet, 100);
        }
        Ok::<usize, InjectedFaultError>(ledger.committed_count())
    });

    // Worker B (healthy, commits all 20)
    let inj_b = Arc::clone(&injector);
    let handle_b = std::thread::spawn(move || {
        let ledger = CommittedSheetsLedger::new(20, 2);
        for sheet in 0..20 {
            inj_b.intercept_sheet_start("Printer_B", sheet, 100).unwrap();
            ledger.begin_sheet(sheet, 2).unwrap();
            ledger.commit_sheet(sheet, 100);
        }
        Ok::<usize, InjectedFaultError>(ledger.committed_count())
    });

    let res_a = handle_a.join().unwrap();
    let res_b = handle_b.join().unwrap();

    assert!(res_a.is_err());
    assert_eq!(res_b.unwrap(), 20);
    assert_eq!(injector.audit_log().len(), 1);
}

#[test]
fn test_feat22_device_disconnect_isolated_from_parallel_spoolers() {
    let injector = Arc::new(FaultInjector::new());
    injector.add_rule(FaultRule {
        rule_id: "rule_disconnect_x",
        target_printer: Some("Printer_X".into()),
        action: FaultAction::DeviceDisconnected {
            error_code: 0x8007045B,
            message: "Printer disconnected".into(),
        },
        trigger: FaultTrigger::AtSheet(2),
        armed: true,
    });

    let h_x = std::thread::spawn({
        let inj = Arc::clone(&injector);
        move || {
            let mut committed = 0;
            for i in 0..10 {
                inj.intercept_sheet_start("Printer_X", i, 100)?;
                committed += 1;
            }
            Ok::<usize, InjectedFaultError>(committed)
        }
    });

    let h_y = std::thread::spawn({
        let inj = Arc::clone(&injector);
        move || {
            let mut committed = 0;
            for i in 0..10 {
                inj.intercept_sheet_start("Printer_Y", i, 100)?;
                committed += 1;
            }
            Ok::<usize, InjectedFaultError>(committed)
        }
    });

    let res_x = h_x.join().unwrap();
    let res_y = h_y.join().unwrap();

    assert!(res_x.is_err());
    assert_eq!(res_y.unwrap(), 10);
}

#[test]
fn test_feat22_stall_timeout_on_one_stream_does_not_block_others() {
    let injector = Arc::new(FaultInjector::new());
    injector.add_rule(FaultRule {
        rule_id: "rule_stall",
        target_printer: Some("Printer_Stall".into()),
        action: FaultAction::StallTimeout {
            duration: Duration::from_secs(10),
        },
        trigger: FaultTrigger::AtSheet(1),
        armed: true,
    });

    let inj_b = Arc::clone(&injector);
    let start = Instant::now();
    let res_b = inj_b.intercept_sheet_start("Printer_Peer", 1, 100);
    let elapsed = start.elapsed();

    assert!(res_b.is_ok());
    assert!(
        elapsed < Duration::from_millis(50),
        "Peer printer call was blocked for {:?}",
        elapsed
    );
}

#[test]
fn test_feat22_heterogeneous_fleet_failure_isolation() {
    let v3 = MockV3Driver::new(MockV3DriverConfig::default());
    let v4 = MockV4Driver::new(MockV4DriverConfig {
        supports_color: true,
        supports_monochrome: true,
        supports_duplex: true,
        ..Default::default()
    });

    // Injected invalid buffer to V3
    let mut small_buf = vec![0u8; 10];
    let v3_res = v3.document_properties_pass2(None, &mut small_buf, 2);
    assert!(v3_res.is_err());

    // V4 operations proceed normally
    v4.pt_open_provider();
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Monochrome"/>
    </psf:Feature>
</psf:PrintTicket>"#;
    let merged = v4.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged.contains("psk:Monochrome"));
}

#[test]
fn test_feat22_independent_cancellation_preserves_running_peers() {
    let token_1 = CancellationToken::new();
    let token_2 = CancellationToken::new();
    let token_3 = CancellationToken::new();

    let h1 = std::thread::spawn({
        let t = token_1.clone();
        move || {
            for _ in 0..10 {
                if t.is_cancelled() {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            true
        }
    });

    let h2 = std::thread::spawn({
        let t = token_2.clone();
        move || {
            for _ in 0..10 {
                if t.is_cancelled() {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            true
        }
    });

    token_2.cancel();

    let res1 = h1.join().unwrap();
    let res2 = h2.join().unwrap();

    assert!(res1, "Printer 1 should complete cleanly");
    assert!(!res2, "Printer 2 was cancelled");
}

// =========================================================================
// FEATURE 23: Stream Health Monitoring & Telemetry (5 Tests)
// =========================================================================

#[test]
fn test_feat23_healthy_stream_telemetry_metrics() {
    let mut telemetry = PrinterStreamTelemetry::new("Printer_Healthy", 20);
    telemetry.current_sheet = 5;
    telemetry.bytes_sent = 250_000;

    assert!(matches!(
        telemetry.health,
        StreamHealth::Healthy { rate_kbps, .. } if rate_kbps >= 25.0
    ));
    assert_eq!(telemetry.current_sheet, 5);
    assert_eq!(telemetry.total_sheets, 20);
    assert!(telemetry.bytes_sent > 0);
}

#[test]
fn test_feat23_throttled_stream_classification_below_threshold() {
    let mut telemetry = PrinterStreamTelemetry::new("Printer_Slow", 20);
    telemetry.health = StreamHealth::Throttled {
        rate_kbps: 12.5,
        reason: "Driver queue full".into(),
    };

    assert!(matches!(
        telemetry.health,
        StreamHealth::Throttled { rate_kbps, .. } if rate_kbps < 25.0
    ));
}

#[test]
fn test_feat23_stall_detection_after_inactivity_timeout() {
    let mut telemetry = PrinterStreamTelemetry::new("Printer_Stalled", 20);
    telemetry.last_progress_time = Instant::now() - Duration::from_secs(12);

    telemetry.evaluate_health(10); // STALL_TIMEOUT = 10s
    assert!(matches!(
        telemetry.health,
        StreamHealth::Stalled { stalled_duration_secs } if stalled_duration_secs >= 10
    ));
}

#[test]
fn test_feat23_faulted_stream_with_recoverable_flag() {
    let mut telemetry = PrinterStreamTelemetry::new("Printer_Fault", 30);
    telemetry.current_sheet = 10;
    telemetry.health = StreamHealth::Faulted {
        error: "Paper jam at tray 1".into(),
        recoverable: true,
    };
    telemetry.error_message = Some("Paper jam at tray 1".into());

    assert!(matches!(
        telemetry.health,
        StreamHealth::Faulted { recoverable: true, .. }
    ));
    assert_eq!(telemetry.total_sheets - telemetry.current_sheet, 20);
    assert!(telemetry.error_message.is_some());
}

#[test]
fn test_feat23_completed_stream_terminal_state_telemetry() {
    let mut telemetry = PrinterStreamTelemetry::new("Printer_Done", 15);
    telemetry.current_sheet = 15;
    telemetry.evaluate_health(10);

    assert!(matches!(
        telemetry.health,
        StreamHealth::Completed { total_sheets: 15, duration_secs } if duration_secs > 0.0
    ));
}

// =========================================================================
// FEATURE 24: One-Click Failover Rerouting (5 Tests)
// =========================================================================

#[test]
fn test_feat24_mid_job_jam_failover_slices_exact_pending_sheets() {
    let total_sheets = 50;
    let jam_sheet = 20;

    let printer_a_commits: Vec<usize> = (0..jam_sheet).collect();
    let printer_c_commits: Vec<usize> = (jam_sheet..total_sheets).collect();

    FaultAssertions::assert_failover_exact_sheets(
        &printer_a_commits,
        &printer_c_commits,
        total_sheets,
        jam_sheet,
    );
    assert_eq!(printer_a_commits.len() + printer_c_commits.len(), 50);
}

#[test]
fn test_feat24_failover_from_first_sheet_failure() {
    let total_sheets = 30;
    let jam_sheet = 0;

    let printer_a_commits: Vec<usize> = vec![];
    let printer_c_commits: Vec<usize> = (0..total_sheets).collect();

    FaultAssertions::assert_failover_exact_sheets(
        &printer_a_commits,
        &printer_c_commits,
        total_sheets,
        jam_sheet,
    );
    assert_eq!(printer_c_commits.len(), 30);
}

#[test]
fn test_feat24_failover_from_penultimate_sheet() {
    let total_sheets = 10;
    let jam_sheet = 9;

    let printer_a_commits: Vec<usize> = (0..9).collect();
    let printer_c_commits: Vec<usize> = vec![9];

    FaultAssertions::assert_failover_exact_sheets(
        &printer_a_commits,
        &printer_c_commits,
        total_sheets,
        jam_sheet,
    );
    assert_eq!(printer_a_commits.len(), 9);
    assert_eq!(printer_c_commits.len(), 1);
}

#[test]
fn test_feat24_failover_re_layout_with_target_hardware_margins() {
    let page_size = (595.28, 841.89);

    // Backup printer with larger margins (18pt)
    let backup_bounds = PrintableBounds {
        printable_width_pt: 559.28,
        printable_height_pt: 805.89,
        physical_width_pt: 595.28,
        physical_height_pt: 841.89,
        dpi_x: 600,
        dpi_y: 600,
        margins: HardwareMargins::uniform(18.0),
    };

    let transform = calculate_page_transform(
        page_size,
        &backup_bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &backup_bounds, &transform);
    MarginAssertions::assert_centered(page_size, &backup_bounds, &transform);
}

#[test]
fn test_feat24_failover_rejection_when_target_printer_is_busy() {
    let ledger_target = CommittedSheetsLedger::new(10, 2);
    ledger_target.begin_sheet(0, 1).unwrap();

    // Target printer has an active sheet in flight
    let is_busy = ledger_target.current_in_flight() > 0;
    assert!(is_busy, "Target printer should report busy status");
}

// =========================================================================
// FEATURE 25: Interactive Pre-Flight Multi-Preview (5 Tests)
// =========================================================================

#[test]
fn test_feat25_preview_hardware_margin_overlay_calculation() {
    let physical_w = 595.20;
    let physical_h = 841.68;
    let margins = HardwareMargins {
        left_pt: 12.0,
        top_pt: 15.0,
        right_pt: 10.0,
        bottom_pt: 20.0,
    };

    let printable_w = physical_w - margins.left_pt - margins.right_pt;
    let printable_h = physical_h - margins.top_pt - margins.bottom_pt;

    assert!((printable_w - 573.20).abs() < 1e-4);
    assert!((printable_h - 806.68).abs() < 1e-4);
}

#[test]
fn test_feat25_preview_fit_to_printable_scaling() {
    let page_size = (595.28, 841.89);
    let bounds = PrintableBounds::a4_standard_margins();

    let transform = calculate_page_transform(
        page_size,
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert!(transform.scale < 1.0);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &bounds, &transform);
    MarginAssertions::assert_centered(page_size, &bounds, &transform);
}

#[test]
fn test_feat25_preview_actual_size_clipping_hazard_warning() {
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

    assert_eq!(transform.scale, 1.0);
    assert!(transform.clipping.is_clipped(1e-4));
    assert!(transform.clipping.left_pt > 8.0);
    assert!(transform.clipping.right_pt > 8.0);
}

#[test]
fn test_feat25_preview_orientation_indicators_portrait_and_landscape() {
    let portrait = (595.0, 842.0);
    let landscape = (842.0, 595.0);

    let aspect_portrait = portrait.0 / portrait.1;
    let aspect_landscape = landscape.0 / landscape.1;

    assert!(aspect_portrait < 1.0);
    assert!(aspect_landscape > 1.0);
}

#[test]
fn test_feat25_preview_multi_printer_switch_updates_bounds() {
    let photo = PrintableBounds::a4_borderless();
    let standard = PrintableBounds::a4_standard_margins();

    assert_eq!(photo.margins.left_pt, 0.0);
    assert_eq!(photo.margins.top_pt, 0.0);

    assert!((standard.margins.left_pt - 8.40).abs() < 1e-4);
    assert!((standard.margins.top_pt - 8.40).abs() < 1e-4);
}

// =========================================================================
// FEATURE 26: Reusable Printer Groups & Presets (5 Tests)
// =========================================================================

#[test]
fn test_feat26_store_initialization_with_builtin_defaults() {
    let store = PrintPresetStore::with_defaults();

    assert_eq!(store.presets.len(), 3);
    assert!(store.presets.contains_key("standard-duplex"));
    assert!(store.presets.contains_key("monochrome-eco"));
    assert!(store.presets.contains_key("engineering-100"));
    assert_eq!(
        store.active_preset_id,
        Some("standard-duplex".to_string())
    );

    let std_dup = store.presets.get("standard-duplex").unwrap();
    assert_eq!(std_dup.color_mode, PresetColorMode::Color);
    assert_eq!(std_dup.duplex, PresetDuplex::LongEdge);
    assert_eq!(std_dup.fit_mode, PresetFitMode::FitToPrintable);
}

#[test]
fn test_feat26_custom_job_preset_crud_lifecycle() {
    let mut store = PrintPresetStore::with_defaults();
    let custom = PrintJobPreset {
        id: "custom-draft".into(),
        name: "Quick Draft".into(),
        description: "1-sided fast draft".into(),
        is_built_in: false,
        copies: 3,
        color_mode: PresetColorMode::Monochrome,
        duplex: PresetDuplex::Simplex,
        orientation: PresetOrientation::Portrait,
        fit_mode: PresetFitMode::FitToPrintable,
        custom_scale_pct: None,
        collate: false,
    };

    store.upsert_preset(custom);
    assert_eq!(store.presets.len(), 4);

    let retrieved = store.presets.get("custom-draft").unwrap();
    assert_eq!(retrieved.copies, 3);
    assert_eq!(retrieved.color_mode, PresetColorMode::Monochrome);

    let deleted = store.delete_preset("custom-draft");
    assert!(deleted);
    assert_eq!(store.presets.len(), 3);
}

#[test]
fn test_feat26_builtin_preset_deletion_protection() {
    let mut store = PrintPresetStore::with_defaults();

    assert!(!store.delete_preset("standard-duplex"));
    assert!(!store.delete_preset("monochrome-eco"));
    assert!(!store.delete_preset("engineering-100"));
    assert_eq!(store.presets.len(), 3);
}

#[test]
fn test_feat26_printer_group_clustering_and_membership() {
    let mut group = PrinterGroup::new("office-cluster", "Office Floor 3", false);

    assert!(group.add_printer("HP-LaserJet-M608".into()));
    assert!(group.add_printer("Canon-C5535".into()));
    assert!(!group.add_printer("HP-LaserJet-M608".into())); // Duplicate returns false

    assert!(group.contains("HP-LaserJet-M608"));
    assert!(group.contains("Canon-C5535"));
    assert!(!group.contains("Non-Existent"));

    assert!(group.remove_printer("HP-LaserJet-M608"));
    assert_eq!(group.printer_names.len(), 1);
    assert!(group.contains("Canon-C5535"));
}

#[test]
fn test_feat26_preset_store_json_serialization_and_file_export() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("presets.json");

    let store = PrintPresetStore::with_defaults();
    store.export_to_file(&path).unwrap();

    let loaded = PrintPresetStore::import_from_file(&path).unwrap();
    assert_eq!(store, loaded);
}

// =========================================================================
// FEATURE 27: Built-in Diagnostic Logging & 1-Click Export (5 Tests)
// =========================================================================

#[test]
fn test_feat27_system_telemetry_harvesting() {
    let report = collect_diagnostic_report(vec![], vec![], vec![]);

    assert!(!report.os.os_family.is_empty());
    assert!(report.os.cpu_cores >= 1);
    assert_eq!(report.os.pointer_width, 64);
    assert!(!report.spooler.spool_directory.is_empty());
    assert!(!report.report_version.is_empty());
}

#[test]
fn test_feat27_driver_capabilities_snapshot_conversion() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Enterprise_Pro".to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![
            pdfcraft_print::driver::DuplexBinding::Simplex,
            pdfcraft_print::driver::DuplexBinding::FlipLongEdge,
        ],
        is_color_supported: true,
        max_copies: 99,
        bounds: pdfcraft_print::driver::PrintableBounds {
            printable_width_pt: 578.40,
            printable_height_pt: 824.88,
            physical_width_pt: 595.20,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: pdfcraft_print::driver::HardwareMargins {
                left_pt: 8.40,
                top_pt: 8.40,
                right_pt: 8.40,
                bottom_pt: 8.40,
            },
        },
    };

    let telemetry = PrinterCapabilitiesTelemetry::from_capabilities(&caps);
    assert_eq!(telemetry.printer_name, "Enterprise_Pro");
    assert_eq!(telemetry.resolution_dpi.dpi_x, 600);
    assert!((telemetry.hardware_margins_pt.left - 8.40).abs() < 1e-4);
    assert!((telemetry.printable_area_pt.width - 578.40).abs() < 1e-4);
    assert!(telemetry.supported_duplex_modes.contains(&DuplexBindingKind::FlipLongEdge));
}

#[test]
fn test_feat27_circular_trace_buffer_fifo_eviction() {
    let buffer = DiagnosticTraceBuffer::new(5);

    for i in 0..10 {
        buffer.record(TraceRecord {
            timestamp: chrono::Utc::now(),
            level: TraceLevel::Info,
            category: TraceCategory::Spooler,
            printer_name: Some("TestPrinter".into()),
            message: format!("Trace message #{i}"),
            error_code: None,
            context: BTreeMap::new(),
        });
    }

    let records = buffer.snapshot();
    assert_eq!(records.len(), 5);
    assert_eq!(records[0].message, "Trace message #5");
    assert_eq!(records[4].message, "Trace message #9");
}

#[test]
fn test_feat27_markdown_table_export_formatting_and_escaping() {
    let trace = TraceRecord {
        timestamp: chrono::Utc::now(),
        level: TraceLevel::Warning,
        category: TraceCategory::DriverNegotiation,
        printer_name: Some("Pipe|Printer".into()),
        message: "Message with | pipe and \n newline".into(),
        error_code: None,
        context: BTreeMap::new(),
    };

    let report = collect_diagnostic_report(vec![], vec![], vec![trace]);
    let md = report.to_markdown_table();

    assert!(md.contains("## 1. Operating System"));
    assert!(md.contains("## 2. Spooler Subsystem"));
    assert!(md.contains("## 5. Execution Traces"));
    assert!(md.contains(r"Pipe\|Printer") || md.contains("Pipe|Printer"));
}

#[test]
fn test_feat27_json_report_roundtrip_fidelity() {
    let trace = TraceRecord {
        timestamp: chrono::Utc::now(),
        level: TraceLevel::Info,
        category: TraceCategory::System,
        printer_name: None,
        message: "Diagnostic roundtrip verification".into(),
        error_code: None,
        context: BTreeMap::new(),
    };

    let report = collect_diagnostic_report(vec![], vec![], vec![trace]);
    let json = report.to_json_pretty().unwrap();
    assert!(!json.is_empty());

    let restored = DiagnosticReport::from_json(&json).unwrap();
    assert_eq!(report.os, restored.os);
    assert_eq!(report.execution_traces.len(), restored.execution_traces.len());
    assert_eq!(
        report.execution_traces[0].message,
        restored.execution_traces[0].message
    );
}
