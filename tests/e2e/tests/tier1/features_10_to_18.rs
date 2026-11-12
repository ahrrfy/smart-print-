#![allow(unused_imports, unused_variables, unused_must_use, dead_code)]
//! Tier 1 Happy-Path Feature Verification: Features 10 to 18 (45 tests total)
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
use pdfcraft_print::driver::{
    validate_job_capabilities, ColorMode, PreFlightSeverity,
};
use pdfcraft_print::duplex::{plan_duplex_sheets, DuplexBinding as PrintDuplex};
use pdfcraft_print::stream::{
    bounded_stream_channel, CancellationToken, SheetBuffer, SheetData,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::ops::{Deref, Range};
use std::path::Path;
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
// Zero-Copy Shared Memory Model for Feature 17
// =========================================================================

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("File is empty (0 bytes) and cannot be memory-mapped")]
    EmptyFile,
    #[error("Invalid slice range {range:?} for source of length {len}")]
    OutOfBounds { range: Range<usize>, len: usize },
}

#[derive(Clone)]
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

    pub fn from_arc(arc: Arc<Vec<u8>>) -> Self {
        Self::Heap(arc)
    }

    pub fn is_mmap(&self) -> bool {
        matches!(self, Self::Mmap(_))
    }

    pub fn len(&self) -> usize {
        self.as_ref().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_ref().is_empty()
    }

    pub fn slice(&self, range: Range<usize>) -> Option<&[u8]> {
        let bytes = self.as_ref();
        if range.start <= range.end && range.end <= bytes.len() {
            Some(&bytes[range])
        } else {
            None
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        self.as_ref()
    }
}

impl AsRef<[u8]> for SharedPdfSource {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mmap(m) => &m[..],
            Self::Heap(h) => &h[..],
        }
    }
}

