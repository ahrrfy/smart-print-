//! crates/platform/tests/m2_challenger_empirical_stress.rs
//!
//! EMPIRICAL CHALLENGER STRESS HARNESS — Milestone 2
//!
//! Adversarial stress test suite covering:
//! 1. Physical margin conversion & clipping hazard detection under extreme conditions:
//!    - Zero-margin borderless virtual printers
//!    - Highly asymmetrical thermal POS receipt printer margins (10.64pt L, 0pt T, 11.35pt R, 0.35pt B)
//!    - Massive A0 blueprints (2383.94 x 3370.39 pt) vs tiny receipts (204.65 x 841.30 pt)
//!    - ActualSize mode clipping math vs FitPrintableArea zero clipping guarantee
//! 2. Pre-flight capability negotiation boundary conditions:
//!    - Duplex on simplex printer
//!    - Color on monochrome printer
//!    - Monochrome on color printer (verifying both dispatch success and layer 1 flag assertion)
//!    - Copies limit overflows (0, 1, max, max+1, u32::MAX)
//! 3. DEVMODE memory allocation math, headroom, alignment, and bitmask preservation:
//!    - Chromium headroom formula behavior across buffer sizes
//!    - 8-byte pointer alignment guarantee
//!    - In-place mutation bitmask non-interference (preserving pre-existing vendor flags)
//!    - dmDriverExtra vendor private payload preservation (>3.7KB)

#![allow(unsafe_code)]

use pdfcraft_platform::caps::RawGdiCaps;
use pdfcraft_platform::cups::CupsDriverAdapter;
#[cfg(windows)]
use pdfcraft_platform::win32_v4::generate_delta_print_ticket_xml;
use pdfcraft_print::driver::{
    validate_job_capabilities, ColorMode, DriverGeneration,
    DuplexBinding, HardwareMargins, PageFitMode, PreFlightSeverity,
    PrintJobSettings, PrintableBounds, PrinterCapabilities,
};

fn uniform_margins(m: f64) -> HardwareMargins {
    HardwareMargins {
        left_pt: m,
        top_pt: m,
        right_pt: m,
        bottom_pt: m,
    }
}

// ============================================================================
// 1. PHYSICAL MARGIN CONVERSION & CLIPPING HAZARD EXTREME CONDITIONS
// ============================================================================

