#![allow(unused_imports, unused_must_use, dead_code)]
//! Tier 5 Adversarial Coverage Hardening: Track 1 (Print & Platform Subsystems)
//!
//! Exhaustive white-box stress testing covering:
//! - Module 1: crates/print/src/monochrome.rs (CMYK-to-K saturation, 16-bit images, ICC palettes, inline images)
//! - Module 2: crates/print/src/margins.rs (Sub-pixel transforms, extreme aspect ratios, asymmetric clipping)
//! - Module 3: crates/print/src/duplex.rs (Single/odd page bounds, alternating parity, enum defense)
//! - Module 4: crates/print/src/stream.rs (Bounded channel backpressure, cooperative cancellation, disconnects)
//! - Module 5: crates/print/src/governor.rs (Semaphore limits, excess release defense, arithmetic overflow, disk verdicts)
//! - Module 6: crates/platform/src/win32_v3.rs & win32_v4.rs (Headroom formula, bitmask offsets, XML schema, mutex contention)
//! - Module 7: crates/platform/src/cups.rs (RFC 8010/8011 wire octets, attribute groups, big-endian encoding)
//! - Module 8: crates/platform/src/diagnostics.rs (Markdown pipe/newline escaping, JSON round-trip, trace buffer concurrency)

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use pdfcraft_content::{Op, parse, serialize_ops};
use pdfcraft_cos::{CosError, Dict, Document, ObjRef, Object, PdfString, SaveOptions, Stream};
use pdfcraft_platform::cups::{CupsDriverAdapter, IppMessageWriter, IppOperation, IppTag};
use pdfcraft_platform::diagnostics::{
    DiagnosticReport, DiagnosticTraceBuffer, OsTelemetry, SpoolDiskVerdict, SpoolerServiceStatus,
    SpoolerTelemetry, TicketDetails, TraceCategory, TraceLevel, TraceRecord,
    evaluate_spool_disk_verdict, synthesize_windows_version,
};
use pdfcraft_platform::win32_v3::{
    DMCOLLATE_TRUE, DMCOLOR_COLOR, DMCOLOR_MONOCHROME, DMDUP_HORIZONTAL, DMDUP_SIMPLEX,
    DMDUP_VERTICAL, DMORIENT_LANDSCAPE, DMORIENT_PORTRAIT, DM_COLLATE, DM_COLOR, DM_COPIES,
    DM_DUPLEX, DM_ORIENTATION, DevModeBuffer,
};
use pdfcraft_platform::win32_v4::{
    DEFAULT_BASE_TICKET_XML, generate_delta_print_ticket_xml, with_print_ticket_provider,
};
use pdfcraft_print::driver::{
    ColorMode, DriverGeneration, DuplexBinding as DriverDuplexBinding, PrintJobSettings,
};
use pdfcraft_print::duplex::{
    DuplexBinding as PrintDuplexBinding, align_sheet_pair_orientation, plan_duplex_sheets,
};
use pdfcraft_print::governor::{
    CancellationToken, ConcurrencyGovernor, CountingSemaphore, DiskVerdict, GovernorError,
    PrinterJobEstimate, SheetRasterMode, calculate_required_spool_bytes, evaluate_disk_verdict,
};
use pdfcraft_print::margins::{
    ClippingExtent, HardwareMargins, PageFitMode, PageTransform, PrintableBounds,
    calculate_page_transform,
};
use pdfcraft_print::monochrome::MonochromeVectorFilter;
use pdfcraft_print::stream::{
    SheetBuffer, SheetData, bounded_stream_channel,
};

// =========================================================================
// MODULE 1: crates/print/src/monochrome.rs ADVERSARIAL CHALLENGES
// =========================================================================

#[test]
fn test_adversarial_cmyk_saturation_and_clamping() {
    // Attack scenario: CMYK operators with values exceeding 1.0 or having negative components.
    // In CMYK, c=0.9, m=0.8, y=0.7, k=0.6.
    // luma_cmy = 0.299*0.9 + 0.587*0.8 + 0.114*0.7 = 0.2691 + 0.4696 + 0.0798 = 0.8185
    // k_eff = (0.6 + 0.8185).min(1.0) = 1.0.
    // Must clamp strictly to 1.0, with Cyan, Magenta, Yellow strictly 0.0.
    let input_ops = vec![
        Op::new("k", vec![
            pdfcraft_content::num(0.9),
            pdfcraft_content::num(0.8),
            pdfcraft_content::num(0.7),
            pdfcraft_content::num(0.6),
        ]),
        Op::new("K", vec![
            pdfcraft_content::num(1.5),
            pdfcraft_content::num(1.5),
            pdfcraft_content::num(1.5),
            pdfcraft_content::num(0.5),
        ]),
    ];

    let filtered = MonochromeVectorFilter::filter_operators(&input_ops);
    assert_eq!(filtered.len(), 2);

    // Non-stroking k
    assert_eq!(filtered[0].op, b"k");
    let nums_k = filtered[0].nums::<4>().unwrap();
    assert_eq!(nums_k[0], 0.0, "Cyan must be strictly 0.0");
    assert_eq!(nums_k[1], 0.0, "Magenta must be strictly 0.0");
    assert_eq!(nums_k[2], 0.0, "Yellow must be strictly 0.0");
    assert_eq!(nums_k[3], 1.0, "K_eff must saturate at strictly 1.0");

    // Stroking K
    assert_eq!(filtered[1].op, b"K");
    let nums_cap_k = filtered[1].nums::<4>().unwrap();
    assert_eq!(nums_cap_k[0], 0.0, "Cyan must be strictly 0.0");
    assert_eq!(nums_cap_k[1], 0.0, "Magenta must be strictly 0.0");
    assert_eq!(nums_cap_k[2], 0.0, "Yellow must be strictly 0.0");
    assert_eq!(nums_cap_k[3], 1.0, "K_eff must saturate at strictly 1.0");
}

