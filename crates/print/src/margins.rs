//! Hardware margin adaptation, printable boundary coordinate transforms, and page fitting engines.
//!
//! Provides dynamic page transformations to adapt PDF pages to physical printer hardware margins:
//! - "Fit to Printable Area": Uniformly scales content to fit within hardware margins (0% margin clipping).
//! - "Shrink Oversized": Scales down pages exceeding printable bounds; leaves smaller pages 1:1.
//! - "Actual Size": 1:1 scale rendering with exact pre-flight clipping hazard detection across all 4 edges.

use crate::duplex::DuplexBinding;

/// Physical unprintable margins of a printer in points (1/72 inch).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HardwareMargins {
    pub left_pt: f64,
    pub top_pt: f64,
    pub right_pt: f64,
    pub bottom_pt: f64,
}

impl HardwareMargins {
    pub fn zero() -> Self {
        Self {
            left_pt: 0.0,
            top_pt: 0.0,
            right_pt: 0.0,
            bottom_pt: 0.0,
        }
    }

    pub fn uniform(margin_pt: f64) -> Self {
        let m = margin_pt.max(0.0);
        Self {
            left_pt: m,
            top_pt: m,
            right_pt: m,
            bottom_pt: m,
        }
    }
}

/// Hardware printable boundary dimensions and physical sheet properties.
#[derive(Debug, Clone, PartialEq)]
pub struct PrintableBounds {
    pub printable_width_pt: f64,
    pub printable_height_pt: f64,
    pub physical_width_pt: f64,
    pub physical_height_pt: f64,
    pub dpi_x: u32,
    pub dpi_y: u32,
    pub margins: HardwareMargins,
}

impl PrintableBounds {
    /// Constructs PrintableBounds from Win32 GDI `GetDeviceCaps` query results.
    #[allow(clippy::too_many_arguments)]
    pub fn from_gdi_caps(
        phys_w_px: i32,
        phys_h_px: i32,
        horzres_px: i32,
        vertres_px: i32,
        offset_x_px: i32,
        offset_y_px: i32,
        dpi_x: u32,
        dpi_y: u32,
    ) -> Self {
        let dpi_x_f = (dpi_x.max(1)) as f64;
        let dpi_y_f = (dpi_y.max(1)) as f64;

        let to_pt_x = |px: i32| (px.max(0) as f64 / dpi_x_f) * 72.0;
        let to_pt_y = |px: i32| (px.max(0) as f64 / dpi_y_f) * 72.0;

        let physical_width_pt = to_pt_x(phys_w_px);
        let physical_height_pt = to_pt_y(phys_h_px);
        let printable_width_pt = to_pt_x(horzres_px);
        let printable_height_pt = to_pt_y(vertres_px);

        let left_pt = to_pt_x(offset_x_px);
        let top_pt = to_pt_y(offset_y_px);

        let right_px = phys_w_px.saturating_sub(horzres_px).saturating_sub(offset_x_px).max(0);
        let bottom_px = phys_h_px.saturating_sub(vertres_px).saturating_sub(offset_y_px).max(0);
        let right_pt = to_pt_x(right_px);
        let bottom_pt = to_pt_y(bottom_px);

        Self {
            printable_width_pt,
            printable_height_pt,
            physical_width_pt,
            physical_height_pt,
            dpi_x,
            dpi_y,
            margins: HardwareMargins {
                left_pt,
                top_pt,
                right_pt,
                bottom_pt,
            },
        }
    }

    /// Factory for borderless or virtual printers (e.g. Microsoft Print to PDF).
    pub fn zero_margins(width_pt: f64, height_pt: f64, dpi: u32) -> Self {
        Self {
            printable_width_pt: width_pt,
            printable_height_pt: height_pt,
            physical_width_pt: width_pt,
            physical_height_pt: height_pt,
            dpi_x: dpi,
            dpi_y: dpi,
            margins: HardwareMargins::zero(),
        }
    }

    /// Standard fallback safety margins (0.25 inch = 18 pt).
    pub fn default_safety(width_pt: f64, height_pt: f64) -> Self {
        let m = 18.0;
        Self {
            printable_width_pt: (width_pt - 2.0 * m).max(1.0),
            printable_height_pt: (height_pt - 2.0 * m).max(1.0),
            physical_width_pt: width_pt,
            physical_height_pt: height_pt,
            dpi_x: 600,
            dpi_y: 600,
            margins: HardwareMargins::uniform(m),
        }
    }
}

