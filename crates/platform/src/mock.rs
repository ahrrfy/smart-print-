//! crates/platform/src/mock.rs — High-Fidelity Mock Virtual Driver Adapter (Layer 7)
//!
//! Provides a configurable in-memory simulated printer driver supporting emulation of
//! V3 DEVMODE, V4 XPS PrintTicket, and CUPS IPP 2.0 subsystems. Includes comprehensive
//! failure injection triggers and captured job ledgers for automated testing.

use pdfcraft_print::driver::{
    DriverError, DriverGeneration, DuplexBinding, HardwareMargins,
    PreFlightReport, PreFlightSeverity, PrintDriverAdapter, PrintJobSettings,
    PrintableBounds, PrinterCapabilities, SpoolSummary,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct MockDriverConfig {
    pub printer_name: String,
    pub driver_generation: DriverGeneration,
    pub is_duplex_supported: bool,
    pub supported_duplex: Vec<DuplexBinding>,
    pub is_color_supported: bool,
    pub max_copies: u32,
    pub bounds: PrintableBounds,

    // Failure Injection Triggers
    pub fail_query_capabilities: Option<DriverError>,
    pub fail_validate_job: Option<PreFlightSeverity>,
    pub fail_spool_job: Option<DriverError>,
    pub fail_after_sheets: Option<usize>,
    pub spool_delay_per_sheet: Option<Duration>,
}

impl Default for MockDriverConfig {
    fn default() -> Self {
        Self {
            printer_name: "Mock-Virtual-Printer".to_string(),
            driver_generation: DriverGeneration::MockVirtual,
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
                printable_height_pt: 824.8,
                physical_width_pt: 595.2,
                physical_height_pt: 841.8,
                dpi_x: 600,
                dpi_y: 600,
                margins: HardwareMargins {
                    left_pt: 8.4,
                    top_pt: 8.4,
                    right_pt: 8.4,
                    bottom_pt: 8.4,
                },
            },
            fail_query_capabilities: None,
            fail_validate_job: None,
            fail_spool_job: None,
            fail_after_sheets: None,
            spool_delay_per_sheet: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpooledJobRecord {
    pub job_id: u32,
    pub printer_name: String,
    pub settings: PrintJobSettings,
    pub pdf_bytes: Vec<u8>,
    pub timestamp: std::time::Instant,
}

#[derive(Clone, Debug)]
pub struct MockVirtualDriver {
    config: MockDriverConfig,
    spooled_jobs: Arc<Mutex<Vec<SpooledJobRecord>>>,
    spooled_sheets_counter: Arc<Mutex<usize>>,
}

impl MockVirtualDriver {
    pub fn new(config: MockDriverConfig) -> Self {
        Self {
            config,
            spooled_jobs: Arc::new(Mutex::new(Vec::new())),
            spooled_sheets_counter: Arc::new(Mutex::new(0)),
        }
    }

    /// Multifunction Office Printer (Duplex + Color + 8.4pt margins)
    pub fn office_duplex_color(name: &str) -> Self {
        Self::new(MockDriverConfig {
            printer_name: name.to_string(),
            ..MockDriverConfig::default()
        })
    }

    /// Thermal POS Receipt Printer (Simplex only + Mono only + Asymmetric margins)
    pub fn thermal_receipt_mono_simplex(name: &str) -> Self {
        Self::new(MockDriverConfig {
            printer_name: name.to_string(),
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
            ..MockDriverConfig::default()
        })
    }

    /// Borderless Virtual PDF Printer (0pt margins)
    pub fn borderless_virtual_pdf(name: &str) -> Self {
        Self::new(MockDriverConfig {
            printer_name: name.to_string(),
            is_duplex_supported: false,
            supported_duplex: vec![DuplexBinding::Simplex],
            bounds: PrintableBounds {
                printable_width_pt: 595.28,
                printable_height_pt: 841.89,
                physical_width_pt: 595.28,
                physical_height_pt: 841.89,
                dpi_x: 600,
                dpi_y: 600,
                margins: HardwareMargins::ZERO,
            },
            ..MockDriverConfig::default()
        })
    }

    /// Failing Printer simulating hardware fault or offline status
    pub fn failing(name: &str, error: DriverError) -> Self {
        Self::new(MockDriverConfig {
            printer_name: name.to_string(),
            fail_spool_job: Some(error),
            ..MockDriverConfig::default()
        })
    }

    /// Returns the history of all spooled jobs.
    pub fn spooled_jobs(&self) -> Vec<SpooledJobRecord> {
        self.spooled_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Returns the most recently spooled job record.
    pub fn last_job(&self) -> Option<SpooledJobRecord> {
        self.spooled_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
    }

    /// Resets captured jobs and failure counters.
    pub fn reset(&self) {
        self.spooled_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        *self
            .spooled_sheets_counter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = 0;
    }
}

impl PrintDriverAdapter for MockVirtualDriver {
    fn query_capabilities(&self, _printer_name: &str) -> Result<PrinterCapabilities, DriverError> {
        if let Some(ref err) = self.config.fail_query_capabilities {
            return Err(err.clone());
        }
        Ok(PrinterCapabilities {
            name: self.config.printer_name.clone(),
            driver_generation: self.config.driver_generation,
            is_duplex_supported: self.config.is_duplex_supported,
            supported_duplex: self.config.supported_duplex.clone(),
            is_color_supported: self.config.is_color_supported,
            max_copies: self.config.max_copies,
            bounds: self.config.bounds.clone(),
        })
    }

    fn validate_job(
        &self,
        caps: &PrinterCapabilities,
        settings: &PrintJobSettings,
    ) -> PreFlightReport {
        let mut report = pdfcraft_print::driver::validate_job_capabilities(caps, settings);

        if let Some(ref custom_issue) = self.config.fail_validate_job {
            report.issues.push(custom_issue.clone());
            if matches!(custom_issue, PreFlightSeverity::CriticalMismatch(_)) {
                report.is_valid_for_dispatch = false;
            }
        }

        report
    }

    fn spool_job(
        &self,
        settings: &PrintJobSettings,
        pdf_bytes: &[u8],
    ) -> Result<SpoolSummary, DriverError> {
        // Check immediate failure injection
        if let Some(ref err) = self.config.fail_spool_job {
            return Err(err.clone());
        }

        // Check simulated delay per sheet
        if let Some(delay) = self.config.spool_delay_per_sheet {
            std::thread::sleep(delay);
        }

        let mut counter = self
            .spooled_sheets_counter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *counter = counter.saturating_add(1);

        // Check mid-job sheet failure injection
        if let Some(limit) = self.config.fail_after_sheets
            && *counter > limit
        {
            return Err(DriverError::SpoolFailed(format!(
                "Simulated printer failure after {} sheets",
                limit
            )));
        }

        let mut jobs = self
            .spooled_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let job_id = (jobs.len() as u32).saturating_add(1);

        jobs.push(SpooledJobRecord {
            job_id,
            printer_name: settings.printer_name.clone(),
            settings: settings.clone(),
            pdf_bytes: pdf_bytes.to_vec(),
            timestamp: std::time::Instant::now(),
        });

        Ok(SpoolSummary {
            job_id,
            pages_spooled: 1,
            bytes_written: pdf_bytes.len(),
        })
    }
}