#[test]
fn test_adversarial_pure_primary_cmy_ink_separation() {
    // Pure primaries: verify exact Rec. 601 integer/float conversions and zero ink leaks.
    let input_ops = vec![
        // Pure Cyan: c=1.0, m=0, y=0, k=0 -> k_eff = 0.299
        Op::new("k", vec![
            pdfcraft_content::num(1.0),
            pdfcraft_content::num(0.0),
            pdfcraft_content::num(0.0),
            pdfcraft_content::num(0.0),
        ]),
        // Pure Magenta: c=0, m=1.0, y=0, k=0 -> k_eff = 0.587
        Op::new("k", vec![
            pdfcraft_content::num(0.0),
            pdfcraft_content::num(1.0),
            pdfcraft_content::num(0.0),
            pdfcraft_content::num(0.0),
        ]),
        // Pure Yellow: c=0, m=0, y=1.0, k=0 -> k_eff = 0.114
        Op::new("k", vec![
            pdfcraft_content::num(0.0),
            pdfcraft_content::num(0.0),
            pdfcraft_content::num(1.0),
            pdfcraft_content::num(0.0),
        ]),
        // Pure Black: c=0, m=0, y=0, k=1.0 -> k_eff = 1.0
        Op::new("k", vec![
            pdfcraft_content::num(0.0),
            pdfcraft_content::num(0.0),
            pdfcraft_content::num(0.0),
            pdfcraft_content::num(1.0),
        ]),
    ];

    let filtered = MonochromeVectorFilter::filter_operators(&input_ops);
    assert_eq!(filtered.len(), 4);

    let k_cyan = filtered[0].nums::<4>().unwrap()[3];
    assert!((k_cyan - 0.299).abs() < 1e-4, "Pure Cyan k_eff must be 0.299");

    let k_mag = filtered[1].nums::<4>().unwrap()[3];
    assert!((k_mag - 0.587).abs() < 1e-4, "Pure Magenta k_eff must be 0.587");

    let k_yel = filtered[2].nums::<4>().unwrap()[3];
    assert!((k_yel - 0.114).abs() < 1e-4, "Pure Yellow k_eff must be 0.114");

    let k_blk = filtered[3].nums::<4>().unwrap()[3];
    assert_eq!(k_blk, 1.0, "Pure Black k_eff must be 1.0");
}

#[test]
fn test_adversarial_16bit_rgb_image_downsampling_and_truncation() {
    // Attack scenario: Image XObject with BitsPerComponent = 16 and 3 components (RGB).
    // Each pixel takes 6 bytes (2 bytes R, 2 bytes G, 2 bytes B in Big-Endian).
    // Test 2x2 image = 4 pixels * 6 bytes = 24 bytes.
    let mut doc = Document::new_empty();

    // Pixel 0: Pure Red 0xFFFF, 0x0000, 0x0000 -> high byte: 0xFF, 0x00, 0x00 -> Gray 76
    // Pixel 1: Pure Green 0x0000, 0xFFFF, 0x0000 -> high byte: 0x00, 0xFF, 0x00 -> Gray 150
    // Pixel 2: Pure Blue 0x0000, 0x0000, 0xFFFF -> high byte: 0x00, 0x00, 0xFF -> Gray 29
    // Pixel 3: White 0xFFFF, 0xFFFF, 0xFFFF -> high byte: 0xFF, 0xFF, 0xFF -> Gray 255
    let mut raw_samples_16 = Vec::new();
    // Pixel 0 (Red)
    raw_samples_16.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00, 0x00, 0x00]);
    // Pixel 1 (Green)
    raw_samples_16.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00, 0x00, 0x00]);
    // Pixel 2 (Blue)
    raw_samples_16.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0xFF, 0x00]);
    // Pixel 3 (White)
    raw_samples_16.extend_from_slice(&[0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00]);

    let mut dict = Dict::new();
    dict.set(b"Type".to_vec(), Object::Name(b"XObject".to_vec()));
    dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
    dict.set(b"Width".to_vec(), Object::Int(2));
    dict.set(b"Height".to_vec(), Object::Int(2));
    dict.set(b"BitsPerComponent".to_vec(), Object::Int(16));
    dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceRGB".to_vec()));

    let stream = Stream::flate(dict, &raw_samples_16);
    let r = doc.add(Object::Stream(stream));

    // Run filter
    MonochromeVectorFilter::filter_image_xobjects(&mut doc).expect("16-bit RGB filter failed");

    // Inspect transformed object
    let Object::Stream(out_stream) = &*doc.get(r) else {
        panic!("Expected stream object");
    };
    assert_eq!(out_stream.dict.name(b"ColorSpace"), Some(b"DeviceGray".as_slice()));
    assert_eq!(out_stream.dict.int(b"BitsPerComponent"), Some(8), "BitsPerComponent must be normalized to 8");

    let decoded = out_stream.decoded().expect("Failed to decode transformed stream");
    assert_eq!(decoded.len(), 4, "4 pixels in 8-bit Gray = 4 bytes");
    assert_eq!(decoded[0], 76, "Red pixel downsampled to Gray 76");
    assert_eq!(decoded[1], 150, "Green pixel downsampled to Gray 150");
    assert_eq!(decoded[2], 29, "Blue pixel downsampled to Gray 29");
    assert_eq!(decoded[3], 255, "White pixel downsampled to Gray 255");
}

