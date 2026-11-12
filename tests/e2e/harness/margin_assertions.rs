use super::mock_driver::{DuplexBinding, PrintableBounds};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageFitMode {
    FitPrintableArea,
    ShrinkOversized,
    ActualSize,
}

/// Overlap of content extending into non-printable hardware margins.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClippingExtent {
    pub left_pt: f64,
    pub top_pt: f64,
    pub right_pt: f64,
    pub bottom_pt: f64,
}

impl ClippingExtent {
    pub fn zero() -> Self {
        Self::default()
    }

    pub fn total_pt(&self) -> f64 {
        self.left_pt + self.top_pt + self.right_pt + self.bottom_pt
    }

    pub fn is_clipped(&self, tolerance_pt: f64) -> bool {
        self.total_pt() > tolerance_pt
    }

    pub fn to_mm(&self) -> ClippingExtentMm {
        const PT_TO_MM: f64 = 25.4 / 72.0;
        ClippingExtentMm {
            left_mm: self.left_pt * PT_TO_MM,
            top_mm: self.top_pt * PT_TO_MM,
            right_mm: self.right_pt * PT_TO_MM,
            bottom_mm: self.bottom_pt * PT_TO_MM,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClippingExtentMm {
    pub left_mm: f64,
    pub top_mm: f64,
    pub right_mm: f64,
    pub bottom_mm: f64,
}

/// Calculated transformation applied to a page before spooling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageTransform {
    pub scale: f64,
    pub dx_pt: f64, // translation in GDI printable coordinates
    pub dy_pt: f64,
    pub rotation_deg: u16,
    pub clipping: ClippingExtent,
}

/// Computes the exact geometric transformation for a page.
pub fn calculate_page_transform(
    page_bounds: (f64, f64),
    printable: &PrintableBounds,
    mode: PageFitMode,
    is_duplex_back_page: bool,
    master_is_portrait: bool,
    duplex_mode: DuplexBinding,
) -> PageTransform {
    let (mut w_page, mut h_page) = page_bounds;
    let mut rotation_deg = 0;

    let page_is_portrait = h_page >= w_page;

    // Duplex Sheet Master Orientation Alignment:
    // If back page orientation mismatches sheet master orientation, rotate 90 or 270 deg
    if is_duplex_back_page && page_is_portrait != master_is_portrait && duplex_mode != DuplexBinding::Simplex {
        rotation_deg = match duplex_mode {
            DuplexBinding::FlipLongEdge => 90,
            DuplexBinding::FlipShortEdge => 270,
            DuplexBinding::Simplex => 0,
        };
        std::mem::swap(&mut w_page, &mut h_page);
    }

    let w_print = printable.printable_width_pt;
    let h_print = printable.printable_height_pt;

    let (scale, clipping) = match mode {
        PageFitMode::FitPrintableArea => {
            let scale_x = w_print / w_page;
            let scale_y = h_print / h_page;
            let s = scale_x.min(scale_y);
            (s, ClippingExtent::zero())
        }
        PageFitMode::ShrinkOversized => {
            if w_page > w_print || h_page > h_print {
                let scale_x = w_print / w_page;
                let scale_y = h_print / h_page;
                let s = scale_x.min(scale_y);
                (s, ClippingExtent::zero())
            } else {
                (1.0, ClippingExtent::zero())
            }
        }
        PageFitMode::ActualSize => {
            // Actual Size: 1.0 scale centered on physical sheet
            let w_phys = printable.physical_width_pt;
            let h_phys = printable.physical_height_pt;
            let margins = printable.margins;

            let sheet_x0 = (w_phys - w_page) / 2.0;
            let sheet_y0 = (h_phys - h_page) / 2.0;

            let clip_left = (margins.left_pt - sheet_x0).max(0.0);
            let clip_top = (margins.top_pt - sheet_y0).max(0.0);
            let clip_right = ((sheet_x0 + w_page) - (w_phys - margins.right_pt)).max(0.0);
            let clip_bottom = ((sheet_y0 + h_page) - (h_phys - margins.bottom_pt)).max(0.0);

            (
                1.0,
                ClippingExtent {
                    left_pt: clip_left,
                    top_pt: clip_top,
                    right_pt: clip_right,
                    bottom_pt: clip_bottom,
                },
            )
        }
    };

    // Calculate centering translation in GDI printable coordinates
    let scaled_w = w_page * scale;
    let scaled_h = h_page * scale;

    let (dx_pt, dy_pt) = match mode {
        PageFitMode::ActualSize => (
            (printable.physical_width_pt - scaled_w) / 2.0 - printable.margins.left_pt,
            (printable.physical_height_pt - scaled_h) / 2.0 - printable.margins.top_pt,
        ),
        _ => (
            (w_print - scaled_w) / 2.0,
            (h_print - scaled_h) / 2.0,
        ),
    };

    PageTransform {
        scale,
        dx_pt,
        dy_pt,
        rotation_deg,
        clipping,
    }
}

pub struct MarginAssertions;

impl MarginAssertions {
    pub const TOLERANCE_PT: f64 = 1e-4; // 0.035 micrometers

    /// Assert that the transformed page is completely within printable bounds with zero clipping.
    pub fn assert_within_printable(
        page_size: (f64, f64),
        printable: &PrintableBounds,
        transform: &PageTransform,
    ) {
        let (w_page, h_page) = page_size;
        let x0 = transform.dx_pt;
        let y0 = transform.dy_pt;
        let x1 = x0 + w_page * transform.scale;
        let y1 = y0 + h_page * transform.scale;

        assert!(
            x0 >= -Self::TOLERANCE_PT,
            "Left edge extends outside printable area: x0 = {:.4} pt",
            x0
        );
        assert!(
            y0 >= -Self::TOLERANCE_PT,
            "Top edge extends outside printable area: y0 = {:.4} pt",
            y0
        );
        assert!(
            x1 <= printable.printable_width_pt + Self::TOLERANCE_PT,
            "Right edge extends outside printable area: x1 = {:.4} pt, max = {:.4} pt",
            x1,
            printable.printable_width_pt
        );
        assert!(
            y1 <= printable.printable_height_pt + Self::TOLERANCE_PT,
            "Bottom edge extends outside printable area: y1 = {:.4} pt, max = {:.4} pt",
            y1,
            printable.printable_height_pt
        );
        assert!(
            !transform.clipping.is_clipped(Self::TOLERANCE_PT),
            "Expected 0 clipping, found: {:?}",
            transform.clipping
        );
    }

    /// Assert that page is centered within printable bounds.
    pub fn assert_centered(
        page_size: (f64, f64),
        printable: &PrintableBounds,
        transform: &PageTransform,
    ) {
        let (w_page, h_page) = page_size;
        let expected_dx = (printable.printable_width_pt - w_page * transform.scale) / 2.0;
        let expected_dy = (printable.printable_height_pt - h_page * transform.scale) / 2.0;

        assert!(
            (transform.dx_pt - expected_dx).abs() <= Self::TOLERANCE_PT,
            "Horizontal centering mismatch: dx = {:.4}, expected = {:.4}",
            transform.dx_pt,
            expected_dx
        );
        assert!(
            (transform.dy_pt - expected_dy).abs() <= Self::TOLERANCE_PT,
            "Vertical centering mismatch: dy = {:.4}, expected = {:.4}",
            transform.dy_pt,
            expected_dy
        );
    }

    /// Assert exact clipping extent matches expected values for Actual Size mode.
    pub fn assert_clipping_hazard(actual: &ClippingExtent, expected: &ClippingExtent) {
        assert!(
            (actual.left_pt - expected.left_pt).abs() <= Self::TOLERANCE_PT,
            "Left clipping mismatch: actual = {:.4}, expected = {:.4}",
            actual.left_pt,
            expected.left_pt
        );
        assert!(
            (actual.top_pt - expected.top_pt).abs() <= Self::TOLERANCE_PT,
            "Top clipping mismatch: actual = {:.4}, expected = {:.4}",
            actual.top_pt,
            expected.top_pt
        );
        assert!(
            (actual.right_pt - expected.right_pt).abs() <= Self::TOLERANCE_PT,
            "Right clipping mismatch: actual = {:.4}, expected = {:.4}",
            actual.right_pt,
            expected.right_pt
        );
        assert!(
            (actual.bottom_pt - expected.bottom_pt).abs() <= Self::TOLERANCE_PT,
            "Bottom clipping mismatch: actual = {:.4}, expected = {:.4}",
            actual.bottom_pt,
            expected.bottom_pt
        );
    }
}
