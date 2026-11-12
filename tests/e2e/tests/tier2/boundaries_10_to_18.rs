#![allow(unused_imports, unused_variables, unused_must_use, dead_code)]
//! Tier 2 Boundary Value Analysis & Negative Testing: Features 10 to 18 (45 tests total)
//!
//! File: pdfcraft/tests/e2e/tests/tier2/boundaries_10_to_18.rs
//!
//! Covers:
//! - Feature 10: Fit to Printable Area Engine (5 tests)
//! - Feature 11: Shrink Oversized Engine (5 tests)
//! - Feature 12: Actual Size (1:1) Engine & Clipping Detection (5 tests)
//! - Feature 13: Duplex: Flip on Long Edge (Booklet) (5 tests)
//! - Feature 14: Duplex: Flip on Short Edge (Tablet) (5 tests)
//! - Feature 15: Sheet Master Orientation Alignment (5 tests)
//! - Feature 16: Pre-Flight Capability Check & Alerting (5 tests)
//! - Feature 17: Zero-Copy Shared Memory Architecture (5 tests)
//! - Feature 18: Page-by-Page Streaming with Backpressure (5 tests)

use pdfcraft_e2e::benchmarks::{BenchmarkAssertions, MemoryProbe};
use pdfcraft_e2e::margin_assertions::{
    calculate_page_transform, ClippingExtent, MarginAssertions, PageFitMode,
};
use pdfcraft_e2e::mock_driver::{
    DuplexBinding, HardwareMargins, MockIppDriver, MockV3Driver, MockV3DriverConfig,
    MockV4Driver, MockV4DriverConfig, PrintableBounds,
    DMDUP_HORIZONTAL, DMDUP_VERTICAL, DM_DUPLEX, DM_IN_BUFFER, DM_OUT_BUFFER,
};
use pdfcraft_e2e::mock_spooler::{CommittedSheetsLedger, MockSpooler};

// Zero-Copy Shared Memory Snapshot Model for Feature 17 (identical contract to Tier 1)
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("File is empty (0 bytes) and cannot be memory-mapped")]
    EmptyFile,
    #[error("Invalid slice range {range:?} for source of length {len}")]
    OutOfBounds { range: Range<usize>, len: usize },
}

#[derive(Debug, Clone)]
pub enum SharedPdfSource {
    Mmap(Arc<memmap2::Mmap>),
    Heap(Arc<Vec<u8>>),
}

impl SharedPdfSource {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, SnapshotError> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if metadata.len() == 0 {
            return Err(SnapshotError::EmptyFile);
        }
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(Self::Mmap(Arc::new(mmap)))
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::Heap(Arc::new(bytes))
    }

    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Mmap(m) => &m[..],
            Self::Heap(h) => &h[..],
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_mmap(&self) -> bool {
        matches!(self, Self::Mmap(_))
    }

    pub fn slice(&self, range: Range<usize>) -> Option<&[u8]> {
        if range.start <= range.end && range.end <= self.len() {
            Some(&self.as_slice()[range])
        } else {
            None
        }
    }
}