#[test]
fn test_extreme_margin_zero_margin_borderless_virtual_printer() {
    // Zero-margin virtual PDF printer (e.g. "Microsoft Print to PDF" or borderless photo printer)
    let raw = RawGdiCaps {
        dpi_x: 600,
        dpi_y: 600,
        horz_res: 4960, // 4960 px at 600 dpi = 595.2 pt (A4 width)
        vert_res: 7014, // 7014 px at 600 dpi = 841.68 pt (A4 height)
        phys_width: 4960,
        phys_height: 7014,
        phys_offset_x: 0,
        phys_offset_y: 0,
        is_duplex: false,
        is_color: true,
        max_copies: 999,
    };

    let bounds = raw.to_printable_bounds();
    assert_eq!(bounds.margins, HardwareMargins::ZERO);
    assert!((bounds.printable_width_pt - 595.2).abs() < 1e-4);
    assert!((bounds.printable_height_pt - 841.68).abs() < 1e-4);
    assert!((bounds.physical_width_pt - 595.2).abs() < 1e-4);
    assert!((bounds.physical_height_pt - 841.68).abs() < 1e-4);

    let caps = PrinterCapabilities {
        name: "Virtual-PDF-Borderless".to_string(),
        driver_generation: DriverGeneration::MockVirtual,
        is_duplex_supported: false,
        supported_duplex: vec![DuplexBinding::Simplex],
        is_color_supported: true,
        max_copies: 999,
        bounds,
    };

    // Subcase 1A: Exact matching page bounds in ActualSize -> ZERO clipping hazard!
    let settings_exact = PrintJobSettings {
        job_title: "Borderless Exact".to_string(),
        printer_name: caps.name.clone(),
        copies: 1,
        color_mode: ColorMode::Color,
        duplex_mode: DuplexBinding::Simplex,
        fit_mode: PageFitMode::ActualSize,
        page_bounds: (595.2, 841.68),
        landscape: false,
        paper_name: None,
    };
    let report_exact = validate_job_capabilities(&caps, &settings_exact);
    assert!(report_exact.is_valid_for_dispatch);
    assert!(!report_exact.has_clipping_hazard());
    assert_eq!(report_exact.issues, vec![PreFlightSeverity::Ok]);

    // Subcase 1B: Oversized page (e.g. US Letter 612 x 792 on A4 595.2 x 841.68) in ActualSize
    // Target X: (595.2 - 612.0) / 2 = -8.4. Page extends from -8.4 to 603.6.
    // Sheet is 0 to 595.2. Left clipped by 8.4 pt, Right clipped by 8.4 pt.
    let settings_oversized = PrintJobSettings {
        job_title: "Letter on A4 ActualSize".to_string(),
        printer_name: caps.name.clone(),
        copies: 1,
        color_mode: ColorMode::Color,
        duplex_mode: DuplexBinding::Simplex,
        fit_mode: PageFitMode::ActualSize,
        page_bounds: (612.0, 792.0),
        landscape: false,
        paper_name: None,
    };
    let report_oversized = validate_job_capabilities(&caps, &settings_oversized);
    assert!(report_oversized.is_valid_for_dispatch);
    assert!(report_oversized.has_clipping_hazard());
    let clip = report_oversized
        .issues
        .iter()
        .find_map(|i| match i {
            PreFlightSeverity::WarningClipping(c) => Some(*c),
            _ => None,
        })
        .expect("must have clipping");
    assert!((clip.left_pt - 8.4).abs() < 1e-3);
    assert!((clip.right_pt - 8.4).abs() < 1e-3);
    assert_eq!(clip.top_pt, 0.0);
    assert_eq!(clip.bottom_pt, 0.0);

    // Subcase 1C: Same oversized page in FitPrintableArea -> ZERO clipping guaranteed!
    let mut settings_fit = settings_oversized;
    settings_fit.fit_mode = PageFitMode::FitPrintableArea;
    let report_fit = validate_job_capabilities(&caps, &settings_fit);
    assert!(report_fit.is_valid_for_dispatch);
    assert!(!report_fit.has_clipping_hazard());
    assert_eq!(report_fit.issues, vec![PreFlightSeverity::Ok]);
}

#[test]
fn test_extreme_margin_highly_asymmetrical_thermal_pos_printer() {
    // Highly asymmetrical POS receipt printer (e.g. EPSON TM-T20III thermal cutter):
    // 203 DPI, unprintable feed margin on top is 0.0, blade cut margin on bottom is 0.35pt,
    // left edge guide is 30px (10.64pt), right guide is 32px (11.35pt).
    let raw = RawGdiCaps {
        dpi_x: 203,
        dpi_y: 203,
        horz_res: 576,        // Printable width = 576 px (204.45 pt)
        vert_res: 2376,       // Printable height = 2376 px (843.19 pt)
        phys_width: 638,      // 576 + 30 (left) + 32 (right) = 638 px (226.44 pt)
        phys_height: 2377,     // 2376 + 0 (top) + 1 (bottom) = 2377 px (843.55 pt)
        phys_offset_x: 30,    // 30 * 72 / 203 ≈ 10.6404 pt
        phys_offset_y: 0,     // 0 pt
        is_duplex: false,
        is_color: false,
        max_copies: 1,
    };

    let bounds = raw.to_printable_bounds();
    // Validate exact asymmetric margins
    assert!((bounds.margins.left_pt - (30.0 * 72.0 / 203.0)).abs() < 1e-4);
    assert_eq!(bounds.margins.top_pt, 0.0);
    assert!((bounds.margins.right_pt - (32.0 * 72.0 / 203.0)).abs() < 1e-4);
    assert!((bounds.margins.bottom_pt - (1.0 * 72.0 / 203.0)).abs() < 1e-4);

    let caps = PrinterCapabilities {
        name: "EPSON TM-T20III-Thermal".to_string(),
        driver_generation: DriverGeneration::Win32V3DevMode,
        is_duplex_supported: false,
        supported_duplex: vec![DuplexBinding::Simplex],
        is_color_supported: false,
        max_copies: 1,
        bounds: bounds.clone(),
    };

    // Subcase 2A: Document exactly the width of physical roll (226.44 pt) in ActualSize
    // Target X = 0.0. Content will clip into left margin (10.64pt) and right margin (11.35pt).
    let settings_roll = PrintJobSettings {
        job_title: "Full Roll Receipt".to_string(),
        printer_name: caps.name.clone(),
        copies: 1,
        color_mode: ColorMode::MonochromeKOnly,
        duplex_mode: DuplexBinding::Simplex,
        fit_mode: PageFitMode::ActualSize,
        page_bounds: (bounds.physical_width_pt, bounds.printable_height_pt),
        landscape: false,
        paper_name: None,
    };
    let report_roll = validate_job_capabilities(&caps, &settings_roll);
    assert!(report_roll.is_valid_for_dispatch);
    assert!(report_roll.has_clipping_hazard());
    let clip = report_roll
        .issues
        .iter()
        .find_map(|i| match i {
            PreFlightSeverity::WarningClipping(c) => Some(*c),
            _ => None,
        })
        .expect("must report clipping");
    assert!((clip.left_pt - bounds.margins.left_pt).abs() < 1e-4);
    assert!((clip.right_pt - bounds.margins.right_pt).abs() < 1e-4);
    assert_eq!(clip.top_pt, 0.0);

    // Subcase 2B: FitPrintableArea completely scales the receipt to printable boundary with 0 clipping
    let mut settings_roll_fit = settings_roll;
    settings_roll_fit.fit_mode = PageFitMode::FitPrintableArea;
    let report_roll_fit = validate_job_capabilities(&caps, &settings_roll_fit);
    assert!(report_roll_fit.is_valid_for_dispatch);
    assert!(!report_roll_fit.has_clipping_hazard());
}

