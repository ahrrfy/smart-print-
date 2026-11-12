//! crates/print/src/driver.rs — Unified Print Driver Contract & Pre-Flight Negotiation (Layer 4)
//!
//! Defines the driver adapter trait (`PrintDriverAdapter`), capability models,
//! margin metrics, pre-flight negotiation, and clipping hazard calculation.

use thiserror::Error;

/// Target driver generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DriverGeneration {
    Win32V3DevMode,
    Win32V4PrintTicket,
    CupsIpp,
    MockVirtual,
}

/// Double-sided printing binding edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum DuplexBinding {
    #[default]
    Simplex,
    FlipLongEdge,
    FlipShortEdge,
}

/// Color output mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum ColorMode {
    Color,
    #[default]
    MonochromeKOnly,
}

/// Page sizing / fit mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum PageFitMode {
    #[default]
    FitPrintableArea,
    ShrinkOversized,
    ActualSize,
}

/// Physical hardware unprintable margins in points (1/72 inch).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct HardwareMargins {
    pub left_pt: f64,
    pub top_pt: f64,
    pub right_pt: f64,
    pub bottom_pt: f64,
}

impl HardwareMargins {
    pub const ZERO: Self = Self {
        left_pt: 0.0,
        top_pt: 0.0,
        right_pt: 0.0,
        bottom_pt: 0.0,
    };

    pub fn left_mm(&self) -> f64 {
        self.left_pt * 25.4 / 72.0
    }

    pub fn top_mm(&self) -> f64 {
        self.top_pt * 25.4 / 72.0
    }

    pub fn right_mm(&self) -> f64 {
        self.right_pt * 25.4 / 72.0
    }

    pub fn bottom_mm(&self) -> f64 {
        self.bottom_pt * 25.4 / 72.0
    }
}

/// Printable boundary and sheet dimensions.
#[derive(Clone, Debug, PartialEq)]
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
    pub fn printable_width_mm(&self) -> f64 {
        self.printable_width_pt * 25.4 / 72.0
    }

    pub fn printable_height_mm(&self) -> f64 {
        self.printable_height_pt * 25.4 / 72.0
    }

    pub fn physical_width_mm(&self) -> f64 {
        self.physical_width_pt * 25.4 / 72.0
    }

    pub fn physical_height_mm(&self) -> f64 {
        self.physical_height_pt * 25.4 / 72.0
    }
}

/// Queried hardware capabilities of a printer.
#[derive(Clone, Debug, PartialEq)]
pub struct PrinterCapabilities {
    pub name: String,
    pub driver_generation: DriverGeneration,
    pub is_duplex_supported: bool,
    pub supported_duplex: Vec<DuplexBinding>,
    pub is_color_supported: bool,
    pub max_copies: u32,
    pub bounds: PrintableBounds,
}

/// Clipped extent outside hardware margins in points.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
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

    pub fn is_clipping(&self) -> bool {
        self.total_clipped_pt() > 0.01
    }
}

/// Severity of pre-flight issues.
#[derive(Clone, Debug, PartialEq)]
pub enum PreFlightSeverity {
    Ok,
    WarningClipping(ClippingExtent),
    CriticalMismatch(String),
}

/// Result of pre-flight job validation.
#[derive(Clone, Debug, PartialEq)]
pub struct PreFlightReport {
    pub is_valid_for_dispatch: bool,
    pub issues: Vec<PreFlightSeverity>,
}

impl PreFlightReport {
    pub fn ok() -> Self {
        Self {
            is_valid_for_dispatch: true,
            issues: vec![PreFlightSeverity::Ok],
        }
    }

    pub fn has_clipping_hazard(&self) -> bool {
        self.issues
            .iter()
            .any(|i| matches!(i, PreFlightSeverity::WarningClipping(_)))
    }

    pub fn has_critical_mismatch(&self) -> bool {
        self.issues
            .iter()
            .any(|i| matches!(i, PreFlightSeverity::CriticalMismatch(_)))
    }
}