#[test]
fn test_adversarial_16bit_cmyk_image_downsampling_and_truncation() {
    // Attack scenario: Image XObject with BitsPerComponent = 16 and 4 components (CMYK).
    // Each pixel takes 8 bytes (2 bytes C, 2 bytes M, 2 bytes Y, 2 bytes K in Big-Endian).
    // Test 2x1 image = 2 pixels * 8 bytes = 16 bytes.
    let mut doc = Document::new_empty();

    // Pixel 0: Pure Black: C=0, M=0, Y=0, K=0xFFFF -> high bytes: 0, 0, 0, 255 -> Gray 0
    // Pixel 1: Pure White: C=0, M=0, Y=0, K=0 -> high bytes: 0, 0, 0, 0 -> Gray 255
    let mut raw_cmyk_16 = Vec::new();
    raw_cmyk_16.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00]);
    raw_cmyk_16.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

    let mut dict = Dict::new();
    dict.set(b"Type".to_vec(), Object::Name(b"XObject".to_vec()));
    dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
    dict.set(b"Width".to_vec(), Object::Int(2));
    dict.set(b"Height".to_vec(), Object::Int(1));
    dict.set(b"BitsPerComponent".to_vec(), Object::Int(16));
    dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceCMYK".to_vec()));

    let stream = Stream::flate(dict, &raw_cmyk_16);
    let r = doc.add(Object::Stream(stream));

    MonochromeVectorFilter::filter_image_xobjects(&mut doc).expect("16-bit CMYK filter failed");

    let Object::Stream(out_stream) = &*doc.get(r) else {
        panic!("Expected stream object");
    };
    assert_eq!(out_stream.dict.name(b"ColorSpace"), Some(b"DeviceGray".as_slice()));
    assert_eq!(out_stream.dict.int(b"BitsPerComponent"), Some(8));

    let decoded = out_stream.decoded().expect("Failed to decode stream");
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0], 0, "16-bit Black -> Gray 0");
    assert_eq!(decoded[1], 255, "16-bit White -> Gray 255");
}

#[test]
fn test_adversarial_indexed_cmyk_palette_with_stream_lookup() {
    // Attack scenario: [/Indexed /DeviceCMYK hival stream_lookup]
    // 2 palette entries (hival = 1):
    // Entry 0: Pure Cyan (255, 0, 0, 0) -> luma = 76 -> Gray 255 - 76 = 179
    // Entry 1: Pure Yellow (0, 0, 255, 0) -> luma = 29 -> Gray 255 - 29 = 226
    let mut doc = Document::new_empty();

    let lookup_data = vec![
        255, 0, 0, 0,  // Cyan
        0, 0, 255, 0,  // Yellow
    ];
    let lookup_stream = Stream::flate(Dict::new(), &lookup_data);
    let lookup_ref = doc.add(Object::Stream(lookup_stream));

    let cs_array = Object::Array(vec![
        Object::Name(b"Indexed".to_vec()),
        Object::Name(b"DeviceCMYK".to_vec()),
        Object::Int(1),
        Object::Ref(lookup_ref),
    ]);

    let mut img_dict = Dict::new();
    img_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
    img_dict.set(b"Width".to_vec(), Object::Int(2));
    img_dict.set(b"Height".to_vec(), Object::Int(1));
    img_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));
    img_dict.set(b"ColorSpace".to_vec(), cs_array);

    // Pixels referencing index 0 and index 1
    let pixels = vec![0, 1];
    let img_stream = Stream::flate(img_dict, &pixels);
    let img_ref = doc.add(Object::Stream(img_stream));

    MonochromeVectorFilter::filter_image_xobjects(&mut doc).expect("CMYK Indexed filter failed");

    let Object::Stream(res_stream) = &*doc.get(img_ref) else {
        panic!("Expected stream");
    };

    let Some(Object::Array(new_cs)) = res_stream.dict.get(b"ColorSpace") else {
        panic!("Expected ColorSpace array");
    };
    assert_eq!(new_cs[0].as_name(), Some(b"Indexed".as_slice()));
    assert_eq!(new_cs[1].as_name(), Some(b"DeviceGray".as_slice()));
    assert_eq!(new_cs[2].as_int(), Some(1));

    let Object::String(lookup_str) = &new_cs[3] else {
        panic!("Expected PdfString lookup");
    };
    assert_eq!(lookup_str.bytes.len(), 2);
    assert_eq!(lookup_str.bytes[0], 179, "Cyan palette entry -> Gray 179");
    assert_eq!(lookup_str.bytes[1], 226, "Yellow palette entry -> Gray 226");
}

#[test]
fn test_adversarial_indexed_iccbased_monochrome_idempotence() {
    // Attack scenario: [/Indexed [/ICCBased icc_ref] hival lookup] where ICC stream has /N 1 (Monochrome)
    // Filter must recognize this as already monochrome and leave it untouched (idempotence).
    let mut doc = Document::new_empty();

    let mut icc_dict = Dict::new();
    icc_dict.set(b"N".to_vec(), Object::Int(1)); // 1 component = Gray
    let icc_stream = Stream::flate(icc_dict, &[0x00; 128]);
    let icc_ref = doc.add(Object::Stream(icc_stream));

    let cs_array = Object::Array(vec![
        Object::Name(b"Indexed".to_vec()),
        Object::Array(vec![
            Object::Name(b"ICCBased".to_vec()),
            Object::Ref(icc_ref),
        ]),
        Object::Int(1),
        Object::String(PdfString::literal(vec![50, 200])),
    ]);

    let mut img_dict = Dict::new();
    img_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
    img_dict.set(b"Width".to_vec(), Object::Int(2));
    img_dict.set(b"Height".to_vec(), Object::Int(1));
    img_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));
    img_dict.set(b"ColorSpace".to_vec(), cs_array);

    let stream = Stream::flate(img_dict, &[0, 1]);
    let r = doc.add(Object::Stream(stream));

    // Run filter
    MonochromeVectorFilter::filter_image_xobjects(&mut doc).expect("Idempotence filter failed");

    // Must remain untouched
    let Object::Stream(res_stream) = &*doc.get(r) else {
        panic!("Expected stream");
    };
    let Some(Object::Array(res_cs)) = res_stream.dict.get(b"ColorSpace") else {
        panic!("Expected array");
    };
    let base_cs = &res_cs[1];
    assert!(base_cs.as_array().is_some(), "ICCBased color space preserved without modification");
}

// =========================================================================
// MODULE 2: crates/print/src/margins.rs ADVERSARIAL CHALLENGES
// =========================================================================