/// Page fitting mode selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageFitMode {
    #[default]
    FitPrintableArea,
    ShrinkOversized,
    ActualSize,
}

/// Exact clipped point measurement across the 4 edges of the physical paper.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClippingExtent {
    pub left_pt: f64,
    pub top_pt: f64,
    pub right_pt: f64,
    pub bottom_pt: f64,
}

impl ClippingExtent {
    pub fn total_clipped_pt(&self) -> f64 {
        self.left_pt + self.top_pt + self.right_pt + self.bottom_pt
    }

    pub fn has_clipping(&self) -> bool {
        self.total_clipped_pt() > 1e-4
    }
}

/// Computed coordinate transform and pre-flight clipping hazard report.
#[derive(Debug, Clone, PartialEq)]
pub struct PageTransform {
    pub scale: f64,
    pub dx_pt: f64,
    pub dy_pt: f64,
    pub rotation_deg: u16,
    pub clipping_hazard: Option<ClippingExtent>,
}

impl PageTransform {
    /// Generates a PDF 6-element transformation matrix `[a, b, c, d, e, f]`.
    pub fn to_matrix(&self, page_bounds: (f64, f64)) -> [f64; 6] {
        let s = self.scale;
        let (x, y) = (self.dx_pt, self.dy_pt);
        let (w, h) = page_bounds;
        let pw = w * s;
        let ph = h * s;

        match self.rotation_deg % 360 {
            0 => [s, 0.0, 0.0, s, x, y],
            90 => [0.0, -s, s, 0.0, x, y + pw],
            180 => [-s, 0.0, 0.0, -s, x + pw, y + ph],
            270 => [0.0, s, -s, 0.0, x + ph, y],
            _ => [s, 0.0, 0.0, s, x, y],
        }
    }
}