/// Print job configuration submitted for validation or spooling.
#[derive(Clone, Debug, PartialEq)]
pub struct PrintJobSettings {
    pub job_title: String,
    pub printer_name: String,
    pub copies: u32,
    pub color_mode: ColorMode,
    pub duplex_mode: DuplexBinding,
    pub fit_mode: PageFitMode,
    pub page_bounds: (f64, f64), // (width_pt, height_pt)
    pub landscape: bool,
    pub paper_name: Option<String>,
}

impl Default for PrintJobSettings {
    fn default() -> Self {
        Self {
            job_title: "Untitled Document".to_string(),
            printer_name: String::new(),
            copies: 1,
            color_mode: ColorMode::MonochromeKOnly,
            duplex_mode: DuplexBinding::Simplex,
            fit_mode: PageFitMode::FitPrintableArea,
            page_bounds: (595.28, 841.89), // Standard A4 points
            landscape: false,
            paper_name: None,
        }
    }
}

/// Summary of an executed spool job.
#[derive(Clone, Debug, PartialEq)]
pub struct SpoolSummary {
    pub job_id: u32,
    pub pages_spooled: usize,
    pub bytes_written: usize,
}

/// Driver and spooling errors.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum DriverError {
    #[error("printer not found: {0}")]
    PrinterNotFound(String),
    #[error("failed to create device context for printer: {0}")]
    DeviceContextFailed(String),
    #[error("failed to query device capabilities: {0}")]
    CapabilitiesQueryFailed(String),
    #[error("spooling failed: {0}")]
    SpoolFailed(String),
    #[error("unsupported driver operation: {0}")]
    Unsupported(String),
    #[error("driver communication timeout: {0}")]
    Timeout(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("devmode error: {0}")]
    DevModeError(String),
    #[error("gdi error: {0}")]
    GdiError(String),
    #[error("print ticket error: {0}")]
    PrintTicketError(String),
    #[error("dll not found: {0}")]
    DllNotFound(String),
}

/// Pre-flight capability negotiation logic.
pub fn validate_job_capabilities(
    caps: &PrinterCapabilities,
    settings: &PrintJobSettings,
) -> PreFlightReport {
    let mut issues = Vec::new();
    let mut valid = true;

    // 1. Duplex Check
    if settings.duplex_mode != DuplexBinding::Simplex {
        if !caps.is_duplex_supported || !caps.supported_duplex.contains(&settings.duplex_mode) {
            issues.push(PreFlightSeverity::CriticalMismatch(format!(
                "Double-sided printing ({:?}) requested, but printer '{}' only supports simplex.",
                settings.duplex_mode, caps.name
            )));
            valid = false;
        }
    }

    // 2. Color Check
    if settings.color_mode == ColorMode::Color && !caps.is_color_supported {
        issues.push(PreFlightSeverity::CriticalMismatch(format!(
            "Color printing requested, but printer '{}' is monochrome-only.",
            caps.name
        )));
        valid = false;
    }

    // 3. Copies Check
    if settings.copies > caps.max_copies {
        issues.push(PreFlightSeverity::CriticalMismatch(format!(
            "Requested copies ({}) exceeds printer maximum supported copies ({}).",
            settings.copies, caps.max_copies
        )));
        valid = false;
    }

    // 4. Margins & Clipping Hazard Check
    if settings.fit_mode == PageFitMode::ActualSize {
        let (page_w, page_h) = settings.page_bounds;
        let phys_w = caps.bounds.physical_width_pt;
        let phys_h = caps.bounds.physical_height_pt;
        let m = &caps.bounds.margins;

        // Centered on physical sheet coordinates:
        let target_x = (phys_w - page_w) / 2.0;
        let target_y = (phys_h - page_h) / 2.0;

        let clip_l = (m.left_pt - target_x).max(0.0);
        let clip_t = (m.top_pt - target_y).max(0.0);
        let clip_r = ((target_x + page_w) - (phys_w - m.right_pt)).max(0.0);
        let clip_b = ((target_y + page_h) - (phys_h - m.bottom_pt)).max(0.0);

        let clipping = ClippingExtent {
            left_pt: clip_l,
            top_pt: clip_t,
            right_pt: clip_r,
            bottom_pt: clip_b,
        };

        if clipping.is_clipping() {
            issues.push(PreFlightSeverity::WarningClipping(clipping));
        }
    }

    if issues.is_empty() {
        issues.push(PreFlightSeverity::Ok);
    }

    PreFlightReport {
        is_valid_for_dispatch: valid,
        issues,
    }
}