impl Deref for SharedPdfSource {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

// =========================================================================
// FEATURE 10: Fit to Printable Area Engine (5 Tests)
// =========================================================================

#[test]
fn test_feat10_fit_oversized_bleed_a4_to_margined_a4() {
    let page_size = (595.28, 841.89);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert!((transform.scale - 0.971643).abs() < 1e-4);
    assert_eq!(transform.rotation_deg, 0);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
    MarginAssertions::assert_centered(page_size, &printable, &transform);
}

#[test]
fn test_feat10_fit_extreme_aspect_ratio_wide_landscape_to_portrait() {
    let page_size = (1200.0, 400.0);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // s = min(578.4 / 1200.0, 824.88 / 400.0) = 0.482
    assert!((transform.scale - 0.482).abs() < 1e-4);
    assert!((transform.dx_pt - 0.0).abs() < 1e-4);
    assert!((transform.dy_pt - 316.04).abs() < 1e-4);
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

#[test]
fn test_feat10_fit_tall_narrow_strip_to_us_letter() {
    let page_size = (200.0, 900.0);
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

    // s = min(576 / 200, 756 / 900) = 0.84
    assert!((transform.scale - 0.84).abs() < 1e-4);
    assert!((transform.dx_pt - 204.0).abs() < 1e-4);
    assert!((transform.dy_pt - 0.0).abs() < 1e-4);
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

#[test]
fn test_feat10_fit_undersized_a6_index_card_enlargement() {
    let page_size = (297.64, 420.94);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert!(transform.scale > 1.0);
    assert!((transform.scale - 1.94328).abs() < 1e-4);
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
    MarginAssertions::assert_centered(page_size, &printable, &transform);
}

#[test]
fn test_feat10_fit_asymmetric_hardware_margins_thermal_printer() {
    let page_size = (600.0, 800.0);
    let printable = PrintableBounds {
        printable_width_pt: 570.0,
        printable_height_pt: 720.0,
        physical_width_pt: 600.0,
        physical_height_pt: 800.0,
        dpi_x: 203,
        dpi_y: 203,
        margins: HardwareMargins {
            left_pt: 10.0,
            top_pt: 30.0,
            right_pt: 20.0,
            bottom_pt: 50.0,
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

    // s = min(570 / 600, 720 / 800) = min(0.95, 0.90) = 0.90
    assert!((transform.scale - 0.90).abs() < 1e-4);
    assert!((transform.dx_pt - 15.0).abs() < 1e-4);
    assert!((transform.dy_pt - 0.0).abs() < 1e-4);
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

// =========================================================================
// FEATURE 11: Shrink Oversized Engine (5 Tests)
// =========================================================================

#[test]
fn test_feat11_shrink_oversized_tabloid_drawing_to_a4() {
    let page_size = (792.0, 1224.0);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert!((transform.scale - 0.67392).abs() < 1e-4);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

#[test]
fn test_feat11_shrink_smaller_page_unscaled_1_to_1() {
    let page_size = (419.53, 595.28);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!((transform.dx_pt - 79.435).abs() < 1e-3);
    assert!((transform.dy_pt - 114.80).abs() < 1e-3);
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

#[test]
fn test_feat11_shrink_partially_oversized_width_only() {
    let page_size = (650.0, 500.0);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // 650 > 578.4 -> scale = 578.4 / 650.0 = 0.889846
    assert!((transform.scale - 0.889846).abs() < 1e-4);
    assert!((transform.dx_pt - 0.0).abs() < 1e-4);
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

#[test]
fn test_feat11_shrink_partially_oversized_height_only() {
    let page_size = (500.0, 850.0);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    // 850 > 824.88 -> scale = 824.88 / 850.0 = 0.970447
    assert!((transform.scale - 0.970447).abs() < 1e-4);
    assert!((transform.dy_pt - 0.0).abs() < 1e-4);
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

#[test]
fn test_feat11_shrink_exact_boundary_match_at_limit() {
    let page_size = (578.40, 824.88);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ShrinkOversized,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert!((transform.scale - 1.0).abs() < 1e-6);
    assert!((transform.dx_pt - 0.0).abs() < 1e-6);
    assert!((transform.dy_pt - 0.0).abs() < 1e-6);
    MarginAssertions::assert_within_printable(page_size, &printable, &transform);
}

// =========================================================================
// FEATURE 12: Actual Size (1:1) Engine & Clipping Detection (5 Tests)
// =========================================================================

#[test]
fn test_feat12_actual_size_bleed_page_uniform_clipping_hazard() {
    let page_size = (595.20, 841.68);
    let printable = PrintableBounds::a4_standard_margins();
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
    assert!((transform.clipping.left_pt - 8.40).abs() < 1e-4);
    assert!((transform.clipping.top_pt - 8.40).abs() < 1e-4);
    assert!((transform.clipping.right_pt - 8.40).abs() < 1e-4);
    assert!((transform.clipping.bottom_pt - 8.40).abs() < 1e-4);
    assert!((transform.clipping.total_pt() - 33.60).abs() < 1e-3);
}

#[test]
fn test_feat12_actual_size_within_margins_zero_clipping() {
    let page_size = (500.0, 700.0);
    let printable = PrintableBounds {
        printable_width_pt: 560.0,
        printable_height_pt: 760.0,
        physical_width_pt: 600.0,
        physical_height_pt: 800.0,
        dpi_x: 300,
        dpi_y: 300,
        margins: HardwareMargins {
            left_pt: 20.0,
            top_pt: 20.0,
            right_pt: 20.0,
            bottom_pt: 20.0,
        },
    };

    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!(!transform.clipping.is_clipped(1e-4));
    assert_eq!(transform.clipping.total_pt(), 0.0);
    assert!((transform.dx_pt - 30.0).abs() < 1e-4);
    assert!((transform.dy_pt - 30.0).abs() < 1e-4);
}

#[test]
fn test_feat12_actual_size_asymmetric_single_edge_clipping() {
    let printable = PrintableBounds {
        printable_width_pt: 555.0,
        printable_height_pt: 770.0,
        physical_width_pt: 600.0,
        physical_height_pt: 800.0,
        dpi_x: 300,
        dpi_y: 300,
        margins: HardwareMargins {
            left_pt: 10.0,
            top_pt: 15.0,
            right_pt: 35.0,
            bottom_pt: 15.0,
        },
    };
    let page_size = (570.0, 700.0);

    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert_eq!(transform.clipping.left_pt, 0.0);
    assert_eq!(transform.clipping.top_pt, 0.0);
    assert!((transform.clipping.right_pt - 20.0).abs() < 1e-4);
    assert_eq!(transform.clipping.bottom_pt, 0.0);
}

#[test]
fn test_feat12_actual_size_zero_margin_virtual_pdf_printer() {
    let printable = PrintableBounds::a4_borderless();
    let page_size = (595.32, 841.92);
    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.scale, 1.0);
    assert!((transform.dx_pt - 0.0).abs() < 1e-6);
    assert!((transform.dy_pt - 0.0).abs() < 1e-6);
    assert_eq!(transform.clipping.total_pt(), 0.0);
}

#[test]
fn test_feat12_actual_size_submillimeter_precision_and_metric_conversion() {
    let pt_per_mm = 72.0 / 25.4;
    let page_size = (595.20 + 2.0 * pt_per_mm, 841.68 + 2.0 * pt_per_mm);
    let printable = PrintableBounds::a4_standard_margins();

    let transform = calculate_page_transform(
        page_size,
        &printable,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );

    let mm = transform.clipping.to_mm();
    // 8.4 pt / pt_per_mm = 2.963 mm margin + 1 mm bleed = ~3.963 mm
    assert!(mm.left_mm > 3.9);
    assert!(mm.right_mm > 3.9);
    assert!(mm.left_mm + mm.top_mm + mm.right_mm + mm.bottom_mm > 15.0);
}

// =========================================================================
// FEATURE 13: Duplex: Flip on Long Edge (Booklet) (5 Tests)
// =========================================================================

#[test]
fn test_feat13_v3_devmode_long_edge_dmdup_vertical() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();
    devmode_buf[72..76].copy_from_slice(&DM_DUPLEX.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_VERTICAL.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    ).unwrap();

    mock.assert_duplex_asserted(DuplexBinding::FlipLongEdge);
    let last = mock.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_duplex, Some(2));
}

#[test]
fn test_feat13_v4_xps_printticket_long_edge_merge() {
    let mock = MockV4Driver::new(MockV4DriverConfig {
        supports_duplex: true,
        ..Default::default()
    });
    mock.pt_open_provider();

    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
        <psf:Option name="psk:TwoSidedLongEdge"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged.contains("psk:TwoSidedLongEdge"));
    assert!(mock.conflicts().is_empty());
}

#[test]
fn test_feat13_cups_ipp_long_edge_attribute_submission() {
    let mock = MockIppDriver::new("Office_Laser", true);
    let mut attrs = HashMap::new();
    attrs.insert("sides".to_string(), "two-sided-long-edge".to_string());

    let job_id = mock.submit_ipp_job(attrs, b"%PDF payload").unwrap();
    assert!(job_id >= 1001);
    mock.assert_attribute("sides", "two-sided-long-edge");
}

#[test]
fn test_feat13_duplex_sheet_pair_planning_even_page_count() {
    let sizes = vec![
        (595.0, 842.0),
        (595.0, 842.0),
        (595.0, 842.0),
        (595.0, 842.0),
    ];
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipLongEdge);
    assert_eq!(sheets.len(), 2);
    assert_eq!(sheets[0].front_page, 0);
    assert_eq!(sheets[0].back_page, Some(1));
    assert!(sheets[0].master_is_portrait);
    assert!(!sheets[0].back_needs_rotation);
    assert_eq!(sheets[1].front_page, 2);
    assert_eq!(sheets[1].back_page, Some(3));
}

#[test]
fn test_feat13_duplex_sheet_pair_planning_odd_page_count() {
    let sizes = vec![
        (595.0, 842.0),
        (595.0, 842.0),
        (595.0, 842.0),
    ];
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipLongEdge);
    assert_eq!(sheets.len(), 2);
    assert_eq!(sheets[0].front_page, 0);
    assert_eq!(sheets[0].back_page, Some(1));
    assert_eq!(sheets[1].front_page, 2);
    assert_eq!(sheets[1].back_page, None);
    assert!(!sheets[1].back_needs_rotation);
}

// =========================================================================
// FEATURE 14: Duplex: Flip on Short Edge (Tablet) (5 Tests)
// =========================================================================

#[test]
fn test_feat14_v3_devmode_short_edge_dmdup_horizontal() {
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
fn test_feat14_v4_xps_printticket_short_edge_merge() {
    let mock = MockV4Driver::new(MockV4DriverConfig {
        supports_duplex: true,
        ..Default::default()
    });
    mock.pt_open_provider();

    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
        <psf:Option name="psk:TwoSidedShortEdge"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged.contains("psk:TwoSidedShortEdge"));
    assert!(mock.conflicts().is_empty());
}

#[test]
fn test_feat14_cups_ipp_short_edge_attribute_submission() {
    let mock = MockIppDriver::new("Office_Laser", true);
    let mut attrs = HashMap::new();
    attrs.insert("sides".to_string(), "two-sided-short-edge".to_string());

    let job_id = mock.submit_ipp_job(attrs, b"%PDF payload").unwrap();
    assert!(job_id >= 1001);
    mock.assert_attribute("sides", "two-sided-short-edge");
}

#[test]
fn test_feat14_presentation_calendar_landscape_planning() {
    let sizes = vec![
        (842.0, 595.0),
        (842.0, 595.0),
        (842.0, 595.0),
        (842.0, 595.0),
    ];
    let sheets = plan_duplex_sheets(&sizes, PrintDuplex::FlipShortEdge);
    assert_eq!(sheets.len(), 2);
    assert!(!sheets[0].master_is_portrait);
    assert!(!sheets[0].back_needs_rotation);
}

#[test]
fn test_feat14_duplex_binding_roundtrip_conversion() {
    let b = PrintDuplex::FlipShortEdge;
    assert_eq!(b.to_devmode_duplex(), 3);
    assert_eq!(PrintDuplex::from_devmode_duplex(3), Some(b));
    assert_eq!(b.to_printticket_option(), "psk:TwoSidedShortEdge");
    assert_eq!(b.to_ipp_sides(), "two-sided-short-edge");
    assert!(b.is_two_sided());
}

// =========================================================================
// FEATURE 15: Sheet Master Orientation Alignment (5 Tests)
// =========================================================================

#[test]
fn test_feat15_portrait_master_landscape_back_long_edge() {
    let back_size = (842.0, 595.0);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        back_size,
        &printable,
        PageFitMode::FitPrintableArea,
        true,
        true,
        DuplexBinding::FlipLongEdge,
    );

    assert!(transform.rotation_deg == 90 || transform.rotation_deg == 270);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable((back_size.1, back_size.0), &printable, &transform);
}

#[test]
fn test_feat15_portrait_master_landscape_back_short_edge() {
    let back_size = (842.0, 595.0);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        back_size,
        &printable,
        PageFitMode::FitPrintableArea,
        true,
        true,
        DuplexBinding::FlipShortEdge,
    );

    assert!(transform.rotation_deg != 0);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable((back_size.1, back_size.0), &printable, &transform);
}

#[test]
fn test_feat15_landscape_master_portrait_back_long_edge() {
    let back_size = (595.0, 842.0);
    let printable = PrintableBounds {
        printable_width_pt: 824.88,
        printable_height_pt: 578.40,
        physical_width_pt: 841.68,
        physical_height_pt: 595.20,
        dpi_x: 600,
        dpi_y: 600,
        margins: HardwareMargins {
            left_pt: 8.4,
            top_pt: 8.4,
            right_pt: 8.4,
            bottom_pt: 8.4,
        },
    };

    let transform = calculate_page_transform(
        back_size,
        &printable,
        PageFitMode::FitPrintableArea,
        true,
        false, // master is landscape
        DuplexBinding::FlipLongEdge,
    );

    assert!(transform.rotation_deg != 0);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable((back_size.1, back_size.0), &printable, &transform);
}

#[test]
fn test_feat15_matching_orientations_zero_rotation() {
    let back_size = (595.0, 842.0);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        back_size,
        &printable,
        PageFitMode::FitPrintableArea,
        true,
        true, // both portrait
        DuplexBinding::FlipLongEdge,
    );

    assert_eq!(transform.rotation_deg, 0);
}

#[test]
fn test_feat15_simplex_mixed_orientations_ignores_duplex_alignment() {
    let landscape_page = (842.0, 595.0);
    let printable = PrintableBounds::a4_standard_margins();
    let transform = calculate_page_transform(
        landscape_page,
        &printable,
        PageFitMode::FitPrintableArea,
        true,
        true,
        DuplexBinding::Simplex,
    );

    assert_eq!(transform.rotation_deg, 0);
}

// =========================================================================
// FEATURE 16: Pre-Flight Capability Check & Alerting (5 Tests)
// =========================================================================

#[test]
fn test_feat16_preflight_valid_job_matches_hardware() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "EPSON_WF_C579R".to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![
            pdfcraft_print::driver::DuplexBinding::Simplex,
            pdfcraft_print::driver::DuplexBinding::FlipLongEdge,
        ],
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
        printer_name: "EPSON_WF_C579R".to_string(),
        copies: 2,
        color_mode: ColorMode::Color,
        duplex_mode: pdfcraft_print::driver::DuplexBinding::FlipLongEdge,
        fit_mode: pdfcraft_print::driver::PageFitMode::FitPrintableArea,
        page_bounds: (595.28, 841.89),
        ..Default::default()
    };

    let report = validate_job_capabilities(&caps, &settings);
    assert!(report.is_valid_for_dispatch);
    assert_eq!(report.issues, vec![PreFlightSeverity::Ok]);
    assert!(!report.has_critical_mismatch());
}

