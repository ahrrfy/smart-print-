use pdfcraft_e2e::benchmarks::{
    BenchmarkAssertions, MemoryProbe, UiFluidityMonitor,
};
use pdfcraft_e2e::fault_injector::{
    FaultAction, FaultAssertions, FaultInjector, FaultRule, FaultTrigger, InjectedFaultError,
};
use pdfcraft_e2e::margin_assertions::{
    calculate_page_transform, MarginAssertions, PageFitMode,
};
use pdfcraft_e2e::mock_driver::{
    create_mock_virtual_pdf_printer, DevModeValidationError, DuplexBinding, HardwareMargins,
    MockIppDriver, MockV3Driver, MockV3DriverConfig, PrintableBounds, DMCOLOR_MONOCHROME,
    DMDUP_VERTICAL, DM_COLOR, DM_DUPLEX, DM_IN_BUFFER, DM_ORIENTATION, DM_OUT_BUFFER,
    IPP_STATUS_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
};
use pdfcraft_e2e::mock_spooler::{
    CommittedSheetsLedger, DiskVerdict, MockSpooler,
};
use pdfcraft_e2e::stream_inspector::{
    DefaultStreamInspector, PdfStreamInspector, StreamViolation,
};
use std::collections::HashMap;
use std::time::Duration;

#[test]
fn test_v3_devmode_2pass_negotiation_and_flag_injection() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());

    // Pass 1: Application queries required buffer size
    let needed = mock.document_properties_pass1();
    assert_eq!(needed, 220 + 3704);

    let mut devmode_buf = vec![0u8; needed as usize];

    // Populate defaults from driver into output buffer
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Failed to populate default DEVMODE");

    // Canary extra data preserved
    let mut out_buf = vec![0u8; needed as usize];

    // Mutate DEVMODE: assert DM_COLOR, DM_DUPLEX, DM_ORIENTATION
    let dm_fields = DM_COLOR | DM_DUPLEX | DM_ORIENTATION;
    devmode_buf[72..76].copy_from_slice(&dm_fields.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_VERTICAL.to_le_bytes());

    // Pass 2: Submit back to driver
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Pass 2 DocumentProperties failed");

    // Assertions
    mock.assert_pass1_called();
    mock.assert_monochrome_asserted();
    mock.assert_duplex_asserted(DuplexBinding::FlipLongEdge);
    mock.assert_driver_extra_preserved();
}

#[test]
fn test_v3_devmode_undersized_buffer_rejected() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let mut undersized = vec![0u8; 100]; // Less than 220 + 3704

    let result = mock.document_properties_pass2(None, &mut undersized, DM_OUT_BUFFER);
    assert!(matches!(
        result,
        Err(DevModeValidationError::BufferTooSmall { .. })
    ));
}

#[test]
fn test_v3_devmode_corrupted_vendor_extra_rejected() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.required_buffer_size();
    let mut devmode_buf = vec![0u8; needed];

    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .unwrap();

    // Corrupt canary byte in vendor extra payload
    devmode_buf[220 + 10] ^= 0xFF;

    let mut out_buf = vec![0u8; needed];
    let result = mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );
    assert!(matches!(
        result,
        Err(DevModeValidationError::CorruptedVendorExtraPayload { .. })
    ));
}

#[test]
fn test_v4_xps_printticket_merge_and_conflict_resolution() {
    // Virtual PDF printer lacks monochrome option
    let mock = create_mock_virtual_pdf_printer("Microsoft_Print_To_PDF");
    mock.pt_open_provider().unwrap();

    let caps_xml = mock.pt_get_print_capabilities().unwrap();
    assert!(caps_xml.contains("psk:Color"));
    assert!(!caps_xml.contains("psk:Monochrome"));

    let delta_xml = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework"
                                         xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
        <psf:Feature name="psk:PageOutputColor"><psf:Option name="psk:Monochrome"/></psf:Feature>
    </psf:PrintTicket>"#;

    let merged_xml = mock.pt_merge_and_validate(None, delta_xml).unwrap();
    assert!(merged_xml.contains("psk:Color"));

    let conflicts = mock.conflicts();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].feature, "psk:PageOutputColor");
    assert_eq!(conflicts[0].requested_option, "psk:Monochrome");
    assert_eq!(conflicts[0].resolved_option, "psk:Color");

    mock.assert_no_concurrency_violations();
}