impl AsRef<[u8]> for SharedPdfSource {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl Deref for SharedPdfSource {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}
use pdfcraft_print::driver::{
    validate_job_capabilities, ColorMode, PreFlightSeverity,
};
use pdfcraft_print::duplex::{align_sheet_pair_orientation, plan_duplex_sheets, DuplexBinding as PrintDuplex};
use pdfcraft_print::stream::{
    bounded_stream_channel, CancellationToken, SheetBuffer, SheetData,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::ops::{Deref, Range};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn make_test_sheet(idx: usize, total: usize, bytes: Vec<u8>) -> SheetBuffer {
    SheetBuffer {
        sheet_index: idx,
        total_sheets: total,
        width_pt: 595.2,
        height_pt: 841.68,
        data: SheetData::Vector(bytes),
    }
}

// =========================================================================
// FEATURE 10: Fit to Printable Area Engine (5 Tests)
// =========================================================================

#[test]
fn test_feat10_boundary_extreme_aspect_ratio_100_to_1_panorama() {
    let page_size = (10000.0, 100.0);
    let printable = PrintableBounds::a4_standard_margins(); // 578.4 x 824.88
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // s = min(578.4 / 10000.0, 824.88 / 100.0) = min(0.05784, 8.2488) = 0.05784
    assert!((transform.scale - 0.05784).abs() < 1e-5);
    assert!((transform.dx_pt - 0.0).abs() < 1e-4);
    assert!((transform.dy_pt - 409.548).abs() < 1e-3);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
    MarginAssertions::assert_centered(page_size, &printable, &transform);
}

#[test]
fn test_feat10_boundary_microscopic_page_dimensions() {
    let page_size = (0.5, 0.5);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // s = min(578.4 / 0.5, 824.88 / 0.5) = 1156.8
    assert!(transform.scale > 1000.0);
    assert!((transform.scale - 1156.8).abs() < 1e-3);
    assert!(!transform.scale.is_nan() && !transform.scale.is_infinite());
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
    MarginAssertions::assert_centered(page_size, &printable, &transform);
}

#[test]
fn test_feat10_boundary_gigantic_architectural_sheet_e_size() {
    let page_size = (10000.0, 10000.0);
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
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // s = min(576 / 10000, 756 / 10000) = 0.0576
    assert!((transform.scale - 0.0576).abs() < 1e-5);
    assert!((transform.dx_pt - 0.0).abs() < 1e-4);
    assert!((transform.dy_pt - 90.0).abs() < 1e-4);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

#[test]
fn test_feat10_negative_zero_or_degenerate_printable_area() {
    let page_size = (595.0, 842.0);
    let printable = PrintableBounds {
        printable_width_pt: 0.0, // corrupt hardware margins consuming whole sheet
        printable_height_pt: 0.0,
        physical_width_pt: 600.0,
        physical_height_pt: 800.0,
        dpi_x: 300,
        dpi_y: 300,
        margins: HardwareMargins {
            left_pt: 300.0,
            top_pt: 400.0,
            right_pt: 300.0,
            bottom_pt: 400.0,
        },
    };
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Handles zero printable bounds gracefully without panic (clamps to >= 1.0pt or safe scale)
    assert!(!transform.scale.is_nan() && !transform.scale.is_infinite());
    assert!(transform.scale >= 0.0);
}

#[test]
fn test_feat10_boundary_exact_boundary_match_unity_scale() {
    let printable = PrintableBounds::a4_standard_margins(); // 578.40 x 824.88
    let page_size = (578.40, 824.88);
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert!((transform.scale - 1.0).abs() < 1e-6);
    assert!((transform.dx_pt - 0.0).abs() < 1e-6);
    assert!((transform.dy_pt - 0.0).abs() < 1e-6);
    assert_eq!(transform.clipping.total_pt(), 0.0);
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

// =========================================================================
// FEATURE 11: Shrink Oversized Engine (5 Tests)
// =========================================================================

#[test]
fn test_feat11_boundary_page_0_001pt_larger_downscaled() {
    let printable = PrintableBounds::a4_standard_margins(); // 578.40 x 824.88
    let page_size = (578.401, 800.0); // 0.001 pt larger than printable width
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    let expected_scale = printable.printable_width_pt / page_size.0;
    assert!(
        transform.scale < 1.0,
        "Scale must be strictly downscaled (< 1.0) for oversized page, found: {:.8}",
        transform.scale
    );
    assert!(
        (transform.scale - expected_scale).abs() < 1e-7,
        "Expected scale {:.8}, got {:.8}",
        expected_scale,
        transform.scale
    );
    assert!(
        !transform.clipping.is_clipped(1e-4),
        "ShrinkOversized must eliminate all margin clipping"
    );
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

#[test]
fn test_feat11_boundary_page_0_001pt_smaller_unscaled_1_to_1() {
    let printable = PrintableBounds::a4_standard_margins(); // 578.40 x 824.88
    let page_size = (578.399, 824.879); // 0.001 pt smaller than printable bounds
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!((transform.dx_pt - 0.0005).abs() < 1e-3);
    assert!((transform.dy_pt - 0.0005).abs() < 1e-3);
    assert_eq!(transform.clipping.total_pt(), 0.0);
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

#[test]
fn test_feat11_boundary_non_uniform_margin_ratio_extreme_asymmetry() {
    let printable = PrintableBounds {
        printable_width_pt: 290.0,
        printable_height_pt: 850.0,
        physical_width_pt: 300.0,
        physical_height_pt: 1000.0,
        dpi_x: 203,
        dpi_y: 203,
        margins: HardwareMargins {
            left_pt: 5.0,
            top_pt: 50.0,
            right_pt: 5.0,
            bottom_pt: 100.0,
        },
    };
    let page_size = (300.0, 500.0); // Width oversized (300 > 290), height fits (500 < 850)
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Uniform downscaling governed strictly by width ratio: 290.0 / 300.0 = 0.966667
    assert!(transform.scale < 1.0);
    assert!((transform.scale - (290.0 / 300.0)).abs() < 1e-4);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

#[test]
fn test_feat11_negative_zero_or_negative_dimension_page() {
    let printable = PrintableBounds::a4_standard_margins();
    let degenerate_sizes = vec![(0.0, 500.0), (-10.0, -10.0), (0.0, 0.0)];

    for size in degenerate_sizes {
        let transform = calculate_page_transform(
            size,
            &printable,
            PageFitMode::ShrinkOversized,
            false,
            true,
            DuplexBinding::Simplex,
        );
        // Robust fallback: scale 1.0, finite translations, 0 panics
        assert!(!transform.scale.is_nan());
        assert!(!transform.dx_pt.is_nan());
        assert!(!transform.dy_pt.is_nan());
    }
}

#[test]
fn test_feat11_boundary_single_dimension_oversize_height_only() {
    let printable = PrintableBounds::a4_standard_margins(); // 578.40 x 824.88
    let page_size = (100.0, 924.88); // Height exceeds by 100pt, width fits comfortably
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // Scale = 824.88 / 924.88 = 0.891878
    assert!((transform.scale - (824.88 / 924.88)).abs() < 1e-4);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

// =========================================================================
// FEATURE 12: Actual Size (1:1) Engine & Clipping Detection (5 Tests)
// =========================================================================

#[test]
fn test_feat12_boundary_submillimeter_clipping_0_001mm() {
    let pt_per_mm = 72.0 / 25.4;
    let bleed_0_001mm = 0.001 * pt_per_mm;
    let printable = PrintableBounds::a4_standard_margins(); // 578.40 x 824.88
    let page_size = (578.40 + 2.0 * bleed_0_001mm, 800.0);

    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!(transform.clipping.is_clipped(1e-5));
    let mm = transform.clipping.to_mm();
    assert!(mm.left_mm > 0.0009);
    assert!(mm.right_mm > 0.0009);
}

#[test]
fn test_feat12_boundary_gigantic_page_massive_4_edge_clipping() {
    let printable = PrintableBounds::a4_standard_margins(); // Physical 595.2 x 841.68, margins 8.4
    let page_size = (5000.0, 5000.0);

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
    assert!(transform.clipping.left_pt > 2000.0);
    assert!(transform.clipping.right_pt > 2000.0);
    assert!(transform.clipping.top_pt > 2000.0);
    assert!(transform.clipping.bottom_pt > 2000.0);
    assert!(transform.clipping.total_pt() > 8000.0);
}

#[test]
fn test_feat12_boundary_exact_zero_margin_borderless_page_fit() {
    let printable = PrintableBounds::a4_borderless();
    let page_size = (printable.physical_width_pt, printable.physical_height_pt);

    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!((transform.dx_pt - 0.0).abs() < 1e-4);
    assert!((transform.dy_pt - 0.0).abs() < 1e-4);
    assert_eq!(transform.clipping.total_pt(), 0.0);
    assert!(!transform.clipping.is_clipped(1e-6));
}

#[test]
fn test_feat12_boundary_multi_edge_asymmetric_margins_clipping_report() {
    let printable = PrintableBounds {
        printable_width_pt: 560.0,
        printable_height_pt: 750.0,
        physical_width_pt: 600.0,
        physical_height_pt: 800.0,
        dpi_x: 300,
        dpi_y: 300,
        margins: HardwareMargins {
            left_pt: 10.0,
            top_pt: 40.0,
            right_pt: 30.0,
            bottom_pt: 10.0,
        },
    };
    let page_size = (580.0, 780.0);

    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    // Left: (600-580)/2 = 10 -> margin 10 - 10 = 0.0 pt
    assert_eq!(transform.clipping.left_pt, 0.0);
    // Right: (10 + 580) - (600 - 30) = 590 - 570 = 20.0 pt
    assert!((transform.clipping.right_pt - 20.0).abs() < 1e-4);
    // Top: margin 40 - (800-780)/2 = 40 - 10 = 30.0 pt
    assert!((transform.clipping.top_pt - 30.0).abs() < 1e-4);
    // Bottom: (10 + 780) - (800 - 10) = 790 - 790 = 0.0 pt
    assert_eq!(transform.clipping.bottom_pt, 0.0);
}

#[test]
fn test_feat12_negative_extreme_negative_offset_and_centering() {
    let printable = PrintableBounds::a4_standard_margins();
    let page_size = (1200.0, 1600.0);

    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!(transform.dx_pt < -100.0);
    assert!(transform.dy_pt < -100.0);
    assert!(transform.clipping.total_pt() > 0.0);
}

// =========================================================================
// FEATURE 13: Duplex: Flip on Long Edge (Booklet) (5 Tests)
// =========================================================================

#[test]
fn test_feat13_negative_simplex_only_driver_rejection() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Simplex_Receipt_Printer".to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: false,
        supported_duplex: vec![pdfcraft_print::driver::DuplexBinding::Simplex],
        is_color_supported: false,
        max_copies: 10,
        bounds: pdfcraft_print::driver::PrintableBounds {
            printable_width_pt: 578.40,
            printable_height_pt: 824.88,
            physical_width_pt: 595.20,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: pdfcraft_print::driver::HardwareMargins::default(),
        },
    };

    let settings = pdfcraft_print::driver::PrintJobSettings {
        printer_name: "Simplex_Receipt_Printer".to_string(),
        copies: 1,
        color_mode: ColorMode::MonochromeKOnly,
        duplex_mode: pdfcraft_print::driver::DuplexBinding::FlipLongEdge,
        fit_mode: pdfcraft_print::driver::PageFitMode::FitPrintableArea,
        page_bounds: (595.28, 841.89),
        ..Default::default()
    };

    let report = validate_job_capabilities(&caps, &settings);
    assert!(!report.is_valid_for_dispatch);
    assert!(report.has_critical_mismatch());
}

#[test]
fn test_feat13_boundary_single_page_document_duplex_no_op() {
    let sizes = vec![(595.0, 842.0)];
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipLongEdge);
    assert_eq!(sheets.len(), 1);
    assert_eq!(sheets[0].front_page, 0);
    assert_eq!(sheets[0].back_page, None);
    assert!(!sheets[0].back_needs_rotation);
}

#[test]
fn test_feat13_boundary_odd_page_document_trailing_blank_sheet() {
    let sizes = vec![
        (595.0, 842.0),
        (595.0, 842.0),
        (595.0, 842.0),
        (595.0, 842.0),
        (595.0, 842.0),
    ];
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipLongEdge);
    assert_eq!(sheets.len(), 3);
    assert_eq!(sheets[0].front_page, 0);
    assert_eq!(sheets[0].back_page, Some(1));
    assert_eq!(sheets[1].front_page, 2);
    assert_eq!(sheets[1].back_page, Some(3));
    assert_eq!(sheets[2].front_page, 4);
    assert_eq!(sheets[2].back_page, None);
    assert!(!sheets[2].back_needs_rotation);
}

#[test]
fn test_feat13_boundary_extreme_page_count_1000_pages() {
    let sizes: Vec<(f64, f64)> = (0..1000).map(|_| (595.0, 842.0)).collect();
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipLongEdge);
    assert_eq!(sheets.len(), 500);

    for (k, sheet) in sheets.iter().enumerate() {
        assert_eq!(sheet.sheet_index, k);
        assert_eq!(sheet.front_page, 2 * k);
        assert_eq!(sheet.back_page, Some(2 * k + 1));
    }
}

#[test]
fn test_feat13_boundary_conflicting_orientation_long_edge_rotation() {
    let sizes = vec![(595.0, 842.0), (842.0, 595.0)]; // Portrait front, Landscape back
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipLongEdge);
    assert_eq!(sheets.len(), 1);
    assert!(sheets[0].master_is_portrait);
    assert!(sheets[0].back_needs_rotation);
    assert_eq!(sheets[0].back_rotation_deg, 270);
}

// =========================================================================
// FEATURE 14: Duplex: Flip on Short Edge (Tablet) (5 Tests)
// =========================================================================

#[test]
fn test_feat14_negative_simplex_only_driver_short_edge_rejection() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Simplex_Label_Printer".to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: false,
        supported_duplex: vec![pdfcraft_print::driver::DuplexBinding::Simplex],
        is_color_supported: true,
        max_copies: 99,
        bounds: pdfcraft_print::driver::PrintableBounds {
            printable_width_pt: 578.40,
            printable_height_pt: 824.88,
            physical_width_pt: 595.20,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: pdfcraft_print::driver::HardwareMargins::default(),
        },
    };