#[test]
fn test_feat16_preflight_duplex_on_simplex_printer_alert() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Simple_Desktop".to_string(),
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
            margins: pdfcraft_print::driver::HardwareMargins {
                left_pt: 8.40,
                top_pt: 8.40,
                right_pt: 8.40,
                bottom_pt: 8.40,
            },
        },
    };

    let settings = pdfcraft_print::driver::PrintJobSettings {
        printer_name: "Simple_Desktop".to_string(),
        copies: 1,
        color_mode: ColorMode::Color,
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
fn test_feat16_preflight_color_on_monochrome_printer_alert() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Mono_Office".to_string(),
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
            margins: pdfcraft_print::driver::HardwareMargins {
                left_pt: 8.40,
                top_pt: 8.40,
                right_pt: 8.40,
                bottom_pt: 8.40,
            },
        },
    };

    let settings = pdfcraft_print::driver::PrintJobSettings {
        printer_name: "Mono_Office".to_string(),
        copies: 1,
        color_mode: ColorMode::Color,
        duplex_mode: pdfcraft_print::driver::DuplexBinding::Simplex,
        fit_mode: pdfcraft_print::driver::PageFitMode::FitPrintableArea,
        page_bounds: (595.28, 841.89),
        ..Default::default()
    };

    let report = validate_job_capabilities(&caps, &settings);
    assert!(!report.is_valid_for_dispatch);
    assert!(report.has_critical_mismatch());
}

