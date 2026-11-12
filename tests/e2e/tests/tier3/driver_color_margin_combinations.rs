//! Tier 3 Combinatorial Pairwise Interactions: Tests 1 to 9
//!
//! Evaluates orthogonal pairwise interactions across 3 architectural axes:
//! 1. Driver Generation: Win32 V3 DEVMODE, Win32 V4 XPS PrintTicket, CUPS/IPP 2.0
//! 2. Color Mode: Full Color, Dual-Layer K-Only Grayscale, Image XObject 8-bit Luminance
//! 3. Margin Engine: Fit to Printable Area, Shrink Oversized, Actual Size (1:1) with Clipping Hazard Detection
//!
//! Follows an exact Taguchi L9(3^3) orthogonal array providing 100% pairwise coverage.

use pdfcraft_e2e::margin_assertions::{
    calculate_page_transform, ClippingExtent, MarginAssertions, PageFitMode,
};
use pdfcraft_e2e::mock_driver::{
    create_mock_virtual_pdf_printer,
    DuplexBinding, HardwareMargins, MockIppDriver, MockV3Driver,
    MockV3DriverConfig, MockV4Driver, MockV4DriverConfig, PrintableBounds,
    DMCOLOR_COLOR, DMCOLOR_MONOCHROME,
    DM_COLOR, DM_IN_BUFFER, DM_OUT_BUFFER,
};
use pdfcraft_e2e::stream_inspector::{
    DefaultStreamInspector, PdfStreamInspector, StreamAssertions,
};
use pdfcraft_platform::cups::{IppMessageWriter, IppOperation, IppTag};
use pdfcraft_print::monochrome::MonochromeVectorFilter;
use std::collections::HashMap;
use std::io::Write;

/// Helper: passes raw content stream bytes through MonochromeVectorFilter
fn filter_content_stream_bytes(input: &[u8]) -> Vec<u8> {
    let parsed = pdfcraft_content::parse(input);
    let filtered_ops = MonochromeVectorFilter::filter_operators(&parsed.ops);
    pdfcraft_content::serialize_ops(&filtered_ops)
}

// =========================================================================
// TEST 1: V3 DEVMODE × Full Color × Fit to Printable Area
// =========================================================================