#[test]
fn test_cups_ipp_driver_submission_and_attributes() {
    let mock = MockIppDriver::new("Office_Laser_IPP", true);

    let mut attrs = HashMap::new();
    attrs.insert("sides".into(), "two-sided-long-edge".into());
    attrs.insert("print-color-mode".into(), "monochrome".into());
    attrs.insert("media".into(), "iso_a4_210x297mm".into());

    let job_id = mock.submit_ipp_job(attrs, &[0x25, 0x50, 0x44, 0x46]).unwrap();
    assert!(job_id >= 1001);

    mock.assert_attribute("sides", "two-sided-long-edge");
    mock.assert_attribute("print-color-mode", "monochrome");
    mock.assert_attribute("media", "iso_a4_210x297mm");

    // Simplex printer rejects duplex attribute
    let simplex_mock = MockIppDriver::new("Receipt_Printer", false);
    let mut duplex_attrs = HashMap::new();
    duplex_attrs.insert("sides".into(), "two-sided-long-edge".into());

    let err = simplex_mock.submit_ipp_job(duplex_attrs, &[]);
    assert_eq!(
        err,
        Err(IPP_STATUS_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED)
    );
}

#[test]
fn test_spooler_disk_space_governor_evaluations() {
    let spooler = MockSpooler::new();
    let estimated_bytes = 100 * 1024 * 1024; // 100 MB

    // Sufficient space
    spooler.disk_governor.set_sufficient();
    let status_ok = spooler
        .disk_governor
        .evaluate(&spooler.spool_path(), estimated_bytes);
    assert_eq!(status_ok.verdict, DiskVerdict::Sufficient);

    // Low space warning
    spooler.disk_governor.set_low_space(estimated_bytes);
    let status_low = spooler
        .disk_governor
        .evaluate(&spooler.spool_path(), estimated_bytes);
    assert!(matches!(status_low.verdict, DiskVerdict::LowSpaceWarning { .. }));

    // Critical exhaustion
    spooler.disk_governor.set_critical_exhaustion(estimated_bytes);
    let status_crit = spooler
        .disk_governor
        .evaluate(&spooler.spool_path(), estimated_bytes);
    assert!(matches!(
        status_crit.verdict,
        DiskVerdict::CriticalExhaustion { .. }
    ));
}

#[test]
fn test_streaming_backpressure_and_failover_slicing() {
    let spooler = MockSpooler::new();
    let total_sheets = 50;
    let max_in_flight = 2;

    let ledger = CommittedSheetsLedger::new(total_sheets, max_in_flight);

    // Inject paper jam at sheet 20
    spooler.inject_failure_at_sheet(20);

    let result = spooler.spool_sheet_stream(201, &ledger, 1, |_idx| {
        Some(vec![0xAA; 512]) // 512B synthetic sheet
    });

    assert!(result.is_err());
    assert_eq!(ledger.committed_count(), 20); // Sheets 0..19 committed
    assert!(ledger.peak_in_flight_observed() <= max_in_flight);

    // Slice pending sheets for failover
    let pending = ledger.pending_sheets_for_failover();
    assert_eq!(pending.len(), 30); // Sheets 20..49
    assert_eq!(pending[0], 20);
    assert_eq!(*pending.last().unwrap(), 49);

    // Backup printer takes over pending sheets
    let backup_spooler = MockSpooler::new();
    let backup_ledger = CommittedSheetsLedger::new(pending.len(), max_in_flight);

    let backup_result = backup_spooler.spool_sheet_stream(202, &backup_ledger, 2, |_| {
        Some(vec![0xBB; 512])
    });

    assert!(backup_result.is_ok());
    assert!(backup_ledger.is_all_committed());
}