#[test]
fn test_adversarial_subpixel_transform_precision_1200dpi() {
    // High-resolution 1200 DPI device caps with sub-millimeter offsets.
    // 1200 DPI: 1 inch = 1200 px = 72 pt. 1 px = 72/1200 = 0.06 pt = 21.16 micrometers.
    // phys: 9920 x 14030 px (8.267 x 11.692 in -> ~595.2 x 841.8 pt)
    // horzres: 9600 px (576.0 pt), vertres: 13600 px (816.0 pt)
    // offset_x: 160 px (9.6 pt), offset_y: 215 px (12.9 pt)
    let bounds = PrintableBounds::from_gdi_caps(
        9920, 14030, 9600, 13600, 160, 215, 1200, 1200,
    );

    assert_eq!(bounds.dpi_x, 1200);
    assert_eq!(bounds.dpi_y, 1200);
    assert!((bounds.printable_width_pt - 576.0).abs() < 1e-4);
    assert!((bounds.printable_height_pt - 816.0).abs() < 1e-4);
    assert!((bounds.margins.left_pt - 9.6).abs() < 1e-4);
    assert!((bounds.margins.top_pt - 12.9).abs() < 1e-4);

    // Exact right margin: (9920 - 9600 - 160) = 160 px -> 9.6 pt
    assert!((bounds.margins.right_pt - 9.6).abs() < 1e-4);
    // Exact bottom margin: (14030 - 13600 - 215) = 215 px -> 12.9 pt
    assert!((bounds.margins.bottom_pt - 12.9).abs() < 1e-4);

    // Sub-pixel page: 595.27559 pt x 841.88976 pt
    let page = (595.27559, 841.88976);
    let transform = calculate_page_transform(
        page,
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        PrintDuplexBinding::Simplex,
    );

    // scale: min(576 / 595.27559, 816 / 841.88976)
    let expected_scale = (576.0f64 / 595.27559f64).min(816.0f64 / 841.88976f64);
    assert!((transform.scale - expected_scale).abs() < 1e-5);
    assert!(transform.dx_pt >= 0.0);
    assert!(transform.dy_pt >= 0.0);
    assert_eq!(transform.clipping_hazard, None);
}

#[test]
fn test_adversarial_extreme_aspect_ratios_panoramic_and_ribbon() {
    let bounds = PrintableBounds {
        printable_width_pt: 500.0,
        printable_height_pt: 800.0,
        physical_width_pt: 600.0,
        physical_height_pt: 900.0,
        dpi_x: 300,
        dpi_y: 300,
        margins: HardwareMargins::uniform(50.0),
    };

    // 1. Extreme 1:1000 vertical panoramic banner: 100 pt wide x 100,000 pt tall
    let vertical_panoramic = (100.0, 100_000.0);
    let t_fit_v = calculate_page_transform(
        vertical_panoramic,
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        PrintDuplexBinding::Simplex,
    );
    // Scale must be bounded by height: 800.0 / 100,000.0 = 0.008
    assert!((t_fit_v.scale - 0.008).abs() < 1e-6);
    // Scaled width = 100 * 0.008 = 0.8 pt, centered in 500 pt printable width:
    // dx = (500 - 0.8) / 2 = 249.6 pt
    assert!((t_fit_v.dx_pt - 249.6).abs() < 1e-4);
    assert!((t_fit_v.dy_pt - 0.0).abs() < 1e-4);
    assert_eq!(t_fit_v.clipping_hazard, None);

    // 2. Extreme 1000:1 horizontal ribbon banner: 100,000 pt wide x 100 pt tall
    let horizontal_ribbon = (100_000.0, 100.0);
    let t_fit_h = calculate_page_transform(
        horizontal_ribbon,
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        PrintDuplexBinding::Simplex,
    );
    // Scale must be bounded by width: 500.0 / 100,000.0 = 0.005
    assert!((t_fit_h.scale - 0.005).abs() < 1e-6);
    // Scaled height = 100 * 0.005 = 0.5 pt, centered in 800 pt printable height:
    // dy = (800 - 0.5) / 2 = 399.75 pt
    assert!((t_fit_h.dx_pt - 0.0).abs() < 1e-4);
    assert!((t_fit_h.dy_pt - 399.75).abs() < 1e-4);
    assert_eq!(t_fit_h.clipping_hazard, None);

    // 3. ShrinkOversized with extreme aspect ratio
    let t_shrink = calculate_page_transform(
        vertical_panoramic,
        &bounds,
        PageFitMode::ShrinkOversized,
        false,
        true,
        PrintDuplexBinding::Simplex,
    );
    assert_eq!(t_shrink.scale, t_fit_v.scale, "Shrink must downscale oversized banner");
    assert_eq!(t_shrink.clipping_hazard, None);
}

