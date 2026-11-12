use pdfcraft_e2e::benchmarks::{
    BenchmarkAssertions, MemoryProbe, UiFluidityMonitor,
};
use pdfcraft_e2e::fault_injector::{
    FaultAction, FaultAssertions, FaultInjector, FaultRule, FaultTrigger, InjectedFaultError,
};
use pdfcraft_e2e::margin_assertions::{
    calculate_page_transform, ClippingExtent, MarginAssertions, PageFitMode,
};
use pdfcraft_e2e::mock_driver::{
    DuplexBinding, HardwareMargins, PrintableBounds,
};
use pdfcraft_e2e::stream_inspector::{
    DefaultStreamInspector, PdfStreamInspector, StreamAssertions,
};
use std::time::Duration;

// =========================================================================
// 1. STREAM INSPECTOR CHALLENGES
// =========================================================================

#[test]
fn challenge_stream_inspector_cmyk_chromatic_subtle() {
    let inspector = DefaultStreamInspector::new();

    // 1a. Test subtle non-zero chromatic CMYK: c = 0.0001, m = 0, y = 0, k = 0.5
    // Note: Stream inspector uses `const EPSILON: f64 = 1e-4; if c.abs() > EPSILON`.
    // Let's test exactly 0.0001, 0.00010001, and 0.001.
    let subtle_stream = b"0.0001 0 0 0.5 k";
    let violations = inspector.inspect_content_stream(subtle_stream, 0);

    // Check behavior for 0.0001 vs 0.00010001:
    let subtle_above_eps = b"0.00010001 0 0 0.5 k";
    let violations_above = inspector.inspect_content_stream(subtle_above_eps, 0);
    assert_eq!(
        violations_above.len(),
        1,
        "Chromatic CMYK c=0.00010001 must be detected"
    );

    // Also check capital K operator with chromatic magenta
    let subtle_magenta = b"0 0.0005 0 0.2 K";
    let violations_m = inspector.inspect_content_stream(subtle_magenta, 0);
    assert_eq!(
        violations_m.len(),
        1,
        "Chromatic CMYK m=0.0005 with operator K must be detected"
    );

    // Log finding regarding exact 0.0001 threshold:
    println!(
        "Observation: c = 0.0001 yields {} violations (EPSILON is 1e-4 with strict > comparison)",
        violations.len()
    );
}

#[test]
fn challenge_stream_inspector_true_k_only_permitted() {
    let inspector = DefaultStreamInspector::new();

    // True K-only: c = 0, m = 0, y = 0, k = 0.8 (both lowercase k and uppercase K)
    let k_only_stream = b"q 0 0 0 0.8 k 10 10 100 100 re f 0 0 0 1.0 K 20 20 50 50 re S Q";
    let violations = inspector.inspect_content_stream(k_only_stream, 0);
    assert!(
        violations.is_empty(),
        "True K-only stream (c=0, m=0, y=0, k=0.8) must be permitted without violations, got: {:?}",
        violations
    );

    // Gray operators g and G must also be permitted
    let gray_stream = b"0.5 g 10 10 20 20 re f 0.2 G 30 30 40 40 re S";
    let violations_gray = inspector.inspect_content_stream(gray_stream, 0);
    assert!(
        violations_gray.is_empty(),
        "Gray operators g/G must be permitted"
    );

    // StreamAssertions::assert_content_stream_k_only must pass
    StreamAssertions::assert_content_stream_k_only(k_only_stream);
    StreamAssertions::assert_content_stream_k_only(gray_stream);
}

#[test]
fn challenge_stream_inspector_forbidden_cs_and_color_spaces() {
    let inspector = DefaultStreamInspector::new();

    // 1. /DeviceRGB cs
    let stream_rgb_cs = b"/DeviceRGB cs 1.0 0.0 0.0 sc";
    let v_rgb_cs = inspector.inspect_content_stream(stream_rgb_cs, 0);
    assert!(
        !v_rgb_cs.is_empty(),
        "/DeviceRGB cs must be flagged as forbidden"
    );

    // 2. /DeviceCMYK cs
    let stream_cmyk_cs = b"/DeviceCMYK cs 0.1 0.2 0.3 0.4 sc";
    let v_cmyk_cs = inspector.inspect_content_stream(stream_cmyk_cs, 0);
    assert!(
        !v_cmyk_cs.is_empty(),
        "/DeviceCMYK cs must be flagged as forbidden"
    );

    // 3. /DeviceRGB CS (uppercase stroking)
    let stream_rgb_caps_cs = b"/DeviceRGB CS 0.0 1.0 0.0 SC";
    let v_rgb_caps_cs = inspector.inspect_content_stream(stream_rgb_caps_cs, 0);
    assert!(
        !v_rgb_caps_cs.is_empty(),
        "/DeviceRGB CS must be flagged as forbidden"
    );

    // 4. /DeviceCMYK CS (uppercase stroking)
    let stream_cmyk_caps_cs = b"/DeviceCMYK CS 0.5 0.5 0.5 0.5 SC";
    let v_cmyk_caps_cs = inspector.inspect_content_stream(stream_cmyk_caps_cs, 0);
    assert!(
        !v_cmyk_caps_cs.is_empty(),
        "/DeviceCMYK CS must be flagged as forbidden"
    );
}