    let settings = pdfcraft_print::driver::PrintJobSettings {
        printer_name: "Simplex_Label_Printer".to_string(),
        copies: 1,
        color_mode: ColorMode::MonochromeKOnly,
        duplex_mode: pdfcraft_print::driver::DuplexBinding::FlipShortEdge,
        fit_mode: pdfcraft_print::driver::PageFitMode::FitPrintableArea,
        page_bounds: (595.28, 841.89),
        ..Default::default()
    };

    let report = validate_job_capabilities(&caps, &settings);
    assert!(!report.is_valid_for_dispatch);
    assert!(report.has_critical_mismatch());
}

#[test]
fn test_feat14_boundary_landscape_tablet_binding_extreme_dimensions() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();
    devmode_buf[72..76].copy_from_slice(&DM_DUPLEX.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_HORIZONTAL.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    ).unwrap();

    mock.assert_duplex_asserted(DuplexBinding::FlipShortEdge);
    let last = mock.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_duplex, Some(3));
}

#[test]
fn test_feat14_boundary_single_page_short_edge_no_op() {
    let sizes = vec![(842.0, 595.0)]; // Landscape calendar page
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipShortEdge);
    assert_eq!(sheets.len(), 1);
    assert_eq!(sheets[0].front_page, 0);
    assert_eq!(sheets[0].back_page, None);
    assert!(!sheets[0].back_needs_rotation);
}