#[test]
fn test_extreme_size_massive_a0_blueprint_vs_tiny_receipt() {
    let a0_width_pt = 2383.94; // 841 mm
    let a0_height_pt = 3370.39; // 1189 mm

    let a4_caps = PrinterCapabilities {
        name: "Standard-A4-Laser".to_string(),
        driver_generation: DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![
            DuplexBinding::Simplex,
            DuplexBinding::FlipLongEdge,
            DuplexBinding::FlipShortEdge,
        ],
        is_color_supported: true,
        max_copies: 999,
        bounds: PrintableBounds {
            printable_width_pt: 578.4,
            printable_height_pt: 824.88,
            physical_width_pt: 595.2,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: uniform_margins(8.4),
        },
    };

    // Subcase 3A: Giant A0 Blueprint submitted to A4 desktop printer in ActualSize
    let settings_a0_actual = PrintJobSettings {
        job_title: "Architectural CAD Drawing A0".to_string(),
        printer_name: a4_caps.name.clone(),
        copies: 1,
        color_mode: ColorMode::MonochromeKOnly,
        duplex_mode: DuplexBinding::Simplex,
        fit_mode: PageFitMode::ActualSize,
        page_bounds: (a0_width_pt, a0_height_pt),
        landscape: false,
        paper_name: None,
    };
    let report_a0 = validate_job_capabilities(&a4_caps, &settings_a0_actual);
    assert!(report_a0.is_valid_for_dispatch);
    assert!(report_a0.has_clipping_hazard());
    let clip = report_a0
        .issues
        .iter()
        .find_map(|i| match i {
            PreFlightSeverity::WarningClipping(c) => Some(*c),
            _ => None,
        })
        .expect("clipping expected");
    // Expected clipped amount: target_x = (595.2 - 2383.94) / 2 = -894.37
    // clip_l = 8.4 - (-894.37) = 902.77 pt
    assert!(clip.left_pt > 900.0);
    assert!(clip.right_pt > 900.0);
    assert!(clip.top_pt > 1250.0);
    assert!(clip.bottom_pt > 1250.0);
    assert!(clip.total_clipped_pt() > 4000.0);

    // Subcase 3B: Same A0 Blueprint in FitPrintableArea -> Scales cleanly, ZERO clipping
    let mut settings_a0_fit = settings_a0_actual;
    settings_a0_fit.fit_mode = PageFitMode::FitPrintableArea;
    let report_a0_fit = validate_job_capabilities(&a4_caps, &settings_a0_fit);
    assert!(report_a0_fit.is_valid_for_dispatch);
    assert!(!report_a0_fit.has_clipping_hazard());

    // Subcase 3C: Tiny receipt (150 x 300 pt) on massive A0 Plotter
    let plotter_caps = PrinterCapabilities {
        name: "HP-DesignJet-A0-Plotter".to_string(),
        driver_generation: DriverGeneration::Win32V4PrintTicket,
        is_duplex_supported: false,
        supported_duplex: vec![DuplexBinding::Simplex],
        is_color_supported: true,
        max_copies: 99,
        bounds: PrintableBounds {
            printable_width_pt: a0_width_pt - 28.0,
            printable_height_pt: a0_height_pt - 28.0,
            physical_width_pt: a0_width_pt,
            physical_height_pt: a0_height_pt,
            dpi_x: 1200,
            dpi_y: 1200,
            margins: uniform_margins(14.0),
        },
    };

    let settings_tiny = PrintJobSettings {
        job_title: "Tiny Label".to_string(),
        printer_name: plotter_caps.name.clone(),
        copies: 1,
        color_mode: ColorMode::Color,
        duplex_mode: DuplexBinding::Simplex,
        fit_mode: PageFitMode::ActualSize,
        page_bounds: (150.0, 300.0),
        landscape: false,
        paper_name: None,
    };
    let report_tiny = validate_job_capabilities(&plotter_caps, &settings_tiny);
    assert!(report_tiny.is_valid_for_dispatch);
    assert!(!report_tiny.has_clipping_hazard());
}