#[test]
fn test_stream_inspector_k_only_and_violations() {
    let inspector = DefaultStreamInspector::new();

    // Pure K-only stream (0 0 0 0.8 k and 0.5 g)
    let clean_stream = b"q 0.5 g 10 10 100 100 re f 0 0 0 0.8 k 20 20 50 50 re f Q";
    let violations = inspector.inspect_content_stream(clean_stream, 0);
    assert!(violations.is_empty());

    // Forbidden RGB operator
    let rgb_stream = b"q 0.2 0.4 0.8 rg 10 10 100 100 re f Q";
    let violations_rgb = inspector.inspect_content_stream(rgb_stream, 0);
    assert_eq!(violations_rgb.len(), 1);
    assert!(matches!(
        violations_rgb[0],
        StreamViolation::RgbColorOperator { .. }
    ));

    // Chromatic CMYK operator (non-zero cyan)
    let cmyk_stream = b"q 0.5 0.0 0.0 0.2 k 10 10 100 100 re f Q";
    let violations_cmyk = inspector.inspect_content_stream(cmyk_stream, 0);
    assert_eq!(violations_cmyk.len(), 1);
    assert!(matches!(
        violations_cmyk[0],
        StreamViolation::CmykChromaticColor { .. }
    ));

    // Forbidden ColorSpace
    let cs_stream = b"/DeviceRGB cs 1 0 0 sc";
    let violations_cs = inspector.inspect_content_stream(cs_stream, 0);
    assert_eq!(violations_cs.len(), 2); // Once for /DeviceRGB token, once for cs
}