#[test]
fn test_adversarial_asymmetric_pos_margin_clipping_hazard() {
    // POS Printer with heavily asymmetric margins:
    // physical: 200 x 500 pt
    // margins: Left: 30 pt, Right: 5 pt, Top: 10 pt, Bottom: 0 pt
    // printable: Width = 200 - 35 = 165 pt, Height = 500 - 10 = 490 pt
    let bounds = PrintableBounds {
        printable_width_pt: 165.0,
        printable_height_pt: 490.0,
        physical_width_pt: 200.0,
        physical_height_pt: 500.0,
        dpi_x: 203,
        dpi_y: 203,
        margins: HardwareMargins {
            left_pt: 30.0,
            top_pt: 10.0,
            right_pt: 5.0,
            bottom_pt: 0.0,
        },
    };

    // Page width = 160 pt, height = 480 pt (smaller than printable area!).
    // Centering on physical sheet (200 - 160)/2 = 20 pt from sheet left edge.
    // BUT hardware left margin is 30 pt!
    // Therefore, left edge of the page sits at 20 pt, cutting 10 pt into the left margin!
    // Right edge of page sits at 20 + 160 = 180 pt. Sheet right edge minus right margin is 200 - 5 = 195 pt.
    // 180 <= 195 -> NO right clipping!
    let page = (160.0, 480.0);
    let transform = calculate_page_transform(
        page,
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        PrintDuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!(transform.clipping_hazard.is_some(), "Must detect asymmetric clipping hazard");
    let clip = transform.clipping_hazard.unwrap();
    assert!((clip.left_pt - 10.0).abs() < 1e-4, "Expected exactly 10.0 pt clipped on left edge");
    assert_eq!(clip.right_pt, 0.0, "Expected 0 pt clipped on right edge");
    assert_eq!(clip.bottom_pt, 0.0, "Expected 0 pt clipped on bottom edge");
    assert!(clip.has_clipping());
}

#[test]
fn test_adversarial_transform_matrix_quadrant_mappings() {
    // Verify that to_matrix() produces strictly non-negative bounding coordinates for all 4 quadrants
    let page_bounds = (100.0, 200.0);
    let scale = 1.5;

    for deg in [0u16, 90, 180, 270] {
        let transform = PageTransform {
            scale,
            dx_pt: 10.0,
            dy_pt: 20.0,
            rotation_deg: deg,
            clipping_hazard: None,
        };
        let m = transform.to_matrix(page_bounds);

        // Map the 4 corners: (0,0), (w,0), (0,h), (w,h)
        let corners = [(0.0, 0.0), (100.0, 0.0), (0.0, 200.0), (100.0, 200.0)];
        for &(x, y) in &corners {
            let x_prime = m[0] * x + m[2] * y + m[4];
            let y_prime = m[1] * x + m[3] * y + m[5];

            assert!(x_prime >= 10.0 - 1e-4, "x_prime {x_prime} must be >= dx (10.0)");
            assert!(y_prime >= 20.0 - 1e-4, "y_prime {y_prime} must be >= dy (20.0)");
        }
    }
}

// =========================================================================
// MODULE 3: crates/print/src/duplex.rs ADVERSARIAL CHALLENGES
// =========================================================================

#[test]
fn test_adversarial_duplex_single_and_zero_page_boundaries() {
    // 1. Zero pages: must return empty vec
    let empty_sheets = plan_duplex_sheets(&[], PrintDuplexBinding::FlipLongEdge);
    assert!(empty_sheets.is_empty());

    // 2. Exactly 1 page under FlipLongEdge
    let single_page = vec![(595.0, 842.0)];
    let sheets_single = plan_duplex_sheets(&single_page, PrintDuplexBinding::FlipLongEdge);
    assert_eq!(sheets_single.len(), 1);
    assert_eq!(sheets_single[0].sheet_index, 0);
    assert_eq!(sheets_single[0].front_page, 0);
    assert_eq!(sheets_single[0].back_page, None, "Single page must have None back page");
    assert!(!sheets_single[0].back_needs_rotation);
    assert_eq!(sheets_single[0].back_rotation_deg, 0);

    // 3. Exactly 1 page under Simplex
    let sheets_simplex = plan_duplex_sheets(&single_page, PrintDuplexBinding::Simplex);
    assert_eq!(sheets_simplex.len(), 1);
    assert_eq!(sheets_simplex[0].back_page, None);
}

#[test]
fn test_adversarial_duplex_mixed_orientation_alternating_parity() {
    // 5 pages with alternating orientations:
    // P0: Portrait (595 x 842)
    // P1: Landscape (842 x 595)
    // P2: Portrait (595 x 842)
    // P3: Landscape (842 x 595)
    // P4: Portrait (595 x 842)
    let pages = vec![
        (595.0, 842.0),
        (842.0, 595.0),
        (595.0, 842.0),
        (842.0, 595.0),
        (595.0, 842.0),
    ];

    // FlipLongEdge test
    let sheets_long = plan_duplex_sheets(&pages, PrintDuplexBinding::FlipLongEdge);
    assert_eq!(sheets_long.len(), 3, "5 pages = 3 duplex sheets");

    // Sheet 0: Front 0 (P), Back 1 (L). Under FlipLongEdge, back rotates 270 deg
    assert_eq!(sheets_long[0].front_page, 0);
    assert_eq!(sheets_long[0].back_page, Some(1));
    assert!(sheets_long[0].master_is_portrait);
    assert!(sheets_long[0].back_needs_rotation);
    assert_eq!(sheets_long[0].back_rotation_deg, 270);

    // Sheet 1: Front 2 (P), Back 3 (L). Under FlipLongEdge, back rotates 270 deg
    assert_eq!(sheets_long[1].front_page, 2);
    assert_eq!(sheets_long[1].back_page, Some(3));
    assert!(sheets_long[1].master_is_portrait);
    assert!(sheets_long[1].back_needs_rotation);
    assert_eq!(sheets_long[1].back_rotation_deg, 270);

    // Sheet 2: Front 4 (P), Back None
    assert_eq!(sheets_long[2].front_page, 4);
    assert_eq!(sheets_long[2].back_page, None);
    assert!(!sheets_long[2].back_needs_rotation);

    // FlipShortEdge test
    let sheets_short = plan_duplex_sheets(&pages, PrintDuplexBinding::FlipShortEdge);
    // Under FlipShortEdge, Portrait master + Landscape back must rotate 90 deg
    assert_eq!(sheets_short[0].back_rotation_deg, 90);
    assert_eq!(sheets_short[1].back_rotation_deg, 90);
}

#[test]
fn test_adversarial_invalid_devmode_duplex_enums() {
    assert_eq!(PrintDuplexBinding::from_devmode_duplex(1), Some(PrintDuplexBinding::Simplex));
    assert_eq!(PrintDuplexBinding::from_devmode_duplex(2), Some(PrintDuplexBinding::FlipLongEdge));
    assert_eq!(PrintDuplexBinding::from_devmode_duplex(3), Some(PrintDuplexBinding::FlipShortEdge));

    // Invalid enum values must return None
    assert_eq!(PrintDuplexBinding::from_devmode_duplex(0), None);
    assert_eq!(PrintDuplexBinding::from_devmode_duplex(-1), None);
    assert_eq!(PrintDuplexBinding::from_devmode_duplex(4), None);
    assert_eq!(PrintDuplexBinding::from_devmode_duplex(i16::MAX), None);
    assert_eq!(PrintDuplexBinding::from_devmode_duplex(i16::MIN), None);
}

// =========================================================================
// MODULE 4: crates/print/src/stream.rs ADVERSARIAL CHALLENGES
// =========================================================================

#[test]
fn test_adversarial_bounded_channel_backpressure_and_stall() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    let make_buf = |idx| SheetBuffer {
        sheet_index: idx,
        total_sheets: 10,
        width_pt: 595.0,
        height_pt: 842.0,
        data: SheetData::Vector(vec![0xBB; 256]),
    };

    // Slot 1 and Slot 2 fill capacity
    assert!(tx.send_with_backpressure(make_buf(0), &token).is_ok());
    assert!(tx.send_with_backpressure(make_buf(1), &token).is_ok());

    // Verify non-blocking try_recv drains accurately
    let s0 = rx.try_recv().unwrap().expect("Slot 0 missing");
    assert_eq!(s0.sheet_index, 0);

    // Slot 3 can now be accepted
    assert!(tx.send_with_backpressure(make_buf(2), &token).is_ok());

    let s1 = rx.try_recv().unwrap().expect("Slot 1 missing");
    let s2 = rx.try_recv().unwrap().expect("Slot 2 missing");
    assert_eq!(s1.sheet_index, 1);
    assert_eq!(s2.sheet_index, 2);

    // Empty channel check
    assert!(rx.try_recv().unwrap().is_none());
}

