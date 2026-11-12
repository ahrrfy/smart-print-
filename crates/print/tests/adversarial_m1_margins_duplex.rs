//! Adversarial empirical verification tests for Milestone 1:
//! - Hardware margins adaptation engine (`margins.rs`)
//! - Duplex sheet orientation alignment engine (`duplex.rs`)

use pdfcraft_print::duplex::{
    align_sheet_pair_orientation, plan_duplex_sheets, DuplexBinding,
};
use pdfcraft_print::margins::{
    calculate_page_transform, HardwareMargins, PageFitMode, PrintableBounds,
};

// ============================================================================
// 1. ADVERSARIAL MARGINS & SCALING TESTS
// ============================================================================

/// Helper to create printable bounds with specific physical dimensions and margins.
fn create_test_bounds(
    phys_w: f64,
    phys_h: f64,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
) -> PrintableBounds {
    let p_w = (phys_w - left - right).max(1.0);
    let p_h = (phys_h - top - bottom).max(1.0);
    PrintableBounds {
        printable_width_pt: p_w,
        printable_height_pt: p_h,
        physical_width_pt: phys_w,
        physical_height_pt: phys_h,
        dpi_x: 600,
        dpi_y: 600,
        margins: HardwareMargins {
            left_pt: left,
            top_pt: top,
            right_pt: right,
            bottom_pt: bottom,
        },
    }
}