#[test]
fn challenge_stream_inspector_image_xobject_dictionary_formats() {
    let inspector = DefaultStreamInspector::new();

    // Format 1: Multi-line formatted dictionary with newlines and indentation
    let dict_multiline = "\
<<\n\
  /Type /XObject\n\
  /Subtype /Image\n\
  /Width 640\n\
  /Height 480\n\
  /ColorSpace /DeviceRGB\n\
  /BitsPerComponent 8\n\
>>";
    let res1 = inspector.inspect_image_xobject("Img1", dict_multiline, &[], 0);
    assert!(
        res1.is_err(),
        "Multi-line /DeviceRGB image dictionary must be rejected"
    );

    // Format 2: Array color space format [/DeviceRGB]
    let dict_array = "<< /Type /XObject /Subtype /Image /Width 100 /Height 100 /ColorSpace [/DeviceRGB] /BitsPerComponent 8 >>";
    let res2 = inspector.inspect_image_xobject("Img2", dict_array, &[], 0);
    assert!(
        res2.is_err(),
        "Array-wrapped [/DeviceRGB] image dictionary must be rejected"
    );

    // Format 3: Compact tokens without whitespace around slashes
    let dict_compact_rgb = "<< /Type/XObject /Subtype/Image /Width 200 /Height 200 /ColorSpace/DeviceRGB >>";
    let res3 = inspector.inspect_image_xobject("Img3", dict_compact_rgb, &[], 0);
    assert!(
        res3.is_err(),
        "Compact /ColorSpace/DeviceRGB must be rejected"
    );

    // Format 4: Valid grayscale image with /CalGray
    let dict_calgray = "<< /Type /XObject /Subtype /Image /Width 100 /Height 100 /ColorSpace /CalGray /BitsPerComponent 8 >>";
    let valid_pixels = vec![128u8; 100 * 100];
    let res4 = inspector.inspect_image_xobject("Img4", dict_calgray, &valid_pixels, 0);
    assert!(res4.is_ok(), "/CalGray grayscale image must be permitted");

    // Format 5: Valid grayscale image with /G abbreviation
    let dict_g = "<< /Type /XObject /Subtype /Image /Width 50 /Height 50 /ColorSpace /G /BitsPerComponent 8 >>";
    let valid_pixels_g = vec![200u8; 50 * 50];
    let res5 = inspector.inspect_image_xobject("Img5", dict_g, &valid_pixels_g, 0);
    assert!(res5.is_ok(), "/G grayscale image must be permitted");

    // Format 6: CMYK Image XObject
    let dict_cmyk = "<< /Type /XObject /Subtype /Image /Width 100 /Height 100 /ColorSpace /DeviceCMYK /BitsPerComponent 8 >>";
    let res6 = inspector.inspect_image_xobject("Img6", dict_cmyk, &[], 0);
    assert!(res6.is_err(), "/DeviceCMYK Image XObject must be rejected");
}

// =========================================================================
// 2. MARGIN ASSERTIONS CHALLENGES
// =========================================================================

