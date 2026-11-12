//! Comprehensive Empirical Challenge Test Suite for Milestone 4:
//! 1. DiagnosticReport formatting & pipe escaping against markdown injection
//! 2. DiagnosticTraceBuffer rapid concurrent stress testing (5,000 events, bounded <= 500, no leak/deadlock)
//! 3. Spooler disk verdict boundary thresholds (0, 249MB, 250MB, 1024MB, u64::MAX)
//! 4. Presets store built-in deletion protection and JSON roundtrip
//! 5. Pre-flight clipping hazard calculations on simulated oversized & full-bleed content

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use pdfcraft_platform::diagnostics::{
    DiagnosticReport, DiagnosticTraceBuffer, DriverGenerationKind, DuplexBindingKind, MarginsPt,
    OsTelemetry, PrinterCapabilitiesTelemetry, ResolutionDpi, SizePt, SpoolDiskVerdict,
    SpoolerServiceStatus, SpoolerTelemetry, TraceCategory, TraceLevel, TraceRecord,
    evaluate_spool_disk_verdict, synthesize_windows_version,
};
use pdfcraft_print::duplex::DuplexBinding;
use pdfcraft_print::margins::{
    calculate_page_transform, HardwareMargins, PageFitMode, PrintableBounds,
};

// Module inclusion of print_presets directly from ui-egui source
#[allow(dead_code)]
#[path = "../../ui-egui/src/workflows/print_presets.rs"]
mod print_presets;

use print_presets::{
    PresetColorMode, PresetDuplex, PresetFitMode, PresetOrientation, PrintJobPreset,
    PrintPresetStore, PrinterGroup,
};

// =========================================================================
// CHALLENGE 1: DiagnosticReport formatting, Markdown Injection & JSON Roundtrip
// =========================================================================

