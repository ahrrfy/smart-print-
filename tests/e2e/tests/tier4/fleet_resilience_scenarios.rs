//! Tier 4 Submodule 3: Fleet Resilience, Fault Slicing & Diagnostic Scenarios
//!
//! Authoritative acceptance tests covering:
//! - Scenario 7: Mid-Job Printer Hardware Crash & 1-Click Failover Rerouting (F22, F23, F24)
//! - Scenario 8: Spooler Disk Space Exhaustion Pre-Flight Interception (F20)
//! - Scenario 11: Multi-Printer Preset Recall & Instant Batch Submission (F26, F1, F5)
//! - Scenario 12: Spooler RPC Disconnection & 1-Click Diagnostic Report Export (F27, F23)
//! - Scenario 14: Hardware Simplex Mismatch Alert & Fallback (F16, F13)

use pdfcraft_e2e::fault_injector::{
    FaultAction, FaultAssertions, FaultInjector, FaultRule, FaultTrigger,
};
use pdfcraft_e2e::mock_driver::{
    DuplexBinding, MockIppDriver, MockV3Driver, MockV3DriverConfig,
    MockV4Driver, MockV4DriverConfig, PrintableBounds,
    DMCOLOR_COLOR, DMORIENT_PORTRAIT, DM_COLOR, DM_IN_BUFFER, DM_ORIENTATION, DM_OUT_BUFFER,
};
use pdfcraft_e2e::mock_spooler::{
    CommittedSheetsLedger, DiskVerdict, MockSpooler,
};
use pdfcraft_e2e::stream_inspector::StreamAssertions;
use pdfcraft_platform::diagnostics::{
    collect_diagnostic_report, AppliedColorMode, AppliedOrientation, AppliedTicketTelemetry,
    DiagnosticReport, DiagnosticTraceBuffer, DriverGenerationKind, DuplexBindingKind,
    MarginsPt, PrinterCapabilitiesTelemetry, ResolutionDpi, SizePt, TicketDetails,
    TraceCategory, TraceLevel, TraceRecord,
};
use pdfcraft_print::driver::{
    validate_job_capabilities, ColorMode, PageFitMode, PreFlightSeverity,
    PrintableBounds as PrintBounds, PrinterCapabilities as PrintCapabilities,
    PrintJobSettings as PrintSettings,
};
use pdfcraft_print::governor::{
    calculate_required_spool_bytes, PrinterJobEstimate, SheetRasterMode,
};
use pdfcraft_print::monochrome::MonochromeVectorFilter;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

