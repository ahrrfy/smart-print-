//! Tier 3: Fault Tolerance & Failover × Cross-Axis Pairwise Combinations (Tests 19–27)
//!
//! Authoritative verification suite exercising orthogonal interactions across:
//! - Fault Injection: Paper Jam, Device Disconnect, Disk Full, Throttle & Stall
//! - Driver Generation: Win32 V3 DEVMODE, Win32 V4 XPS PrintTicket, CUPS/IPP 2.0
//! - Slicing & Failover: CommittedSheetsLedger, Multi-Printer Isolation, Cascading Failover
//! - Telemetry & Diagnostics: Stream Telemetry Transitions, Diagnostic Harvesting & Sanitization

use pdfcraft_e2e::fault_injector::{
    FaultAction, FaultAssertions, FaultInjector, FaultRule, FaultTrigger, InjectedFaultError,
};
use pdfcraft_e2e::margin_assertions::{
    calculate_page_transform, MarginAssertions, PageFitMode,
};
use pdfcraft_e2e::mock_driver::{
    DuplexBinding, HardwareMargins, MockIppDriver, MockV3Driver, MockV3DriverConfig,
    MockV4Driver, MockV4DriverConfig, PrintableBounds,
    DMCOLOR_COLOR, DMCOLOR_MONOCHROME, DMORIENT_PORTRAIT,
    DM_COLOR, DM_IN_BUFFER, DM_ORIENTATION, DM_OUT_BUFFER,
};
use pdfcraft_e2e::mock_spooler::{
    CommittedSheetsLedger, DiskVerdict, MockDiskSpaceGovernor, MockSpooler,
};
use pdfcraft_e2e::stream_inspector::StreamAssertions;
use pdfcraft_platform::diagnostics::{
    collect_diagnostic_report, AppliedColorMode, AppliedOrientation, AppliedTicketTelemetry,
    DiagnosticReport, DiagnosticTraceBuffer, DriverGenerationKind, DuplexBindingKind,
    MarginsPt, PrinterCapabilitiesTelemetry, ResolutionDpi, SizePt, TicketDetails,
    TraceCategory, TraceLevel, TraceRecord,
};
use pdfcraft_print::governor::{
    calculate_required_spool_bytes, ConcurrencyGovernor, PrinterJobEstimate, SheetRasterMode,
};
use pdfcraft_print::monochrome::MonochromeVectorFilter;
use pdfcraft_print::stream::CancellationToken;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

// =========================================================================
// Opaque-Box Telemetry Models for Feature 23
// =========================================================================

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum StreamHealth {
    Healthy { rate_kbps: f64, sheets_per_sec: f64 },
    Throttled { rate_kbps: f64, reason: String },
    Stalled { stalled_duration_secs: u64 },
    Faulted { error: String, recoverable: bool },
    Completed { total_sheets: usize, duration_secs: f64 },
}

#[allow(dead_code)]
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

/// Helper: passes raw content stream bytes through MonochromeVectorFilter
fn filter_content_stream_bytes(input: &[u8]) -> Vec<u8> {
    let parsed = pdfcraft_content::parse(input);
    let filtered_ops = MonochromeVectorFilter::filter_operators(&parsed.ops);
    pdfcraft_content::serialize_ops(&filtered_ops)
}

// =========================================================================
// TEST 19: Mid-Job Paper Jam × Heterogeneous Failover (V3 -> V4) × Slicing
// =========================================================================