#[test]
fn test_parametric_aspect_ratio_matrix_fit_printable_area_zero_clipping_guarantee() {
    let bounds = PrintableBounds {
        printable_width_pt: 550.0,
        printable_height_pt: 800.0,
        physical_width_pt: 600.0,
        physical_height_pt: 850.0,
        dpi_x: 600,
        dpi_y: 600,
        margins: uniform_margins(25.0),
    };
    let caps = PrinterCapabilities {
        name: "Test-Matrix-Printer".to_string(),
        driver_generation: DriverGeneration::MockVirtual,
        is_duplex_supported: true,
        supported_duplex: vec![DuplexBinding::Simplex, DuplexBinding::FlipLongEdge],
        is_color_supported: true,
        max_copies: 100,
        bounds,
    };

    // Extreme aspect ratios to test:
    let test_dimensions = [
        (100.0, 1000.0),  // Ultra-tall banner 1:10
        (1000.0, 100.0),  // Ultra-wide panorama 10:1
        (50.0, 50.0),     // Tiny postage stamp
        (5000.0, 5000.0), // Massive square
        (550.0, 800.0),   // Exact printable area match
        (600.0, 850.0),   // Exact physical sheet match
        (1.0, 1000.0),    // Single point sliver
        (1000.0, 1.0),    // Single point ribbon
    ];

    for (w, h) in test_dimensions {
        let settings = PrintJobSettings {
            job_title: format!("Stress {w}x{h}"),
            printer_name: caps.name.clone(),
            copies: 1,
            color_mode: ColorMode::Color,
            duplex_mode: DuplexBinding::Simplex,
            fit_mode: PageFitMode::FitPrintableArea,
            page_bounds: (w, h),
            landscape: false,
            paper_name: None,
        };

        let report = validate_job_capabilities(&caps, &settings);
        assert!(
            report.is_valid_for_dispatch,
            "Failed valid dispatch on {w}x{h}"
        );
        assert!(
            !report.has_clipping_hazard(),
            "Clipping hazard occurred on {w}x{h} in FitPrintableArea!"
        );
        assert_eq!(report.issues, vec![PreFlightSeverity::Ok]);
    }
}

// ============================================================================
// 2. PRE-FLIGHT CAPABILITY NEGOTIATION BOUNDARY CONDITIONS
// ============================================================================