#[test]
fn test_feat16_preflight_copies_exceeding_max_capabilities_alert() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Limited_Copies_Printer".to_string(),
        driver_generation: pdfcraft_print::driver::DriverGeneration::Win32V3DevMode,
        is_duplex_supported: true,
        supported_duplex: vec![pdfcraft_print::driver::DuplexBinding::Simplex],
        is_color_supported: true,
        max_copies: 10,
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
        printer_name: "Limited_Copies_Printer".to_string(),
        copies: 50,
        color_mode: ColorMode::Color,
        duplex_mode: pdfcraft_print::driver::DuplexBinding::Simplex,
        fit_mode: pdfcraft_print::driver::PageFitMode::FitPrintableArea,
        page_bounds: (595.28, 841.89),
        ..Default::default()
    };

    let report = validate_job_capabilities(&caps, &settings);
    assert!(!report.is_valid_for_dispatch);
    assert!(report.has_critical_mismatch());
}

#[test]
fn test_feat16_preflight_actual_size_clipping_warning_allowed() {
    let caps = pdfcraft_print::driver::PrinterCapabilities {
        name: "Standard_Laser".to_string(),
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
        printer_name: "Standard_Laser".to_string(),
        copies: 1,
        color_mode: ColorMode::Color,
        duplex_mode: pdfcraft_print::driver::DuplexBinding::Simplex,
        fit_mode: pdfcraft_print::driver::PageFitMode::ActualSize,
        page_bounds: (595.28, 841.89), // bleed page exceeding printable
        ..Default::default()
    };

    let report = validate_job_capabilities(&caps, &settings);
    assert!(report.is_valid_for_dispatch); // Warning allows dispatch
    assert!(report.has_clipping_hazard());
    assert!(!report.has_critical_mismatch());
}

