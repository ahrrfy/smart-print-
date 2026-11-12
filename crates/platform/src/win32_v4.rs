//! crates/platform/src/win32_v4.rs — Win32 V4 XPS PrintTicket Adapter (Layer 7)
//!
//! Interfaces with `prntvpt.dll` (`PTOpenProvider`, `PTGetPrintCapabilities`,
//! `PTMergeAndValidatePrintTicket`), merges Delta PrintTicket XML, handles
//! `S_PT_CONFLICT_RESOLVED` (0x00040001), and serializes provider access via
//! process-wide mutex to eliminate vendor driver concurrency crashes.

#![allow(unsafe_code)]

use pdfcraft_print::driver::{
    ColorMode, DriverError, DriverGeneration, DuplexBinding, PreFlightReport,
    PrintDriverAdapter, PrintJobSettings, PrinterCapabilities, SpoolSummary,
};

/// Execute `f` while holding the shared process-wide PrintTicket provider lock from `pdfcraft-print`.
pub fn with_print_ticket_provider<T, F: FnOnce() -> Result<T, DriverError>>(
    f: F,
) -> Result<T, DriverError> {
    pdfcraft_print::spool::with_ticket_provider_lock(f)
}

pub const S_OK: i32 = 0;
pub const S_PT_CONFLICT_RESOLVED: i32 = 0x0004_0001;

pub const DEFAULT_BASE_TICKET_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<psf:PrintTicket version=\"1\" \
xmlns:psf=\"http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework\" \
xmlns:psk=\"http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords\" \
xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\">\n\
</psf:PrintTicket>";

/// Generates valid Delta PrintTicket XML specifying user print settings.
pub fn generate_delta_print_ticket_xml(
    color_mode: ColorMode,
    duplex: DuplexBinding,
    landscape: bool,
    copies: u32,
    paper_name: Option<&str>,
) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <psf:PrintTicket version=\"1\" \
        xmlns:psf=\"http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework\" \
        xmlns:psk=\"http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords\" \
        xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
        xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\">\n",
    );

    // 1. Color: psk:PageOutputColor (Layer 1 Driver Flag Assertion)
    let color_val = match color_mode {
        ColorMode::MonochromeKOnly => "psk:Monochrome",
        ColorMode::Color => "psk:Color",
    };
    xml.push_str(&format!(
        "  <psf:Feature name=\"psk:PageOutputColor\">\n    <psf:Option name=\"{}\" />\n  </psf:Feature>\n",
        color_val
    ));

    // 2. Duplex: psk:JobDuplexAllDocumentsContiguously
    let duplex_val = match duplex {
        DuplexBinding::Simplex => "psk:OneSided",
        DuplexBinding::FlipLongEdge => "psk:TwoSidedLongEdge",
        DuplexBinding::FlipShortEdge => "psk:TwoSidedShortEdge",
    };
    xml.push_str(&format!(
        "  <psf:Feature name=\"psk:JobDuplexAllDocumentsContiguously\">\n    <psf:Option name=\"{}\" />\n  </psf:Feature>\n",
        duplex_val
    ));

    // 3. Orientation: psk:PageOrientation
    let orient_val = if landscape {
        "psk:Landscape"
    } else {
        "psk:Portrait"
    };
    xml.push_str(&format!(
        "  <psf:Feature name=\"psk:PageOrientation\">\n    <psf:Option name=\"{}\" />\n  </psf:Feature>\n",
        orient_val
    ));

    // 4. Media size: psk:PageMediaSize
    if let Some(media) = paper_name {
        xml.push_str(&format!(
            "  <psf:Feature name=\"psk:PageMediaSize\">\n    <psf:Option name=\"psk:{}\" />\n  </psf:Feature>\n",
            media
        ));
    }

    // 5. Copies: psk:JobCopiesAllDocuments
    if copies > 1 {
        xml.push_str(&format!(
            "  <psf:ParameterInit name=\"psk:JobCopiesAllDocuments\">\n    <psf:Value xsi:type=\"xsd:integer\">{}</psf:Value>\n  </psf:ParameterInit>\n",
            copies
        ));
    }

    xml.push_str("</psf:PrintTicket>");
    xml
}

/// Win32 V4 XPS PrintTicket Driver Adapter.
#[derive(Clone, Debug, Default)]
pub struct Win32V4Adapter;

impl Win32V4Adapter {
    pub fn new() -> Self {
        Self
    }

    /// Merges user print settings into a validated PrintTicket XML string (`kPTJobScope = 2`)
    /// with deterministic COM `IStream` and `BSTR` cleanup.
    pub fn merge_print_ticket(
        &self,
        printer_name: &str,
        settings: &PrintJobSettings,
    ) -> Result<String, DriverError> {
        let delta_xml = generate_delta_print_ticket_xml(
            settings.color_mode,
            settings.duplex_mode,
            settings.landscape,
            settings.copies,
            settings.paper_name.as_deref(),
        );

        #[cfg(windows)]
        {
            crate::win32_unified::merge_and_read_ticket_xml(
                printer_name,
                DEFAULT_BASE_TICKET_XML.as_bytes(),
                delta_xml.as_bytes(),
            )
        }

        #[cfg(not(windows))]
        {
            let _ = printer_name;
            Ok(delta_xml)
        }
    }
}

impl PrintDriverAdapter for Win32V4Adapter {
    fn query_capabilities(&self, printer_name: &str) -> Result<PrinterCapabilities, DriverError> {
        let mut caps = crate::caps::query_gdi_capabilities(printer_name)?;
        caps.driver_generation = DriverGeneration::Win32V4PrintTicket;
        Ok(caps)
    }

    fn validate_job(
        &self,
        caps: &PrinterCapabilities,
        settings: &PrintJobSettings,
    ) -> PreFlightReport {
        pdfcraft_print::driver::validate_job_capabilities(caps, settings)
    }

    fn spool_job(
        &self,
        settings: &PrintJobSettings,
        pdf_bytes: &[u8],
    ) -> Result<SpoolSummary, DriverError> {
        crate::win32_unified::spool_driver_job(settings, pdf_bytes)
    }
}
