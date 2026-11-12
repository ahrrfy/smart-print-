//! Tier 4 Real-World Office Document, Margin, and Color Scenarios (Scenarios 3, 4, 5, 9, 10)
//!
//! Submodule: `tests/tier4/office_document_scenarios.rs`
//! Runner: `tests/tier4_scenarios.rs`

use pdfcraft_content::{parse, serialize_ops};
use pdfcraft_e2e::margin_assertions::{
    calculate_page_transform, ClippingExtent, MarginAssertions, PageFitMode,
};
use pdfcraft_e2e::mock_driver::{
    create_mock_virtual_pdf_printer, DuplexBinding,
    MockV3Driver, MockV3DriverConfig, MockV4Driver, MockV4DriverConfig, PrintableBounds,
    DMCOLOR_MONOCHROME, DMDUP_VERTICAL,
    DM_COLOR, DM_DUPLEX, DM_IN_BUFFER, DM_OUT_BUFFER,
};
use pdfcraft_e2e::stream_inspector::{
    DefaultStreamInspector, PdfStreamInspector,
};
use pdfcraft_print::driver::{
    validate_job_capabilities, ColorMode,
    PrintJobSettings, PrinterCapabilities,
};
use pdfcraft_print::monochrome::MonochromeVectorFilter;
use std::sync::Arc;

#[cfg(windows)]
use pdfcraft_platform::win32_v4::with_print_ticket_provider;

// Fallback provider lock for cross-platform test compilation
#[cfg(not(windows))]
fn with_print_ticket_provider<T, F: FnOnce() -> Result<T, pdfcraft_print::driver::DriverError>>(
    f: F,
) -> Result<T, pdfcraft_print::driver::DriverError> {
    static FALLBACK_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = FALLBACK_MUTEX.lock().unwrap();
    f()
}

/// Helper: passes raw content stream bytes through MonochromeVectorFilter
fn filter_content_stream_bytes(input: &[u8]) -> Vec<u8> {
    let parsed = parse(input);
    let filtered_ops = MonochromeVectorFilter::filter_operators(&parsed.ops);
    serialize_ops(&filtered_ops)
}

// =========================================================================
// SCENARIO 3: 100-Page Mixed-Orientation Corporate Report Duplex Booklet
// =========================================================================