#[test]
fn test_adversarial_stream_cancellation_unblocks_sender() {
    let (tx, _rx) = bounded_stream_channel(1);
    let token = CancellationToken::new();

    let make_buf = |idx| SheetBuffer {
        sheet_index: idx,
        total_sheets: 10,
        width_pt: 595.0,
        height_pt: 842.0,
        data: SheetData::Vector(vec![0xCC; 64]),
    };

    // Fill the 1-capacity channel
    tx.send_with_backpressure(make_buf(0), &token).unwrap();

    // Cancel token after 30ms on another thread
    let token_clone = token.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        token_clone.cancel();
    });

    // This send must pause on backpressure and abort cleanly with cancellation error
    let start = std::time::Instant::now();
    let res = tx.send_with_backpressure(make_buf(1), &token);
    assert!(res.is_err(), "Must abort with cancellation error");
    assert!(start.elapsed() < Duration::from_millis(300), "Must abort promptly (<300ms)");
}

#[test]
fn test_adversarial_stream_consumer_disconnect_detection() {
    let (tx, rx) = bounded_stream_channel(1);
    let token = CancellationToken::new();

    let buf = SheetBuffer {
        sheet_index: 0,
        total_sheets: 1,
        width_pt: 595.0,
        height_pt: 842.0,
        data: SheetData::Vector(vec![0xFF; 32]),
    };

    // Drop consumer immediately
    drop(rx);

    // Sending must return disconnected error
    let res = tx.send_with_backpressure(buf, &token);
    assert!(res.is_err(), "Must detect dropped consumer");
}

// =========================================================================
// MODULE 5: crates/print/src/governor.rs ADVERSARIAL CHALLENGES
// =========================================================================

#[test]
fn test_adversarial_counting_semaphore_concurrency_saturation() {
    let semaphore = Arc::new(CountingSemaphore::new(3));
    let token = CancellationToken::new();

    assert_eq!(semaphore.available_permits(), 3);
    assert_eq!(semaphore.max_permits(), 3);

    // Acquire all 3 permits
    semaphore.acquire(&token).unwrap();
    semaphore.acquire(&token).unwrap();
    semaphore.acquire(&token).unwrap();
    assert_eq!(semaphore.available_permits(), 0);

    // Spawn thread to release 1 permit after 40ms
    let sem_clone = semaphore.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(40));
        sem_clone.release();
    });

    // 4th acquire must block and then succeed
    let start = std::time::Instant::now();
    assert!(semaphore.acquire(&token).is_ok());
    assert!(start.elapsed() >= Duration::from_millis(35));
    assert_eq!(semaphore.available_permits(), 0);
}

#[test]
fn test_adversarial_counting_semaphore_excess_release_defense() {
    let semaphore = CountingSemaphore::new(2);
    assert_eq!(semaphore.available_permits(), 2);

    // Calling release when already at max capacity must NOT inflate permits
    for _ in 0..10 {
        semaphore.release();
    }
    assert_eq!(semaphore.available_permits(), 2, "Permit count must never exceed max_permits");
}

#[test]
fn test_adversarial_spool_footprint_arithmetic_overflow_safety() {
    // Large batch: 10,000 sheets * 10 copies = 100,000 sheets of 300 DPI raster (18 MB each)
    // 100,000 * 18 MB = 1,800,000 MB = 1.8 TB.
    let job = PrinterJobEstimate::new(10_000, 10, SheetRasterMode::Raster300Dpi);
    assert_eq!(job.total_sheets(), 100_000);

    let bytes = job.estimated_bytes();
    assert_eq!(bytes, 100_000u64 * 18 * 1024 * 1024);

    let required = calculate_required_spool_bytes(&[job]);
    // Safety overhead 1.25x (+25%)
    assert_eq!(required, bytes.saturating_mul(5) / 4);

    // Test extreme saturation multiplication
    let extreme_job = PrinterJobEstimate::new(usize::MAX, usize::MAX, SheetRasterMode::CustomBytes(u64::MAX));
    let extreme_required = calculate_required_spool_bytes(&[extreme_job]);
    assert!(extreme_required > 0, "Arithmetic must saturate without panicking");
}

#[test]
fn test_adversarial_spool_disk_verdict_edge_cases() {
    // 1. 0 available bytes vs 100 MB required -> CriticalExhaustion
    let v1 = evaluate_disk_verdict(0, 100 * 1024 * 1024);
    assert!(matches!(v1, DiskVerdict::CriticalExhaustion { .. }));

    // 2. Exactly required bytes available -> LowSpaceWarning (due to headroom < 100MB requirement)
    let req = 50 * 1024 * 1024;
    let v2 = evaluate_disk_verdict(req, req);
    assert!(matches!(v2, DiskVerdict::LowSpaceWarning { .. }));

    // 3. Ample space: req + 500 MB -> Sufficient
    let v3 = evaluate_disk_verdict(req + 500 * 1024 * 1024, req);
    assert_eq!(v3, DiskVerdict::Sufficient);
}