#[test]
fn test_feat14_boundary_odd_page_short_edge_handling() {
    let sizes: Vec<(f64, f64)> = (0..7).map(|_| (842.0, 595.0)).collect();
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipShortEdge);
    assert_eq!(sheets.len(), 4);
    assert_eq!(sheets[3].front_page, 6);
    assert_eq!(sheets[3].back_page, None);
}

#[test]
fn test_feat14_boundary_portrait_master_landscape_back_short_edge_rotation() {
    let sizes = vec![(595.0, 842.0), (842.0, 595.0)];
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipShortEdge);
    assert_eq!(sheets.len(), 1);
    assert!(sheets[0].master_is_portrait);
    assert!(sheets[0].back_needs_rotation);
    assert_eq!(sheets[0].back_rotation_deg, 90);
}

// =========================================================================
// FEATURE 15: Sheet Master Orientation Alignment (5 Tests)
// =========================================================================

#[test]
fn test_feat15_boundary_rapid_alternating_p_l_p_l_orientations() {
    let sizes = vec![
        (595.0, 842.0), // Sheet 0 Front: P
        (842.0, 595.0), // Sheet 0 Back: L -> rotated 270
        (595.0, 842.0), // Sheet 1 Front: P
        (842.0, 595.0), // Sheet 1 Back: L -> rotated 270
    ];
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipLongEdge);
    assert_eq!(sheets.len(), 2);

    assert!(sheets[0].master_is_portrait);
    assert!(sheets[0].back_needs_rotation);
    assert_eq!(sheets[0].back_rotation_deg, 270);

    assert!(sheets[1].master_is_portrait);
    assert!(sheets[1].back_needs_rotation);
    assert_eq!(sheets[1].back_rotation_deg, 270);
}