#[test]
fn scenario_03_100_page_mixed_orientation_corporate_report_duplex_booklet() {
    // 1. Driver Setup: Win32 V3 Driver configured for Duplex Booklet (Flip Long Edge)
    let mock = MockV3Driver::new(MockV3DriverConfig {
        printer_name: "Xerox_WorkCentre_V3_Duplex".into(),
        driver_extra_size: 3704,
        vendor_canary_pattern: vec![0xBE, 0xEF, 0xCA, 0xFE],
        default_duplex: DuplexBinding::FlipLongEdge,
        default_color: true,
        bounds: PrintableBounds::a4_standard_margins(),
    });

    let needed = mock.document_properties_pass1() as usize;
    assert_eq!(needed, 220 + 3704);

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Pass 2 default populate failed");

    // Inject Duplex Flip Long Edge (DMDUP_VERTICAL = 2)
    devmode_buf[72..76].copy_from_slice(&DM_DUPLEX.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_VERTICAL.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Pass 2 submit failed");

    mock.assert_pass1_called();
    mock.assert_duplex_asserted(DuplexBinding::FlipLongEdge);
    mock.assert_driver_extra_preserved();

    // 2. Workload: 100-page document with alternating orientation blocks
    // Pages 1-10: Portrait, Pages 11-20: Landscape, Pages 21-100: Portrait
    let portrait_size = (595.28, 841.89);
    let landscape_size = (841.89, 595.28);
    let mut page_sizes = Vec::with_capacity(100);
    for i in 0..100 {
        if (10..20).contains(&i) {
            page_sizes.push(landscape_size);
        } else {
            page_sizes.push(portrait_size);
        }
    }
    assert_eq!(page_sizes.len(), 100);

    // 3. Execution & Verification across all 50 physical sheets
    let printable = PrintableBounds::a4_standard_margins();
    let mut inverted_back_pages = 0;

    for sheet_idx in 0..50 {
        let front_idx = sheet_idx * 2;
        let back_idx = front_idx + 1;

        let front_size = page_sizes[front_idx];
        let back_size = page_sizes[back_idx];

        let master_is_portrait = front_size.1 >= front_size.0;
        let back_is_portrait = back_size.1 >= back_size.0;

        // Front page transform
        let front_transform = calculate_page_transform(
            front_size,
            &printable,
            PageFitMode::FitPrintableArea,
            false,
            master_is_portrait,
            DuplexBinding::FlipLongEdge,
        );
        MarginAssertions::assert_within_printable(front_size, &printable, &front_transform);

        // Back page transform with sheet master orientation alignment
        let back_transform = calculate_page_transform(
            back_size,
            &printable,
            PageFitMode::FitPrintableArea,
            true,
            master_is_portrait,
            DuplexBinding::FlipLongEdge,
        );

        // Margin compliance: 0% clipping
        MarginAssertions::assert_within_printable(back_size, &printable, &back_transform);
        assert!(!back_transform.clipping.is_clipped(1e-4));

        // Orientation Alignment Verification
        if back_is_portrait != master_is_portrait {
            assert!(
                back_transform.rotation_deg == 90 || back_transform.rotation_deg == 270,
                "Back page on sheet {sheet_idx} failed to rotate to match sheet master!"
            );
        } else {
            assert_eq!(back_transform.rotation_deg, 0);
        }

        // Effective sheet dimensions after alignment
        let (eff_w, eff_h) = if back_transform.rotation_deg != 0 {
            (back_size.1, back_size.0)
        } else {
            (back_size.0, back_size.1)
        };

        if (eff_h >= eff_w) != master_is_portrait {
            inverted_back_pages += 1;
        }
    }

    assert_eq!(
        inverted_back_pages, 0,
        "Detected {inverted_back_pages} inverted back page(s) across 100-page duplex run!"
    );
}

// =========================================================================
// SCENARIO 4: High-Complexity Color Architectural Blueprint to Mono V3 Driver
// =========================================================================

#[test]
fn scenario_04_high_complexity_color_blueprint_to_monochrome_v3_driver() {
    // 1. Driver Setup: HP Enterprise V3 Driver asserting DMCOLOR_MONOCHROME
    let mock = MockV3Driver::new(MockV3DriverConfig {
        printer_name: "HP_LaserJet_Enterprise_V3_Mono".into(),
        driver_extra_size: 4096,
        vendor_canary_pattern: vec![0xCA, 0xFE, 0xBA, 0xBE],
        default_color: false,
        default_duplex: DuplexBinding::Simplex,
        bounds: PrintableBounds::a4_standard_margins(),
    });

    let needed = mock.document_properties_pass1() as usize;
    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();

    // Layer 1 Protection: Assert DMCOLOR_MONOCHROME (1) with DM_COLOR bit
    devmode_buf[72..76].copy_from_slice(&DM_COLOR.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .unwrap();

    mock.assert_monochrome_asserted();
    mock.assert_driver_extra_preserved();

    // 2. Workload: CAD Architectural Blueprint content stream with RGB lines, CMYK fills, and gray text
    let blueprint_stream = b"q\n\
        1.0 0.0 0.0 RG 2.0 w 10 10 500 700 re S\n\
        0.0 0.5 1.0 rg 20 20 200 150 re f\n\
        0.8 0.4 0.2 0.1 k 250 20 150 150 re f\n\
        0.1 0.9 0.8 0.0 K 1.5 w 20 200 380 200 re S\n\
        0.3 G 0.5 w 10 420 500 0.5 re S\n\
        0.0 0.0 0.0 0.8 k (Scale 1:100 Floorplan) Tj\n\
        Q\n";

    // 3. Layer 2 Protection: In-memory vector filter
    let filtered_stream = filter_content_stream_bytes(blueprint_stream);
    let inspector = DefaultStreamInspector::new();
    let violations = inspector.inspect_content_stream(&filtered_stream, 0);
    assert!(
        violations.is_empty(),
        "Detected color violations in filtered blueprint vector stream: {:?}",
        violations
    );

    let stream_text = String::from_utf8_lossy(&filtered_stream);
    assert!(!stream_text.contains(" rg"), "RGB operator 'rg' not removed");
    assert!(!stream_text.contains(" RG"), "RGB operator 'RG' not removed");
    assert!(!stream_text.contains("0.8 0.4 0.2"), "Chromatic CMYK not converted");

    // 4. Image XObject Re-encoding: 100x100 8-bit RGB legend photo
    let raw_rgb_pixels = vec![200u8; 100 * 100 * 3];
    let reencoded_gray_pixels: Vec<u8> = raw_rgb_pixels
        .chunks_exact(3)
        .map(|rgb| ((299 * rgb[0] as u32 + 587 * rgb[1] as u32 + 114 * rgb[2] as u32 + 500) / 1000) as u8)
        .collect();
    assert_eq!(reencoded_gray_pixels.len(), 100 * 100);

    let image_dict = "<< /Type /XObject /Subtype /Image /Width 100 /Height 100 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let img_res = inspector.inspect_image_xobject("ImLegend", image_dict, &reencoded_gray_pixels, 0);
    assert!(img_res.is_ok(), "Image XObject inspection failed: {:?}", img_res);
}

// =========================================================================
// SCENARIO 5: Full-Bleed Marketing Flyer Margin Adaptation & Clipping
// =========================================================================

#[test]
fn scenario_05_full_bleed_marketing_flyer_margin_adaptation_and_clipping() {
    let flyer_size = (595.20, 841.68); // Full-bleed A4
    let printable = PrintableBounds::a4_standard_margins(); // 8.4pt margins, 578.40 x 824.88 pt printable

    let caps = PrinterCapabilities {
        name: "Mock_Color_Laser".into(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![
            pdfcraft_print::driver::DuplexBinding::Simplex,
            pdfcraft_print::driver::DuplexBinding::FlipLongEdge,
        ],
        is_color_supported: true,
        max_copies: 99,
        bounds: pdfcraft_print::driver::PrintableBounds {
            printable_width_pt: printable.printable_width_pt,
            printable_height_pt: printable.printable_height_pt,
            physical_width_pt: printable.physical_width_pt,
            physical_height_pt: printable.physical_height_pt,
            dpi_x: printable.dpi_x,
            dpi_y: printable.dpi_y,
            margins: pdfcraft_print::driver::HardwareMargins {
                left_pt: printable.margins.left_pt,
                top_pt: printable.margins.top_pt,
                right_pt: printable.margins.right_pt,
                bottom_pt: printable.margins.bottom_pt,
            },
        },
    };

    // Sub-test 1: Actual Size (1:1) Clipping Detection
    let actual_transform = calculate_page_transform(
        flyer_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(actual_transform.scale, 1.0);
    assert!(actual_transform.clipping.is_clipped(1e-4));

    // Expected clipping: 8.40pt on each of the 4 edges
    let expected_clipping = ClippingExtent {
        left_pt: 8.40,
        top_pt: 8.40,
        right_pt: 8.40,
        bottom_pt: 8.40,
    };
    MarginAssertions::assert_clipping_hazard(&actual_transform.clipping, &expected_clipping);

    let clipping_mm = actual_transform.clipping.to_mm();
    assert!((clipping_mm.left_mm - 2.963).abs() < 1e-3);
    let total_mm = clipping_mm.left_mm + clipping_mm.top_mm + clipping_mm.right_mm + clipping_mm.bottom_mm;
    assert!((total_mm - 11.853).abs() < 1e-2);

    // Pre-flight validation emits WarningClipping
    let settings = PrintJobSettings {
        job_title: "Full Bleed Flyer".into(),
        printer_name: "Mock_Color_Laser".into(),
        copies: 1,
        color_mode: ColorMode::Color,
        duplex_mode: pdfcraft_print::driver::DuplexBinding::Simplex,
        fit_mode: pdfcraft_print::driver::PageFitMode::ActualSize,
        page_bounds: flyer_size,
        landscape: false,
        paper_name: None,
    };
    let report = validate_job_capabilities(&caps, &settings);
    assert!(report.has_clipping_hazard());

    // Sub-test 2: Fit to Printable Area (Uniform scaling with 0% clipping)
    let fit_transform = calculate_page_transform(
        flyer_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    let expected_scale = 578.40 / 595.20; // 0.971774
    assert!((fit_transform.scale - expected_scale).abs() < 1e-4);
    assert!(!fit_transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(flyer_size, &printable, &fit_transform);
    MarginAssertions::assert_centered(flyer_size, &printable, &fit_transform);

    // Sub-test 3: Shrink Oversized Engine
    let shrink_transform = calculate_page_transform(
        flyer_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(shrink_transform.scale, fit_transform.scale);
    MarginAssertions::assert_within_printable(flyer_size, &printable, &shrink_transform);

    // Smaller page (A5: 420 x 595 pt) passes through at 1:1 scale
    let a5_size = (420.0, 595.0);
    let a5_transform = calculate_page_transform(
        a5_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(a5_transform.scale, 1.0);
    MarginAssertions::assert_within_printable(a5_size, &printable, &a5_transform);
}

// =========================================================================
// SCENARIO 9: V4 XPS PrintTicket Merge with Custom Envelope & Finishing
// =========================================================================

#[test]
fn scenario_09_v4_xps_print_ticket_merge_custom_envelope_finishing() {
    let mock = Arc::new(MockV4Driver::new(MockV4DriverConfig {
        printer_name: "XPS_Custom_Envelope_V4".into(),
        supports_color: true,
        supports_monochrome: true,
        supports_duplex: true,
        bounds: PrintableBounds::a4_borderless(),
    }));

    mock.pt_open_provider().expect("Open provider failed");

    // Delta ticket requesting Custom DL Envelope (110mm x 220mm) and high quality
    let delta_ticket = r#"<?xml version="1.0" encoding="UTF-8"?>
<psf:PrintTicket version="1"
    xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework"
    xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
    xmlns:xsd="http://www.w3.org/2001/XMLSchema">
    <psf:Feature name="psk:PageMediaSize">
        <psf:Option name="psk:ISO_DL_Envelope" />
    </psf:Feature>
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Color" />
    </psf:Feature>
    <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
        <psf:Option name="psk:OneSided" />
    </psf:Feature>
</psf:PrintTicket>"#;

    // Concurrency synchronization test: 10 concurrent threads invoking PTMergeAndValidate
    // Must be serialized via with_print_ticket_provider
    let thread_count = 10;
    let mut handles = Vec::with_capacity(thread_count);

    for _ in 0..thread_count {
        let m = Arc::clone(&mock);
        let delta = delta_ticket.to_string();
        handles.push(std::thread::spawn(move || {
            with_print_ticket_provider(|| {
                m.pt_merge_and_validate(None, &delta)
                    .map_err(|e| pdfcraft_print::driver::DriverError::PrintTicketError(e.to_string()))
            })
        }));
    }

    for h in handles {
        let res = h.join().expect("Worker thread panicked");
        assert!(res.is_ok(), "PrintTicket merge failed under provider lock");
    }

    // Verify 0 concurrency violations occurred in the provider
    mock.assert_no_concurrency_violations();

    // Verify merged PrintTicket schema compliance
    let last_ticket = mock.last_merged_ticket().expect("Merged ticket missing");
    assert!(last_ticket.contains("psk:PageOutputColor"));
    assert!(last_ticket.contains("psk:Color"));
    assert!(last_ticket.contains("psk:OneSided"));
    assert!(mock.conflicts().is_empty());
}

// =========================================================================
// SCENARIO 10: Mixed Composite Graphic Monochrome Conversion
// =========================================================================

#[test]
fn scenario_10_mixed_composite_graphic_monochrome_conversion() {
    // 1. Driver Setup: Virtual printer omitting monochrome (simulating Microsoft Print to PDF)
    let mock = create_mock_virtual_pdf_printer("Microsoft Print to PDF");
    mock.pt_open_provider().unwrap();

    // Delta ticket requests monochrome
    let delta_ticket = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
        <psf:Feature name="psk:PageOutputColor"><psf:Option name="psk:Monochrome"/></psf:Feature>
    </psf:PrintTicket>"#;

    let merged_ticket = mock.pt_merge_and_validate(None, delta_ticket).unwrap();
    // Driver resolves to psk:Color due to lack of hardware monochrome capability
    assert!(merged_ticket.contains("psk:Color"));
    let conflicts = mock.conflicts();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].feature, "psk:PageOutputColor");
    assert_eq!(conflicts[0].resolved_option, "psk:Color");

    // 2. Layer 2 In-Memory Grayscale Engine: Content stream transformation
    // Composite content: RGB headlines, CMYK charts, neutral gray text
    let composite_stream = b"q\n\
        0.2 0.4 0.9 rg (Company Newsletter Header) Tj\n\
        0.8 0.1 0.1 RG 2.0 w 10 10 200 50 re S\n\
        0.9 0.2 0.1 0.0 k 50 100 100 100 re f\n\
        0.1 0.8 0.3 0.1 k 160 100 100 100 re f\n\
        0.0 0.0 0.0 1.0 k 50 210 210 2 re f\n\
        0.0 g (Standard Body Text in pure black) Tj\n\
        BI /W 4 /H 4 /CS /RGB /BPC 8 ID 0123456789abcdef0123456789abcdef0123456789abcdef EI\n\
        Q\n";

    let filtered_stream = filter_content_stream_bytes(composite_stream);
    let inspector = DefaultStreamInspector::new();
    let violations = inspector.inspect_content_stream(&filtered_stream, 0);
    assert!(
        violations.is_empty(),
        "Violations found in composite mono stream: {:?}",
        violations
    );

    let stream_text = String::from_utf8_lossy(&filtered_stream);
    assert!(!stream_text.contains(" rg"));
    assert!(!stream_text.contains(" RG"));
    assert!(!stream_text.contains("/CS /RGB"));

    // 3. Image XObject Luminance Re-encoding: 120x120 8-bit RGB photo
    let raw_photo_rgb = vec![180u8; 120 * 120 * 3];
    let reencoded_photo_gray: Vec<u8> = raw_photo_rgb
        .chunks_exact(3)
        .map(|rgb| ((299 * rgb[0] as u32 + 587 * rgb[1] as u32 + 114 * rgb[2] as u32 + 500) / 1000) as u8)
        .collect();
    assert_eq!(reencoded_photo_gray.len(), 120 * 120);

    let photo_dict = "<< /Type /XObject /Subtype /Image /Width 120 /Height 120 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let photo_res = inspector.inspect_image_xobject("NewsPhoto", photo_dict, &reencoded_photo_gray, 0);
    assert!(photo_res.is_ok(), "NewsPhoto gray re-encoding invalid: {:?}", photo_res);
}