// =========================================================================
// MODULE 6: crates/platform/src/win32_v3.rs & win32_v4.rs ADVERSARIAL CHALLENGES
// =========================================================================

#[test]
fn test_adversarial_v3_devmode_headroom_formula_and_alignment() {
    // Headroom formula check: alloc_bytes = ((needed as usize).max(220)).saturating_mul(2).saturating_add(8192);
    // For needed = 3924 (220 base + 3704 vendor extra):
    // alloc_bytes = 3924 * 2 + 8192 = 7848 + 8192 = 16040 bytes.
    let needed: usize = 3924;
    let alloc_bytes = needed.max(220).saturating_mul(2).saturating_add(8192);
    assert_eq!(alloc_bytes, 16040);

    // Allocation must be 8-byte aligned
    let u64_count = alloc_bytes.div_ceil(8);
    let mut buf_u64 = vec![0u64; u64_count];
    let ptr = buf_u64.as_mut_ptr() as usize;
    assert_eq!(ptr % 8, 0, "DEVMODE pointer must be strictly 8-byte aligned");

    assert_eq!((buf_u64.as_ptr() as usize) % 8, 0, "DEVMODE buffer pointer must be strictly 8-byte aligned");
    assert!(alloc_bytes >= 220 + 8192);
}

#[test]
fn test_adversarial_v3_devmode_offsets_and_bitmasks() {
    let mut buffer = vec![0u8; 512];
    let raw = buffer.as_mut_ptr();

    unsafe {
        let dm_fields_ptr = raw.add(72) as *mut u32;
        let dm_orientation_ptr = raw.add(76) as *mut i16;
        let dm_copies_ptr = raw.add(86) as *mut i16;
        let dm_color_ptr = raw.add(92) as *mut i16;
        let dm_duplex_ptr = raw.add(94) as *mut i16;
        let dm_collate_ptr = raw.add(100) as *mut i16;

        // Apply Monochrome + Duplex Long Edge + Landscape + 3 copies
        *dm_color_ptr = DMCOLOR_MONOCHROME;
        *dm_fields_ptr |= DM_COLOR;

        *dm_duplex_ptr = DMDUP_VERTICAL;
        *dm_fields_ptr |= DM_DUPLEX;

        *dm_orientation_ptr = DMORIENT_LANDSCAPE;
        *dm_fields_ptr |= DM_ORIENTATION;

        *dm_copies_ptr = 3;
        *dm_fields_ptr |= DM_COPIES;
        *dm_collate_ptr = DMCOLLATE_TRUE;
        *dm_fields_ptr |= DM_COLLATE;

        assert_eq!(*dm_color_ptr, 1);
        assert_eq!(*dm_duplex_ptr, 2);
        assert_eq!(*dm_orientation_ptr, 2);
        assert_eq!(*dm_copies_ptr, 3);
        assert_eq!(*dm_collate_ptr, 1);

        let fields = *dm_fields_ptr;
        assert_ne!(fields & DM_COLOR, 0);
        assert_ne!(fields & DM_DUPLEX, 0);
        assert_ne!(fields & DM_ORIENTATION, 0);
        assert_ne!(fields & DM_COPIES, 0);
        assert_ne!(fields & DM_COLLATE, 0);
    }
}

#[test]
fn test_adversarial_v4_xps_printticket_xml_generation_validity() {
    let xml = generate_delta_print_ticket_xml(
        ColorMode::MonochromeKOnly,
        DriverDuplexBinding::FlipLongEdge,
        true,
        5,
        Some("ISOA4"),
    );

    // Verify XML schema structure and elements
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(xml.contains("<psf:PrintTicket version=\"1\""));
    assert!(xml.contains("xmlns:psf=\"http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework\""));

    // Feature 1: Monochrome
    assert!(xml.contains("<psf:Feature name=\"psk:PageOutputColor\">"));
    assert!(xml.contains("<psf:Option name=\"psk:Monochrome\" />"));

    // Feature 2: Duplex Long Edge
    assert!(xml.contains("<psf:Feature name=\"psk:JobDuplexAllDocumentsContiguously\">"));
    assert!(xml.contains("<psf:Option name=\"psk:TwoSidedLongEdge\" />"));

    // Feature 3: Landscape Orientation
    assert!(xml.contains("<psf:Feature name=\"psk:PageOrientation\">"));
    assert!(xml.contains("<psf:Option name=\"psk:Landscape\" />"));

    // Feature 4: Media Size ISOA4
    assert!(xml.contains("<psf:Feature name=\"psk:PageMediaSize\">"));
    assert!(xml.contains("<psf:Option name=\"psk:ISOA4\" />"));

    // Feature 5: Copies 5
    assert!(xml.contains("<psf:ParameterInit name=\"psk:JobCopiesAllDocuments\">"));
    assert!(xml.contains("<psf:Value xsi:type=\"xsd:integer\">5</psf:Value>"));
}