// =========================================================================
// FEATURE 17: Zero-Copy Shared Memory Architecture (5 Tests)
// =========================================================================

#[test]
fn test_feat17_zerocopy_pointer_identity_across_concurrent_workers() {
    let payload = vec![0x55u8; 1_000_000];
    let original_ptr = payload.as_ptr() as usize;
    let source = SharedPdfSource::from_bytes(payload);

    let mut handles = Vec::new();
    for _ in 0..4 {
        let src_clone = source.clone();
        handles.push(std::thread::spawn(move || {
            src_clone.as_ptr() as usize
        }));
    }

    for h in handles {
        let worker_ptr = h.join().unwrap();
        assert_eq!(worker_ptr, original_ptr);
    }
}

#[test]
fn test_feat17_zerocopy_mmap_from_disk_file() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("document.pdf");
    let content = vec![0xAAu8; 65536];
    std::fs::write(&file_path, &content).unwrap();

    let source = SharedPdfSource::from_file(&file_path).unwrap();
    assert!(source.is_mmap());
    assert_eq!(source.len(), 65536);
    assert_eq!(source.as_slice(), &content[..]);
}

#[test]
fn test_feat17_zerocopy_subslice_bounds_safety() {
    let content = vec![0x12u8; 1000];
    let source = SharedPdfSource::from_bytes(content);

    let slice_valid = source.slice(100..200);
    assert!(slice_valid.is_some());
    assert_eq!(slice_valid.unwrap().len(), 100);

    let slice_oob = source.slice(900..1100);
    assert!(slice_oob.is_none());

    let slice_empty = source.slice(50..50);
    assert_eq!(slice_empty, Some(&[][..]));
}