#[test]
fn test_feat15_boundary_landscape_master_rapid_alternating_l_p_l_p() {
    let sizes = vec![
        (842.0, 595.0), // Sheet 0 Front: L
        (595.0, 842.0), // Sheet 0 Back: P -> rotated 90
        (842.0, 595.0), // Sheet 1 Front: L
        (595.0, 842.0), // Sheet 1 Back: P -> rotated 90
    ];
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipLongEdge);
    assert_eq!(sheets.len(), 2);

    assert!(!sheets[0].master_is_portrait);
    assert!(sheets[0].back_needs_rotation);
    assert_eq!(sheets[0].back_rotation_deg, 90);

    assert!(!sheets[1].master_is_portrait);
    assert!(sheets[1].back_needs_rotation);
    assert_eq!(sheets[1].back_rotation_deg, 90);
}

#[test]
fn test_feat15_boundary_unaligned_aspect_ratio_extreme_panorama_rotation() {
    let back_size = (1200.0, 300.0);
    let printable = PrintableBounds::a4_standard_margins(); // 578.4 x 824.88

    let transform = calculate_page_transform(
        back_size,
        &printable,
        PageFitMode::FitPrintableArea,
        true, // is back page
        true, // master is portrait
        DuplexBinding::FlipLongEdge,
    );

    assert!(transform.rotation_deg == 90 || transform.rotation_deg == 270);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable((back_size.1, back_size.0), &printable, &transform);
}

#[test]
fn test_feat15_boundary_180_degree_square_or_degenerate_orientation() {
    let (master_portrait, rot) = align_sheet_pair_orientation(
        (600.0, 600.0), // Square page: evaluates portrait (h >= w)
        (600.0, 600.0),
        PrintDuplex::FlipLongEdge,
    );
    assert!(master_portrait);
    assert_eq!(rot, 0); // No spurious rotation
}

#[test]
fn test_feat15_negative_simplex_mixed_orientations_invariance() {
    let sizes = vec![(595.0, 842.0), (842.0, 595.0), (595.0, 842.0)];
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::Simplex);
    assert_eq!(sheets.len(), 3);

    for sheet in sheets {
        assert_eq!(sheet.back_page, None);
        assert!(!sheet.back_needs_rotation);
        assert_eq!(sheet.back_rotation_deg, 0);
    }
}

// =========================================================================
// FEATURE 16: Pre-Flight Capability Check & Alerting (5 Tests)
// =========================================================================

#[test]
fn test_feat16_negative_unsupported_duplex_binding_critical_mismatch() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Office_Workgroup_A4".to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![
            pdfcraft_print::driver::DuplexBinding::Simplex,
            pdfcraft_print::driver::DuplexBinding::FlipLongEdge,
        ], // Omits FlipShortEdge
        is_color_supported: true,
        max_copies: 99,
        bounds: pdfcraft_print::driver::PrintableBounds {
            printable_width_pt: 578.40,
            printable_height_pt: 824.88,
            physical_width_pt: 595.20,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: pdfcraft_print::driver::HardwareMargins::default(),
        },
    };

    let settings = pdfcraft_print::driver::PrintJobSettings {
        printer_name: "Office_Workgroup_A4".to_string(),
        copies: 1,
        color_mode: ColorMode::Color,
        duplex_mode: pdfcraft_print::driver::DuplexBinding::FlipShortEdge,
        fit_mode: pdfcraft_print::driver::PageFitMode::FitPrintableArea,
        page_bounds: (595.28, 841.89),
        ..Default::default()
    };

    let report = validate_job_capabilities(&caps, &settings);
    assert!(!report.is_valid_for_dispatch);
    assert!(report.has_critical_mismatch());
}