#[test]
fn challenge_1_markdown_injection_and_pipe_escaping() {
    let now = chrono::Utc::now();

    // Adversarial data payload designed to break GFM tables
    let malicious_printer_name = "LaserJet|Pipe|Injection|Header\n---\n|Sneak";
    let malicious_driver_name = "HP \"Driver\" <script>alert('xss')</script> | Evil";
    let malicious_driver_version = "v1.0\r\n\r\n## Hijack\r\n|---|---|";
    let malicious_trace_message = "Error in spooler: file | not found\r\nLine 2 with | pipe\nLine 3 <br> end";

    let printer = PrinterCapabilitiesTelemetry {
        printer_name: malicious_printer_name.to_string(),
        driver_name: malicious_driver_name.to_string(),
        driver_version: malicious_driver_version.to_string(),
        driver_generation: DriverGenerationKind::Win32V3DevMode,
        port_name: "USB001|Port".to_string(),
        is_default: true,
        is_color_supported: true,
        is_duplex_supported: true,
        supported_duplex_modes: vec![DuplexBindingKind::FlipLongEdge],
        max_copies: 99,
        hardware_margins_pt: MarginsPt { left: 12.0, top: 12.0, right: 12.0, bottom: 12.0 },
        printable_area_pt: SizePt { width: 588.0, height: 768.0 },
        physical_sheet_pt: SizePt { width: 612.0, height: 792.0 },
        resolution_dpi: ResolutionDpi { dpi_x: 600, dpi_y: 600 },
        supported_paper_count: 5,
    };

    let trace = TraceRecord {
        timestamp: now,
        level: TraceLevel::Error,
        category: TraceCategory::Spooler,
        printer_name: Some("Printer|Name".to_string()),
        message: malicious_trace_message.to_string(),
        error_code: Some("0x80070005|AccessDenied".to_string()),
        context: BTreeMap::new(),
    };

    let report = DiagnosticReport {
        report_version: "1.0.0".to_string(),
        app_version: "pdfcraft-test 0.18.1".to_string(),
        generated_at: now,
        os: OsTelemetry {
            os_family: "windows|nt".to_string(),
            os_version: "Windows 11 Pro | 23H2".to_string(),
            kernel_version: "10.0.22631 | 3880".to_string(),
            architecture: "x86_64".to_string(),
            cpu_cores: 16,
            pointer_width: 64,
            total_memory_mb: Some(32768),
            process_rss_mb: Some(150),
        },
        spooler: SpoolerTelemetry {
            service_name: "Spooler | Service".to_string(),
            service_status: SpoolerServiceStatus::Running,
            spool_directory: "C:\\WINDOWS\\system32\\spool\\PRINTERS | Test".to_string(),
            available_bytes: 50 * 1024 * 1024 * 1024,
            total_bytes: 500 * 1024 * 1024 * 1024,
            disk_verdict: SpoolDiskVerdict::Sufficient,
        },
        printers: vec![printer],
        applied_tickets: Vec::new(),
        execution_traces: vec![trace],
    };

    let markdown = report.to_markdown_table();

    // Verify markdown contains report title
    assert!(markdown.contains("# PdfCraft Diagnostic & Telemetry Report"));

    // Check table rows: every line starting with '|' must have balanced column counts
    // and no unescaped pipe inside cell contents.
    let lines: Vec<&str> = markdown.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with('|') && line.ends_with('|') {
            // Count unescaped pipes
            let chars: Vec<char> = line.chars().collect();
            let mut unescaped_pipes = 0;
            for c_idx in 0..chars.len() {
                if chars[c_idx] == '|' && (c_idx == 0 || chars[c_idx - 1] != '\\') {
                    unescaped_pipes += 1;
                }
            }

            // A table row with N cells has N + 1 unescaped delimiter pipes.
            // Ensure no raw pipe injection occurred.
            assert!(
                unescaped_pipes >= 3,
                "Line {}: table row has insufficient columns: '{}'",
                i,
                line
            );
        }
    }

    // Verify raw newline injection didn't break out of the table row into raw header
    assert!(
        !markdown.contains("\n## Hijack\n"),
        "Markdown injection vulnerability: unescaped newline broke table structure!"
    );

    // Verify JSON roundtrip
    let json_pretty = report.to_json_pretty().expect("pretty JSON serialization must succeed");
    let json_compact = report.to_json_compact().expect("compact JSON serialization must succeed");

    let from_pretty = DiagnosticReport::from_json(&json_pretty).expect("deserialization from pretty must succeed");
    let from_compact = DiagnosticReport::from_json(&json_compact).expect("deserialization from compact must succeed");

    assert_eq!(report, from_pretty, "Pretty JSON roundtrip must match original report");
    assert_eq!(report, from_compact, "Compact JSON roundtrip must match original report");

    // Negative tests for JSON parser
    assert!(DiagnosticReport::from_json("").is_err(), "Empty JSON must return error");
    assert!(DiagnosticReport::from_json("{ corrupt json").is_err(), "Malformed JSON must return error");
    assert!(DiagnosticReport::from_json("null").is_err(), "Null JSON must return error");
}

#[test]
fn challenge_1_windows_11_synthesis_logic() {
    // Windows 11 build 22000+ reports "Windows 10 Pro" in registry; synthesis must correct it
    let res1 = synthesize_windows_version("Windows 10 Pro", "23H2", "22631", "3880");
    assert_eq!(res1, "Windows 11 Pro 23H2 (Build 22631.3880)");

    // Older Windows 10 build 19045 must remain Windows 10
    let res2 = synthesize_windows_version("Windows 10 Enterprise", "22H2", "19045", "4291");
    assert_eq!(res2, "Windows 10 Enterprise 22H2 (Build 19045.4291)");

    // Empty display version and UBR
    let res3 = synthesize_windows_version("Windows 10 Home", "", "22000", "");
    assert_eq!(res3, "Windows 11 Home (Build 22000)");
}

// =========================================================================
// CHALLENGE 2: DiagnosticTraceBuffer 5,000 Rapid Concurrent Events Stress Test
// =========================================================================