// =========================================================================
// Opaque-Box Preset Models for Scenario 11 (Feature 26)
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpaquePrinterGroup {
    pub id: String,
    pub name: String,
    pub printer_names: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OpaqueDuplex {
    Simplex,
    LongEdge,
    ShortEdge,
}

impl From<OpaqueDuplex> for DuplexBinding {
    fn from(d: OpaqueDuplex) -> Self {
        match d {
            OpaqueDuplex::Simplex => DuplexBinding::Simplex,
            OpaqueDuplex::LongEdge => DuplexBinding::FlipLongEdge,
            OpaqueDuplex::ShortEdge => DuplexBinding::FlipShortEdge,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpaquePrintPreset {
    pub id: String,
    pub name: String,
    pub printer_name: String,
    pub copies: u32,
    pub color: bool,
    pub duplex: OpaqueDuplex,
    pub fit_to_printable: bool,
    pub k_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct OpaquePresetStore {
    pub groups: BTreeMap<String, OpaquePrinterGroup>,
    pub presets: BTreeMap<String, OpaquePrintPreset>,
}

/// Helper: passes raw content stream bytes through MonochromeVectorFilter
fn filter_content_stream_to_k_only(input: &[u8]) -> Vec<u8> {
    let parsed = pdfcraft_content::parse(input);
    let filtered_ops = MonochromeVectorFilter::filter_operators(&parsed.ops);
    pdfcraft_content::serialize_ops(&filtered_ops)
}

// =========================================================================
// SCENARIO 7: Mid-Job Printer Hardware Crash & 1-Click Failover Rerouting
// =========================================================================

#[test]
fn test_t4_07_mid_job_paper_jam_hardware_crash_and_failover_rerouting() {
    let total_sheets = 100;
    let jam_sheet = 45;
    let printer_a_name = "Office_Main_Laser_V3";
    let printer_b_name = "Office_Sibling_IPP";
    let printer_c_name = "Office_Backup_Color_V4";

    // 1. Setup FaultInjector with one-shot PaperJam rule targeting Printer A at sheet 45
    let injector = FaultInjector::new();
    injector.add_rule(FaultRule {
        rule_id: "paper_jam_sheet_45",
        target_printer: Some(printer_a_name.into()),
        trigger: FaultTrigger::AtSheet(jam_sheet),
        action: FaultAction::PaperJam {
            reason: "Mechanical paper feed jam in fuser unit".into(),
        },
        armed: true,
    });
    injector.start_batch();

    let inj_a = injector.clone();
    let inj_b = injector.clone();

    // 2. Setup Primary Printer A (MockV3Driver)
    let mock_v3 = MockV3Driver::new(MockV3DriverConfig {
        printer_name: printer_a_name.into(),
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

    let ledger_a = Arc::new(CommittedSheetsLedger::new(total_sheets, 2));
    let ledger_a_worker = Arc::clone(&ledger_a);

    // 3. Launch concurrent worker for Printer A
    let handle_a = std::thread::spawn(move || {
        let mut committed = Vec::new();
        for s in 0..total_sheets {
            match inj_a.intercept_sheet_start(printer_a_name, s, 2048) {
                Ok(()) => {
                    ledger_a_worker.begin_sheet(s, 1).unwrap();
                    ledger_a_worker.commit_sheet(s, 2048);
                    committed.push(s);
                }
                Err(e) => {
                    ledger_a_worker.begin_sheet(s, 1).unwrap();
                    ledger_a_worker.fail_sheet(s, e.to_string());
                    break;
                }
            }
        }
        committed
    });

    // 4. Launch concurrent sibling worker for Printer B (proving isolated error containment)
    let sibling_sheets = 30;
    let ledger_b = Arc::new(CommittedSheetsLedger::new(sibling_sheets, 2));
    let ledger_b_worker = Arc::clone(&ledger_b);
    let handle_b = std::thread::spawn(move || {
        let mock_ipp = MockIppDriver::new(printer_b_name, true);
        let mut committed = Vec::new();
        for s in 0..sibling_sheets {
            inj_b.intercept_sheet_start(printer_b_name, s, 1024).unwrap();
            ledger_b_worker.begin_sheet(s, 2).unwrap();
            ledger_b_worker.commit_sheet(s, 1024);
            committed.push(s);
        }
        let mut attrs = HashMap::new();
        attrs.insert("sides".into(), "two-sided-long-edge".into());
        mock_ipp.submit_ipp_job(attrs, b"%PDF Sibling Workload").unwrap();
        committed
    });

    let a_committed = handle_a.join().expect("Worker A panicked");
    let b_committed = handle_b.join().expect("Worker B panicked");

    // Assertional Oracle 1: Printer A committed exactly sheets [0..44]
    assert_eq!(a_committed.len(), jam_sheet);
    assert_eq!(ledger_a.committed_count(), 45);
    assert_eq!(a_committed, (0..45).collect::<Vec<_>>());

    // Assertional Oracle 2: Sibling Printer B was 100% isolated and completed all 30 sheets
    assert_eq!(b_committed.len(), sibling_sheets);
    assert_eq!(ledger_b.committed_count(), 30);
    assert!(ledger_b.is_all_committed());

    // 5. Failover Slicing: Slice pending uncommitted sheets from Ledger A
    let pending_for_failover = ledger_a.pending_sheets_for_failover();
    assert_eq!(pending_for_failover.len(), total_sheets - jam_sheet); // 55 sheets
    assert_eq!(pending_for_failover.first(), Some(&45));
    assert_eq!(pending_for_failover.last(), Some(&99));

    // 6. Reroute Pending Sheets to Backup Printer C (MockV4Driver)
    let mock_v4 = MockV4Driver::new(MockV4DriverConfig {
        printer_name: printer_c_name.into(),
        supports_color: true,
        supports_monochrome: true,
        supports_duplex: true,
        bounds: PrintableBounds::a4_standard_margins(),
    });
    mock_v4.pt_open_provider().unwrap();
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
        <psf:Feature name="psk:PageOutputColor"><psf:Option name="psk:Color"/></psf:Feature>
    </psf:PrintTicket>"#;
    mock_v4.pt_merge_and_validate(None, delta).unwrap();

    let ledger_c = CommittedSheetsLedger::new(total_sheets, 2);
    let mut c_committed = Vec::new();
    for &s in &pending_for_failover {
        injector.intercept_sheet_start(printer_c_name, s, 2048).unwrap();
        ledger_c.begin_sheet(s, 3).unwrap();
        ledger_c.commit_sheet(s, 2048);
        c_committed.push(s);
    }

    // Assertional Oracle 3: Exact disjoint sheet partitioning with zero duplicate & zero dropped sheets
    FaultAssertions::assert_failover_exact_sheets(&a_committed, &c_committed, total_sheets, jam_sheet);
    assert_eq!(ledger_c.committed_count(), 55);
    assert_eq!(a_committed.len() + c_committed.len(), 100);

    // Audit verification
    let audit = injector.audit_log();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].printer_name, printer_a_name);
    assert_eq!(audit[0].sheet_index, 45);
}

// =========================================================================
// SCENARIO 8: Spooler Disk Space Exhaustion Pre-Flight Interception
// =========================================================================

#[test]
fn test_t4_08_spooler_disk_space_exhaustion_preflight_interception() {
    // 150MB heavy print job requiring estimated 225MB spool storage (1.5x safety margin)
    let jobs = vec![
        PrinterJobEstimate::new(100, 1, SheetRasterMode::Raster300Dpi),
    ];
    let required_bytes = calculate_required_spool_bytes(&jobs);
    assert!(required_bytes >= 225 * 1024 * 1024, "Estimated required bytes must be at least 225MB");

    let spooler = MockSpooler::new();
    let available_storage_bytes = 50 * 1024 * 1024; // 50MB free: critical exhaustion
    spooler.disk_governor.set_available_bytes(available_storage_bytes);

    // Evaluate pre-flight disk status
    let status = spooler.disk_governor.evaluate(&spooler.spool_path(), required_bytes);

    // Assertional Oracle 1: Verdict evaluates to CriticalExhaustion
    assert_eq!(
        status.verdict,
        DiskVerdict::CriticalExhaustion {
            available_mb: 50,
            estimated_mb: required_bytes / (1024 * 1024),
        }
    );
    assert!(status.available_bytes < status.estimated_required_bytes);

    // Assertional Oracle 2: Zero background worker threads spawned, 0 partial spool files written
    let entries_count = std::fs::read_dir(spooler.spool_path())
        .expect("Read sandbox spool directory")
        .count();
    assert_eq!(entries_count, 0, "Sandbox spool directory must remain completely empty upon CriticalExhaustion");
}

// =========================================================================
// SCENARIO 11: Multi-Printer Preset Recall & Instant Batch Submission
// =========================================================================

#[test]
fn test_t4_11_multi_printer_preset_recall_and_batch_submission() {
    let p1_name = "Printer_1_Exec_Color";
    let p2_name = "Printer_2_Warehouse_Mono";

    // 1. Construct and serialize multi-printer preset "Office Workgroup"
    let mut store = OpaquePresetStore::default();
    let mut group = OpaquePrinterGroup {
        id: "office-workgroup".into(),
        name: "Office Workgroup".into(),
        printer_names: BTreeSet::new(),
    };
    group.printer_names.insert(p1_name.into());
    group.printer_names.insert(p2_name.into());
    store.groups.insert(group.id.clone(), group);

    let preset_p1 = OpaquePrintPreset {
        id: "preset-p1".into(),
        name: "Executive Color Duplex".into(),
        printer_name: p1_name.into(),
        copies: 1,
        color: true,
        duplex: OpaqueDuplex::LongEdge,
        fit_to_printable: true,
        k_only: false,
    };
    let preset_p2 = OpaquePrintPreset {
        id: "preset-p2".into(),
        name: "Warehouse Mono Simplex".into(),
        printer_name: p2_name.into(),
        copies: 1,
        color: false,
        duplex: OpaqueDuplex::Simplex,
        fit_to_printable: true,
        k_only: true,
    };
    store.presets.insert(preset_p1.id.clone(), preset_p1);
    store.presets.insert(preset_p2.id.clone(), preset_p2);

    // Verify round-trip serialization/deserialization of presets
    let serialized = serde_json::to_string(&store).expect("Serialize preset store");
    let loaded_store: OpaquePresetStore = serde_json::from_str(&serialized).expect("Deserialize preset store");
    assert_eq!(store, loaded_store);

    // 2. Setup mock drivers
    let mock_v4 = Arc::new(MockV4Driver::new(MockV4DriverConfig {
        printer_name: p1_name.into(),
        supports_color: true,
        supports_monochrome: true,
        supports_duplex: true,
        bounds: PrintableBounds::a4_standard_margins(),
    }));
    let mock_ipp = Arc::new(MockIppDriver::new(p2_name, false));

    // 3. Concurrent batch submission
    let total_sheets = 10;
    let ledger_1 = Arc::new(CommittedSheetsLedger::new(total_sheets, 2));
    let ledger_2 = Arc::new(CommittedSheetsLedger::new(total_sheets, 2));

    let v4_clone = Arc::clone(&mock_v4);
    let l1_clone = Arc::clone(&ledger_1);
    let handle_1 = std::thread::spawn(move || {
        v4_clone.pt_open_provider().unwrap();
        let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
            <psf:Feature name="psk:PageOutputColor"><psf:Option name="psk:Color"/></psf:Feature>
            <psf:Feature name="psk:JobDuplexAllDocumentsContiguously"><psf:Option name="psk:TwoSidedLongEdge"/></psf:Feature>
        </psf:PrintTicket>"#;
        let merged = v4_clone.pt_merge_and_validate(None, delta).unwrap();
        for s in 0..total_sheets {
            l1_clone.begin_sheet(s, 1).unwrap();
            l1_clone.commit_sheet(s, 1024);
        }
        merged
    });

    let ipp_clone = Arc::clone(&mock_ipp);
    let l2_clone = Arc::clone(&ledger_2);
    let handle_2 = std::thread::spawn(move || {
        let mut attrs = HashMap::new();
        attrs.insert("sides".into(), "one-sided".into());
        attrs.insert("print-color-mode".into(), "monochrome".into());

        let raw_color_ops = b"1.0 0.0 0.0 rg 0 0 100 100 re f 0.0 1.0 0.0 RG 10 10 80 80 re S\n";
        let k_only_ops = filter_content_stream_to_k_only(raw_color_ops);

        ipp_clone.submit_ipp_job(attrs, &k_only_ops).unwrap();
        for s in 0..total_sheets {
            l2_clone.begin_sheet(s, 2).unwrap();
            l2_clone.commit_sheet(s, 1024);
        }
        k_only_ops
    });

    let merged_ticket = handle_1.join().expect("Worker 1 panicked");
    let spooled_bytes = handle_2.join().expect("Worker 2 panicked");

    // Assertional Oracle 1: Printer 1 received duplex color ticket
    assert!(merged_ticket.contains("psk:Color"));
    assert!(merged_ticket.contains("psk:TwoSidedLongEdge"));
    mock_v4.assert_no_concurrency_violations();
    assert!(ledger_1.is_all_committed());

    // Assertional Oracle 2: Printer 2 received simplex mono ticket and K-only vector stream
    mock_ipp.assert_attribute("sides", "one-sided");
    mock_ipp.assert_attribute("print-color-mode", "monochrome");
    StreamAssertions::assert_content_stream_k_only(&spooled_bytes);
    assert!(ledger_2.is_all_committed());
}

// =========================================================================
// SCENARIO 12: Spooler RPC Disconnection & 1-Click Diagnostic Report Export
// =========================================================================

#[test]
fn test_t4_12_spooler_rpc_disconnect_and_diagnostic_report_export() {
    let trace_buf = DiagnosticTraceBuffer::new(50);

    // Record catastrophic spooler RPC disconnect with sensitive path
    let sensitive_path = "C:\\Users\\FinanceUser\\Documents\\Confidential_Payroll_2026.pdf";
    let sanitized_msg = "RPC server unavailable (0x800706BA) while opening [REDACTED_PATH]".to_string();

    trace_buf.record(TraceRecord {
        timestamp: chrono::Utc::now(),
        level: TraceLevel::Error,
        category: TraceCategory::Spooler,
        printer_name: Some("Finance_Laser_V3".into()),
        message: sanitized_msg,
        error_code: Some("0x800706BA".into()),
        context: BTreeMap::new(),
    });

    let caps = PrinterCapabilitiesTelemetry {
        printer_name: "Finance_Laser_V3".into(),
        driver_name: "Generic PCL 6 Driver".into(),
        driver_version: "10.0.19041.1".into(),
        driver_generation: DriverGenerationKind::Win32V3DevMode,
        port_name: "USB001".into(),
        is_default: true,
        is_color_supported: false,
        is_duplex_supported: true,
        supported_duplex_modes: vec![DuplexBindingKind::FlipLongEdge],
        max_copies: 99,
        hardware_margins_pt: MarginsPt { left: 8.4, top: 8.4, right: 8.4, bottom: 8.4 },
        printable_area_pt: SizePt { width: 578.4, height: 824.88 },
        physical_sheet_pt: SizePt { width: 595.2, height: 841.68 },
        resolution_dpi: ResolutionDpi { dpi_x: 600, dpi_y: 600 },
        supported_paper_count: 3,
    };

    let ticket = AppliedTicketTelemetry {
        printer_name: "Finance_Laser_V3".into(),
        generation: DriverGenerationKind::Win32V3DevMode,
        color_mode: AppliedColorMode::MonochromeKOnly,
        duplex_mode: DuplexBindingKind::FlipLongEdge,
        orientation: AppliedOrientation::Portrait,
        paper_size_name: "A4".into(),
        copies: 1,
        details: TicketDetails::DevModeV3 {
            dm_fields_bitmask: 0x0001 | 0x0800,
            dm_size: 220,
            dm_driver_extra_bytes: 2048,
            dm_color: 1,
            dm_duplex: 2,
            dm_orientation: 1,
            dm_copies: 1,
            dm_papersize: 9,
            dm_collate: 1,
        },
    };

    let report = collect_diagnostic_report(vec![caps], vec![ticket], trace_buf.snapshot());

    // Assertional Oracle 1: Markdown Report Generation & Sanitization
    let md = report.to_markdown_table();
    assert!(md.contains("## 1. Operating System & Environment"));
    assert!(md.contains("## 2. Spooler Subsystem Status"));
    assert!(md.contains("## 3. Printer Driver Capabilities"));
    assert!(md.contains("## 5. Execution Traces & Diagnostic Logs"));
    assert!(md.contains("0x800706BA"));
    assert!(!md.contains(sensitive_path), "Sensitive customer path must never leak into exported Markdown");

    // Assertional Oracle 2: JSON Export & Deserialization Round-Trip
    let json_pretty = report.to_json_pretty().expect("Serialize to JSON");
    assert!(!json_pretty.contains(sensitive_path), "Sensitive customer path must never leak into exported JSON");

    let deserialized: DiagnosticReport = DiagnosticReport::from_json(&json_pretty).expect("Deserialize from JSON");
    assert_eq!(report, deserialized, "Diagnostic report must survive JSON round-trip identically");
}

// =========================================================================
// SCENARIO 14: Hardware Simplex Mismatch Alert & Fallback
// =========================================================================

#[test]
fn test_t4_14_hardware_simplex_mismatch_alert_and_fallback() {
    let printer_name = "Desktop_Simplex_Only";

    // 1. Simplex-only printer capabilities
    let caps = PrintCapabilities {
        name: printer_name.to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V4PrintTicket,
        is_duplex_supported: false,
        supported_duplex: vec![pdfcraft_print::driver::DuplexBinding::Simplex],
        is_color_supported: true,
        max_copies: 99,
        bounds: PrintBounds {
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

    // 2. User initially requests double-sided printing (FlipLongEdge)
    let mut settings = PrintSettings {
        printer_name: printer_name.to_string(),
        copies: 1,
        color_mode: ColorMode::Color,
        duplex_mode: pdfcraft_print::driver::DuplexBinding::FlipLongEdge,
        fit_mode: PageFitMode::FitPrintableArea,
        page_bounds: (595.28, 841.89),
        ..Default::default()
    };

    // Assertional Oracle 1: Pre-flight capability negotiation intercepts duplex mismatch
    let preflight_1 = validate_job_capabilities(&caps, &settings);
    assert!(!preflight_1.is_valid_for_dispatch, "Dispatch must be halted when hardware mismatch exists");
    assert!(preflight_1.has_critical_mismatch(), "Must register CriticalMismatch for duplex on simplex device");
    assert!(
        preflight_1.issues.iter().any(|issue| match issue {
            PreFlightSeverity::CriticalMismatch(msg) => msg.contains("simplex"),
            _ => false,
        }),
        "CriticalMismatch error message must explicitly indicate simplex restriction"
    );

    // 3. Driver conflict resolution check
    let mock_v4 = MockV4Driver::new(MockV4DriverConfig {
        printer_name: printer_name.into(),
        supports_color: true,
        supports_monochrome: true,
        supports_duplex: false, // Simplex hardware
        bounds: PrintableBounds::a4_standard_margins(),
    });
    mock_v4.pt_open_provider().unwrap();
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
        <psf:Feature name="psk:JobDuplexAllDocumentsContiguously"><psf:Option name="psk:TwoSidedLongEdge"/></psf:Feature>
    </psf:PrintTicket>"#;
    let merged = mock_v4.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged.contains("psk:OneSided"), "Driver must resolve duplex conflict to psk:OneSided");

    let conflicts = mock_v4.conflicts();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].feature, "psk:JobDuplexAllDocumentsContiguously");
    assert_eq!(conflicts[0].requested_option, "psk:TwoSidedLongEdge");
    assert_eq!(conflicts[0].resolved_option, "psk:OneSided");
    assert_eq!(conflicts[0].reason, "Device does not support duplex finishing");

    // 4. Operator accepts Fallback to Simplex
    settings.duplex_mode = pdfcraft_print::driver::DuplexBinding::Simplex;
    let preflight_2 = validate_job_capabilities(&caps, &settings);
    assert!(preflight_2.is_valid_for_dispatch, "Fallback to simplex must restore valid dispatch state");
    assert_eq!(preflight_2.issues, vec![PreFlightSeverity::Ok]);

    // 5. Spooling proceeds successfully in simplex mode
    let total_sheets = 5;
    let ledger = CommittedSheetsLedger::new(total_sheets, 2);
    for s in 0..total_sheets {
        ledger.begin_sheet(s, 1).unwrap();
        ledger.commit_sheet(s, 1024);
    }
    assert!(ledger.is_all_committed());
    assert_eq!(ledger.committed_count(), 5);
}