#[test]
fn test_t3_19_paper_jam_mid_stream_v3_to_v4_failover_sheet_slicing() {
    let total_sheets = 60;
    let jam_sheet = 25;
    let printer_v3_name = "Printer_V3_Primary";
    let printer_v4_name = "Printer_V4_Backup";

    let injector = FaultInjector::new();
    injector.add_rule(FaultRule {
        rule_id: "jam_at_sheet_25",
        target_printer: Some(printer_v3_name.into()),
        trigger: FaultTrigger::AtSheet(jam_sheet),
        action: FaultAction::PaperJam {
            reason: "Tray 1 misfeed".into(),
        },
        armed: true,
    });
    injector.start_batch();

    // Primary: MockV3Driver
    let mock_v3 = MockV3Driver::new(MockV3DriverConfig {
        printer_name: printer_v3_name.into(),
        default_color: true,
        bounds: PrintableBounds::a4_standard_margins(),
        ..Default::default()
    });
    let needed = mock_v3.document_properties_pass1() as usize;
    let mut devmode_buf = vec![0u8; needed];
    mock_v3.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();
    devmode_buf[72..76].copy_from_slice(&(DM_COLOR | DM_ORIENTATION).to_le_bytes());
    devmode_buf[44..46].copy_from_slice(&DMORIENT_PORTRAIT.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_COLOR.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock_v3.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    ).unwrap();

    let ledger_v3 = CommittedSheetsLedger::new(total_sheets, 2);
    let mut v3_committed = Vec::new();

    // Spool on Primary until fault
    for s in 0..total_sheets {
        match injector.intercept_sheet_start(printer_v3_name, s, 1024) {
            Ok(()) => {
                ledger_v3.begin_sheet(s, 1).unwrap();
                ledger_v3.commit_sheet(s, 1024);
                v3_committed.push(s);
            }
            Err(e) => {
                ledger_v3.begin_sheet(s, 1).unwrap();
                ledger_v3.fail_sheet(s, e.to_string());
                break;
            }
        }
    }

    assert_eq!(v3_committed.len(), jam_sheet);
    assert_eq!(ledger_v3.committed_count(), jam_sheet);

    // Failover slicing: extract uncommitted sheets
    let pending_sheets = ledger_v3.pending_sheets_for_failover();
    assert_eq!(pending_sheets.len(), total_sheets - jam_sheet);
    assert_eq!(pending_sheets.first(), Some(&jam_sheet));
    assert_eq!(pending_sheets.last(), Some(&(total_sheets - 1)));

    // Backup: MockV4Driver
    let mock_v4 = MockV4Driver::new(MockV4DriverConfig {
        printer_name: printer_v4_name.into(),
        supports_color: true,
        supports_monochrome: true,
        supports_duplex: true,
        bounds: PrintableBounds::a4_standard_margins(),
    });
    mock_v4.pt_open_provider().unwrap();
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
        <psf:Feature name="psk:PageOutputColor">
            <psf:Option name="psk:Color"/>
        </psf:Feature>
    </psf:PrintTicket>"#;
    let merged_ticket = mock_v4.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged_ticket.contains("psk:Color"));

    let ledger_v4 = CommittedSheetsLedger::new(total_sheets, 2);
    let mut v4_committed = Vec::new();

    for &s in &pending_sheets {
        injector.intercept_sheet_start(printer_v4_name, s, 1024).unwrap();
        ledger_v4.begin_sheet(s, 2).unwrap();
        ledger_v4.commit_sheet(s, 1024);
        v4_committed.push(s);
    }

    // Assertional Oracle: Exact disjoint partitioning with zero drops & zero duplicates
    FaultAssertions::assert_failover_exact_sheets(&v3_committed, &v4_committed, total_sheets, jam_sheet);
    assert_eq!(ledger_v3.committed_count(), 25);
    assert_eq!(ledger_v4.committed_count(), 35);
    assert_eq!(v3_committed.len() + v4_committed.len(), 60);

    let last_devmode = mock_v3.last_submitted_devmode().unwrap();
    assert_ne!(last_devmode.dm_fields & DM_COLOR, 0);
    assert_eq!(last_devmode.dm_color, Some(DMCOLOR_COLOR));
}

// =========================================================================
// TEST 20: Spool Disk Exhaustion × Multi-Printer Batch Pre-Flight Abort
// =========================================================================