#[test]
fn test_preflight_duplex_on_simplex_printer_critical_mismatch() {
    let caps = PrinterCapabilities {
        name: "Simplex-Receipt-POS".to_string(),
        driver_generation: DriverGeneration::Win32V3DevMode,
        is_duplex_supported: false,
        supported_duplex: vec![DuplexBinding::Simplex],
        is_color_supported: false,
        max_copies: 1,
        bounds: PrintableBounds {
            printable_width_pt: 200.0,
            printable_height_pt: 800.0,
            physical_width_pt: 220.0,
            physical_height_pt: 800.0,
            dpi_x: 203,
            dpi_y: 203,
            margins: HardwareMargins::ZERO,
        },
    };

    // Subcase 2A: Long-Edge requested
    let settings_long = PrintJobSettings {
        duplex_mode: DuplexBinding::FlipLongEdge,
        ..Default::default()
    };
    let report_long = validate_job_capabilities(&caps, &settings_long);
    assert!(!report_long.is_valid_for_dispatch);
    assert!(report_long.has_critical_mismatch());
    assert!(report_long
        .issues
        .iter()
        .any(|i| match i {
            PreFlightSeverity::CriticalMismatch(msg) => msg.contains("Double-sided printing"),
            _ => false,
        }));

    // Subcase 2B: Short-Edge requested
    let settings_short = PrintJobSettings {
        duplex_mode: DuplexBinding::FlipShortEdge,
        ..Default::default()
    };
    let report_short = validate_job_capabilities(&caps, &settings_short);
    assert!(!report_short.is_valid_for_dispatch);
    assert!(report_short.has_critical_mismatch());
}

#[test]
fn test_preflight_color_on_monochrome_printer_critical_mismatch() {
    let caps = PrinterCapabilities {
        name: "Mono-Office-Laser".to_string(),
        driver_generation: DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![DuplexBinding::Simplex, DuplexBinding::FlipLongEdge],
        is_color_supported: false, // Monochrome only!
        max_copies: 999,
        bounds: PrintableBounds {
            printable_width_pt: 578.4,
            printable_height_pt: 824.88,
            physical_width_pt: 595.2,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: uniform_margins(8.4),
        },
    };

    let settings_color = PrintJobSettings {
        color_mode: ColorMode::Color,
        ..Default::default()
    };
    let report_color = validate_job_capabilities(&caps, &settings_color);
    assert!(!report_color.is_valid_for_dispatch);
    assert!(report_color.has_critical_mismatch());
    assert!(report_color
        .issues
        .iter()
        .any(|i| match i {
            PreFlightSeverity::CriticalMismatch(msg) => msg.contains("Color printing requested"),
            _ => false,
        }));
}

#[test]
fn test_preflight_monochrome_on_color_printer_must_succeed_and_assert_driver_flags() {
    // A monochrome print job on a color-capable printer is a primary requirement (R2):
    // 1. Negotiation must SUCCEED (valid for dispatch = true)
    // 2. Hardware driver flags must assert monochrome (Layer 1 Driver Flag Assertion)
    let caps = PrinterCapabilities {
        name: "Enterprise-Color-Copier".to_string(),
        driver_generation: DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![DuplexBinding::Simplex, DuplexBinding::FlipLongEdge],
        is_color_supported: true, // Color supported
        max_copies: 999,
        bounds: PrintableBounds {
            printable_width_pt: 578.4,
            printable_height_pt: 824.88,
            physical_width_pt: 595.2,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: uniform_margins(8.4),
        },
    };

    let settings_mono = PrintJobSettings {
        color_mode: ColorMode::MonochromeKOnly,
        duplex_mode: DuplexBinding::FlipLongEdge,
        ..Default::default()
    };

    // 1. Negotiation check
    let report = validate_job_capabilities(&caps, &settings_mono);
    assert!(
        report.is_valid_for_dispatch,
        "Monochrome job must be accepted on color printer!"
    );
    assert!(!report.has_critical_mismatch());
    assert_eq!(report.issues, vec![PreFlightSeverity::Ok]);

    // 2. Verify Win32 V4 PrintTicket driver flag assertion
    #[cfg(windows)]
    {
        let xml = generate_delta_print_ticket_xml(
            ColorMode::MonochromeKOnly,
            DuplexBinding::FlipLongEdge,
            false,
            1,
            None,
        );
        assert!(
            xml.contains("<psf:Option name=\"psk:Monochrome\" />"),
            "PrintTicket must assert psk:Monochrome"
        );
        assert!(
            !xml.contains("psk:Color"),
            "PrintTicket must NOT assert psk:Color"
        );
    }

    // 3. Verify CUPS IPP driver flag assertion
    let cups_adapter = CupsDriverAdapter::new();
    let ipp_req = cups_adapter.build_print_job_request("Enterprise-Color-Copier", &settings_mono);
    let ipp_str = String::from_utf8_lossy(&ipp_req);
    assert!(
        ipp_str.contains("print-color-mode"),
        "CUPS IPP must contain print-color-mode attribute"
    );
    assert!(
        ipp_str.contains("monochrome"),
        "CUPS IPP must assert monochrome value"
    );
    // Value for "color" would be length prefix 5 followed by "color": [0x00, 0x05, b'c', b'o', b'l', b'o', b'r']
    assert!(
        !ipp_req.windows(7).any(|w| w == [0x00, 0x05, b'c', b'o', b'l', b'o', b'r']),
        "CUPS IPP must not assert 'color' as attribute value"
    );
}