#[test]
fn test_zero_margins_borderless_printing() {
    // A4 borderless: 595.28 x 841.89 pt with 0 margins
    let bounds = PrintableBounds::zero_margins(595.28, 841.89, 600);
    assert_eq!(bounds.margins.left_pt, 0.0);
    assert_eq!(bounds.margins.right_pt, 0.0);
    assert_eq!(bounds.margins.top_pt, 0.0);
    assert_eq!(bounds.margins.bottom_pt, 0.0);
    assert_eq!(bounds.printable_width_pt, 595.28);
    assert_eq!(bounds.printable_height_pt, 841.89);

    // Exact matching page
    let transform = calculate_page_transform(
        (595.28, 841.89),
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(transform.scale, 1.0);
    assert!((transform.dx_pt).abs() < 1e-5);
    assert!((transform.dy_pt).abs() < 1e-5);
    assert_eq!(transform.clipping_hazard, None);

    // FitPrintableArea on borderless
    let transform_fit = calculate_page_transform(
        (595.28, 841.89),
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(transform_fit.scale, 1.0);
    assert!((transform_fit.dx_pt).abs() < 1e-5);
    assert!((transform_fit.dy_pt).abs() < 1e-5);
    assert_eq!(transform_fit.clipping_hazard, None);
}

#[test]
fn test_massive_unprintable_margins_receipt_printer() {
    // Receipt printer: 80mm roll (226.77 pt), 50mm unprintable top margin (~141.73 pt)
    let margin_50mm = (50.0 / 25.4) * 72.0; // 141.732 pt
    let phys_w = 226.77;
    let phys_h = 800.0;
    let bounds = create_test_bounds(phys_w, phys_h, 15.0, margin_50mm, 15.0, 30.0);

    let doc_size = (200.0, 700.0);

    // 1. FitPrintableArea must scale to avoid the massive margins
    let t_fit = calculate_page_transform(
        doc_size,
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert!(t_fit.scale < 1.0);
    assert_eq!(t_fit.clipping_hazard, None);
    let rendered_w = doc_size.0 * t_fit.scale;
    let rendered_h = doc_size.1 * t_fit.scale;
    assert!(rendered_w <= bounds.printable_width_pt + 1e-6);
    assert!(rendered_h <= bounds.printable_height_pt + 1e-6);

    // 2. ActualSize must detect severe clipping from the massive 50mm margin
    let t_act = calculate_page_transform(
        doc_size,
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(t_act.scale, 1.0);
    assert!(t_act.clipping_hazard.is_some());
    let clip = t_act.clipping_hazard.unwrap();
    assert!(clip.has_clipping());
    assert!(clip.top_pt > 0.0);
}

#[test]
fn test_asymmetric_margins_all_four_sides_different() {
    // Letter sheet: 612 x 792 pt
    // Asymmetric margins: left=36pt (0.5"), top=18pt (0.25"), right=72pt (1.0"), bottom=54pt (0.75")
    let bounds = create_test_bounds(612.0, 792.0, 36.0, 18.0, 72.0, 54.0);
    assert_eq!(bounds.printable_width_pt, 612.0 - 36.0 - 72.0); // 504.0
    assert_eq!(bounds.printable_height_pt, 792.0 - 18.0 - 54.0); // 720.0

    // Full physical size document: 612 x 792 pt
    let t_act = calculate_page_transform(
        (612.0, 792.0),
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(t_act.scale, 1.0);
    let clip = t_act.clipping_hazard.expect("Expected clipping hazard for full bleed on margined paper");
    // When centered on sheet, x_sheet = 0, y_sheet = 0
    // clip_left = 36 - 0 = 36.0
    // clip_right = 612 - (612 - 72) = 72.0
    // clip_top = 18 - 0 = 18.0
    // clip_bottom = 792 - (792 - 54) = 54.0
    assert!((clip.left_pt - 36.0).abs() < 1e-4, "Expected left clip 36, got {}", clip.left_pt);
    assert!((clip.right_pt - 72.0).abs() < 1e-4, "Expected right clip 72, got {}", clip.right_pt);
    assert!((clip.top_pt - 18.0).abs() < 1e-4, "Expected top clip 18, got {}", clip.top_pt);
    assert!((clip.bottom_pt - 54.0).abs() < 1e-4, "Expected bottom clip 54, got {}", clip.bottom_pt);
    assert!((clip.total_clipped_pt() - 180.0).abs() < 1e-4);
}

#[test]
fn test_extreme_aspect_ratios_banner_and_receipt() {
    let bounds = create_test_bounds(612.0, 792.0, 18.0, 18.0, 18.0, 18.0);
    let p_w = bounds.printable_width_pt; // 576.0
    let p_h = bounds.printable_height_pt; // 756.0

    // Extremely wide banner: 2000 x 200 pt (aspect ratio 10:1)
    let t_banner = calculate_page_transform(
        (2000.0, 200.0),
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(t_banner.clipping_hazard, None);
    assert!((t_banner.scale - (p_w / 2000.0)).abs() < 1e-4);
    assert!(t_banner.dx_pt >= 0.0);
    assert!(t_banner.dy_pt >= 0.0);
    let banner_scaled_h = 200.0 * t_banner.scale;
    assert!((t_banner.dy_pt - (p_h - banner_scaled_h) / 2.0).abs() < 1e-4);

    // Extremely tall strip: 200 x 3000 pt (aspect ratio 1:15)
    let t_strip = calculate_page_transform(
        (200.0, 3000.0),
        &bounds,
        PageFitMode::FitPrintableArea,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(t_strip.clipping_hazard, None);
    assert!((t_strip.scale - (p_h / 3000.0)).abs() < 1e-4);
    assert!(t_strip.dx_pt >= 0.0);
    assert!(t_strip.dy_pt >= 0.0);
    let strip_scaled_w = 200.0 * t_strip.scale;
    assert!((t_strip.dx_pt - (p_w - strip_scaled_w) / 2.0).abs() < 1e-4);
}

#[test]
fn test_shrink_oversized_never_upscales() {
    let bounds = create_test_bounds(600.0, 800.0, 20.0, 20.0, 20.0, 20.0);
    let p_w = bounds.printable_width_pt; // 560.0
    let p_h = bounds.printable_height_pt; // 760.0

    // Test a sweep of smaller page sizes
    let smaller_sizes = [
        (10.0, 10.0),
        (50.0, 100.0),
        (200.0, 300.0),
        (559.9, 759.9),
        (p_w, p_h), // Exact boundary
    ];

    for &(w, h) in &smaller_sizes {
        let t = calculate_page_transform(
            (w, h),
            &bounds,
            PageFitMode::ShrinkOversized,
            false,
            true,
            DuplexBinding::Simplex,
        );
        assert_eq!(t.scale, 1.0, "ShrinkOversized upscaled a smaller page of size ({}, {})!", w, h);
        assert_eq!(t.clipping_hazard, None);
        assert!(t.dx_pt >= 0.0);
        assert!(t.dy_pt >= 0.0);
    }

    // Now test oversized pages (must downscale strictly < 1.0)
    let oversized_sizes = [
        (p_w + 0.1, p_h),
        (p_w, p_h + 0.1),
        (1000.0, 500.0),
        (500.0, 1000.0),
        (2000.0, 2000.0),
    ];

    for &(w, h) in &oversized_sizes {
        let t = calculate_page_transform(
            (w, h),
            &bounds,
            PageFitMode::ShrinkOversized,
            false,
            true,
            DuplexBinding::Simplex,
        );
        assert!(t.scale < 1.0, "ShrinkOversized failed to downscale oversized page ({}, {})!", w, h);
        assert_eq!(t.clipping_hazard, None);
        assert!(w * t.scale <= p_w + 1e-6);
        assert!(h * t.scale <= p_h + 1e-6);
    }
}

#[test]
fn test_actual_size_precision_at_threshold() {
    let bounds = create_test_bounds(600.0, 800.0, 20.0, 20.0, 20.0, 20.0);
    let p_w = bounds.printable_width_pt; // 560.0
    let p_h = bounds.printable_height_pt; // 760.0

    // Exact fit inside printable area (centered on physical sheet: 600x800 -> 560x760)
    let t_exact = calculate_page_transform(
        (p_w, p_h),
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(t_exact.clipping_hazard, None);

    // Below 1e-4 pt threshold (e.g. 5e-5 pt clipping): should be treated as clean / floating noise
    let delta_sub = 5e-5;
    let t_sub = calculate_page_transform(
        (p_w + 2.0 * delta_sub, p_h),
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert_eq!(t_sub.clipping_hazard, None, "Clipping below 1e-4 pt should be ignored as float noise");

    // Above 1e-4 pt threshold (e.g. 2e-4 pt clipping on each side): must detect clipping!
    let delta_above = 2e-4;
    let t_above = calculate_page_transform(
        (p_w + 2.0 * delta_above, p_h),
        &bounds,
        PageFitMode::ActualSize,
        false,
        true,
        DuplexBinding::Simplex,
    );
    assert!(t_above.clipping_hazard.is_some(), "Clipping above 1e-4 pt must be detected!");
    let clip = t_above.clipping_hazard.unwrap();
    assert!((clip.left_pt - delta_above).abs() < 1e-5);
    assert!((clip.right_pt - delta_above).abs() < 1e-5);
}

#[test]
fn test_degenerate_and_adversarial_page_dimensions() {
    let bounds = create_test_bounds(600.0, 800.0, 20.0, 20.0, 20.0, 20.0);

    let degenerate_inputs = [
        (0.0, 100.0),
        (100.0, 0.0),
        (-10.0, 100.0),
        (100.0, -50.0),
        (f64::NAN, 100.0),
        (100.0, f64::NAN),
        (f64::INFINITY, 100.0),
        (100.0, f64::NEG_INFINITY),
    ];

    for &(w, h) in &degenerate_inputs {
        let t = calculate_page_transform(
            (w, h),
            &bounds,
            PageFitMode::FitPrintableArea,
            false,
            true,
            DuplexBinding::Simplex,
        );
        assert_eq!(t.scale, 1.0);
        assert_eq!(t.dx_pt, 0.0);
        assert_eq!(t.dy_pt, 0.0);
        assert_eq!(t.rotation_deg, 0);
        assert_eq!(t.clipping_hazard, None);
    }
}

#[test]
fn test_transformation_matrix_corners() {
    // Check that to_matrix() produces valid CTM for 0, 90, 180, 270 deg
    let page_w = 200.0;
    let page_h = 300.0;

    for &rot in &[0, 90, 180, 270] {
        let transform = pdfcraft_print::margins::PageTransform {
            scale: 1.5,
            dx_pt: 10.0,
            dy_pt: 20.0,
            rotation_deg: rot,
            clipping_hazard: None,
        };
        let m = transform.to_matrix((page_w, page_h));

        // Evaluate corners under matrix [a, b, c, d, e, f]:
        // x' = a*u + c*v + e
        // y' = b*u + d*v + f
        let corners = [(0.0, 0.0), (page_w, 0.0), (0.0, page_h), (page_w, page_h)];
        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_y = f64::NEG_INFINITY;

        for &(u, v) in &corners {
            let x_prime = m[0] * u + m[2] * v + m[4];
            let y_prime = m[1] * u + m[3] * v + m[5];
            min_x = min_x.min(x_prime);
            max_x = max_x.max(x_prime);
            min_y = min_y.min(y_prime);
            max_y = max_y.max(y_prime);
        }

        assert!((min_x - 10.0).abs() < 1e-4, "min_x mismatch at rot {}", rot);
        assert!((min_y - 20.0).abs() < 1e-4, "min_y mismatch at rot {}", rot);

        let (expected_w, expected_h) = if rot == 90 || rot == 270 {
            (page_h * 1.5, page_w * 1.5)
        } else {
            (page_w * 1.5, page_h * 1.5)
        };
        assert!((max_x - min_x - expected_w).abs() < 1e-4, "width mismatch at rot {}", rot);
        assert!((max_y - min_y - expected_h).abs() < 1e-4, "height mismatch at rot {}", rot);
    }
}

#[test]
fn test_gdi_caps_robustness_and_conversions() {
    // Standard 600 DPI Letter:
    // phys_w_px = 5100, phys_h_px = 6600
    // horzres_px = 4800, vertres_px = 6300
    // offset_x_px = 150, offset_y_px = 150
    let b = PrintableBounds::from_gdi_caps(
        5100, 6600, 4800, 6300, 150, 150, 600, 600,
    );
    assert_eq!(b.physical_width_pt, 612.0);
    assert_eq!(b.physical_height_pt, 792.0);
    assert_eq!(b.printable_width_pt, 576.0);
    assert_eq!(b.printable_height_pt, 756.0);
    assert_eq!(b.margins.left_pt, 18.0);
    assert_eq!(b.margins.top_pt, 18.0);
    assert_eq!(b.margins.right_pt, 18.0);
    assert_eq!(b.margins.bottom_pt, 18.0);

    // Adversarial: 0 DPI (must not divide by 0), negative px
    let b_zero_dpi = PrintableBounds::from_gdi_caps(
        5100, 6600, 4800, 6300, -50, -50, 0, 0,
    );
    assert!(b_zero_dpi.physical_width_pt.is_finite());
    assert!(b_zero_dpi.printable_width_pt.is_finite());
    assert_eq!(b_zero_dpi.margins.left_pt, 0.0);
    assert_eq!(b_zero_dpi.margins.top_pt, 0.0);
}

#[test]
fn test_property_based_stress_simulation_1000_combinations() {
    // Seeded linear congruential generator for reproducible pseudo-random numbers
    let mut state: u64 = 0xDEADBEEFCAFEBABE;
    let mut next_f64 = |min_val: f64, max_val: f64| -> f64 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let normalized = (state >> 11) as f64 / (1u64 << 53) as f64;
        min_val + normalized * (max_val - min_val)
    };

    for _ in 0..1000 {
        let phys_w = next_f64(100.0, 1500.0);
        let phys_h = next_f64(100.0, 2000.0);
        let m_left = next_f64(0.0, phys_w * 0.4);
        let m_right = next_f64(0.0, phys_w * 0.4);
        let m_top = next_f64(0.0, phys_h * 0.4);
        let m_bottom = next_f64(0.0, phys_h * 0.4);

        let bounds = create_test_bounds(phys_w, phys_h, m_left, m_top, m_right, m_bottom);
        let p_w = bounds.printable_width_pt;
        let p_h = bounds.printable_height_pt;

        let page_w = next_f64(10.0, 3000.0);
        let page_h = next_f64(10.0, 3000.0);

        // 1. Invariant: FitPrintableArea NEVER clips
        let t_fit = calculate_page_transform(
            (page_w, page_h),
            &bounds,
            PageFitMode::FitPrintableArea,
            false,
            true,
            DuplexBinding::Simplex,
        );
        assert!(t_fit.scale > 0.0 && t_fit.scale.is_finite());
        assert!(t_fit.dx_pt >= -1e-6);
        assert!(t_fit.dy_pt >= -1e-6);
        let rendered_w = page_w * t_fit.scale;
        let rendered_h = page_h * t_fit.scale;
        assert!(
            rendered_w <= p_w + 1e-5,
            "FitPrintableArea rendered_w {} > p_w {}",
            rendered_w,
            p_w
        );
        assert!(
            rendered_h <= p_h + 1e-5,
            "FitPrintableArea rendered_h {} > p_h {}",
            rendered_h,
            p_h
        );
        assert_eq!(t_fit.clipping_hazard, None);

        // 2. Invariant: ShrinkOversized never upscales
        let t_shrink = calculate_page_transform(
            (page_w, page_h),
            &bounds,
            PageFitMode::ShrinkOversized,
            false,
            true,
            DuplexBinding::Simplex,
        );
        if page_w <= p_w && page_h <= p_h {
            assert_eq!(t_shrink.scale, 1.0);
        } else {
            assert!(t_shrink.scale < 1.0);
            assert!(page_w * t_shrink.scale <= p_w + 1e-5);
            assert!(page_h * t_shrink.scale <= p_h + 1e-5);
        }
        assert_eq!(t_shrink.clipping_hazard, None);
    }
}

// ============================================================================
// 2. ADVERSARIAL DUPLEX SHEET ALIGNMENT TESTS
// ============================================================================

#[test]
fn test_duplex_all_16_four_page_permutations() {
    let p_portrait = (595.0, 842.0);
    let p_landscape = (842.0, 595.0);

    for mask in 0..16 {
        let pages: Vec<(f64, f64)> = (0..4)
            .map(|bit| if (mask & (1 << bit)) != 0 { p_landscape } else { p_portrait })
            .collect();

        for &binding in &[DuplexBinding::FlipLongEdge, DuplexBinding::FlipShortEdge] {
            let sheets = plan_duplex_sheets(&pages, binding);
            assert_eq!(sheets.len(), 2, "4 pages should yield exactly 2 duplex sheets");

            for sheet in &sheets {
                // Front page must NEVER be rotated
                let front_size = pages[sheet.front_page];
                let expected_master_portrait = front_size.1 >= front_size.0;
                assert_eq!(sheet.master_is_portrait, expected_master_portrait);

                // Check back page
                let back_idx = sheet.back_page.expect("Duplex sheet of 4 pages must have back page");
                let back_size = pages[back_idx];
                let back_portrait = back_size.1 >= back_size.0;

                if expected_master_portrait == back_portrait {
                    // Same orientation: rotation must be 0
                    assert!(!sheet.back_needs_rotation);
                    assert_eq!(sheet.back_rotation_deg, 0);
                } else {
                    // Mixed orientation: must rotate
                    assert!(sheet.back_needs_rotation);
                    let expected_rot = match (expected_master_portrait, binding) {
                        (true, DuplexBinding::FlipLongEdge) => 270,
                        (true, DuplexBinding::FlipShortEdge) => 90,
                        (false, DuplexBinding::FlipLongEdge) => 90,
                        (false, DuplexBinding::FlipShortEdge) => 270,
                        _ => unreachable!(),
                    };
                    assert_eq!(sheet.back_rotation_deg, expected_rot);
                }
            }
        }
    }
}

#[test]
fn test_duplex_various_page_counts() {
    let p_p = (595.0, 842.0);
    let p_l = (842.0, 595.0);

    // 1-page document
    let pages_1 = vec![p_p];
    let sheets_1 = plan_duplex_sheets(&pages_1, DuplexBinding::FlipLongEdge);
    assert_eq!(sheets_1.len(), 1);
    assert_eq!(sheets_1[0].front_page, 0);
    assert_eq!(sheets_1[0].back_page, None);
    assert_eq!(sheets_1[0].back_rotation_deg, 0);

    // 2-page document (mixed)
    let pages_2 = vec![p_p, p_l];
    let sheets_2 = plan_duplex_sheets(&pages_2, DuplexBinding::FlipLongEdge);
    assert_eq!(sheets_2.len(), 1);
    assert_eq!(sheets_2[0].front_page, 0);
    assert_eq!(sheets_2[0].back_page, Some(1));
    assert_eq!(sheets_2[0].back_rotation_deg, 270);

    // 3-page document (mixed P, L, P)
    let pages_3 = vec![p_p, p_l, p_p];
    let sheets_3 = plan_duplex_sheets(&pages_3, DuplexBinding::FlipShortEdge);
    assert_eq!(sheets_3.len(), 2);
    // Sheet 0
    assert_eq!(sheets_3[0].front_page, 0);
    assert_eq!(sheets_3[0].back_page, Some(1));
    assert_eq!(sheets_3[0].back_rotation_deg, 90);
    // Sheet 1 (unbacked trailing odd page)
    assert_eq!(sheets_3[1].front_page, 2);
    assert_eq!(sheets_3[1].back_page, None);
    assert_eq!(sheets_3[1].back_rotation_deg, 0);

    // 7-page document (P, L, L, P, P, L, L)
    let pages_7 = vec![p_p, p_l, p_l, p_p, p_p, p_l, p_l];
    let sheets_7 = plan_duplex_sheets(&pages_7, DuplexBinding::FlipLongEdge);
    assert_eq!(sheets_7.len(), 4);
    // Sheet 0: Front 0 (P), Back 1 (L) -> rot 270
    assert_eq!(sheets_7[0].back_rotation_deg, 270);
    // Sheet 1: Front 2 (L), Back 3 (P) -> rot 90
    assert_eq!(sheets_7[1].back_rotation_deg, 90);
    // Sheet 2: Front 4 (P), Back 5 (L) -> rot 270
    assert_eq!(sheets_7[2].back_rotation_deg, 270);
    // Sheet 3: Front 6 (L), Back None -> rot 0
    assert_eq!(sheets_7[3].back_rotation_deg, 0);

    // 10-page document
    let pages_10 = vec![p_p; 10];
    let sheets_10 = plan_duplex_sheets(&pages_10, DuplexBinding::FlipLongEdge);
    assert_eq!(sheets_10.len(), 5);
    for s in &sheets_10 {
        assert!(s.back_page.is_some());
        assert_eq!(s.back_rotation_deg, 0);
    }
}

#[test]
fn test_simplex_mode_never_rotates_and_never_pairs() {
    let pages = vec![
        (595.0, 842.0),
        (842.0, 595.0),
        (842.0, 595.0),
        (595.0, 842.0),
        (500.0, 500.0),
    ];
    let sheets = plan_duplex_sheets(&pages, DuplexBinding::Simplex);
    assert_eq!(sheets.len(), 5, "Simplex mode must yield 1 sheet per page");

    for (i, sheet) in sheets.iter().enumerate() {
        assert_eq!(sheet.sheet_index, i);
        assert_eq!(sheet.front_page, i);
        assert_eq!(sheet.back_page, None);
        assert!(!sheet.back_needs_rotation);
        assert_eq!(sheet.back_rotation_deg, 0);
    }
}

#[test]
fn test_square_pages_orientation_consistency() {
    let square = (500.0, 500.0);
    let landscape = (600.0, 400.0);
    let portrait = (400.0, 600.0);

    // Square has h >= w -> treated as master portrait
    let (master_p, rot) = align_sheet_pair_orientation(square, landscape, DuplexBinding::FlipLongEdge);
    assert!(master_p, "Square page should be treated as portrait");
    assert_eq!(rot, 270);

    // Square front and portrait back: both are portrait -> rot 0
    let (master_p2, rot2) = align_sheet_pair_orientation(square, portrait, DuplexBinding::FlipLongEdge);
    assert!(master_p2);
    assert_eq!(rot2, 0);

    // Square front and square back: rot 0
    let (master_p3, rot3) = align_sheet_pair_orientation(square, square, DuplexBinding::FlipLongEdge);
    assert!(master_p3);
    assert_eq!(rot3, 0);
}

#[test]
fn test_duplex_binding_conversions() {
    assert!(!DuplexBinding::Simplex.is_two_sided());
    assert!(DuplexBinding::FlipLongEdge.is_two_sided());
    assert!(DuplexBinding::FlipShortEdge.is_two_sided());

    assert_eq!(DuplexBinding::Simplex.to_devmode_duplex(), 1);
    assert_eq!(DuplexBinding::FlipLongEdge.to_devmode_duplex(), 2);
    assert_eq!(DuplexBinding::FlipShortEdge.to_devmode_duplex(), 3);

    assert_eq!(DuplexBinding::from_devmode_duplex(1), Some(DuplexBinding::Simplex));
    assert_eq!(DuplexBinding::from_devmode_duplex(2), Some(DuplexBinding::FlipLongEdge));
    assert_eq!(DuplexBinding::from_devmode_duplex(3), Some(DuplexBinding::FlipShortEdge));
    assert_eq!(DuplexBinding::from_devmode_duplex(0), None);
    assert_eq!(DuplexBinding::from_devmode_duplex(4), None);

    assert_eq!(DuplexBinding::Simplex.to_printticket_option(), "psk:OneSided");
    assert_eq!(DuplexBinding::FlipLongEdge.to_printticket_option(), "psk:TwoSidedLongEdge");
    assert_eq!(DuplexBinding::FlipShortEdge.to_printticket_option(), "psk:TwoSidedShortEdge");

    assert_eq!(DuplexBinding::Simplex.to_ipp_sides(), "one-sided");
    assert_eq!(DuplexBinding::FlipLongEdge.to_ipp_sides(), "two-sided-long-edge");
    assert_eq!(DuplexBinding::FlipShortEdge.to_ipp_sides(), "two-sided-short-edge");
}