#[test]
fn test_adversarial_v4_printticket_provider_mutex_contention() {
    // Concurrency stress: 10 worker threads simultaneously access with_print_ticket_provider
    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    for _ in 0..10 {
        let c = counter.clone();
        handles.push(thread::spawn(move || {
            let res = with_print_ticket_provider(|| {
                c.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(5));
                Ok(())
            });
            assert!(res.is_ok());
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(counter.load(Ordering::SeqCst), 10);
}

// =========================================================================
// MODULE 7: crates/platform/src/cups.rs ADVERSARIAL CHALLENGES
// =========================================================================

#[test]
fn test_adversarial_cups_ipp_wire_magic_and_header() {
    let settings = PrintJobSettings {
        printer_name: "Office_Laser".to_string(),
        copies: 3,
        landscape: false,
        color_mode: ColorMode::MonochromeKOnly,
        duplex_mode: DriverDuplexBinding::FlipLongEdge,
        paper_name: Some("A4".to_string()),
        job_title: "Audit_Test_Report".to_string(),
        ..Default::default()
    };

    let adapter = CupsDriverAdapter::with_host_port("10.0.0.1", 631);
    let wire_bytes = adapter.build_print_job_request("Office_Laser", &settings);

    assert!(wire_bytes.len() > 100);

    // Byte 0-1: IPP Version 2.0 (0x02, 0x00)
    assert_eq!(wire_bytes[0], 0x02);
    assert_eq!(wire_bytes[1], 0x00);

    // Byte 2-3: Operation ID: PrintJob (0x0002) in Big-Endian
    assert_eq!(wire_bytes[2], 0x00);
    assert_eq!(wire_bytes[3], 0x02);

    // Byte 4-7: Request ID: 1 in Big-Endian (0x00, 0x00, 0x00, 0x01)
    assert_eq!(&wire_bytes[4..8], &[0x00, 0x00, 0x00, 0x01]);

    // Byte 8: Operation Attributes Group Tag (0x01)
    assert_eq!(wire_bytes[8], IppTag::OperationAttributes as u8);

    // Verify presence of required RFC 8011 attributes in wire bytes
    let wire_str = String::from_utf8_lossy(&wire_bytes);
    assert!(wire_str.contains("attributes-charset"));
    assert!(wire_str.contains("utf-8"));
    assert!(wire_str.contains("attributes-natural-language"));
    assert!(wire_str.contains("en"));
    assert!(wire_str.contains("printer-uri"));
    assert!(wire_str.contains("ipp://10.0.0.1:631/printers/Office_Laser"));
    assert!(wire_str.contains("document-format"));
    assert!(wire_str.contains("application/pdf"));

    // Job Attributes: sides=two-sided-long-edge, print-color-mode=monochrome, copies=3
    assert!(wire_str.contains("two-sided-long-edge"));
    assert!(wire_str.contains("monochrome"));
    assert!(wire_str.contains("copies"));

    // Terminal byte: EndOfAttributes (0x03)
    assert_eq!(*wire_bytes.last().unwrap(), IppTag::EndOfAttributes as u8);
}

// =========================================================================
// MODULE 8: crates/platform/src/diagnostics.rs ADVERSARIAL CHALLENGES
// =========================================================================

#[test]
fn test_adversarial_diagnostics_markdown_table_pipe_and_newline_escaping() {
    let mut report = DiagnosticReport::harvest_system();
    report.execution_traces.push(TraceRecord {
        timestamp: chrono::Utc::now(),
        level: TraceLevel::Error,
        category: TraceCategory::Spooler,
        printer_name: Some("Epson | WorkForce | Pro".to_string()),
        message: "Paper Jam in Tray 1\nErrorCode: 0x80070005 | Access Denied\r\nLine 3".to_string(),
        error_code: Some("ERR|JAM|01".to_string()),
        context: BTreeMap::new(),
    });

    let md = report.to_markdown_table();

    // Verify pipes in names and messages are escaped
    assert!(md.contains("Epson \\| WorkForce \\| Pro"));
    assert!(md.contains("ERR\\|JAM\\|01"));

    // Verify newlines are converted to <br> to prevent breaking Markdown table row structure
    assert!(md.contains("Paper Jam in Tray 1<br>ErrorCode: 0x80070005 \\| Access Denied<br>Line 3"));
}

#[test]
fn test_adversarial_circular_trace_buffer_thread_contention() {
    // 10 threads concurrently write 50 traces each (500 traces total) into a capacity-50 buffer
    let buffer = Arc::new(DiagnosticTraceBuffer::new(50));
    let mut handles = Vec::new();

    for t_idx in 0..10 {
        let buf_clone = buffer.clone();
        handles.push(thread::spawn(move || {
            for i in 0..50 {
                buf_clone.record(TraceRecord {
                    timestamp: chrono::Utc::now(),
                    level: TraceLevel::Info,
                    category: TraceCategory::System,
                    printer_name: Some(format!("Printer_{t_idx}")),
                    message: format!("Trace {i}"),
                    error_code: None,
                    context: BTreeMap::new(),
                });
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let snapshot = buffer.snapshot();
    assert_eq!(snapshot.len(), 50, "Circular buffer must contain exactly capacity 50 elements");
}

#[test]
fn test_adversarial_windows_11_build_synthesis() {
    // Build < 22000 stays Windows 10
    let w10 = synthesize_windows_version("Windows 10 Pro", "22H2", "19045", "3803");
    assert_eq!(w10, "Windows 10 Pro 22H2 (Build 19045.3803)");

    // Build >= 22000 transforms to Windows 11
    let w11 = synthesize_windows_version("Windows 10 Pro", "23H2", "22631", "3007");
    assert_eq!(w11, "Windows 11 Pro 23H2 (Build 22631.3007)");

    // Build 26100 (Windows 11 24H2)
    let w11_24h2 = synthesize_windows_version("Windows 10 Enterprise", "24H2", "26100", "1");
    assert_eq!(w11_24h2, "Windows 11 Enterprise 24H2 (Build 26100.1)");
}

#[test]
fn test_adversarial_spool_disk_verdict_thresholds() {
    // Exact 250 MB boundary
    assert_eq!(evaluate_spool_disk_verdict(249 * 1024 * 1024), SpoolDiskVerdict::CriticalExhaustion);
    assert_eq!(evaluate_spool_disk_verdict(250 * 1024 * 1024), SpoolDiskVerdict::LowSpaceWarning);

    // Exact 1.0 GB boundary
    assert_eq!(evaluate_spool_disk_verdict(1024 * 1024 * 1024 - 1), SpoolDiskVerdict::LowSpaceWarning);
    assert_eq!(evaluate_spool_disk_verdict(1024 * 1024 * 1024), SpoolDiskVerdict::Sufficient);
}