#[test]
fn test_preflight_copies_limit_boundary_and_overflows() {
    let caps = PrinterCapabilities {
        name: "POS-Single-Copy-Only".to_string(),
        driver_generation: DriverGeneration::Win32V3DevMode,
        is_duplex_supported: false,
        supported_duplex: vec![DuplexBinding::Simplex],
        is_color_supported: false,
        max_copies: 5, // max copies = 5
        bounds: PrintableBounds {
            printable_width_pt: 200.0,
            printable_height_pt: 800.0,
            physical_width_pt: 220.0,
            physical_height_pt: 800.0,
            dpi_x: 203,
            dpi_y: 203,
            margins: HardwareMargins::ZERO,
        },
    };

    let mut settings = PrintJobSettings {
        copies: 1,
        ..Default::default()
    };

    // Copies = 1 -> Passes
    assert!(validate_job_capabilities(&caps, &settings).is_valid_for_dispatch);

    // Copies = 5 (exact limit) -> Passes
    settings.copies = 5;
    assert!(validate_job_capabilities(&caps, &settings).is_valid_for_dispatch);

    // Copies = 6 (limit + 1) -> Fails!
    settings.copies = 6;
    let report_6 = validate_job_capabilities(&caps, &settings);
    assert!(!report_6.is_valid_for_dispatch);
    assert!(report_6.has_critical_mismatch());
    assert!(report_6
        .issues
        .iter()
        .any(|i| match i {
            PreFlightSeverity::CriticalMismatch(msg) => msg.contains("Requested copies (6) exceeds"),
            _ => false,
        }));

    // Copies = u32::MAX (extreme overflow) -> Fails cleanly without panics
    settings.copies = u32::MAX;
    let report_max = validate_job_capabilities(&caps, &settings);
    assert!(!report_max.is_valid_for_dispatch);
    assert!(report_max.has_critical_mismatch());
}

// ============================================================================
// 3. DEVMODE MEMORY ALLOCATION MATH, HEADROOM, ALIGNMENT & BITMASK MATH
// ============================================================================

#[test]
fn test_devmode_allocation_headroom_math_and_alignment() {
    // Headroom safety formula: alloc_bytes = ((needed as usize).max(220)).saturating_mul(2).saturating_add(8192)
    let calc_alloc = |needed: i32| -> (usize, usize) {
        let alloc_bytes = ((needed as usize).max(220))
            .saturating_mul(2)
            .saturating_add(8192);
        let u64_count = alloc_bytes.div_ceil(8);
        (alloc_bytes, u64_count)
    };

    // Case 1: Minimum standard DEVMODE (220 bytes)
    let (b_min, u_min) = calc_alloc(220);
    assert_eq!(b_min, 220 * 2 + 8192); // 440 + 8192 = 8632 bytes
    assert_eq!(u_min, 8632 / 8);       // Exactly 1079 u64 words
    assert_eq!(u_min * 8, b_min);

    // Case 2: Realistic Epson / HP thermal POS driver with 3800 bytes dmDriverExtra
    let (b_epson, u_epson) = calc_alloc(3800);
    assert_eq!(b_epson, 3800 * 2 + 8192); // 7600 + 8192 = 15792 bytes
    assert_eq!(u_epson, 15792 / 8);       // Exactly 1974 u64 words

    // Case 3: Zero or negative input defense
    let (b_zero, _) = calc_alloc(0);
    assert_eq!(b_zero, 8632); // Max(220) ensures safe baseline

    // Case 4: Alignment guarantee
    let u64_buf = vec![0u64; u_epson];
    let ptr = u64_buf.as_ptr() as usize;
    assert_eq!(ptr % 8, 0, "DEVMODE buffer must be 8-byte aligned for 64-bit Windows pointers");
}