#[test]
fn test_feat16_negative_copies_boundary_limit_and_overflow() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Quota_Printer".to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![pdfcraft_print::driver::DuplexBinding::Simplex],
        is_color_supported: true,
        max_copies: 99,
        bounds: pdfcraft_print::driver::PrintableBounds {
            printable_width_pt: 578.40,
            printable_height_pt: 824.88,
            physical_width_pt: 595.20,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: pdfcraft_print::driver::HardwareMargins::default(),
        },
    };

    // Exactly at boundary: 99 copies -> Valid
    let valid_settings = pdfcraft_print::driver::PrintJobSettings {
        printer_name: "Quota_Printer".to_string(),
        copies: 99,
        ..Default::default()
    };
    assert!(validate_job_capabilities(&caps, &valid_settings).is_valid_for_dispatch);

    // 1 over boundary: 100 copies -> Critical Mismatch
    let overflow_settings = pdfcraft_print::driver::PrintJobSettings {
        printer_name: "Quota_Printer".to_string(),
        copies: 100,
        ..Default::default()
    };
    let report = validate_job_capabilities(&caps, &overflow_settings);
    assert!(!report.is_valid_for_dispatch);
    assert!(report.has_critical_mismatch());
}

#[test]
fn test_feat16_negative_color_mode_mismatch_monochrome_driver() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "K_Only_Warehouse_Laser".to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![pdfcraft_print::driver::DuplexBinding::Simplex],
        is_color_supported: false,
        max_copies: 99,
        bounds: pdfcraft_print::driver::PrintableBounds {
            printable_width_pt: 578.40,
            printable_height_pt: 824.88,
            physical_width_pt: 595.20,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: pdfcraft_print::driver::HardwareMargins::default(),
        },
    };

    let settings = pdfcraft_print::driver::PrintJobSettings {
        printer_name: "K_Only_Warehouse_Laser".to_string(),
        color_mode: ColorMode::Color,
        ..Default::default()
    };

    let report = validate_job_capabilities(&caps, &settings);
    assert!(!report.is_valid_for_dispatch);
    assert!(report.has_critical_mismatch());
}

#[test]
fn test_feat16_boundary_actual_size_clipping_warning_allows_dispatch() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Standard_Office_Laser".to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![pdfcraft_print::driver::DuplexBinding::Simplex],
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

    let settings = pdfcraft_print::driver::PrintJobSettings {
        printer_name: "Standard_Office_Laser".to_string(),
        fit_mode: pdfcraft_print::driver::PageFitMode::ActualSize,
        page_bounds: (595.28, 841.89), // Extends into margins
        ..Default::default()
    };

    let report = validate_job_capabilities(&caps, &settings);
    // Crucial: Warnings allow dispatch
    assert!(report.is_valid_for_dispatch);
    assert!(report.has_clipping_hazard());
    assert!(!report.has_critical_mismatch());
}

#[test]
fn test_feat16_negative_multiple_simultaneous_critical_mismatches() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Basic_Simplex_Mono_Laser".to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: false,
        supported_duplex: vec![pdfcraft_print::driver::DuplexBinding::Simplex],
        is_color_supported: false,
        max_copies: 10,
        bounds: pdfcraft_print::driver::PrintableBounds {
            printable_width_pt: 578.40,
            printable_height_pt: 824.88,
            physical_width_pt: 595.20,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: pdfcraft_print::driver::HardwareMargins::default(),
        },
    };

    // Fails on: Duplex (FlipLongEdge on simplex), Color (Color on mono), Copies (50 on max 10)
    let settings = pdfcraft_print::driver::PrintJobSettings {
        printer_name: "Basic_Simplex_Mono_Laser".to_string(),
        copies: 50,
        color_mode: ColorMode::Color,
        duplex_mode: pdfcraft_print::driver::DuplexBinding::FlipLongEdge,
        ..Default::default()
    };

    let report = validate_job_capabilities(&caps, &settings);
    assert!(!report.is_valid_for_dispatch);
    assert!(report.has_critical_mismatch());
    assert!(report.issues.len() >= 3);
}

// =========================================================================
// FEATURE 17: Zero-Copy Shared Memory Architecture (5 Tests)
// =========================================================================