#[test]
fn challenge_margin_assertions_pos_receipt_extreme_asymmetric() {
    // POS Receipt printer with extreme asymmetric margins:
    // Left 10.64pt, Right 11.35pt, Top 0.0pt, Bottom 0.35pt
    let pos_bounds = PrintableBounds {
        printable_width_pt: 178.01,
        printable_height_pt: 599.65,
        physical_width_pt: 200.0,
        physical_height_pt: 600.0,
        dpi_x: 203,
        dpi_y: 203,
        margins: HardwareMargins {
            left_pt: 10.64,
            top_pt: 0.0,
            right_pt: 11.35,
            bottom_pt: 0.35,
        },
    };

    let receipt_page = (190.0, 500.0);

    // 1. Fit to Printable Area
    let transform_fit = calculate_page_transform(
        receipt_page,
        &pos_bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    // Receipt page width (190) > printable width (178.01), so it must scale down:
    let expected_scale = 178.01 / 190.0;
    assert!(
        (transform_fit.scale - expected_scale).abs() <= MarginAssertions::TOLERANCE_PT,
        "FitPrintableArea scale mismatch: got {}, expected {}",
        transform_fit.scale,
        expected_scale
    );
    // Must be completely within printable bounds with 0 clipping
    MarginAssertions::assert_within_printable(receipt_page, &pos_bounds, &transform_fit);
    MarginAssertions::assert_centered(receipt_page, &pos_bounds, &transform_fit);

    // 2. Actual Size with clipping hazard calculation on POS printer
    let transform_actual = calculate_page_transform(
        receipt_page,
        &pos_bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(transform_actual.scale, 1.0);
    // Check clipping math:
    // Physical centering: sheet_x0 = (200 - 190) / 2 = 5.0
    // Left margin is 10.64, so content extends into left margin by 10.64 - 5.0 = 5.64 pt
    // Right: (sheet_x0 + w_page) = 5.0 + 190.0 = 195.0. Physical limit = 200.0 - 11.35 = 188.65.
    // Content extends into right margin by 195.0 - 188.65 = 6.35 pt
    // Top margin is 0.0. sheet_y0 = (600 - 500) / 2 = 50.0. No top clipping (0.0).
    // Bottom limit = 600.0 - 0.35 = 599.65. sheet_y0 + h_page = 550.0. No bottom clipping (0.0).
    let expected_clipping = ClippingExtent {
        left_pt: 5.64,
        top_pt: 0.0,
        right_pt: 6.35,
        bottom_pt: 0.0,
    };
    MarginAssertions::assert_clipping_hazard(&transform_actual.clipping, &expected_clipping);
}

#[test]
fn challenge_margin_assertions_borderless_photo_zero_margins() {
    // Borderless photo printer: 4x6 inch (288.0 x 432.0 pt) with 0pt margins
    let borderless_bounds = PrintableBounds {
        printable_width_pt: 288.0,
        printable_height_pt: 432.0,
        physical_width_pt: 288.0,
        physical_height_pt: 432.0,
        dpi_x: 1200,
        dpi_y: 1200,
        margins: HardwareMargins {
            left_pt: 0.0,
            top_pt: 0.0,
            right_pt: 0.0,
            bottom_pt: 0.0,
        },
    };

    let photo_page = (288.0, 432.0);

    // Actual Size on borderless printer must have 0.0 clipping
    let transform_actual = calculate_page_transform(
        photo_page,
        &borderless_bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(transform_actual.scale, 1.0);
    assert_eq!(transform_actual.dx_pt, 0.0);
    assert_eq!(transform_actual.dy_pt, 0.0);
    MarginAssertions::assert_within_printable(photo_page, &borderless_bounds, &transform_actual);
    assert!(!transform_actual.clipping.is_clipped(MarginAssertions::TOLERANCE_PT));
}

#[test]
fn challenge_margin_assertions_oversized_2000x3000_on_a4() {
    // Oversized document (2000 x 3000 pt) on A4 sheet
    let a4_bounds = PrintableBounds::a4_standard_margins(); // 578.40 x 824.88 printable

    let oversized_page = (2000.0, 3000.0);

    let transform = calculate_page_transform(
        oversized_page,
        &a4_bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Verify scale: min(578.40 / 2000.0, 824.88 / 3000.0) = min(0.2892, 0.27496) = 0.27496
    let expected_scale: f64 = f64::min(824.88 / 3000.0, 578.40 / 2000.0);
    assert!(
        (transform.scale - expected_scale).abs() <= MarginAssertions::TOLERANCE_PT,
        "Oversized scale mismatch"
    );

    // Must be completely inside printable bounds with zero clipping
    MarginAssertions::assert_within_printable(oversized_page, &a4_bounds, &transform);
    MarginAssertions::assert_centered(oversized_page, &a4_bounds, &transform);
}

#[test]
fn challenge_margin_assertions_clipping_tolerance_sub_micrometer() {
    // Test clipping calculation precision down to 10^-4 pt (0.035 micrometers)
    let bounds = PrintableBounds {
        printable_width_pt: 500.0,
        printable_height_pt: 500.0,
        physical_width_pt: 600.0,
        physical_height_pt: 600.0,
        dpi_x: 600,
        dpi_y: 600,
        margins: HardwareMargins {
            left_pt: 50.0,
            top_pt: 50.0,
            right_pt: 50.0,
            bottom_pt: 50.0,
        },
    };

    // Page that exceeds printable width by exactly 0.0002 pt (0.0001 pt on each side)
    let page_subtle = (500.0002, 500.0);
    let transform = calculate_page_transform(
        page_subtle,
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // sheet_x0 = (600 - 500.0002) / 2 = 49.9999
    // clip_left = 50.0 - 49.9999 = 0.0001 pt
    // clip_right = (49.9999 + 500.0002) - 550.0 = 550.0001 - 550.0 = 0.0001 pt
    let expected = ClippingExtent {
        left_pt: 0.0001,
        top_pt: 0.0,
        right_pt: 0.0001,
        bottom_pt: 0.0,
    };
    MarginAssertions::assert_clipping_hazard(&transform.clipping, &expected);
}

// =========================================================================
// 3. FAULT INJECTOR CHALLENGES
// =========================================================================

#[test]
fn challenge_fault_injector_concurrent_4_printers_with_failover() {
    let injector = FaultInjector::new();
    injector.start_batch();

    // Configure 4 distinct printer rules:
    // Printer 1: PaperJam at sheet 5
    injector.add_rule(FaultRule {
        rule_id: "RULE_JAM_P1",
        target_printer: Some("Printer_1".into()),
        trigger: FaultTrigger::AtSheet(5),
        action: FaultAction::PaperJam {
            reason: "Roller slip".into(),
        },
        armed: true,
    });

    // Printer 2: DeviceDisconnected at sheet 10
    injector.add_rule(FaultRule {
        rule_id: "RULE_DISC_P2",
        target_printer: Some("Printer_2".into()),
        trigger: FaultTrigger::AtSheet(10),
        action: FaultAction::DeviceDisconnected {
            error_code: 0x80004005,
            message: "USB I/O bus reset".into(),
        },
        armed: true,
    });

    // Printer 3: Throttle at 2048 kbps
    injector.add_rule(FaultRule {
        rule_id: "RULE_THROTTLE_P3",
        target_printer: Some("Printer_3".into()),
        trigger: FaultTrigger::AtSheet(3),
        action: FaultAction::Throttle { rate_kbps: 2048.0 },
        armed: true,
    });

    // Printer 4: Clean, no rules!

    let total_sheets = 20;

    // Spawn 4 concurrent threads simulating simultaneous dispatch
    let inj1 = injector.clone();
    let inj2 = injector.clone();
    let inj3 = injector.clone();
    let inj4 = injector.clone();

    let h1 = std::thread::spawn(move || {
        let mut committed = Vec::new();
        for s in 0..total_sheets {
            match inj1.intercept_sheet_start("Printer_1", s, 1024) {
                Ok(()) => committed.push(s),
                Err(InjectedFaultError::PaperJam { .. }) => break,
                Err(e) => panic!("Unexpected error on Printer 1: {:?}", e),
            }
        }
        committed
    });

    let h2 = std::thread::spawn(move || {
        let mut committed = Vec::new();
        for s in 0..total_sheets {
            match inj2.intercept_sheet_start("Printer_2", s, 1024) {
                Ok(()) => committed.push(s),
                Err(InjectedFaultError::DeviceDisconnected { .. }) => break,
                Err(e) => panic!("Unexpected error on Printer 2: {:?}", e),
            }
        }
        committed
    });

    let h3 = std::thread::spawn(move || {
        let mut committed = Vec::new();
        for s in 0..total_sheets {
            match inj3.intercept_sheet_start("Printer_3", s, 1024) {
                Ok(()) => committed.push(s),
                Err(e) => panic!("Unexpected error on Printer 3: {:?}", e),
            }
        }
        committed
    });

    let h4 = std::thread::spawn(move || {
        let mut committed = Vec::new();
        for s in 0..total_sheets {
            match inj4.intercept_sheet_start("Printer_4", s, 1024) {
                Ok(()) => committed.push(s),
                Err(e) => panic!("Unexpected error on Printer 4: {:?}", e),
            }
        }
        committed
    });

    let p1_committed = h1.join().unwrap();
    let p2_committed = h2.join().unwrap();
    let p3_committed = h3.join().unwrap();
    let p4_committed = h4.join().unwrap();

    // Assertions on independent concurrency results:
    // 1. Printer 1 stopped exactly after committing sheets 0..4 (jam at 5)
    assert_eq!(p1_committed, (0..5).collect::<Vec<_>>());

    // 2. Printer 2 stopped exactly after committing sheets 0..9 (disconnect at 10)
    assert_eq!(p2_committed, (0..10).collect::<Vec<_>>());

    // 3. Printer 3 throttled but completed all 20 sheets
    assert_eq!(p3_committed.len(), total_sheets);

    // 4. Printer 4 completed all 20 sheets cleanly without being affected by siblings
    assert_eq!(p4_committed.len(), total_sheets);
    assert_eq!(p4_committed, (0..total_sheets).collect::<Vec<_>>());

    // 5. Failover slicing verification for Printer 1
    // Simulate failover printer picking up pending sheets [5..20]
    let p1_failover_sheets: Vec<usize> = (5..total_sheets).collect();
    FaultAssertions::assert_failover_exact_sheets(
        &p1_committed,
        &p1_failover_sheets,
        total_sheets,
        5,
    );
}

// =========================================================================
// 4. BENCHMARK CHALLENGES
// =========================================================================

#[test]
fn challenge_benchmarks_memory_probe_and_ui_fluidity_adversarial() {
    // 1. Memory probe sampling verification
    let probe = MemoryProbe::start();

    // Allocate memory to verify RSS tracking
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    for _ in 0..5 {
        chunks.push(vec![0xAA; 500_000]); // 500 KB per chunk
        std::thread::sleep(Duration::from_millis(30)); // allow background probe to sample
    }

    let (initial, peak, delta) = probe.stop();
    println!(
        "MemoryProbe result: initial = {} KB, peak = {} KB, delta = {} KB",
        initial / 1024,
        peak / 1024,
        delta / 1024
    );
    assert!(
        peak >= initial,
        "Peak memory ({}) must be >= initial memory ({})",
        peak,
        initial
    );
    BenchmarkAssertions::assert_peak_memory_delta(delta, 50.0);

    // 2. UI Fluidity Monitor: 60 FPS nominal frames
    let mut monitor = UiFluidityMonitor::new(60.0);
    for _ in 0..120 {
        monitor.record_frame_duration(Duration::from_millis(10)); // 10ms < 16.67ms
    }
    let report = monitor.finish();
    assert_eq!(report.total_frames, 120);
    assert_eq!(report.dropped_frames, 0);
    assert_eq!(report.stutter_frames, 0);
    assert_eq!(report.freeze_events, 0);
    assert!(report.max_frame_duration_ms <= 10.01);
    assert!(report.avg_frame_duration_ms >= 9.99 && report.avg_frame_duration_ms <= 10.01);
    BenchmarkAssertions::assert_ui_fluidity_60fps(&report);

    // 3. UI Fluidity Monitor: Edge case with 0 frames
    let empty_monitor = UiFluidityMonitor::new(60.0);
    let empty_report = empty_monitor.finish();
    assert_eq!(empty_report.total_frames, 0);
    assert_eq!(empty_report.dropped_frames, 0);
    assert_eq!(empty_report.max_frame_duration_ms, 0.0);

    // 4. UI Fluidity Monitor: Frame with dropped frame and stutter detection
    let mut degraded_monitor = UiFluidityMonitor::new(60.0);
    degraded_monitor.record_frame_duration(Duration::from_millis(10));
    degraded_monitor.record_frame_duration(Duration::from_millis(25)); // dropped (>16.6ms)
    degraded_monitor.record_frame_duration(Duration::from_millis(50)); // stutter (>33ms)
    degraded_monitor.record_frame_duration(Duration::from_millis(150)); // freeze (>100ms)
    let degraded_report = degraded_monitor.finish();
    assert_eq!(degraded_report.total_frames, 4);
    assert_eq!(degraded_report.dropped_frames, 3); // 25ms, 50ms, 150ms are > 16.6ms
    assert_eq!(degraded_report.stutter_frames, 2); // 50ms, 150ms are > 33ms
    assert_eq!(degraded_report.freeze_events, 1); // 150ms is > 100ms
}