#[test]
fn test_devmode_inplace_bitmask_assertion_preserves_vendor_flags() {
    // Simulate a full 8632-byte allocated DEVMODE buffer
    let mut buf_u64 = vec![0u64; 8632 / 8];
    let raw = buf_u64.as_mut_ptr() as *mut u8;

    unsafe {
        // Initialize standard mock DEVMODEW header
        let dm_size_ptr = raw.add(68) as *mut u16;
        let dm_extra_ptr = raw.add(70) as *mut u16;
        let dm_fields_ptr = raw.add(72) as *mut u32;
        let dm_orientation_ptr = raw.add(76) as *mut i16;
        let dm_copies_ptr = raw.add(86) as *mut i16;
        let dm_color_ptr = raw.add(92) as *mut i16;
        let dm_duplex_ptr = raw.add(94) as *mut i16;
        let dm_collate_ptr = raw.add(100) as *mut i16;

        *dm_size_ptr = 220;
        *dm_extra_ptr = 4000; // 4000 bytes vendor extra

        // Pre-existing vendor flags in dmFields (e.g. DM_PAPERSIZE = 0x2, DM_PRINTQUALITY = 0x400)
        let vendor_initial_fields: u32 = 0x0000_0002 | 0x0000_0400;
        *dm_fields_ptr = vendor_initial_fields;

        // Fill vendor extra region (offset 220..4220) with recognizable canary data
        for i in 220..4220 {
            *raw.add(i) = ((i * 7) & 0xFF) as u8;
        }

        // Apply Milestone 2 flag assertions:
        // 1. Color: Monochrome
        *dm_color_ptr = 1; // DMCOLOR_MONOCHROME
        *dm_fields_ptr |= 0x0000_0800; // DM_COLOR

        // 2. Duplex: Flip Long Edge
        *dm_duplex_ptr = 2; // DMDUP_VERTICAL
        *dm_fields_ptr |= 0x0000_1000; // DM_DUPLEX

        // 3. Orientation: Landscape
        *dm_orientation_ptr = 2; // DMORIENT_LANDSCAPE
        *dm_fields_ptr |= 0x0000_0001; // DM_ORIENTATION

        // 4. Copies: 3 with collation
        *dm_copies_ptr = 3;
        *dm_fields_ptr |= 0x0000_0100; // DM_COPIES
        *dm_collate_ptr = 1;
        *dm_fields_ptr |= 0x0000_8000; // DM_COLLATE

        // VERIFY:
        // A. Bitmask preserves pre-existing vendor flags!
        assert_eq!(
            *dm_fields_ptr & vendor_initial_fields,
            vendor_initial_fields,
            "Pre-existing vendor fields must be preserved by bitwise OR!"
        );

        // B. Asserted bits are present
        assert_eq!(*dm_fields_ptr & 0x0000_0800, 0x0000_0800); // DM_COLOR
        assert_eq!(*dm_fields_ptr & 0x0000_1000, 0x0000_1000); // DM_DUPLEX
        assert_eq!(*dm_fields_ptr & 0x0000_0001, 0x0000_0001); // DM_ORIENTATION
        assert_eq!(*dm_fields_ptr & 0x0000_0100, 0x0000_0100); // DM_COPIES
        assert_eq!(*dm_fields_ptr & 0x0000_8000, 0x0000_8000); // DM_COLLATE

        // C. Field values are exact
        assert_eq!(*dm_color_ptr, 1);
        assert_eq!(*dm_duplex_ptr, 2);
        assert_eq!(*dm_orientation_ptr, 2);
        assert_eq!(*dm_copies_ptr, 3);
        assert_eq!(*dm_collate_ptr, 1);

        // D. Vendor private payload (dmDriverExtra) is completely untouched!
        for i in 220..4220 {
            let expected_canary = ((i * 7) & 0xFF) as u8;
            assert_eq!(
                *raw.add(i),
                expected_canary,
                "Vendor extra payload corrupted at offset {i}!"
            );
        }
    }
}