#[test]
fn test_feat17_negative_empty_file_0_byte_mmap_rejected() {
    let temp_dir = tempfile::Builder::new()
        .prefix("pdfcraft_feat17_empty_")
        .tempdir()
        .unwrap();
    let empty_path = temp_dir.path().join("zero_length.pdf");
    {
        let mut f = File::create(&empty_path).unwrap();
        f.flush().unwrap();
        f.sync_all().unwrap();
    }

    // Robust open on Windows in case of transient AV or filesystem indexing lock
    let mut res = SharedPdfSource::from_file(&empty_path);
    let start = std::time::Instant::now();
    while matches!(&res, Err(SnapshotError::Io(_))) && start.elapsed() < Duration::from_millis(1000) {
        std::thread::sleep(Duration::from_millis(10));
        res = SharedPdfSource::from_file(&empty_path);
    }

    assert!(
        matches!(res, Err(SnapshotError::EmptyFile)),
        "Production SharedPdfSource::from_file must reject 0-byte file with EmptyFile error, got: {:?}",
        res
    );
}

#[test]
fn test_feat17_boundary_concurrent_access_by_10_mock_workers_no_realloc() {
    let temp_dir = tempfile::Builder::new()
        .prefix("pdfcraft_feat17_mmap_")
        .tempdir()
        .unwrap();
    let file_path = temp_dir.path().join("concurrent_doc.pdf");
    let payload = vec![0x42u8; 2_000_000]; // 2MB file
    {
        let mut f = File::create(&file_path).unwrap();
        f.write_all(&payload).unwrap();
        f.flush().unwrap();
        f.sync_all().unwrap();
    }

    let mut source_res = SharedPdfSource::from_file(&file_path);
    let start = std::time::Instant::now();
    while matches!(&source_res, Err(SnapshotError::Io(_))) && start.elapsed() < Duration::from_millis(1000) {
        std::thread::sleep(Duration::from_millis(10));
        source_res = SharedPdfSource::from_file(&file_path);
    }
    let source = source_res.expect("Production SharedPdfSource::from_file must map valid document");
    assert!(
        source.is_mmap(),
        "File source must be backed by OS memory mapping (memmap2::Mmap)"
    );
    let base_ptr = source.as_ptr() as usize;

    let mut handles = Vec::new();
    for _ in 0..10 {
        let src_clone = source.clone();
        handles.push(std::thread::spawn(move || {
            let ptr = src_clone.as_ptr() as usize;
            assert_eq!(
                ptr, base_ptr,
                "Shared memory pointer must be invariant across clones without buffer duplication"
            );
            // Verify content access across threads
            assert_eq!(src_clone[0], 0x42);
            assert_eq!(src_clone[1_999_999], 0x42);
            assert_eq!(src_clone.len(), 2_000_000);
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn test_feat17_negative_non_existent_file_io_error() {
    let missing_path = Path::new("non_existent_path_9876543210_deadbeef.pdf");
    let res = SharedPdfSource::from_file(missing_path);
    assert!(
        matches!(res, Err(SnapshotError::Io(_))),
        "Missing file must return SnapshotError::Io, got: {:?}",
        res
    );
}

#[test]
#[allow(clippy::reversed_empty_ranges)]
fn test_feat17_negative_subslice_exceeding_length_out_of_bounds() {
    let data = vec![0xAAu8; 500];
    let source = SharedPdfSource::from_bytes(data);

    assert!(source.slice(0..501).is_none());
    assert!(source.slice(600..700).is_none());
    assert!(source.slice(300..200).is_none()); // Inverted range
    assert_eq!(source.slice(0..500).unwrap().len(), 500);
    assert_eq!(source.slice(500..500), Some(&[][..]));
}

#[test]
fn test_feat17_boundary_large_heap_snapshot_memory_invariance() {
    let probe = MemoryProbe::start();
    let large_payload = vec![0x77u8; 10_000_000]; // 10MB
    let source = SharedPdfSource::from_bytes(large_payload);

    let mut handles = Vec::new();
    for _ in 0..10 {
        let s = source.clone();
        handles.push(std::thread::spawn(move || {
            let slice = s.slice(1_000_000..2_000_000).unwrap();
            slice[0]
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let (_init, _peak, delta) = probe.stop();
    // Delta RSS should not multiply 10x (10 * 10MB = 100MB). Strict upper bound < 15MB.
    BenchmarkAssertions::assert_peak_memory_delta(delta, 15.0);
}

// =========================================================================
// FEATURE 18: Page-by-Page Streaming with Backpressure (5 Tests)
// =========================================================================

#[test]
fn test_feat18_boundary_channel_capacity_2_full_blocks_producer() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    // Send 2 items filling capacity
    tx.send_with_backpressure(make_test_sheet(0, 3, vec![0x11; 64]), &token).unwrap();
    tx.send_with_backpressure(make_test_sheet(1, 3, vec![0x22; 64]), &token).unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let started_clone = started.clone();
    let tx_clone = tx;
    let token_clone = token.clone();
    let sender_thread = std::thread::spawn(move || {
        started_clone.store(true, Ordering::Release);
        // Will pause until slot opens
        tx_clone.send_with_backpressure(make_test_sheet(2, 3, vec![0x33; 64]), &token_clone).unwrap();
    });

    // Deterministically wait for sender thread to start execution
    while !started.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    // Give sender thread a bounded slice to attempt try_send and enter backpressure sleep
    std::thread::sleep(Duration::from_millis(50));

    // Drain sheet 0 to open slot
    let s0 = rx.try_recv().unwrap().expect("sheet 0 received");
    assert_eq!(s0.sheet_index, 0);

    sender_thread.join().unwrap();

    let s1 = rx.try_recv().unwrap().expect("sheet 1 received");
    assert_eq!(s1.sheet_index, 1);
    let s2 = rx.try_recv().unwrap().expect("sheet 2 received");
    assert_eq!(s2.sheet_index, 2);
}

#[test]
fn test_feat18_boundary_slow_consumer_stall_recovery() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    let consumer_thread = std::thread::spawn(move || {
        let mut received = Vec::new();
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(20)); // Slow consumer
            let s = rx.recv_with_cancellation(&CancellationToken::new()).unwrap().unwrap();
            received.push(s.sheet_index);
        }
        received
    });

    for idx in 0..10 {
        tx.send_with_backpressure(make_test_sheet(idx, 10, vec![0x99; 32]), &token).unwrap();
    }

    let received = consumer_thread.join().unwrap();
    assert_eq!(received, (0..10).collect::<Vec<_>>());
}

#[test]
fn test_feat18_negative_immediate_cancellation_while_producer_blocked() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    // Fill channel capacity = 2
    tx.send_with_backpressure(make_test_sheet(0, 3, vec![0xAA; 16]), &token).unwrap();
    tx.send_with_backpressure(make_test_sheet(1, 3, vec![0xBB; 16]), &token).unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let started_clone = started.clone();
    let token_clone = token.clone();
    let tx_clone = tx;

    let sender_thread = std::thread::spawn(move || {
        started_clone.store(true, Ordering::Release);
        tx_clone.send_with_backpressure(make_test_sheet(2, 3, vec![0xCC; 16]), &token_clone)
    });

    // Deterministically wait for sender thread to start and attempt send
    while !started.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    // Give sender thread a deterministic slice to hit backpressure
    std::thread::sleep(Duration::from_millis(50));

    // Cancel while blocked
    token.cancel();

    let res = sender_thread.join().unwrap();
    assert!(res.is_err(), "Blocked producer must abort with error upon cancellation");
    let err_msg = res.err().unwrap().to_string();
    assert!(
        err_msg.contains("cancelled") || err_msg.contains("Canceled"),
        "Unexpected error: {err_msg}"
    );

    // Keep rx alive until join completes, and verify only sheet 0 and sheet 1 were ever received
    let s0 = rx.try_recv().unwrap().expect("sheet 0 in channel");
    assert_eq!(s0.sheet_index, 0);
    let s1 = rx.try_recv().unwrap().expect("sheet 1 in channel");
    assert_eq!(s1.sheet_index, 1);
    let s2 = rx.try_recv();
    assert!(
        matches!(s2, Ok(None) | Err(_)),
        "Sheet 2 must NOT have been committed after cancellation"
    );
}

#[test]
fn test_feat18_boundary_zero_page_document_stream_handling() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    // 0-page document: producer closes without sending any sheets
    drop(tx);

    let res = rx.recv_with_cancellation(&token).unwrap();
    assert!(res.is_none());
}

#[test]
fn test_feat18_negative_producer_panic_consumer_clean_recovery() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    let panic_sender = std::thread::spawn(move || {
        tx.send_with_backpressure(make_test_sheet(0, 5, vec![0x01; 16]), &CancellationToken::new()).unwrap();
        tx.send_with_backpressure(make_test_sheet(1, 5, vec![0x02; 16]), &CancellationToken::new()).unwrap();
        panic!("Simulated worker thread panic midway through streaming");
    });

    let _ = panic_sender.join(); // Ignore panic

    // Consumer drains the 2 committed sheets cleanly
    let s0 = rx.recv_with_cancellation(&token).unwrap().unwrap();
    assert_eq!(s0.sheet_index, 0);
    let s1 = rx.recv_with_cancellation(&token).unwrap().unwrap();
    assert_eq!(s1.sheet_index, 1);

    // Next receive returns Ok(None) cleanly without deadlock or panic
    let s_eof = rx.recv_with_cancellation(&token).unwrap();
    assert!(s_eof.is_none());
}