#[test]
fn test_feat17_zerocopy_empty_file_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let empty_path = dir.path().join("empty.pdf");
    File::create(&empty_path).unwrap();

    let res = SharedPdfSource::from_file(&empty_path);
    assert!(matches!(res, Err(SnapshotError::EmptyFile)));
}

#[test]
fn test_feat17_zerocopy_memory_probe_rss_invariance_benchmark() {
    let probe = MemoryProbe::start();
    let heavy_payload = vec![0x33u8; 5_000_000];
    let source = SharedPdfSource::from_bytes(heavy_payload);

    let mut handles = Vec::new();
    for _ in 0..5 {
        let s = source.clone();
        handles.push(std::thread::spawn(move || {
            let slice = s.slice(0..1_000_000).unwrap();
            slice[0]
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let (_init, _peak, delta) = probe.stop();
    // Memory delta should not scale by 5x (5 * 5MB = 25MB). Delta RSS comfortably < 15MB.
    BenchmarkAssertions::assert_peak_memory_delta(delta, 15.0);
}

// =========================================================================
// FEATURE 18: Page-by-Page Streaming with Backpressure (5 Tests)
// =========================================================================

#[test]
fn test_feat18_bounded_stream_channel_capacity_2_backpressure() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    // Push item 0 and item 1 (succeed immediately with capacity 2)
    tx.send_with_backpressure(make_test_sheet(0, 3, vec![0; 10]), &token).unwrap();
    tx.send_with_backpressure(make_test_sheet(1, 3, vec![1; 10]), &token).unwrap();

    // Drain item 0 to free up capacity
    let popped0 = rx.try_recv().unwrap().expect("received sheet 0");
    assert_eq!(popped0.sheet_index, 0);

    // Sender pushes item 2 into the newly freed slot
    tx.send_with_backpressure(make_test_sheet(2, 3, vec![2; 10]), &token).unwrap();

    let popped1 = rx.try_recv().unwrap().expect("received sheet 1");
    assert_eq!(popped1.sheet_index, 1);
    let popped2 = rx.try_recv().unwrap().expect("received sheet 2");
    assert_eq!(popped2.sheet_index, 2);
}

#[test]
fn test_feat18_o1_memory_invariance_across_100_streaming_sheets() {
    let spooler = MockSpooler::new();
    let ledger = CommittedSheetsLedger::new(100, 2);
    let probe = MemoryProbe::start();

    spooler
        .spool_sheet_stream(1801, &ledger, 100, |_idx| {
            Some(vec![0xAAu8; 65536])
        })
        .expect("Spool 100 sheets");

    let (_init, _peak, delta) = probe.stop();
    assert!(ledger.is_all_committed());
    // Bounded O(1) memory: 100 * 64KB discarded after commit, delta RSS < 20MB
    BenchmarkAssertions::assert_peak_memory_delta(delta, 20.0);
}

#[test]
fn test_feat18_ledger_peak_in_flight_backpressure_enforcement() {
    let ledger = CommittedSheetsLedger::new(20, 2);

    for i in 0..20 {
        ledger.begin_sheet(i, 1).unwrap();
        ledger.commit_sheet(i, 1024);
    }

    assert!(ledger.peak_in_flight_observed() <= 2);
    assert_eq!(ledger.current_in_flight(), 0);
    assert_eq!(ledger.committed_count(), 20);
}

#[test]
fn test_feat18_cooperative_cancellation_mid_stream() {
    let (tx, _rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    tx.send_with_backpressure(make_test_sheet(0, 2, vec![1; 10]), &token).unwrap();
    token.cancel();

    let res = tx.send_with_backpressure(make_test_sheet(1, 2, vec![2; 10]), &token);
    assert!(res.is_err());
    let err = res.err().unwrap().to_string();
    assert!(err.contains("cancelled") || err.contains("Canceled"));
}

#[test]
fn test_feat18_consumer_disconnection_clean_abort() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    // Push 2 items
    tx.send_with_backpressure(make_test_sheet(0, 3, vec![1; 10]), &token).unwrap();
    tx.send_with_backpressure(make_test_sheet(1, 3, vec![2; 10]), &token).unwrap();

    // Drop consumer
    drop(rx);

    // Third send should detect disconnected consumer and error cleanly without panic
    let res = tx.send_with_backpressure(make_test_sheet(2, 3, vec![3; 10]), &token);
    assert!(res.is_err());
}