#[test]
fn tier3_test_01_v3_devmode_color_fit_margin_interaction() {
    // 1. Driver Setup: Epson V3 Driver with 3704-byte vendor private extra payload
    let mock = MockV3Driver::new(MockV3DriverConfig {
        printer_name: "EPSON_WF_C579R_V3_Color".into(),
        driver_extra_size: 3704,
        vendor_canary_pattern: vec![0xAA, 0x55, 0xDE, 0xAD, 0xBE, 0xEF],
        default_color: true,
        bounds: PrintableBounds::a4_standard_margins(),
        ..Default::default()
    });

    // 2-pass DEVMODE negotiation: Pass 1 query required buffer size
    let needed = mock.document_properties_pass1() as usize;
    assert_eq!(needed, 220 + 3704);

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Pass 2 default population failed");

    // Assert Full Color settings into DEVMODE
    devmode_buf[72..76].copy_from_slice(&DM_COLOR.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_COLOR.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Pass 2 submit failed");

    mock.assert_pass1_called();
    mock.assert_driver_extra_preserved();
    let last = mock.last_submitted_devmode().expect("Submitted DEVMODE missing");
    assert_ne!(last.dm_fields & DM_COLOR, 0);
    assert_eq!(last.dm_color, Some(DMCOLOR_COLOR));

    // 2. Vector Stream: Full color RGB vector drawing
    let color_stream = b"q 1.0 0.2 0.5 rg 10 10 200 150 re f 0.1 0.8 0.3 RG 2.0 w 5 5 190 140 re S Q\n";
    let inspector = DefaultStreamInspector::new();
    let violations = inspector.inspect_content_stream(color_stream, 0);
    assert_eq!(violations.len(), 2, "Color stream must contain RGB operators");

    // 3. Margin Engine: Fit to Printable Area on oversized document (650 x 900 pt)
    let page_size = (650.0, 900.0);
    let printable = PrintableBounds::a4_standard_margins(); // 578.4 x 824.88 pt printable
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Expected scale: min(578.4 / 650.0, 824.88 / 900.0) = min(0.889846, 0.916533) = 0.889846
    assert!((transform.scale - 0.889846).abs() < 1e-4);
    assert!((transform.dx_pt - 0.0).abs() < 1e-4);
    assert!((transform.dy_pt - 12.0092).abs() < 1e-3);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

// =========================================================================
// TEST 2: V3 DEVMODE × Dual-Layer K-Only × Shrink Oversized
// =========================================================================

#[test]
fn tier3_test_02_v3_devmode_k_only_shrink_margin_interaction() {
    // 1. Driver Setup: HP Enterprise V3 Driver with 4096-byte vendor extra
    let mock = MockV3Driver::new(MockV3DriverConfig {
        printer_name: "HP_LaserJet_Enterprise_V3".into(),
        driver_extra_size: 4096,
        vendor_canary_pattern: vec![0xCA, 0xFE, 0xBA, 0xBE],
        default_color: false,
        bounds: PrintableBounds::a4_standard_margins(),
        ..Default::default()
    });

    let needed = mock.document_properties_pass1() as usize;
    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();

    // Layer 1 Protection: Assert DMCOLOR_MONOCHROME
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

    // 2. Layer 2 Protection: In-memory vector stream transformation
    let chromatic_stream = b"0.8 0.4 0.2 0.1 k 10 10 100 100 re f 0.9 0.1 0.1 rg 50 50 80 80 re f\n";
    let filtered_stream = filter_content_stream_bytes(chromatic_stream);

    let inspector = DefaultStreamInspector::new();
    let violations = inspector.inspect_content_stream(&filtered_stream, 0);
    assert!(violations.is_empty(), "Violations in filtered mono stream: {:?}", violations);

    let stream_text = String::from_utf8_lossy(&filtered_stream);
    assert!(!stream_text.contains(" rg"));
    assert!(stream_text.contains(" g\n") || stream_text.contains(" g "));
    assert!(!stream_text.contains("0.8 0.4 0.2")); // Chromatic CMYK converted

    // 3. Margin Engine: Shrink Oversized on oversized CAD blueprint (800 x 1100 pt)
    let page_size = (800.0, 1100.0);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Expected scale: min(578.4 / 800.0, 824.88 / 1100.0) = min(0.7230, 0.74989) = 0.7230
    assert!((transform.scale - 0.7230).abs() < 1e-4);
    assert!((transform.dx_pt - 0.0).abs() < 1e-4);
    assert!((transform.dy_pt - 14.79).abs() < 1e-2);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

// =========================================================================
// TEST 3: V3 DEVMODE × Image XObject Luminance × Actual Size (1:1)
// =========================================================================

#[test]
fn tier3_test_03_v3_devmode_image_xobject_actual_size_interaction() {
    // 1. Driver Setup: Canon V3 Driver (512-byte extra, 8.4pt standard margins)
    let mock = MockV3Driver::new(MockV3DriverConfig {
        printer_name: "Canon_LBP_Mono_V3".into(),
        driver_extra_size: 512,
        vendor_canary_pattern: vec![0x11, 0x22, 0x33, 0x44],
        default_color: false,
        bounds: PrintableBounds::a4_standard_margins(),
        ..Default::default()
    });

    let needed = mock.document_properties_pass1() as usize;
    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();
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

    // 2. Image XObject Luminance Re-encoding (200x200 RGB photo -> 8-bit DeviceGray)
    let inspector = DefaultStreamInspector::new();
    let dict = "<< /Type /XObject /Subtype /Image /Width 200 /Height 200 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let gray_pixels = vec![128u8; 40000]; // 200 * 200 * 1 byte
    let img_res = inspector.inspect_image_xobject("ImPhoto", dict, &gray_pixels, 0);
    assert!(img_res.is_ok(), "Grayscale Image XObject inspection failed: {:?}", img_res);

    // 3. Margin Engine: Actual Size (1:1) with Bleed Document (595.20 x 841.68 pt)
    let page_size = (595.20, 841.68);
    let printable = PrintableBounds::a4_standard_margins(); // Physical: 595.20 x 841.68, Margins: 8.4pt
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!(transform.clipping.is_clipped(1e-4));
    let expected_clipping = ClippingExtent {
        left_pt: 8.40,
        top_pt: 8.40,
        right_pt: 8.40,
        bottom_pt: 8.40,
    };
    MarginAssertions::assert_clipping_hazard(&transform.clipping, &expected_clipping);
    assert!((transform.clipping.total_pt() - 33.60).abs() < 1e-3);
}

// =========================================================================
// TEST 4: V4 XPS PrintTicket × Full Color × Shrink Oversized
// =========================================================================

#[test]
fn tier3_test_04_v4_printticket_color_shrink_margin_interaction() {
    // 1. Driver Setup: Modern Win32 V4 XPS Driver
    let mock = MockV4Driver::new(MockV4DriverConfig {
        printer_name: "XPS_Color_V4_Printer".into(),
        supports_color: true,
        supports_monochrome: true,
        supports_duplex: true,
        bounds: PrintableBounds::a4_standard_margins(),
    });
    mock.pt_open_provider().expect("Open V4 provider");

    // Delta PrintTicket requesting Full Color
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Color"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock.pt_merge_and_validate(None, delta).expect("Merge color ticket");
    assert!(merged.contains("psk:Color"));
    mock.assert_no_concurrency_violations();

    // 2. Vector Stream: Full color vector operators untouched
    let color_stream = b"0.2 0.7 0.9 rg 20 20 150 150 re f\n";
    let inspector = DefaultStreamInspector::new();
    let violations = inspector.inspect_content_stream(color_stream, 0);
    assert_eq!(violations.len(), 1, "Expected color operator detected");

    // 3. Margin Engine: Shrink Oversized on Undersized Document (400 x 600 pt)
    let page_size = (400.0, 600.0);
    let printable = PrintableBounds::a4_standard_margins(); // Printable: 578.4 x 824.88
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Shrink Oversized does NOT enlarge undersized pages -> scale must remain exactly 1.0
    assert_eq!(transform.scale, 1.0);
    assert!((transform.dx_pt - 89.20).abs() < 1e-4);
    assert!((transform.dy_pt - 112.44).abs() < 1e-4);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

// =========================================================================
// TEST 5: V4 XPS PrintTicket × Dual-Layer K-Only × Actual Size (1:1) Borderless
// =========================================================================

#[test]
fn tier3_test_05_v4_printticket_k_only_actual_size_interaction() {
    // 1. Driver Setup: Borderless Virtual PDF Printer (0pt margins)
    let mock = create_mock_virtual_pdf_printer("Virtual_PDF_Writer");
    mock.pt_open_provider().unwrap();

    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Monochrome"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let _merged = mock.pt_merge_and_validate(None, delta).unwrap();
    // Virtual printer resolves monochrome conflict if unsupported, but in-memory engine guarantees K-only
    mock.assert_no_concurrency_violations();

    // 2. Vector Stream: Filter complex RGB & CMYK to pure K-only
    let input_stream = b"0.1 0.2 0.3 rg 10 10 50 50 re f 0.4 0.3 0.2 0.1 k 60 60 40 40 re f\n";
    let filtered_stream = filter_content_stream_bytes(input_stream);
    StreamAssertions::assert_content_stream_k_only(&filtered_stream);

    // 3. Margin Engine: Actual Size (1:1) on Borderless Printer (595.32 x 841.92 pt)
    let page_size = (595.32, 841.92);
    let printable = PrintableBounds::a4_borderless();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert_eq!(transform.dx_pt, 0.0);
    assert_eq!(transform.dy_pt, 0.0);
    assert!(!transform.clipping.is_clipped(1e-4));
    assert_eq!(transform.clipping.total_pt(), 0.0);
}

// =========================================================================
// TEST 6: V4 XPS PrintTicket × Image XObject Luminance × Fit to Printable Area
// =========================================================================

#[test]
fn tier3_test_06_v4_printticket_image_xobject_fit_margin_interaction() {
    // 1. Driver Setup: Office V4 XPS Printer with monochrome support
    let mock = MockV4Driver::new(MockV4DriverConfig {
        printer_name: "Workgroup_XPS_Mono".into(),
        supports_color: true,
        supports_monochrome: true,
        supports_duplex: true,
        bounds: PrintableBounds::a4_standard_margins(),
    });
    mock.pt_open_provider().unwrap();

    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Monochrome"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged.contains("psk:Monochrome"));

    // 2. Image XObject: Flate-compressed 8-bit luminance image (400x200 = 80,000 bytes)
    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    let raw_gray = vec![180u8; 80000];
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw_gray).unwrap();
    let compressed_gray = encoder.finish().unwrap();

    let dict = "<< /Type /XObject /Subtype /Image /Width 400 /Height 200 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode >>";
    let inspector = DefaultStreamInspector::new();
    let img_res = inspector.inspect_image_xobject("ImFlateBanner", dict, &compressed_gray, 0);
    assert!(img_res.is_ok(), "Flate Image XObject validation failed: {:?}", img_res);

    // 3. Margin Engine: Fit to Printable Area on Wide Landscape Banner (1000 x 400 pt)
    let page_size = (1000.0, 400.0);
    let printable = PrintableBounds::a4_standard_margins(); // Printable: 578.40 x 824.88
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Expected scale: min(578.4 / 1000.0, 824.88 / 400.0) = min(0.5784, 2.0622) = 0.5784
    assert!((transform.scale - 0.5784).abs() < 1e-4);
    assert!((transform.dx_pt - 0.0).abs() < 1e-4);
    assert!((transform.dy_pt - 296.76).abs() < 1e-3);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

// =========================================================================
// TEST 7: CUPS / IPP 2.0 × Full Color × Actual Size (1:1) Asymmetric Margins
// =========================================================================

#[test]
fn tier3_test_07_cups_ipp_color_actual_size_interaction() {
    // 1. Driver Setup: POS Thermal Receipt Printer with Asymmetric Margins
    let mock = MockIppDriver::new("POS_Thermal_Receipt_Color", false);

    let mut writer = IppMessageWriter::new(IppOperation::PrintJob, 201);
    writer.start_group(IppTag::OperationAttributes);
    writer.add_attribute_str(IppTag::Charset, "attributes-charset", "utf-8");
    writer.add_attribute_str(IppTag::NaturalLanguage, "attributes-natural-language", "en");
    writer.add_attribute_str(IppTag::NameWithoutLanguage, "print-color-mode", "color");
    writer.add_attribute_str(IppTag::Keyword, "sides", "one-sided");
    writer.end_attributes();
    let wire_bytes = writer.finish();

    let job_id = mock
        .submit_ipp_wire_job(&wire_bytes)
        .expect("Submit IPP wire job failed");
    assert!(job_id >= 1001);
    mock.assert_attribute("print-color-mode", "color");

    // 2. Vector Stream: Retains color graphics
    let color_stream = b"0.8 0.1 0.1 rg 10 10 180 50 re f\n";
    let inspector = DefaultStreamInspector::new();
    let violations = inspector.inspect_content_stream(color_stream, 0);
    assert_eq!(violations.len(), 1);

    // 3. Margin Engine: Actual Size on Asymmetric Margins (220 x 800 pt document)
    // Physical: 226.64 x 841.66 pt. Margins: Left 10.64, Top 0.0, Right 11.35, Bottom 0.35 pt
    let printable = PrintableBounds {
        printable_width_pt: 204.65,
        printable_height_pt: 841.30,
        physical_width_pt: 226.64,
        physical_height_pt: 841.66,
        dpi_x: 203,
        dpi_y: 203,
        margins: HardwareMargins {
            left_pt: 10.64,
            top_pt: 0.0,
            right_pt: 11.35,
            bottom_pt: 0.35,
        },
    };
    let page_size = (220.0, 800.0);
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!(transform.clipping.is_clipped(1e-4));

    // Centered: x0 = (226.64 - 220) / 2 = 3.32 pt.
    // Left clip = max(10.64 - 3.32, 0) = 7.32 pt.
    // Right clip = max(3.32 + 220 - (226.64 - 11.35), 0) = max(223.32 - 215.29, 0) = 8.03 pt.
    let expected_clipping = ClippingExtent {
        left_pt: 7.32,
        top_pt: 0.0,
        right_pt: 8.03,
        bottom_pt: 0.0,
    };
    MarginAssertions::assert_clipping_hazard(&transform.clipping, &expected_clipping);
}

// =========================================================================
// TEST 8: CUPS / IPP 2.0 × Dual-Layer K-Only × Fit to Printable Area
// =========================================================================

#[test]
fn tier3_test_08_cups_ipp_k_only_fit_margin_interaction() {
    // 1. Driver Setup: High-speed CUPS network laser printer
    let mock = MockIppDriver::new("Enterprise_CUPS_Laser", true);

    let mut attrs = HashMap::new();
    attrs.insert("print-color-mode".to_string(), "monochrome".to_string());
    attrs.insert("sides".to_string(), "one-sided".to_string());
    attrs.insert("media".to_string(), "iso_a4_210x297mm".to_string());

    let job_id = mock
        .submit_ipp_job(attrs, b"%PDF-1.7 enterprise CAD")
        .expect("Submit mono IPP job");
    assert!(job_id >= 1001);
    mock.assert_attribute("print-color-mode", "monochrome");

    // 2. Vector Stream: Convert multi-color CAD drawing to pure K-only
    let cad_stream = b"0.0 0.8 0.2 RG 1.5 w 10 10 300 200 re S 0.7 0.1 0.9 rg 20 20 100 100 re f 0.2 0.3 0.4 0.5 K\n";
    let filtered_stream = filter_content_stream_bytes(cad_stream);
    StreamAssertions::assert_content_stream_k_only(&filtered_stream);

    // 3. Margin Engine: Fit to Printable Area on Oversized A3 CAD sheet (1190.55 x 841.89 pt)
    let page_size = (1190.55, 841.89);
    let printable = PrintableBounds::a4_standard_margins(); // Printable: 578.40 x 824.88
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Expected scale: min(578.40 / 1190.55, 824.88 / 841.89) = min(0.485826, 0.979796) = 0.485826
    assert!((transform.scale - 0.485826).abs() < 1e-4);
    assert!((transform.dx_pt - 0.0).abs() < 1e-4);
    assert!((transform.dy_pt - 207.934).abs() < 1e-2);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

// =========================================================================
// TEST 9: CUPS / IPP 2.0 × Image XObject Luminance × Shrink Oversized
// =========================================================================

#[test]
fn tier3_test_09_cups_ipp_image_xobject_shrink_margin_interaction() {
    // 1. Driver Setup: CUPS printer configured with US Letter standard margins
    let mock = MockIppDriver::new("Departmental_IPP_USLetter", true);

    let mut attrs = HashMap::new();
    attrs.insert("print-color-mode".to_string(), "monochrome".to_string());
    attrs.insert("media".to_string(), "na_letter_8.5x11in".to_string());
    let job_id = mock.submit_ipp_job(attrs, b"%PDF-1.7 photo tabloid").unwrap();
    assert!(job_id >= 1001);
    mock.assert_attribute("print-color-mode", "monochrome");

    // 2. Image XObject: 32-bit CMYK converted to 8-bit DeviceGray (100x100 = 10,000 bytes)
    let inspector = DefaultStreamInspector::new();
    let dict = "<< /Type /XObject /Subtype /Image /Width 100 /Height 100 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let gray_pixels = vec![96u8; 10000];
    let img_res = inspector.inspect_image_xobject("ImCmykPhoto", dict, &gray_pixels, 0);
    assert!(img_res.is_ok(), "CMYK to Gray Image XObject failed: {:?}", img_res);

    // 3. Margin Engine: Shrink Oversized on Ledger/Tabloid page (792.0 x 1224.0 pt)
    let printable = PrintableBounds {
        printable_width_pt: 576.0,
        printable_height_pt: 756.0,
        physical_width_pt: 612.0,
        physical_height_pt: 792.0,
        dpi_x: 300,
        dpi_y: 300,
        margins: HardwareMargins {
            left_pt: 18.0,
            top_pt: 18.0,
            right_pt: 18.0,
            bottom_pt: 18.0,
        },
    };
    let page_size = (792.0, 1224.0);
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Expected scale: min(576.0 / 792.0, 756.0 / 1224.0) = min(0.72727, 0.617647) = 0.617647
    assert!((transform.scale - 0.617647).abs() < 1e-4);
    assert!((transform.dx_pt - 43.412).abs() < 1e-2);
    assert!((transform.dy_pt - 0.0).abs() < 1e-4);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}