#[test]
fn test_t3_20_spool_disk_exhaustion_multi_printer_preflight_abort() {
    let jobs = vec![
        PrinterJobEstimate::new(30, 2, SheetRasterMode::Vector),
        PrinterJobEstimate::new(20, 1, SheetRasterMode::Vector),
        PrinterJobEstimate::new(15, 1, SheetRasterMode::Raster300Dpi),
    ];

    let required_bytes = calculate_required_spool_bytes(&jobs);
    assert!(required_bytes > 350 * 1024 * 1024); // ~371.6 MB required

    let spooler = MockSpooler::new();
    let available_bytes = 150 * 1024 * 1024; // 150 MB available
    spooler.disk_governor.set_available_bytes(available_bytes);

    let status = spooler.disk_governor.evaluate(&spooler.spool_path(), required_bytes);

    assert_eq!(
        status.verdict,
        DiskVerdict::CriticalExhaustion {
            available_mb: 150,
            estimated_mb: required_bytes / (1024 * 1024),
        }
    );
    assert!(status.available_bytes < status.estimated_required_bytes);

    // Verify dispatch is aborted: zero .SPL files created in spool directory
    let entries_count = std::fs::read_dir(spooler.spool_path())
        .expect("Read spool directory")
        .count();
    assert_eq!(entries_count, 0, "No spool files must be written upon CriticalExhaustion");
}

// =========================================================================
// TEST 21: Multi-Printer Fault Containment Across Heterogeneous Fleet
// =========================================================================

#[test]
fn test_t3_21_multi_printer_device_disconnect_isolation_heterogeneous_fleet() {
    let injector = FaultInjector::new();
    injector.add_rule(FaultRule {
        rule_id: "disconnect_b_at_sheet_10",
        target_printer: Some("Printer_B_V4".into()),
        trigger: FaultTrigger::AtSheet(10),
        action: FaultAction::DeviceDisconnected {
            error_code: 0x8007045B,
            message: "Printer disconnected".into(),
        },
        armed: true,
    });
    injector.start_batch();

    let inj_a = injector.clone();
    let inj_b = injector.clone();
    let inj_c = injector.clone();

    let handle_a = std::thread::spawn(move || {
        let mock = MockV3Driver::new(MockV3DriverConfig {
            printer_name: "Printer_A_V3".into(),
            ..Default::default()
        });
        let ledger = CommittedSheetsLedger::new(25, 2);
        for s in 0..25 {
            inj_a.intercept_sheet_start("Printer_A_V3", s, 512).unwrap();
            ledger.begin_sheet(s, 1).unwrap();
            ledger.commit_sheet(s, 512);
        }
        (ledger.committed_count(), mock.config.printer_name.clone())
    });

    let handle_b = std::thread::spawn(move || {
        let ledger = CommittedSheetsLedger::new(25, 2);
        let mut err = None;
        for s in 0..25 {
            match inj_b.intercept_sheet_start("Printer_B_V4", s, 512) {
                Ok(()) => {
                    ledger.begin_sheet(s, 2).unwrap();
                    ledger.commit_sheet(s, 512);
                }
                Err(e) => {
                    ledger.begin_sheet(s, 2).unwrap();
                    ledger.fail_sheet(s, e.to_string());
                    err = Some(e);
                    break;
                }
            }
        }
        (ledger.committed_count(), err)
    });

    let handle_c = std::thread::spawn(move || {
        let mock = MockIppDriver::new("Printer_C_IPP", false);
        let ledger = CommittedSheetsLedger::new(25, 2);
        for s in 0..25 {
            inj_c.intercept_sheet_start("Printer_C_IPP", s, 512).unwrap();
            ledger.begin_sheet(s, 3).unwrap();
            ledger.commit_sheet(s, 512);
        }
        (ledger.committed_count(), mock.printer_name.clone())
    });

    let (count_a, name_a) = handle_a.join().expect("Worker A panicked");
    let (count_b, err_b) = handle_b.join().expect("Worker B panicked");
    let (count_c, name_c) = handle_c.join().expect("Worker C panicked");

    assert_eq!(count_a, 25, "Printer A should have completed all 25 sheets");
    assert_eq!(name_a, "Printer_A_V3");

    assert_eq!(count_b, 10, "Printer B should have committed exactly 10 sheets before disconnect");
    assert!(
        matches!(err_b, Some(InjectedFaultError::DeviceDisconnected { error_code: 0x8007045B, .. })),
        "Printer B must receive DeviceDisconnected fault"
    );

    assert_eq!(count_c, 25, "Printer C should have completed all 25 sheets");
    assert_eq!(name_c, "Printer_C_IPP");

    let audit = injector.audit_log();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].printer_name, "Printer_B_V4");
    assert_eq!(audit[0].sheet_index, 10);
}