#[test]
fn test_margin_assertions_fit_shrink_actual() {
    let bounds = PrintableBounds::a4_standard_margins(); // 578.40 x 824.88 printable out of 595.20 x 841.68 physical
    let page_size = (595.28, 841.89); // Full A4 bleed

    // 1. Fit to Printable Area
    let transform_fit = calculate_page_transform(
        page_size,
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert!(transform_fit.scale < 1.0);
    MarginAssertions::assert_within_printable(page_size, &bounds, &transform_fit);
    MarginAssertions::assert_centered(page_size, &bounds, &transform_fit);

    // 2. Shrink Oversized
    let transform_shrink = calculate_page_transform(
        page_size,
        &bounds,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(transform_shrink.scale, transform_fit.scale);

    // Smaller page not scaled under Shrink Oversized
    let small_page = (400.0, 600.0);
    let transform_small = calculate_page_transform(
        small_page,
        &bounds,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(transform_small.scale, 1.0);

    // 3. Actual Size (detects clipping)
    let transform_actual = calculate_page_transform(
        page_size,
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(transform_actual.scale, 1.0);
    assert!(transform_actual.clipping.is_clipped(1e-4));
}

#[test]
fn test_fault_injector_rules_and_audit() {
    let injector = FaultInjector::new();
    injector.start_batch();

    injector.add_rule(FaultRule {
        rule_id: "RULE_JAM_SHEET_5",
        target_printer: Some("Printer_A".into()),
        trigger: FaultTrigger::AtSheet(5),
        action: FaultAction::PaperJam {
            reason: "Tray 2 feed error".into(),
        },
        armed: true,
    });

    // Sheets 0..4 pass
    for sheet in 0..5 {
        let res = injector.intercept_sheet_start("Printer_A", sheet, 1024);
        assert!(res.is_ok());
    }

    // Sheet 5 jams
    let res5 = injector.intercept_sheet_start("Printer_A", 5, 1024);
    assert!(matches!(res5, Err(InjectedFaultError::PaperJam { sheet: 5, .. })));

    // Printer B unaffected
    let res_b = injector.intercept_sheet_start("Printer_B", 5, 1024);
    assert!(res_b.is_ok());

    // Verify audit log
    let audit = injector.audit_log();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].printer_name, "Printer_A");
    assert_eq!(audit[0].sheet_index, 5);

    // Assert failover sheets
    let failed_sheets: Vec<usize> = (0..5).collect();
    let failover_sheets: Vec<usize> = (5..20).collect();
    FaultAssertions::assert_failover_exact_sheets(&failed_sheets, &failover_sheets, 20, 5);
}

#[test]
fn test_benchmarks_memory_probe_and_ui_fluidity() {
    let probe = MemoryProbe::start();
    let mut data = Vec::new();
    for _ in 0..10 {
        data.push(vec![0u8; 100_000]);
        std::thread::sleep(Duration::from_millis(5));
    }
    let (initial, peak, delta) = probe.stop();
    assert!(peak >= initial);
    BenchmarkAssertions::assert_peak_memory_delta(delta, 100.0);

    // UI Fluidity Monitor
    let mut monitor = UiFluidityMonitor::new(60.0);
    for _ in 0..60 {
        monitor.record_frame_duration(Duration::from_millis(8)); // 8ms per frame (<16.6ms budget)
    }
    let report = monitor.finish();
    assert_eq!(report.total_frames, 60);
    assert_eq!(report.dropped_frames, 0);
    assert_eq!(report.freeze_events, 0);
    BenchmarkAssertions::assert_ui_fluidity_60fps(&report);
}

#[test]
fn test_fault_injector_concurrent_non_blocking_during_stall() {
    let injector = FaultInjector::new();
    injector.start_batch();

    // Rule: Printer A stalls for 150ms on sheet 0
    injector.add_rule(FaultRule {
        rule_id: "RULE_STALL_PRINTER_A",
        target_printer: Some("Printer_A".into()),
        trigger: FaultTrigger::AtSheet(0),
        action: FaultAction::StallTimeout {
            duration: Duration::from_millis(150),
        },
        armed: true,
    });

    let inj_a = injector.clone();
    let handle_a = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let res = inj_a.intercept_sheet_start("Printer_A", 0, 1024);
        (res, start.elapsed())
    });

    // Give thread A a moment to enter intercept_sheet_start and begin sleeping
    std::thread::sleep(Duration::from_millis(20));

    // Printer B must NOT be blocked by Printer A's sleep
    let start_b = std::time::Instant::now();
    let res_b = injector.intercept_sheet_start("Printer_B", 0, 1024);
    let elapsed_b = start_b.elapsed();

    assert!(res_b.is_ok(), "Printer B should succeed unaffected");
    assert!(
        elapsed_b < Duration::from_millis(100),
        "Printer B was blocked by Printer A's StallTimeout sleep! Elapsed: {:?}",
        elapsed_b
    );

    let (res_a, elapsed_a) = handle_a.join().unwrap();
    assert!(matches!(res_a, Err(InjectedFaultError::StallTimeout(_))));
    assert!(elapsed_a >= Duration::from_millis(140));
}

#[test]
fn test_stream_inspector_image_xobject_whitespace_parsing() {
    let inspector = DefaultStreamInspector::new();

    // Test standard PDF dictionary with whitespace separation: /Width 800 /Height 600
    let dict_standard = "<< /Type /XObject /Subtype /Image /Width 800 /Height 600 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";

    // Legitimate grayscale image (800 * 600 * 1 = 480,000 bytes)
    let gray_pixels = vec![128u8; 800 * 600];
    let res_valid = inspector.inspect_image_xobject("Im1", dict_standard, &gray_pixels, 0);
    assert!(res_valid.is_ok());

    // Disguised RGB image (800 * 600 * 3 bytes) under /DeviceGray
    let rgb_disguised_pixels = vec![128u8; 800 * 600 * 3];
    let res_disguised = inspector.inspect_image_xobject("Im2", dict_standard, &rgb_disguised_pixels, 0);
    assert!(matches!(
        res_disguised,
        Err(StreamViolation::InvalidImageXObject {
            channels_detected: 3,
            ..
        })
    ));

    // Compact token format: /Width800 /Height600
    let dict_compact = "<< /Type /XObject /Subtype /Image /Width800 /Height600 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let res_compact_disguised = inspector.inspect_image_xobject("Im3", dict_compact, &rgb_disguised_pixels, 0);
    assert!(matches!(
        res_compact_disguised,
        Err(StreamViolation::InvalidImageXObject {
            channels_detected: 3,
            ..
        })
    ));
}