/// Unified Print Driver Adapter Interface.
pub trait PrintDriverAdapter: Send + Sync {
    fn query_capabilities(&self, printer_name: &str) -> Result<PrinterCapabilities, DriverError>;
    fn validate_job(
        &self,
        caps: &PrinterCapabilities,
        settings: &PrintJobSettings,
    ) -> PreFlightReport {
        validate_job_capabilities(caps, settings)
    }
    fn spool_job(
        &self,
        settings: &PrintJobSettings,
        pdf_bytes: &[u8],
    ) -> Result<SpoolSummary, DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_office_caps() -> PrinterCapabilities {
        PrinterCapabilities {
            name: "EPSON WF-C579R".to_string(),
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
                margins: HardwareMargins {
                    left_pt: 8.4,
                    top_pt: 8.4,
                    right_pt: 8.4,
                    bottom_pt: 8.4,
                },
            },
        }
    }

    fn create_test_receipt_caps() -> PrinterCapabilities {
        PrinterCapabilities {
            name: "EPSON TM-T20III".to_string(),
            driver_generation: DriverGeneration::Win32V3DevMode,
            is_duplex_supported: false,
            supported_duplex: vec![DuplexBinding::Simplex],
            is_color_supported: false,
            max_copies: 1,
            bounds: PrintableBounds {
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
            },
        }
    }

    #[test]
    fn test_margin_conversion_and_units() {
        let margins = HardwareMargins {
            left_pt: 72.0,
            top_pt: 36.0,
            right_pt: 72.0,
            bottom_pt: 18.0,
        };
        // 72 pt = 1 inch = 25.4 mm
        assert!((margins.left_mm() - 25.4).abs() < 1e-4);
        assert!((margins.top_mm() - 12.7).abs() < 1e-4);
        assert!((margins.bottom_mm() - 6.35).abs() < 1e-4);

        let bounds = PrintableBounds {
            printable_width_pt: 578.4,
            printable_height_pt: 824.88,
            physical_width_pt: 595.2,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins,
        };
        assert!((bounds.physical_width_mm() - (595.2 * 25.4 / 72.0)).abs() < 1e-4);
    }

    #[test]
    fn test_preflight_valid_job_passes() {
        let caps = create_test_office_caps();
        let settings = PrintJobSettings {
            job_title: "Report".to_string(),
            printer_name: caps.name.clone(),
            copies: 2,
            color_mode: ColorMode::Color,
            duplex_mode: DuplexBinding::FlipLongEdge,
            fit_mode: PageFitMode::FitPrintableArea,
            page_bounds: (595.2, 841.68),
            landscape: false,
            paper_name: None,
        };

        let report = validate_job_capabilities(&caps, &settings);
        assert!(report.is_valid_for_dispatch);
        assert_eq!(report.issues, vec![PreFlightSeverity::Ok]);
        assert!(!report.has_clipping_hazard());
        assert!(!report.has_critical_mismatch());
    }

    #[test]
    fn test_preflight_duplex_mismatch_detected() {
        let caps = create_test_receipt_caps();
        let settings = PrintJobSettings {
            job_title: "Receipt".to_string(),
            printer_name: caps.name.clone(),
            copies: 1,
            color_mode: ColorMode::MonochromeKOnly,
            duplex_mode: DuplexBinding::FlipLongEdge,
            fit_mode: PageFitMode::FitPrintableArea,
            page_bounds: (204.65, 841.30),
            landscape: false,
            paper_name: None,
        };

        let report = validate_job_capabilities(&caps, &settings);
        assert!(!report.is_valid_for_dispatch);
        assert!(report.has_critical_mismatch());
        let has_duplex_error = report.issues.iter().any(|i| match i {
            PreFlightSeverity::CriticalMismatch(msg) => msg.contains("Double-sided printing"),
            _ => false,
        });
        assert!(has_duplex_error);
    }

    #[test]
    fn test_preflight_color_mismatch_detected() {
        let caps = create_test_receipt_caps();
        let settings = PrintJobSettings {
            job_title: "Receipt".to_string(),
            printer_name: caps.name.clone(),
            copies: 1,
            color_mode: ColorMode::Color,
            duplex_mode: DuplexBinding::Simplex,
            fit_mode: PageFitMode::FitPrintableArea,
            page_bounds: (204.65, 841.30),
            landscape: false,
            paper_name: None,
        };

        let report = validate_job_capabilities(&caps, &settings);
        assert!(!report.is_valid_for_dispatch);
        assert!(report.has_critical_mismatch());
        let has_color_error = report.issues.iter().any(|i| match i {
            PreFlightSeverity::CriticalMismatch(msg) => msg.contains("Color printing requested"),
            _ => false,
        });
        assert!(has_color_error);
    }

    #[test]
    fn test_preflight_copies_exceeded_detected() {
        let caps = create_test_receipt_caps(); // max_copies = 1
        let settings = PrintJobSettings {
            job_title: "Receipt".to_string(),
            printer_name: caps.name.clone(),
            copies: 5,
            color_mode: ColorMode::MonochromeKOnly,
            duplex_mode: DuplexBinding::Simplex,
            fit_mode: PageFitMode::FitPrintableArea,
            page_bounds: (204.65, 841.30),
            landscape: false,
            paper_name: None,
        };

        let report = validate_job_capabilities(&caps, &settings);
        assert!(!report.is_valid_for_dispatch);
        assert!(report.has_critical_mismatch());
        let has_copies_error = report.issues.iter().any(|i| match i {
            PreFlightSeverity::CriticalMismatch(msg) => msg.contains("Requested copies"),
            _ => false,
        });
        assert!(has_copies_error);
    }

    #[test]
    fn test_preflight_clipping_hazard_actual_size() {
        let caps = create_test_office_caps(); // physical: 595.2 x 841.68, margins: 8.4 pt
        // Page bounds exactly full sheet (595.2 x 841.68). In ActualSize centered:
        // target_x = 0.0, margin_left = 8.4 => clipped left = 8.4 pt
        // target_y = 0.0, margin_top = 8.4 => clipped top = 8.4 pt
        // right = 595.2, max printable right = 595.2 - 8.4 = 586.8 => clipped right = 8.4 pt
        // bottom = 841.68, max printable bottom = 841.68 - 8.4 = 833.28 => clipped bottom = 8.4 pt
        let settings = PrintJobSettings {
            job_title: "Full Bleed".to_string(),
            printer_name: caps.name.clone(),
            copies: 1,
            color_mode: ColorMode::MonochromeKOnly,
            duplex_mode: DuplexBinding::Simplex,
            fit_mode: PageFitMode::ActualSize,
            page_bounds: (595.2, 841.68),
            landscape: false,
            paper_name: None,
        };

        let report = validate_job_capabilities(&caps, &settings);
        assert!(report.is_valid_for_dispatch); // clipping warning does not prevent dispatch
        assert!(report.has_clipping_hazard());

        let clip = report
            .issues
            .iter()
            .find_map(|i| match i {
                PreFlightSeverity::WarningClipping(c) => Some(*c),
                _ => None,
            })
            .expect("must contain WarningClipping");

        assert!((clip.left_pt - 8.4).abs() < 1e-4);
        assert!((clip.top_pt - 8.4).abs() < 1e-4);
        assert!((clip.right_pt - 8.4).abs() < 1e-4);
        assert!((clip.bottom_pt - 8.4).abs() < 1e-4);
    }

    #[test]
    fn test_preflight_fit_printable_area_no_clipping() {
        let caps = create_test_office_caps();
        let settings = PrintJobSettings {
            job_title: "Scaled".to_string(),
            printer_name: caps.name.clone(),
            copies: 1,
            color_mode: ColorMode::MonochromeKOnly,
            duplex_mode: DuplexBinding::Simplex,
            fit_mode: PageFitMode::FitPrintableArea,
            page_bounds: (595.2, 841.68),
            landscape: false,
            paper_name: None,
        };

        let report = validate_job_capabilities(&caps, &settings);
        assert!(report.is_valid_for_dispatch);
        assert!(!report.has_clipping_hazard());
        assert_eq!(report.issues, vec![PreFlightSeverity::Ok]);
    }
}