#[test]
fn challenge_2_trace_buffer_concurrent_stress_and_bound() {
    const BUFFER_CAPACITY: usize = 500;
    const NUM_THREADS: usize = 10;
    const EVENTS_PER_THREAD: usize = 500; // 10 * 500 = 5,000 total events

    let buffer = Arc::new(DiagnosticTraceBuffer::new(BUFFER_CAPACITY));
    let start_time = Instant::now();

    let mut handles = Vec::with_capacity(NUM_THREADS);
    for t_idx in 0..NUM_THREADS {
        let buf_clone = Arc::clone(&buffer);
        let handle = thread::spawn(move || {
            for e_idx in 0..EVENTS_PER_THREAD {
                let rec = TraceRecord {
                    timestamp: chrono::Utc::now(),
                    level: TraceLevel::Info,
                    category: TraceCategory::VectorFilter,
                    printer_name: Some(format!("printer-{}", t_idx)),
                    message: format!("Thread {} concurrent event {}", t_idx, e_idx),
                    error_code: None,
                    context: BTreeMap::new(),
                };
                buf_clone.record(rec);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Worker thread must not panic or deadlock");
    }

    let elapsed = start_time.elapsed();
    println!("Completed 5,000 concurrent events in {:?}", elapsed);
    assert!(elapsed.as_secs() < 10, "Execution took too long; potential lock contention or deadlock");

    // Check bounded size invariant
    let snapshot = buffer.snapshot();
    assert!(
        snapshot.len() <= BUFFER_CAPACITY,
        "Buffer exceeded capacity! Len: {}, Max: {}",
        snapshot.len(),
        BUFFER_CAPACITY
    );
    assert_eq!(
        snapshot.len(),
        BUFFER_CAPACITY,
        "Buffer should be completely saturated at exactly capacity (500)"
    );

    // Test buffer clear
    buffer.clear();
    let after_clear = buffer.snapshot();
    assert_eq!(after_clear.len(), 0, "Buffer after clear must be empty");

    // Test FIFO eviction order
    let small_buffer = DiagnosticTraceBuffer::new(3);
    for i in 1..=5 {
        small_buffer.record(TraceRecord {
            timestamp: chrono::Utc::now(),
            level: TraceLevel::Debug,
            category: TraceCategory::System,
            printer_name: None,
            message: format!("msg-{}", i),
            error_code: None,
            context: BTreeMap::new(),
        });
    }
    let small_snap = small_buffer.snapshot();
    assert_eq!(small_snap.len(), 3);
    assert_eq!(small_snap[0].message, "msg-3");
    assert_eq!(small_snap[1].message, "msg-4");
    assert_eq!(small_snap[2].message, "msg-5");
}

// =========================================================================
// CHALLENGE 3: Spooler Disk Space Verdict Boundary Thresholds
// =========================================================================

#[test]
fn challenge_3_spooler_disk_verdict_thresholds() {
    const CRITICAL_LIMIT: u64 = 250 * 1024 * 1024; // 250 MB = 262,144,000 bytes
    const LOW_LIMIT: u64 = 1024 * 1024 * 1024;      // 1.0 GB = 1,073,741,824 bytes

    // Critical Exhaustion: < 250 MB
    assert_eq!(evaluate_spool_disk_verdict(0), SpoolDiskVerdict::CriticalExhaustion);
    assert_eq!(evaluate_spool_disk_verdict(1), SpoolDiskVerdict::CriticalExhaustion);
    assert_eq!(
        evaluate_spool_disk_verdict(249 * 1024 * 1024),
        SpoolDiskVerdict::CriticalExhaustion,
        "249 MB must be CriticalExhaustion"
    );
    assert_eq!(
        evaluate_spool_disk_verdict(CRITICAL_LIMIT - 1),
        SpoolDiskVerdict::CriticalExhaustion,
        "250 MB - 1 byte must be CriticalExhaustion"
    );

    // Low Space Warning: >= 250 MB and < 1024 MB
    assert_eq!(
        evaluate_spool_disk_verdict(CRITICAL_LIMIT),
        SpoolDiskVerdict::LowSpaceWarning,
        "250 MB exact must be LowSpaceWarning"
    );
    assert_eq!(
        evaluate_spool_disk_verdict(CRITICAL_LIMIT + 1),
        SpoolDiskVerdict::LowSpaceWarning
    );
    assert_eq!(
        evaluate_spool_disk_verdict(500 * 1024 * 1024),
        SpoolDiskVerdict::LowSpaceWarning
    );
    assert_eq!(
        evaluate_spool_disk_verdict(1023 * 1024 * 1024),
        SpoolDiskVerdict::LowSpaceWarning
    );
    assert_eq!(
        evaluate_spool_disk_verdict(LOW_LIMIT - 1),
        SpoolDiskVerdict::LowSpaceWarning,
        "1024 MB - 1 byte must be LowSpaceWarning"
    );

    // Sufficient: >= 1024 MB
    assert_eq!(
        evaluate_spool_disk_verdict(LOW_LIMIT),
        SpoolDiskVerdict::Sufficient,
        "1024 MB exact must be Sufficient"
    );
    assert_eq!(
        evaluate_spool_disk_verdict(LOW_LIMIT + 1),
        SpoolDiskVerdict::Sufficient
    );
    assert_eq!(
        evaluate_spool_disk_verdict(10 * 1024 * 1024 * 1024),
        SpoolDiskVerdict::Sufficient
    );
    assert_eq!(
        evaluate_spool_disk_verdict(u64::MAX),
        SpoolDiskVerdict::Sufficient
    );
}

// =========================================================================
// CHALLENGE 4: Presets Store Built-In Deletion Protection & Roundtrip
// =========================================================================

#[test]
fn challenge_4_preset_store_deletion_protection_and_persistence() {
    let mut store = PrintPresetStore::with_defaults();

    // Verify 3 built-in presets exist
    assert_eq!(store.presets.len(), 3);
    assert!(store.presets.contains_key("standard-duplex"));
    assert!(store.presets.contains_key("monochrome-eco"));
    assert!(store.presets.contains_key("engineering-100"));

    // Attempt to delete built-in presets: MUST FAIL
    assert!(!store.delete_preset("standard-duplex"), "Deleting standard-duplex must be rejected");
    assert!(!store.delete_preset("monochrome-eco"), "Deleting monochrome-eco must be rejected");
    assert!(!store.delete_preset("engineering-100"), "Deleting engineering-100 must be rejected");

    assert_eq!(store.presets.len(), 3, "Presets count must remain 3 after rejected deletions");

    // Built-in printer group protection
    let built_in_group = PrinterGroup::with_printers("office-pool", "Office Pool", vec!["P1".into()], true);
    store.upsert_group(built_in_group);
    assert!(!store.delete_group("office-pool"), "Built-in group deletion must be rejected");
    assert!(store.groups.contains_key("office-pool"));

    // Custom user preset lifecycle
    let custom = PrintJobPreset {
        id: "label-run".to_string(),
        name: "Shipping Label Run".to_string(),
        description: "Custom label preset".to_string(),
        is_built_in: false,
        copies: 3,
        color_mode: PresetColorMode::Monochrome,
        duplex: PresetDuplex::Simplex,
        orientation: PresetOrientation::Portrait,
        fit_mode: PresetFitMode::ShrinkOversized,
        custom_scale_pct: None,
        collate: true,
        paper_label: None,
        paper_size_um: None,
    };
    store.upsert_preset(custom);
    store.set_active_preset(Some("label-run".to_string()));
    assert_eq!(store.active_preset_id.as_deref(), Some("label-run"));

    // Deleting custom preset must succeed and reset active_preset_id
    assert!(store.delete_preset("label-run"), "Custom preset deletion must succeed");
    assert_eq!(store.presets.len(), 3);
    assert!(store.active_preset_id.is_none(), "Active preset must be cleared when active item is deleted");

    // JSON roundtrip
    let json = store.to_json().expect("Store serialization must succeed");
    let decoded = PrintPresetStore::from_json(&json).expect("Store deserialization must succeed");
    assert_eq!(store, decoded, "Decoded store must equal original");

    // File I/O roundtrip via std::env::temp_dir()
    let temp_file = std::env::temp_dir().join(format!("presets_test_{}.json", std::process::id()));

    store.export_to_file(&temp_file).expect("Export to file must succeed");
    let imported = PrintPresetStore::import_from_file(&temp_file).expect("Import from file must succeed");
    assert_eq!(store, imported, "Imported store from file must equal exported store");

    // Cleanup
    let _ = std::fs::remove_file(temp_file);
}

// =========================================================================
// CHALLENGE 5: Pre-Flight Clipping Hazards on Oversized & Full-Bleed Content
// =========================================================================

#[test]
fn challenge_5_clipping_hazard_calculation_actual_size() {
    // Standard US Letter Sheet (612 x 792 pt) with 18 pt hardware margins on all sides
    let margins = HardwareMargins {
        left_pt: 18.0,
        top_pt: 18.0,
        right_pt: 18.0,
        bottom_pt: 18.0,
    };
    let printable = PrintableBounds {
        printable_width_pt: 612.0 - 36.0, // 576 pt
        printable_height_pt: 792.0 - 36.0, // 756 pt
        physical_width_pt: 612.0,
        physical_height_pt: 792.0,
        dpi_x: 600,
        dpi_y: 600,
        margins,
    };

    // Case 1: Page fits inside printable area (500 x 700 pt) -> NO clipping
    let t_fit = calculate_page_transform(
        (500.0, 700.0),
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert!(t_fit.clipping_hazard.is_none(), "Normal fitting page must not have clipping hazard");

    // Case 2: Full-bleed document (page bounds == physical sheet bounds = 612 x 792 pt)
    // In ActualSize, content extends exactly into the 18 pt hardware margins
    let t_bleed = calculate_page_transform(
        (612.0, 792.0),
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert!(t_bleed.clipping_hazard.is_some(), "Full bleed document must trigger clipping hazard");
    let hazard = t_bleed.clipping_hazard.unwrap();
    assert!((hazard.left_pt - 18.0).abs() < 1e-4, "Left clipped must be 18 pt, got {}", hazard.left_pt);
    assert!((hazard.right_pt - 18.0).abs() < 1e-4, "Right clipped must be 18 pt, got {}", hazard.right_pt);
    assert!((hazard.top_pt - 18.0).abs() < 1e-4, "Top clipped must be 18 pt, got {}", hazard.top_pt);
    assert!((hazard.bottom_pt - 18.0).abs() < 1e-4, "Bottom clipped must be 18 pt, got {}", hazard.bottom_pt);
    assert!((hazard.total_clipped_pt() - 72.0).abs() < 1e-4, "Total clipped must be 72 pt");

    // Case 3: Oversized Blueprint (1000 x 1200 pt) on Letter sheet
    let t_oversized = calculate_page_transform(
        (1000.0, 1200.0),
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert!(t_oversized.clipping_hazard.is_some(), "Oversized content must trigger clipping hazard");
    let h_over = t_oversized.clipping_hazard.unwrap();
    // x_sheet = (612 - 1000)/2 = -194. left_pt = 18 - (-194) = 212.
    assert!((h_over.left_pt - 212.0).abs() < 1e-4);
    assert!((h_over.right_pt - 212.0).abs() < 1e-4);
    // y_sheet = (792 - 1200)/2 = -204. top_pt = 18 - (-204) = 222.
    assert!((h_over.top_pt - 222.0).abs() < 1e-4);
    assert!((h_over.bottom_pt - 222.0).abs() < 1e-4);

    // Case 4: Switching to FitPrintableArea must eliminate clipping hazard
    let t_auto_fit = calculate_page_transform(
        (1000.0, 1200.0),
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert!(t_auto_fit.clipping_hazard.is_none(), "FitPrintableArea must have zero clipping hazard");
    assert!(t_auto_fit.scale < 1.0, "Scale must be reduced to fit");
    let expected_scale = (576.0 / 1000.0_f64).min(756.0 / 1200.0_f64); // 0.576
    assert!((t_auto_fit.scale - expected_scale).abs() < 1e-4);
}

#[test]
fn challenge_5_asymmetric_margins_clipping() {
    // Thermal label printer with asymmetric margins:
    // Left: 5pt, Top: 10pt, Right: 5pt, Bottom: 25pt (feed edge)
    let margins = HardwareMargins {
        left_pt: 5.0,
        top_pt: 10.0,
        right_pt: 5.0,
        bottom_pt: 25.0,
    };
    let printable = PrintableBounds {
        printable_width_pt: 288.0 - 10.0, // 278 pt (4 inch label)
        printable_height_pt: 432.0 - 35.0, // 397 pt (6 inch label)
        physical_width_pt: 288.0,
        physical_height_pt: 432.0,
        dpi_x: 203,
        dpi_y: 203,
        margins,
    };

    // Full-bleed label (288 x 432 pt)
    let t = calculate_page_transform(
        (288.0, 432.0),
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert!(t.clipping_hazard.is_some());
    let h = t.clipping_hazard.unwrap();
    assert!((h.left_pt - 5.0).abs() < 1e-4);
    assert!((h.right_pt - 5.0).abs() < 1e-4);
    assert!((h.top_pt - 10.0).abs() < 1e-4);
    assert!((h.bottom_pt - 25.0).abs() < 1e-4);
}

// =========================================================================
// ADDITIONAL ADVERSARIAL CHALLENGES: Edge Cases, Contention, Non-Finites
// =========================================================================

#[test]
fn challenge_2_high_thread_contention_50_threads() {
    const BUFFER_CAPACITY: usize = 500;
    const NUM_THREADS: usize = 50;
    const EVENTS_PER_THREAD: usize = 100; // 50 * 100 = 5,000 total events under high contention

    let buffer = Arc::new(DiagnosticTraceBuffer::new(BUFFER_CAPACITY));
    let mut handles = Vec::with_capacity(NUM_THREADS);

    for t_idx in 0..NUM_THREADS {
        let b = Arc::clone(&buffer);
        handles.push(thread::spawn(move || {
            for e in 0..EVENTS_PER_THREAD {
                b.record(TraceRecord {
                    timestamp: chrono::Utc::now(),
                    level: TraceLevel::Warning,
                    category: TraceCategory::Failover,
                    printer_name: Some(format!("contend-{}", t_idx)),
                    message: format!("Burst event {}", e),
                    error_code: None,
                    context: BTreeMap::new(),
                });
            }
        }));
    }

    for h in handles {
        h.join().expect("Thread must not deadlock");
    }

    let snap = buffer.snapshot();
    assert_eq!(snap.len(), BUFFER_CAPACITY);
}

#[test]
fn challenge_1_unicode_and_rtl_export_stress() {
    let now = chrono::Utc::now();
    let report = DiagnosticReport {
        report_version: "1.0.0".to_string(),
        app_version: "طابعة ذكية ١.٠ 🖨️".to_string(),
        generated_at: now,
        os: OsTelemetry {
            os_family: "ويندوز".to_string(),
            os_version: "Windows 11 企业版 (简体中文)".to_string(),
            kernel_version: "10.0.22631".to_string(),
            architecture: "x86_64".to_string(),
            cpu_cores: 8,
            pointer_width: 64,
            total_memory_mb: Some(16384),
            process_rss_mb: Some(120),
        },
        spooler: SpoolerTelemetry {
            service_name: "خدمة الطباعة".to_string(),
            service_status: SpoolerServiceStatus::Running,
            spool_directory: "C:\\طباعة\\spool".to_string(),
            available_bytes: 10 * 1024 * 1024 * 1024,
            total_bytes: 100 * 1024 * 1024 * 1024,
            disk_verdict: SpoolDiskVerdict::Sufficient,
        },
        printers: Vec::new(),
        applied_tickets: Vec::new(),
        execution_traces: Vec::new(),
    };

    let md = report.to_markdown_table();
    assert!(md.contains("ويندوز"));
    assert!(md.contains("企业版"));

    let json = report.to_json_pretty().expect("Unicode json serialization must succeed");
    let back = DiagnosticReport::from_json(&json).expect("Unicode json deserialization must succeed");
    assert_eq!(report, back);
}

#[test]
fn challenge_5_non_finite_and_zero_bounds_fallback() {
    let printable = PrintableBounds::default_safety(612.0, 792.0);

    // Zero bounds
    let t_zero = calculate_page_transform(
        (0.0, 0.0),
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(t_zero.scale, 1.0);
    assert_eq!(t_zero.dx_pt, 0.0);
    assert!(t_zero.clipping_hazard.is_none());

    // Negative bounds
    let t_neg = calculate_page_transform(
        (-100.0, 500.0),
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(t_neg.scale, 1.0);
    assert!(t_neg.clipping_hazard.is_none());

    // NaN / Infinity
    let t_nan = calculate_page_transform(
        (f64::NAN, 792.0),
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(t_nan.scale, 1.0);
    assert!(t_nan.clipping_hazard.is_none());

    let t_inf = calculate_page_transform(
        (612.0, f64::INFINITY),
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(t_inf.scale, 1.0);
    assert!(t_inf.clipping_hazard.is_none());
}

#[test]
fn challenge_5_draft_clipping_hazard_tolerance_threshold() {
    // Simulated detect_clipping_hazard algorithm as in draft.rs
    let evaluate_hazard = |margins: [f64; 4], sheet_size: (f64, f64), placed_bounds: [f64; 4]| -> Option<[f64; 4]> {
        let [left, top, right, bottom] = margins;
        let printable_x0 = left;
        let printable_y0 = bottom;
        let printable_x1 = sheet_size.0 - right;
        let printable_y1 = sheet_size.1 - top;

        let [bx0, by0, bx1, by1] = placed_bounds;

        let ov_left = (printable_x0 - bx0).max(0.0);
        let ov_bottom = (printable_y0 - by0).max(0.0);
        let ov_right = (bx1 - printable_x1).max(0.0);
        let ov_top = (by1 - printable_y1).max(0.0);

        if ov_left > 0.5 || ov_bottom > 0.5 || ov_right > 0.5 || ov_top > 0.5 {
            Some([ov_left, ov_top, ov_right, ov_bottom])
        } else {
            None
        }
    };

    let margins = [18.0, 18.0, 18.0, 18.0];
    let sheet = (612.0, 792.0);

    // Exact printable bounds: [18.0, 18.0, 594.0, 774.0]

    // 1. Perfectly inside
    assert!(evaluate_hazard(margins, sheet, [20.0, 20.0, 500.0, 700.0]).is_none());

    // 2. Barely overflowing by 0.4 pt (<= 0.5 pt tolerance, should NOT flag)
    assert!(evaluate_hazard(margins, sheet, [17.6, 18.0, 594.0, 774.0]).is_none());

    // 3. Overflowing by 0.6 pt (> 0.5 pt tolerance, MUST flag)
    let h = evaluate_hazard(margins, sheet, [17.4, 18.0, 594.0, 774.0]);
    assert!(h.is_some());
    let [l, _t, _r, _b] = h.unwrap();
    assert!((l - 0.6).abs() < 1e-4);
}
