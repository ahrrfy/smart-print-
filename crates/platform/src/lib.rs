//! pdfcraft-platform — L7 operating-system services shared by the desktop app (`ui-egui`) and the
//! automation tools (`automation`), which sit on the same layer and can't depend on each other,
//! and by the two executables (`owner_only`, for the `--control` file).

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

pub mod desktop_theme;
pub mod disk;
pub mod owner_only;
pub mod staging;
pub mod zone;

// Milestone 2 Print Subsystems
pub mod caps;
pub mod cups;
pub mod diagnostics;
pub mod mock;
#[cfg(windows)]
pub mod win32_v3;
#[cfg(windows)]
pub mod win32_v4;

pub use disk::{get_default_spool_directory, query_disk_free_bytes, verify_spooler_disk_space};
pub use diagnostics::{
    collect_diagnostic_report, AppliedColorMode, AppliedOrientation, AppliedTicketTelemetry,
    DiagnosticReport, DiagnosticTraceBuffer, DriverGenerationKind, DuplexBindingKind, MarginsPt,
    OsTelemetry, PrinterCapabilitiesTelemetry, ResolutionDpi, SizePt, SpoolDiskVerdict,
    SpoolerServiceStatus, SpoolerTelemetry, TicketDetails, TraceCategory, TraceLevel, TraceRecord,
};

use pdfcraft_print::driver::{DriverError, PrintDriverAdapter, PrinterCapabilities};

/// Query printer capabilities through the platform's native subsystem.
pub fn query_printer_capabilities(printer_name: &str) -> Result<PrinterCapabilities, DriverError> {
    caps::query_gdi_capabilities(printer_name)
}

/// Create the native platform driver adapter for the given printer.
pub fn create_driver_adapter(
    printer_name: &str,
) -> Result<Box<dyn PrintDriverAdapter>, DriverError> {
    #[cfg(windows)]
    {
        if pdfcraft_print::spool::xps_driver(printer_name) == Some(true) {
            Ok(Box::new(win32_v4::Win32V4Adapter::new()))
        } else {
            Ok(Box::new(win32_v3::Win32V3Adapter::new()))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = printer_name;
        Ok(Box::new(cups::CupsDriverAdapter::new()))
    }
}