#[test]
fn test_margin_assertions_asymmetric_margins_and_simplex() {
    // Asymmetric margins: Left 10, Top 20, Right 30, Bottom 40
    let bounds = PrintableBounds {
        printable_width_pt: 560.0,
        printable_height_pt: 740.0,
        physical_width_pt: 600.0,
        physical_height_pt: 800.0,
        dpi_x: 600,
        dpi_y: 600,
        margins: HardwareMargins {
            left_pt: 10.0,
            top_pt: 20.0,
            right_pt: 30.0,
            bottom_pt: 40.0,
        },
    };

    // Page size (580.0, 760.0) in ActualSize
    // Physical centering:
    // sheet_x0 = (600 - 580) / 2 = 10.0
    // sheet_y0 = (800 - 760) / 2 = 20.0
    // Origin in GDI printable space = sheet_x0 - left_margin = 10.0 - 10.0 = 0.0
    // Origin Y in GDI printable space = sheet_y0 - top_margin = 20.0 - 20.0 = 0.0
    let page_size = (580.0, 760.0);
    let transform = calculate_page_transform(
        page_size,
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!((transform.dx_pt - 0.0).abs() <= 1e-4);
    assert!((transform.dy_pt - 0.0).abs() <= 1e-4);
    // Right clipping: (sheet_x0 + w_page) - (w_phys - right_margin) = (10 + 580) - (600 - 30) = 590 - 570 = 20.0
    // Bottom clipping: (sheet_y0 + h_page) - (h_phys - bottom_margin) = (20 + 760) - (800 - 40) = 780 - 760 = 20.0
    assert!((transform.clipping.left_pt - 0.0).abs() <= 1e-4);
    assert!((transform.clipping.top_pt - 0.0).abs() <= 1e-4);
    assert!((transform.clipping.right_pt - 20.0).abs() <= 1e-4);
    assert!((transform.clipping.bottom_pt - 20.0).abs() <= 1e-4);

    // Test simplex back-page does NOT swap dimensions or rotate
    let landscape_page = (800.0, 600.0);
    let transform_simplex_back = calculate_page_transform(
        landscape_page,
        &bounds,
        PageFitMode::FitPrintableArea,
        true, // is_duplex_back_page
        true, // master_is_portrait
        DuplexBinding::Simplex,
    );
    assert_eq!(transform_simplex_back.rotation_deg, 0);
    // w_print = 560, h_print = 740. Since dimensions were not swapped, scale = min(560/800, 740/600) = min(0.7, 1.2333) = 0.7
    assert!((transform_simplex_back.scale - (560.0 / 800.0)).abs() <= 1e-4);
}

#[test]
fn test_mock_spooler_backpressure_no_permit_leak_on_violation() {
    let ledger = CommittedSheetsLedger::new(5, 1);

    // First sheet takes the only permit
    assert!(ledger.begin_sheet(0, 1).is_ok());
    assert_eq!(ledger.current_in_flight(), 1);

    // Second sheet exceeds backpressure (max 1) -> fails
    let err = ledger.begin_sheet(1, 2);
    assert!(err.is_err());
    // Critical check: current_in_flight must be decremented back to 1, not leaked to 2!
    assert_eq!(ledger.current_in_flight(), 1);

    // Commit sheet 0 -> in_flight returns to 0
    ledger.commit_sheet(0, 512);
    assert_eq!(ledger.current_in_flight(), 0);

    // Now sheet 1 can begin successfully because permit was not leaked!
    assert!(ledger.begin_sheet(1, 2).is_ok());
    assert_eq!(ledger.current_in_flight(), 1);
    ledger.commit_sheet(1, 512);
    assert_eq!(ledger.current_in_flight(), 0);
}