/// Calculates the exact transformation, scaling, translation, and clipping hazard for a page.
pub fn calculate_page_transform(
    page_bounds: (f64, f64),
    printable: &PrintableBounds,
    mode: PageFitMode,
    is_duplex_back_page: bool,
    master_is_portrait: bool,
    duplex_mode: DuplexBinding,
) -> PageTransform {
    let (orig_w, orig_h) = page_bounds;
    if !orig_w.is_finite() || !orig_h.is_finite() || orig_w <= 0.0 || orig_h <= 0.0 {
        return PageTransform {
            scale: 1.0,
            dx_pt: 0.0,
            dy_pt: 0.0,
            rotation_deg: 0,
            clipping_hazard: None,
        };
    }

    let page_is_portrait = orig_h >= orig_w;
    let mut rotation_deg = 0u16;

    // Duplex Sheet Master Orientation Alignment
    if is_duplex_back_page && duplex_mode != DuplexBinding::Simplex && page_is_portrait != master_is_portrait {
        rotation_deg = match (master_is_portrait, duplex_mode) {
            (true, DuplexBinding::FlipLongEdge) => 270,
            (true, DuplexBinding::FlipShortEdge) => 90,
            (false, DuplexBinding::FlipLongEdge) => 90,
            (false, DuplexBinding::FlipShortEdge) => 270,
            _ => 0,
        };
    }

    let (eff_w, eff_h) = if rotation_deg == 90 || rotation_deg == 270 {
        (orig_h, orig_w)
    } else {
        (orig_w, orig_h)
    };

    let p_w = printable.printable_width_pt.max(1.0);
    let p_h = printable.printable_height_pt.max(1.0);
    let phys_w = printable.physical_width_pt.max(p_w);
    let phys_h = printable.physical_height_pt.max(p_h);
    let m = printable.margins;

    match mode {
        PageFitMode::FitPrintableArea => {
            let s_x = p_w / eff_w;
            let s_y = p_h / eff_h;
            let scale = s_x.min(s_y);
            let scaled_w = eff_w * scale;
            let scaled_h = eff_h * scale;
            let dx_pt = (p_w - scaled_w) / 2.0;
            let dy_pt = (p_h - scaled_h) / 2.0;

            PageTransform {
                scale,
                dx_pt,
                dy_pt,
                rotation_deg,
                clipping_hazard: None,
            }
        }
        PageFitMode::ShrinkOversized => {
            let is_oversized = eff_w > p_w || eff_h > p_h;
            let scale = if is_oversized {
                (p_w / eff_w).min(p_h / eff_h)
            } else {
                1.0
            };
            let scaled_w = eff_w * scale;
            let scaled_h = eff_h * scale;
            let dx_pt = (p_w - scaled_w) / 2.0;
            let dy_pt = (p_h - scaled_h) / 2.0;

            PageTransform {
                scale,
                dx_pt,
                dy_pt,
                rotation_deg,
                clipping_hazard: None,
            }
        }
        PageFitMode::ActualSize => {
            let scale = 1.0;
            let x_sheet = (phys_w - eff_w) / 2.0;
            let y_sheet = (phys_h - eff_h) / 2.0;

            let dx_pt = x_sheet - m.left_pt;
            let dy_pt = y_sheet - m.top_pt;

            let clip_left = (m.left_pt - x_sheet).max(0.0);
            let clip_right = ((x_sheet + eff_w) - (phys_w - m.right_pt)).max(0.0);
            let clip_top = (m.top_pt - y_sheet).max(0.0);
            let clip_bottom = ((y_sheet + eff_h) - (phys_h - m.bottom_pt)).max(0.0);

            let clipping_hazard = if clip_left > 1e-4 || clip_right > 1e-4 || clip_top > 1e-4 || clip_bottom > 1e-4 {
                Some(ClippingExtent {
                    left_pt: clip_left,
                    top_pt: clip_top,
                    right_pt: clip_right,
                    bottom_pt: clip_bottom,
                })
            } else {
                None
            };

            PageTransform {
                scale,
                dx_pt,
                dy_pt,
                rotation_deg,
                clipping_hazard,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_a4_bounds() -> PrintableBounds {
        // Physical A4: 595.2 x 841.68 pt, 8.4pt uniform margins
        PrintableBounds {
            printable_width_pt: 578.4,
            printable_height_pt: 824.88,
            physical_width_pt: 595.2,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: HardwareMargins::uniform(8.4),
        }
    }

    #[test]
    fn test_fit_to_printable_area_oversized() {
        let bounds = sample_a4_bounds();
        let page = (842.0, 595.0); // Oversized landscape page
        let transform = calculate_page_transform(
            page,
            &bounds,
            PageFitMode::FitPrintableArea,
            false,
            true,
            DuplexBinding::Simplex,
        );

        // Should scale down to fit printable area (578.4 / 842.0 ≈ 0.6869)
        assert!(transform.scale < 1.0);
        assert!((transform.scale - (578.4 / 842.0)).abs() < 1e-4);
        assert!(transform.dx_pt >= 0.0);
        assert!(transform.dy_pt >= 0.0);
        assert_eq!(transform.clipping_hazard, None);
    }

    #[test]
    fn test_fit_to_printable_area_undersized() {
        let bounds = PrintableBounds {
            printable_width_pt: 600.0,
            printable_height_pt: 800.0,
            physical_width_pt: 650.0,
            physical_height_pt: 850.0,
            dpi_x: 300,
            dpi_y: 300,
            margins: HardwareMargins::uniform(25.0),
        };
        let page = (200.0, 300.0);
        let transform = calculate_page_transform(
            page,
            &bounds,
            PageFitMode::FitPrintableArea,
            false,
            true,
            DuplexBinding::Simplex,
        );

        // Scale up: min(600/200, 800/300) = min(3.0, 2.6667) = 2.6667
        assert!((transform.scale - (800.0 / 300.0)).abs() < 1e-4);
        assert_eq!(transform.clipping_hazard, None);
    }

    #[test]
    fn test_shrink_oversized_downscale() {
        let bounds = PrintableBounds {
            printable_width_pt: 600.0,
            printable_height_pt: 800.0,
            physical_width_pt: 650.0,
            physical_height_pt: 850.0,
            dpi_x: 300,
            dpi_y: 300,
            margins: HardwareMargins::uniform(25.0),
        };
        let page = (700.0, 900.0);
        let transform = calculate_page_transform(
            page,
            &bounds,
            PageFitMode::ShrinkOversized,
            false,
            true,
            DuplexBinding::Simplex,
        );

        // min(600/700, 800/900) = min(0.8571, 0.8888) = 0.8571
        assert!((transform.scale - (600.0 / 700.0)).abs() < 1e-4);
        assert_eq!(transform.clipping_hazard, None);
    }

    #[test]
    fn test_shrink_oversized_leaves_smaller_intact() {
        let bounds = PrintableBounds {
            printable_width_pt: 600.0,
            printable_height_pt: 800.0,
            physical_width_pt: 650.0,
            physical_height_pt: 850.0,
            dpi_x: 300,
            dpi_y: 300,
            margins: HardwareMargins::uniform(25.0),
        };
        let page = (400.0, 500.0);
        let transform = calculate_page_transform(
            page,
            &bounds,
            PageFitMode::ShrinkOversized,
            false,
            true,
            DuplexBinding::Simplex,
        );

        // Smaller than printable area: scale strictly 1.0
        assert_eq!(transform.scale, 1.0);
        assert_eq!(transform.dx_pt, 100.0);
        assert_eq!(transform.dy_pt, 150.0);
        assert_eq!(transform.clipping_hazard, None);
    }

    #[test]
    fn test_actual_size_no_clipping() {
        let bounds = PrintableBounds {
            printable_width_pt: 576.0,
            printable_height_pt: 756.0,
            physical_width_pt: 612.0,
            physical_height_pt: 792.0,
            dpi_x: 300,
            dpi_y: 300,
            margins: HardwareMargins::uniform(18.0),
        };
        let page = (400.0, 500.0);
        let transform = calculate_page_transform(
            page,
            &bounds,
            PageFitMode::ActualSize,
            false,
            true,
            DuplexBinding::Simplex,
        );

        assert_eq!(transform.scale, 1.0);
        assert_eq!(transform.clipping_hazard, None);
    }

    #[test]
    fn test_actual_size_detects_clipping_hazard() {
        let bounds = sample_a4_bounds();
        let page = (595.2, 841.68); // Full bleed page on sheet with 8.4pt margins
        let transform = calculate_page_transform(
            page,
            &bounds,
            PageFitMode::ActualSize,
            false,
            true,
            DuplexBinding::Simplex,
        );

        assert_eq!(transform.scale, 1.0);
        assert!(transform.clipping_hazard.is_some());
        let clip = transform.clipping_hazard.unwrap();
        assert!((clip.left_pt - 8.4).abs() < 1e-4);
        assert!((clip.right_pt - 8.4).abs() < 1e-4);
        assert!((clip.top_pt - 8.4).abs() < 1e-4);
        assert!((clip.bottom_pt - 8.4).abs() < 1e-4);
        assert!(clip.has_clipping());
        assert!((clip.total_clipped_pt() - 33.6).abs() < 1e-3);
    }

    #[test]
    fn test_actual_size_asymmetric_pos_margins() {
        let bounds = PrintableBounds {
            printable_width_pt: 180.0,
            printable_height_pt: 500.0,
            physical_width_pt: 202.0,
            physical_height_pt: 501.0,
            dpi_x: 203,
            dpi_y: 203,
            margins: HardwareMargins {
                left_pt: 10.64,
                top_pt: 0.0,
                right_pt: 11.36,
                bottom_pt: 1.0,
            },
        };
        let page = (200.0, 500.0);
        let transform = calculate_page_transform(
            page,
            &bounds,
            PageFitMode::ActualSize,
            false,
            true,
            DuplexBinding::Simplex,
        );

        assert_eq!(transform.scale, 1.0);
        assert!(transform.clipping_hazard.is_some());
        let clip = transform.clipping_hazard.unwrap();
        assert!(clip.left_pt > 0.0);
        assert!(clip.right_pt > 0.0);
    }

    #[test]
    fn test_zero_and_negative_input_defense() {
        let bounds = sample_a4_bounds();
        let transform = calculate_page_transform(
            (0.0, -100.0),
            &bounds,
            PageFitMode::FitPrintableArea,
            false,
            true,
            DuplexBinding::Simplex,
        );
        assert_eq!(transform.scale, 1.0);
        assert_eq!(transform.clipping_hazard, None);
    }

    #[test]
    fn test_matrix_generation() {
        let transform = PageTransform {
            scale: 2.0,
            dx_pt: 10.0,
            dy_pt: 20.0,
            rotation_deg: 0,
            clipping_hazard: None,
        };
        let m = transform.to_matrix((100.0, 200.0));
        assert_eq!(m, [2.0, 0.0, 0.0, 2.0, 10.0, 20.0]);
    }
}