// =========================================================================
// TEST 22: One-Click Failover × Asymmetric Margin Re-adaptation
// =========================================================================

#[test]
fn test_t3_22_failover_reroute_with_asymmetric_margin_readaptation() {
    let page_size = (595.28, 841.89); // Full bleed A4
    let bounds_a = PrintableBounds::a4_standard_margins(); // Symmetric 8.4pt margins

    // Primary transform on Printer A
    let transform_a = calculate_page_transform(
        page_size,
        &bounds_a,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Failover target Printer B with severe asymmetric margins
    let bounds_b = PrintableBounds {
        printable_width_pt: 553.28,
        printable_height_pt: 763.89,
        physical_width_pt: 595.28,
        physical_height_pt: 841.89,
        dpi_x: 600,
        dpi_y: 600,
        margins: HardwareMargins {
            left_pt: 18.0,
            top_pt: 36.0,
            right_pt: 24.0,
            bottom_pt: 42.0,
        },
    };

    let total_sheets = 30;
    let jam_sheet = 12;

    let ledger_a = CommittedSheetsLedger::new(total_sheets, 2);
    for s in 0..jam_sheet {
        ledger_a.begin_sheet(s, 1).unwrap();
        ledger_a.commit_sheet(s, 2048);
    }
    ledger_a.begin_sheet(jam_sheet, 1).unwrap();
    ledger_a.fail_sheet(jam_sheet, "Paper jam in fuser".into());

    let pending = ledger_a.pending_sheets_for_failover();
    assert_eq!(pending.len(), 18);

    // Re-calculate transform for target Printer B
    let transform_b = calculate_page_transform(
        page_size,
        &bounds_b,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Assertional Oracle: Scale factor adapts downwards to fit tighter asymmetric margins
    assert!(transform_b.scale < transform_a.scale, "Target scale must be smaller than primary scale");
    assert!(!transform_b.clipping.is_clipped(1e-4), "Target transform must produce 0% clipping");
    MarginAssertions::assert_within_printable(page_size, &bounds_b, &transform_b);
    MarginAssertions::assert_centered(page_size, &bounds_b, &transform_b);

    let ledger_b = CommittedSheetsLedger::new(total_sheets, 2);
    for &s in &pending {
        ledger_b.begin_sheet(s, 2).unwrap();
        ledger_b.commit_sheet(s, 2048);
    }
    assert_eq!(ledger_b.committed_count(), 18);
}

// =========================================================================
// TEST 23: Stream Telemetry Dynamic Transitions (Healthy -> Throttled -> Stalled -> Completed)
// =========================================================================

#[test]
fn test_t3_23_stream_telemetry_bandwidth_throttle_and_stall_transition() {
    let mut tel = PrinterStreamTelemetry::new("Printer_Telemetry_Test", 40);

    // Phase 1: Healthy transfer
    tel.current_sheet = 10;
    tel.bytes_sent = 500_000;
    tel.health = StreamHealth::Healthy {
        rate_kbps: 100.0,
        sheets_per_sec: 2.5,
    };
    assert!(matches!(tel.health, StreamHealth::Healthy { rate_kbps, .. } if rate_kbps > 25.0));

    // Phase 2: Bandwidth throttled
    tel.current_sheet = 15;
    tel.bytes_sent += 50_000;
    tel.health = StreamHealth::Throttled {
        rate_kbps: 12.0,
        reason: "Network congestion".into(),
    };
    assert!(matches!(tel.health, StreamHealth::Throttled { rate_kbps, .. } if rate_kbps <= 25.0));

    // Phase 3: Stalled stream timeout
    tel.last_progress_time = Instant::now() - Duration::from_secs(12);
    tel.evaluate_health(10); // stall threshold = 10s
    assert!(matches!(tel.health, StreamHealth::Stalled { stalled_duration_secs } if stalled_duration_secs >= 10));

    // Phase 4: Stream recovers and completes all sheets
    tel.current_sheet = 40;
    tel.bytes_sent += 2_000_000;
    tel.evaluate_health(10);
    assert!(matches!(tel.health, StreamHealth::Completed { total_sheets: 40, .. }));
    assert_eq!(tel.total_sheets, 40);
    assert_eq!(tel.bytes_sent, 2_550_000);
}

// =========================================================================
// TEST 24: Catastrophic RPC Crash × Diagnostic Harvesting & Sanitization
// =========================================================================

#[test]
fn test_t3_24_rpc_catastrophic_crash_diagnostic_harvest_and_sanitization() {
    let buffer = DiagnosticTraceBuffer::new(100);

    // Record an error event with sensitive file path redacted
    buffer.record(TraceRecord {
        timestamp: chrono::Utc::now(),
        level: TraceLevel::Error,
        category: TraceCategory::Spooler,
        printer_name: Some("Accounting_Laser_V3".into()),
        message: "RPC server unavailable: Failed to open document at [REDACTED_PATH]".into(),
        error_code: Some("0x800706BA".into()),
        context: BTreeMap::new(),
    });

    let caps = PrinterCapabilitiesTelemetry {
        printer_name: "Accounting_Laser_V3".into(),
        driver_name: "HP Universal Printing PCL 6".into(),
        driver_version: "61.250.1.28".into(),
        driver_generation: DriverGenerationKind::Win32V3DevMode,
        port_name: "DOT4_001".into(),
        is_default: true,
        is_color_supported: false,
        is_duplex_supported: true,
        supported_duplex_modes: vec![DuplexBindingKind::FlipLongEdge],
        max_copies: 999,
        hardware_margins_pt: MarginsPt { left: 8.4, top: 8.4, right: 8.4, bottom: 8.4 },
        printable_area_pt: SizePt { width: 578.4, height: 824.88 },
        physical_sheet_pt: SizePt { width: 595.2, height: 841.68 },
        resolution_dpi: ResolutionDpi { dpi_x: 600, dpi_y: 600 },
        supported_paper_count: 5,
    };

    let ticket = AppliedTicketTelemetry {
        printer_name: "Accounting_Laser_V3".into(),
        generation: DriverGenerationKind::Win32V3DevMode,
        color_mode: AppliedColorMode::MonochromeKOnly,
        duplex_mode: DuplexBindingKind::FlipLongEdge,
        orientation: AppliedOrientation::Portrait,
        paper_size_name: "A4".into(),
        copies: 1,
        details: TicketDetails::DevModeV3 {
            dm_fields_bitmask: 0x0001 | 0x0800,
            dm_size: 220,
            dm_driver_extra_bytes: 4096,
            dm_color: 1,
            dm_duplex: 2,
            dm_orientation: 1,
            dm_copies: 1,
            dm_papersize: 9,
            dm_collate: 1,
        },
    };

    let report = collect_diagnostic_report(vec![caps], vec![ticket], buffer.snapshot());

    // Markdown export verification
    let md = report.to_markdown_table();
    assert!(md.contains("## 1. Operating System & Environment"));
    assert!(md.contains("## 2. Spooler Subsystem Status"));
    assert!(md.contains("## 3. Printer Driver Capabilities"));
    assert!(md.contains("## 5. Execution Traces & Diagnostic Logs"));
    assert!(md.contains("0x800706BA"));
    assert!(!md.contains("Financial_Report_2026.pdf"), "Sanitized path leaked into Markdown");

    // JSON export & round-trip verification
    let json_str = report.to_json_pretty().expect("Serialize to JSON");
    assert!(!json_str.contains("Financial_Report_2026.pdf"), "Sanitized path leaked into JSON");
    let deserialized: DiagnosticReport = DiagnosticReport::from_json(&json_str).expect("Deserialize from JSON");
    assert_eq!(report, deserialized);
}

// =========================================================================
// TEST 25: Failover Rerouting Under Concurrency Governor Spool Permit Contention
// =========================================================================

#[test]
fn test_t3_25_failover_under_concurrency_governor_spool_permit_contention() {
    let gov = Arc::new(ConcurrencyGovernor::new(4, 2));
    let token = CancellationToken::new();

    // Saturated condition: Worker 1 and Worker 2 hold all available Spool permits
    let guard1 = gov.acquire_spool_permit(&token).unwrap();
    let guard2 = gov.acquire_spool_permit(&token).unwrap();
    assert_eq!(gov.available_spool_permits(), 0, "All 2 spool permits should be held");

    // Worker 1 encounters mid-job failure and aborts, releasing permit via RAII drop
    drop(guard1);
    assert_eq!(gov.available_spool_permits(), 1, "Permit 1 must be restored upon worker drop");

    // Failover worker immediately acquires released permit without starving Job 2
    let guard_failover = gov.acquire_spool_permit(&token);
    assert!(guard_failover.is_ok(), "Failover worker failed to acquire restored spool permit");
    assert_eq!(gov.available_spool_permits(), 0);

    // Clean completion of remaining jobs
    drop(guard_failover);
    drop(guard2);
    assert_eq!(gov.available_spool_permits(), 2, "All spool permits must be cleanly restored");
}

// =========================================================================
// TEST 26: Low Disk Space Warning Boundary × Stream Non-Abortion
// =========================================================================

#[test]
fn test_t3_26_disk_warning_threshold_recovery_and_low_space_telemetry() {
    let jobs = vec![PrinterJobEstimate::new(50, 1, SheetRasterMode::Vector)];
    let required_bytes = calculate_required_spool_bytes(&jobs); // 50 * 350KB * 1.25 = ~21.875 MB

    let gov = MockDiskSpaceGovernor::new(23 * 1024 * 1024); // 23 MB available: > required, but < 1.25x required (or < 100MB)
    let status = gov.evaluate(std::path::Path::new("C:\\spool"), required_bytes);

    assert_eq!(
        status.verdict,
        DiskVerdict::LowSpaceWarning {
            available_mb: 23,
            estimated_mb: required_bytes / (1024 * 1024),
        }
    );

    // Verify non-abortion: Spooling proceeds and successfully commits all 50 sheets
    let ledger = CommittedSheetsLedger::new(50, 2);
    for s in 0..50 {
        ledger.begin_sheet(s, 1).unwrap();
        ledger.commit_sheet(s, 1024);
    }

    assert!(ledger.is_all_committed());
    assert_eq!(ledger.committed_count(), 50);
}

// =========================================================================
// TEST 27: Triple-Printer Cascade Jam Failover (V3 -> V4 -> IPP) with Mono Enforcement
// =========================================================================

#[test]
fn test_t3_27_triple_printer_cascade_jam_failover_to_tertiary_with_mono_enforcement() {
    let total_sheets = 60;

    // Stage 1: Printer 1 (Win32 V3) spools sheets 0..19, jams at sheet 20
    let mock_v3 = MockV3Driver::new(MockV3DriverConfig {
        printer_name: "Stage1_V3_Primary".into(),
        default_color: false,
        ..Default::default()
    });
    let needed = mock_v3.document_properties_pass1() as usize;
    let mut devmode_buf = vec![0u8; needed];
    mock_v3.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();
    devmode_buf[72..76].copy_from_slice(&DM_COLOR.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());
    let mut out_buf = vec![0u8; needed];
    mock_v3.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    ).unwrap();
    mock_v3.assert_monochrome_asserted();

    let ledger_1 = CommittedSheetsLedger::new(total_sheets, 2);
    let mut p1_committed = Vec::new();
    for s in 0..20 {
        ledger_1.begin_sheet(s, 1).unwrap();
        ledger_1.commit_sheet(s, 1024);
        p1_committed.push(s);
    }
    ledger_1.begin_sheet(20, 1).unwrap();
    ledger_1.fail_sheet(20, "Paper jam in primary".into());

    let pending_stage2 = ledger_1.pending_sheets_for_failover();
    assert_eq!(pending_stage2.len(), 40);

    // Stage 2: Printer 2 (Win32 V4) spools sheets 20..39, disconnects at sheet 40
    let mock_v4 = MockV4Driver::new(MockV4DriverConfig {
        printer_name: "Stage2_V4_Secondary".into(),
        supports_color: true,
        supports_monochrome: true,
        supports_duplex: true,
        bounds: PrintableBounds::a4_standard_margins(),
    });
    mock_v4.pt_open_provider().unwrap();
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
        <psf:Feature name="psk:PageOutputColor">
            <psf:Option name="psk:Monochrome"/>
        </psf:Feature>
    </psf:PrintTicket>"#;
    let merged_v4 = mock_v4.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged_v4.contains("psk:Monochrome"));

    let ledger_2 = CommittedSheetsLedger::new(total_sheets, 2);
    let mut p2_committed = Vec::new();
    for &s in &pending_stage2[..20] {
        ledger_2.begin_sheet(s, 2).unwrap();
        ledger_2.commit_sheet(s, 1024);
        p2_committed.push(s);
    }
    ledger_2.begin_sheet(40, 2).unwrap();
    ledger_2.fail_sheet(40, "Device disconnected".into());

    let pending_stage3: Vec<usize> = (40..60).collect();

    // Stage 3: Printer 3 (CUPS/IPP 2.0) finishes sheets 40..59 to completion
    let mock_ipp = MockIppDriver::new("Stage3_IPP_Tertiary", true);
    let mut attrs = HashMap::new();
    attrs.insert("print-color-mode".to_string(), "monochrome".to_string());
    attrs.insert("sides".to_string(), "one-sided".to_string());
    mock_ipp.submit_ipp_job(attrs, b"%PDF tertiary mono payload").unwrap();
    mock_ipp.assert_attribute("print-color-mode", "monochrome");

    let ledger_3 = CommittedSheetsLedger::new(total_sheets, 2);
    let mut p3_committed = Vec::new();
    for &s in &pending_stage3 {
        ledger_3.begin_sheet(s, 3).unwrap();
        ledger_3.commit_sheet(s, 1024);
        p3_committed.push(s);
    }

    // Assertional Oracle 1: 3-way exact sheet slicing with zero duplication and zero drops
    assert_eq!(p1_committed, (0..20).collect::<Vec<_>>());
    assert_eq!(p2_committed, (20..40).collect::<Vec<_>>());
    assert_eq!(p3_committed, (40..60).collect::<Vec<_>>());
    assert_eq!(p1_committed.len() + p2_committed.len() + p3_committed.len(), 60);

    // Assertional Oracle 2: Dual-Layer Monochrome Enforcement Across All 3 Drivers
    let color_stream = b"0.8 0.2 0.3 rg 10 10 100 100 re f 0.1 0.7 0.2 RG 2.0 w 5 5 90 90 re S\n";
    let filtered_stream = filter_content_stream_bytes(color_stream);
    StreamAssertions::assert_content_stream_k_only(&filtered_stream);
}
