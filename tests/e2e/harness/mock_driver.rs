use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Driver generation category matching pdfcraft architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriverGeneration {
    Win32V3DevMode,
    Win32V4PrintTicket,
    CupsIpp,
    MockVirtual,
}

/// Duplex binding options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplexBinding {
    Simplex,
    FlipLongEdge,
    FlipShortEdge,
}

/// Hardware margin dimensions in points (1/72 inch).
#[derive(Debug, Clone, Copy, PartialEq)]
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

    pub fn uniform(pt: f64) -> Self {
        Self {
            left_pt: pt,
            top_pt: pt,
            right_pt: pt,
            bottom_pt: pt,
        }
    }
}

/// Printable and physical bounds of a printer device.
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
    pub fn a4_standard_margins() -> Self {
        Self {
            printable_width_pt: 578.40,
            printable_height_pt: 824.88,
            physical_width_pt: 595.20,
            physical_height_pt: 841.68,
            dpi_x: 600,
            dpi_y: 600,
            margins: HardwareMargins::uniform(8.40),
        }
    }

    pub fn a4_borderless() -> Self {
        Self {
            printable_width_pt: 595.32,
            printable_height_pt: 841.92,
            physical_width_pt: 595.32,
            physical_height_pt: 841.92,
            dpi_x: 600,
            dpi_y: 600,
            margins: HardwareMargins::ZERO,
        }
    }
}

/// Configurable printer capabilities queried during pre-flight.
#[derive(Debug, Clone)]
pub struct PrinterCapabilities {
    pub name: String,
    pub driver_generation: DriverGeneration,
    pub is_duplex_supported: bool,
    pub supported_duplex: Vec<DuplexBinding>,
    pub is_color_supported: bool,
    pub max_copies: u32,
    pub bounds: PrintableBounds,
}

/// High-level print job settings passed to the driver adapter.
#[derive(Debug, Clone)]
pub struct PrintJobSettings {
    pub job_title: String,
    pub printer_name: String,
    pub copies: u32,
    pub color_monochrome: bool,
    pub duplex: DuplexBinding,
    pub orientation_landscape: bool,
    pub paper_size_name: String,
}

// ============================================================================
// 1. Win32 V3 DEVMODE Mock Implementation
// ============================================================================

pub const DEVMODEW_BASE_SIZE: usize = 220;

// dmFields bitmask constants per wingdi.h
pub const DM_ORIENTATION: u32 = 0x0000_0001;
pub const DM_PAPERSIZE: u32 = 0x0000_0002;
pub const DM_COPIES: u32 = 0x0000_0100;
pub const DM_COLOR: u32 = 0x0000_0800;
pub const DM_DUPLEX: u32 = 0x0000_1000;

// Public field value constants
pub const DMORIENT_PORTRAIT: i16 = 1;
pub const DMORIENT_LANDSCAPE: i16 = 2;

pub const DMCOLOR_MONOCHROME: i16 = 1;
pub const DMCOLOR_COLOR: i16 = 2;

pub const DMDUP_SIMPLEX: i16 = 1;
pub const DMDUP_VERTICAL: i16 = 2; // Long edge (Booklet)
pub const DMDUP_HORIZONTAL: i16 = 3; // Short edge (Tablet)

// DocumentProperties fMode constants
pub const DM_IN_BUFFER: u32 = 8;
pub const DM_OUT_BUFFER: u32 = 2;
pub const DM_IN_PROMPT: u32 = 4;

/// Captured state of a submitted V3 DEVMODE structure.
#[derive(Debug, Clone)]
pub struct CapturedV3DevMode {
    pub raw_bytes: Vec<u8>,
    pub dm_size: u16,
    pub dm_driver_extra: u16,
    pub dm_fields: u32,
    pub dm_orientation: Option<i16>,
    pub dm_color: Option<i16>,
    pub dm_duplex: Option<i16>,
    pub dm_copies: Option<i16>,
    pub driver_extra_bytes: Vec<u8>,
}

/// Detailed errors detected during DEVMODE 2-pass negotiation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DevModeValidationError {
    #[error("Buffer too small: provided {provided} bytes, required {required} bytes")]
    BufferTooSmall { provided: usize, required: usize },
    #[error("Invalid dmSize: {0} (expected 220)")]
    InvalidDmSize(u16),
    #[error("Missing bitmask in dmFields for field '{field_name}': expected bit 0x{expected_bit:X}")]
    MissingFieldBitmask {
        field_name: &'static str,
        expected_bit: u32,
    },
    #[error("Vendor dmDriverExtra payload corrupted (expected len {expected_len}, actual {actual_len})")]
    CorruptedVendorExtraPayload {
        expected_len: usize,
        actual_len: usize,
    },
    #[error("Invalid duplex value: {0}")]
    InvalidDuplexValue(i16),
    #[error("Invalid color value: {0}")]
    InvalidColorValue(i16),
}

/// Configuration for Mock V3 Driver.
#[derive(Debug, Clone)]
pub struct MockV3DriverConfig {
    pub printer_name: String,
    pub driver_extra_size: usize,
    pub vendor_canary_pattern: Vec<u8>,
    pub default_duplex: DuplexBinding,
    pub default_color: bool,
    pub bounds: PrintableBounds,
}

impl Default for MockV3DriverConfig {
    fn default() -> Self {
        Self {
            printer_name: "EPSON_WF_C579R_V3_Mock".into(),
            driver_extra_size: 3704, // Matches empirical Epson WF-C579R survey
            vendor_canary_pattern: vec![0xAA, 0x55, 0xDE, 0xAD, 0xBE, 0xEF],
            default_duplex: DuplexBinding::Simplex,
            default_color: true,
            bounds: PrintableBounds::a4_standard_margins(),
        }
    }
}

/// Simulated Win32 V3 Driver handling 2-pass DocumentProperties negotiation.
pub struct MockV3Driver {
    pub config: MockV3DriverConfig,
    pass1_queried: AtomicBool,
    submitted_modes: Mutex<Vec<CapturedV3DevMode>>,
    validation_errors: Mutex<Vec<DevModeValidationError>>,
}

impl MockV3Driver {
    pub fn new(config: MockV3DriverConfig) -> Self {
        Self {
            config,
            pass1_queried: AtomicBool::new(false),
            submitted_modes: Mutex::new(Vec::new()),
            validation_errors: Mutex::new(Vec::new()),
        }
    }

    /// Total required DEVMODE size in bytes (dmSize + dmDriverExtra).
    pub fn required_buffer_size(&self) -> usize {
        DEVMODEW_BASE_SIZE + self.config.driver_extra_size
    }

    /// Pass 1 of DocumentPropertiesW: returns required buffer size.
    pub fn document_properties_pass1(&self) -> i32 {
        self.pass1_queried.store(true, Ordering::SeqCst);
        self.required_buffer_size() as i32
    }

    /// Pass 2 of DocumentPropertiesW: merges input DEVMODE into output DEVMODE.
    pub fn document_properties_pass2(
        &self,
        in_buf: Option<&[u8]>,
        out_buf: &mut [u8],
        f_mode: u32,
    ) -> Result<i32, DevModeValidationError> {
        let required = self.required_buffer_size();

        if out_buf.len() < required {
            let err = DevModeValidationError::BufferTooSmall {
                provided: out_buf.len(),
                required,
            };
            self.validation_errors.lock().unwrap().push(err.clone());
            return Err(err);
        }

        // Initialize output buffer with defaults if DM_OUT_BUFFER is set
        if f_mode & DM_OUT_BUFFER != 0 {
            out_buf[..required].fill(0);
            out_buf[68..70].copy_from_slice(&(DEVMODEW_BASE_SIZE as u16).to_le_bytes());
            out_buf[70..72].copy_from_slice(&(self.config.driver_extra_size as u16).to_le_bytes());

            // Fill vendor extra payload with canary pattern
            let extra_slice = &mut out_buf[DEVMODEW_BASE_SIZE..required];
            for (i, byte) in extra_slice.iter_mut().enumerate() {
                *byte = self.config.vendor_canary_pattern[i % self.config.vendor_canary_pattern.len()];
            }
        }

        // If DM_IN_BUFFER is set, validate and merge user fields
        if f_mode & DM_IN_BUFFER != 0
            && let Some(input) = in_buf
        {
            if input.len() < required {
                let err = DevModeValidationError::BufferTooSmall {
                    provided: input.len(),
                    required,
                };
                self.validation_errors.lock().unwrap().push(err.clone());
                return Err(err);
            }

            let dm_size = u16::from_le_bytes(input[68..70].try_into().unwrap());
            if dm_size as usize != DEVMODEW_BASE_SIZE {
                let err = DevModeValidationError::InvalidDmSize(dm_size);
                self.validation_errors.lock().unwrap().push(err.clone());
                return Err(err);
            }

            let dm_fields = u32::from_le_bytes(input[72..76].try_into().unwrap());

            // Orientation validation
            let orientation = if dm_fields & DM_ORIENTATION != 0 {
                Some(i16::from_le_bytes(input[76..78].try_into().unwrap()))
            } else {
                None
            };

            // Color validation
            let color = if dm_fields & DM_COLOR != 0 {
                let val = i16::from_le_bytes(input[92..94].try_into().unwrap());
                if val != DMCOLOR_MONOCHROME && val != DMCOLOR_COLOR {
                    let err = DevModeValidationError::InvalidColorValue(val);
                    self.validation_errors.lock().unwrap().push(err.clone());
                    return Err(err);
                }
                Some(val)
            } else {
                None
            };

            // Duplex validation
            let duplex = if dm_fields & DM_DUPLEX != 0 {
                let val = i16::from_le_bytes(input[94..96].try_into().unwrap());
                if val != DMDUP_SIMPLEX && val != DMDUP_VERTICAL && val != DMDUP_HORIZONTAL {
                    let err = DevModeValidationError::InvalidDuplexValue(val);
                    self.validation_errors.lock().unwrap().push(err.clone());
                    return Err(err);
                }
                Some(val)
            } else {
                None
            };

            let copies = if dm_fields & DM_COPIES != 0 {
                Some(i16::from_le_bytes(input[86..88].try_into().unwrap()))
            } else {
                None
            };

            // Vendor extra payload integrity verification
            let input_extra = &input[DEVMODEW_BASE_SIZE..required];
            for (i, &byte) in input_extra.iter().enumerate() {
                let expected =
                    self.config.vendor_canary_pattern[i % self.config.vendor_canary_pattern.len()];
                if byte != expected {
                    let err = DevModeValidationError::CorruptedVendorExtraPayload {
                        expected_len: self.config.driver_extra_size,
                        actual_len: input_extra.len(),
                    };
                    self.validation_errors.lock().unwrap().push(err.clone());
                    return Err(err);
                }
            }

            // Copy valid input fields to output
            out_buf[72..76].copy_from_slice(&dm_fields.to_le_bytes());
            if let Some(o) = orientation {
                out_buf[76..78].copy_from_slice(&o.to_le_bytes());
            }
            if let Some(c) = color {
                out_buf[92..94].copy_from_slice(&c.to_le_bytes());
            }
            if let Some(d) = duplex {
                out_buf[94..96].copy_from_slice(&d.to_le_bytes());
            }
            if let Some(cp) = copies {
                out_buf[86..88].copy_from_slice(&cp.to_le_bytes());
            }

            let captured = CapturedV3DevMode {
                raw_bytes: input[..required].to_vec(),
                dm_size,
                dm_driver_extra: self.config.driver_extra_size as u16,
                dm_fields,
                dm_orientation: orientation,
                dm_color: color,
                dm_duplex: duplex,
                dm_copies: copies,
                driver_extra_bytes: input_extra.to_vec(),
            };
            self.submitted_modes.lock().unwrap().push(captured);
        }

        Ok(1) // IDOK
    }

    pub fn assert_pass1_called(&self) {
        assert!(
            self.pass1_queried.load(Ordering::SeqCst),
            "Expected Pass 1 of DocumentPropertiesW to be called before buffer allocation"
        );
    }

    pub fn last_submitted_devmode(&self) -> Option<CapturedV3DevMode> {
        self.submitted_modes.lock().unwrap().last().cloned()
    }

    pub fn assert_monochrome_asserted(&self) {
        let last = self
            .last_submitted_devmode()
            .expect("No DEVMODE was submitted to V3 driver");
        assert_ne!(
            last.dm_fields & DM_COLOR,
            0,
            "DM_COLOR bit was NOT asserted in dmFields! Driver will ignore dmColor."
        );
        assert_eq!(
            last.dm_color,
            Some(DMCOLOR_MONOCHROME),
            "Expected dmColor == DMCOLOR_MONOCHROME (1), got {:?}",
            last.dm_color
        );
    }

    pub fn assert_duplex_asserted(&self, expected: DuplexBinding) {
        let last = self
            .last_submitted_devmode()
            .expect("No DEVMODE was submitted to V3 driver");
        assert_ne!(
            last.dm_fields & DM_DUPLEX,
            0,
            "DM_DUPLEX bit was NOT asserted in dmFields! Driver will ignore dmDuplex."
        );
        let expected_val = match expected {
            DuplexBinding::Simplex => DMDUP_SIMPLEX,
            DuplexBinding::FlipLongEdge => DMDUP_VERTICAL,
            DuplexBinding::FlipShortEdge => DMDUP_HORIZONTAL,
        };
        assert_eq!(
            last.dm_duplex,
            Some(expected_val),
            "Expected dmDuplex == {expected_val}, got {:?}",
            last.dm_duplex
        );
    }

    pub fn assert_driver_extra_preserved(&self) {
        let last = self
            .last_submitted_devmode()
            .expect("No DEVMODE was submitted to V3 driver");
        assert_eq!(
            last.driver_extra_bytes.len(),
            self.config.driver_extra_size,
            "Driver extra size mismatch! Private payload was truncated."
        );
        for (i, &byte) in last.driver_extra_bytes.iter().enumerate() {
            let expected =
                self.config.vendor_canary_pattern[i % self.config.vendor_canary_pattern.len()];
            assert_eq!(
                byte, expected,
                "Vendor extra byte corrupted at index {i}! Private driver data lost."
            );
        }
    }
}

// ============================================================================
// 2. Win32 V4 XPS PrintTicket Mock Implementation
// ============================================================================

/// PrintTicket conflict resolution record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintTicketConflict {
    pub feature: String,
    pub requested_option: String,
    pub resolved_option: String,
    pub reason: String,
}

/// Configuration for Mock V4 Driver.
#[derive(Debug, Clone)]
pub struct MockV4DriverConfig {
    pub printer_name: String,
    pub supports_color: bool,
    pub supports_monochrome: bool, // When false, simulates Microsoft Print to PDF
    pub supports_duplex: bool,
    pub bounds: PrintableBounds,
}

impl Default for MockV4DriverConfig {
    fn default() -> Self {
        Self {
            printer_name: "Generic_V4_XPS_Printer".into(),
            supports_color: true,
            supports_monochrome: true,
            supports_duplex: true,
            bounds: PrintableBounds::a4_borderless(),
        }
    }
}

/// Simulated Win32 V4 PrintTicket Provider (prntvpt.dll).
pub struct MockV4Driver {
    pub config: MockV4DriverConfig,
    provider_open: AtomicBool,
    submitted_delta_tickets: Mutex<Vec<String>>,
    merged_tickets: Mutex<Vec<String>>,
    resolved_conflicts: Mutex<Vec<PrintTicketConflict>>,
    active_concurrent_calls: AtomicUsize,
    concurrency_violations: AtomicUsize,
}

impl MockV4Driver {
    pub fn new(config: MockV4DriverConfig) -> Self {
        Self {
            config,
            provider_open: AtomicBool::new(false),
            submitted_delta_tickets: Mutex::new(Vec::new()),
            merged_tickets: Mutex::new(Vec::new()),
            resolved_conflicts: Mutex::new(Vec::new()),
            active_concurrent_calls: AtomicUsize::new(0),
            concurrency_violations: AtomicUsize::new(0),
        }
    }

    pub fn pt_open_provider(&self) -> Result<(), &'static str> {
        self.check_concurrency();
        self.provider_open.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn pt_get_print_capabilities(&self) -> Result<String, &'static str> {
        self.check_concurrency();

        let mut color_options = String::new();
        if self.config.supports_color {
            color_options.push_str("<psf:Option name=\"psk:Color\"/>\n");
        }
        if self.config.supports_monochrome {
            color_options.push_str("<psf:Option name=\"psk:Monochrome\"/>\n");
        }

        let mut duplex_options = String::from("<psf:Option name=\"psk:OneSided\"/>\n");
        if self.config.supports_duplex {
            duplex_options.push_str("<psf:Option name=\"psk:TwoSidedLongEdge\"/>\n");
            duplex_options.push_str("<psf:Option name=\"psk:TwoSidedShortEdge\"/>\n");
        }

        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<psf:PrintCapabilities xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework"
                       xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords"
                       version="1">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Property name="psf:SelectionType"><psf:Value>psk:PickOne</psf:Value></psf:Property>
        {color_options}
    </psf:Feature>
    <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
        <psf:Property name="psf:SelectionType"><psf:Value>psk:PickOne</psf:Value></psf:Property>
        {duplex_options}
    </psf:Feature>
    <psf:Feature name="psk:PageOrientation">
        <psf:Option name="psk:Portrait"/>
        <psf:Option name="psk:Landscape"/>
    </psf:Feature>
</psf:PrintCapabilities>"#
        );
        Ok(xml)
    }

    pub fn pt_merge_and_validate(
        &self,
        _base_ticket_xml: Option<&str>,
        delta_ticket_xml: &str,
    ) -> Result<String, &'static str> {
        self.check_concurrency();

        self.submitted_delta_tickets
            .lock()
            .unwrap()
            .push(delta_ticket_xml.to_string());

        let mut chosen_color = "psk:Color";
        let mut chosen_duplex = "psk:OneSided";
        let mut chosen_orientation = "psk:Portrait";

        if delta_ticket_xml.contains("psk:Monochrome") {
            if self.config.supports_monochrome {
                chosen_color = "psk:Monochrome";
            } else {
                // Simulating Microsoft Print to PDF conflict resolution
                chosen_color = "psk:Color";
                self.resolved_conflicts.lock().unwrap().push(PrintTicketConflict {
                    feature: "psk:PageOutputColor".into(),
                    requested_option: "psk:Monochrome".into(),
                    resolved_option: "psk:Color".into(),
                    reason: "Driver PrintCapabilities omits monochrome; resolved to color".into(),
                });
            }
        }

        if delta_ticket_xml.contains("psk:TwoSidedLongEdge") {
            if self.config.supports_duplex {
                chosen_duplex = "psk:TwoSidedLongEdge";
            } else {
                chosen_duplex = "psk:OneSided";
                self.resolved_conflicts.lock().unwrap().push(PrintTicketConflict {
                    feature: "psk:JobDuplexAllDocumentsContiguously".into(),
                    requested_option: "psk:TwoSidedLongEdge".into(),
                    resolved_option: "psk:OneSided".into(),
                    reason: "Device does not support duplex finishing".into(),
                });
            }
        } else if delta_ticket_xml.contains("psk:TwoSidedShortEdge") {
            if self.config.supports_duplex {
                chosen_duplex = "psk:TwoSidedShortEdge";
            } else {
                chosen_duplex = "psk:OneSided";
            }
        }

        if delta_ticket_xml.contains("psk:Landscape") {
            chosen_orientation = "psk:Landscape";
        }

        let merged_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework"
                 xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords"
                 version="1">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="{chosen_color}"/>
    </psf:Feature>
    <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
        <psf:Option name="{chosen_duplex}"/>
    </psf:Feature>
    <psf:Feature name="psk:PageOrientation">
        <psf:Option name="{chosen_orientation}"/>
    </psf:Feature>
</psf:PrintTicket>"#
        );

        self.merged_tickets.lock().unwrap().push(merged_xml.clone());
        Ok(merged_xml)
    }

    fn check_concurrency(&self) {
        let count = self.active_concurrent_calls.fetch_add(1, Ordering::SeqCst);
        if count > 0 {
            // Concurrent entry detected without external mutex
            self.concurrency_violations.fetch_add(1, Ordering::SeqCst);
        }
        std::thread::sleep(Duration::from_millis(1));
        self.active_concurrent_calls.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn assert_no_concurrency_violations(&self) {
        let violations = self.concurrency_violations.load(Ordering::SeqCst);
        assert_eq!(
            violations, 0,
            "Detected {violations} concurrent calls to MockV4Driver without PRINT_TICKET_PROVIDER lock!"
        );
    }

    pub fn last_merged_ticket(&self) -> Option<String> {
        self.merged_tickets.lock().unwrap().last().cloned()
    }

    pub fn conflicts(&self) -> Vec<PrintTicketConflict> {
        self.resolved_conflicts.lock().unwrap().clone()
    }
}

// ============================================================================
// 3. CUPS / IPP 2.0 Mock Implementation
// ============================================================================

pub const IPP_STATUS_OK: u16 = 0x0000;
pub const IPP_STATUS_ERROR_BAD_REQUEST: u16 = 0x0400;
pub const IPP_STATUS_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED: u16 = 0x040B;
pub const IPP_STATUS_ERROR_SERVER_BUSY: u16 = 0x0507;

#[derive(Debug, Clone)]
pub struct CapturedIppJob {
    pub job_id: u32,
    pub printer_uri: String,
    pub attributes: HashMap<String, String>,
    pub pdf_stream_len: usize,
}

pub struct MockIppDriver {
    pub printer_name: String,
    pub supports_duplex: bool,
    pub supports_monochrome: bool,
    next_job_id: AtomicU32,
    submitted_jobs: Mutex<Vec<CapturedIppJob>>,
}

impl MockIppDriver {
    pub fn new(printer_name: &str, supports_duplex: bool) -> Self {
        Self {
            printer_name: printer_name.into(),
            supports_duplex,
            supports_monochrome: true,
            next_job_id: AtomicU32::new(1001),
            submitted_jobs: Mutex::new(Vec::new()),
        }
    }

    pub fn submit_ipp_job(
        &self,
        attributes: HashMap<String, String>,
        pdf_bytes: &[u8],
    ) -> Result<u32, u16> {
        if let Some(sides) = attributes.get("sides")
            && sides != "one-sided"
            && !self.supports_duplex
        {
            return Err(IPP_STATUS_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED);
        }

        let job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);
        let captured = CapturedIppJob {
            job_id,
            printer_uri: format!("ipp://localhost:631/printers/{}", self.printer_name),
            attributes,
            pdf_stream_len: pdf_bytes.len(),
        };
        self.submitted_jobs.lock().unwrap().push(captured);
        Ok(job_id)
    }

    pub fn last_job(&self) -> Option<CapturedIppJob> {
        self.submitted_jobs.lock().unwrap().last().cloned()
    }

    pub fn assert_attribute(&self, key: &str, expected_val: &str) {
        let last = self.last_job().expect("No IPP job submitted to MockIppDriver");
        let val = last
            .attributes
            .get(key)
            .unwrap_or_else(|| panic!("Attribute '{key}' missing from IPP request"));
        assert_eq!(
            val, expected_val,
            "Attribute '{key}' mismatch! Expected '{expected_val}', got '{val}'"
        );
    }

    /// Validates and parses an incoming IPP 2.0 wire request buffer according to RFC 8010/8011.
    pub fn parse_and_validate_wire_request(raw: &[u8]) -> Result<ParsedIppRequest, IppWireError> {
        if raw.len() < 8 {
            return Err(IppWireError::BufferTooShort {
                len: raw.len(),
                expected: 8,
            });
        }
        let major = raw[0];
        let minor = raw[1];
        if major != 2 {
            return Err(IppWireError::InvalidVersion { major, minor });
        }
        let op_code = u16::from_be_bytes([raw[2], raw[3]]);
        let supported_ops = [0x0002, 0x0004, 0x000B]; // PrintJob, ValidateJob, GetPrinterAttributes
        if !supported_ops.contains(&op_code) {
            return Err(IppWireError::UnsupportedOperation(op_code));
        }
        let request_id = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);

        let mut offset = 8;
        let mut in_group = false;
        let mut attributes = HashMap::new();

        while offset < raw.len() {
            let tag = raw[offset];
            offset += 1;

            if tag == 0x03 {
                // EndOfAttributes
                break;
            }

            // Delimiter tags: 0x01..=0x05
            if (0x01..=0x05).contains(&tag) {
                in_group = true;
                continue;
            }

            // Value tags: 0x10..=0x4A
            if (0x10..=0x4A).contains(&tag) {
                if !in_group {
                    return Err(IppWireError::InvalidTagOctet(tag));
                }
                if offset + 2 > raw.len() {
                    return Err(IppWireError::UnexpectedEof { offset, needed: 2 });
                }
                let name_len = u16::from_be_bytes([raw[offset], raw[offset + 1]]) as usize;
                offset += 2;
                if offset + name_len > raw.len() {
                    return Err(IppWireError::UnexpectedEof { offset, needed: name_len });
                }
                let name = String::from_utf8_lossy(&raw[offset..offset + name_len]).to_string();
                offset += name_len;

                if offset + 2 > raw.len() {
                    return Err(IppWireError::UnexpectedEof { offset, needed: 2 });
                }
                let val_len = u16::from_be_bytes([raw[offset], raw[offset + 1]]) as usize;
                offset += 2;
                if offset + val_len > raw.len() {
                    return Err(IppWireError::UnexpectedEof { offset, needed: val_len });
                }
                let val = String::from_utf8_lossy(&raw[offset..offset + val_len]).to_string();
                offset += val_len;

                attributes.insert(name, val);
            } else {
                return Err(IppWireError::InvalidTagOctet(tag));
            }
        }

        Ok(ParsedIppRequest {
            version: (major, minor),
            operation_id: op_code,
            request_id,
            attributes,
            data_offset: offset,
        })
    }

    /// Parses an IPP 2.0 response buffer (RFC 8010 section 3.1.2).
    pub fn parse_ipp_response(response_bytes: &[u8]) -> Result<ParsedIppResponse, IppWireError> {
        if response_bytes.len() < 8 {
            return Err(IppWireError::BufferTooShort {
                len: response_bytes.len(),
                expected: 8,
            });
        }
        let major = response_bytes[0];
        let minor = response_bytes[1];
        if major != 2 {
            return Err(IppWireError::InvalidVersion { major, minor });
        }
        let status_code = u16::from_be_bytes([response_bytes[2], response_bytes[3]]);
        let request_id = u32::from_be_bytes([
            response_bytes[4],
            response_bytes[5],
            response_bytes[6],
            response_bytes[7],
        ]);
        Ok(ParsedIppResponse {
            version: (major, minor),
            status_code,
            request_id,
        })
    }

    /// Dispatches an IPP wire request directly to the mock driver.
    pub fn submit_ipp_wire_job(&self, wire_bytes: &[u8]) -> Result<u32, IppWireError> {
        let parsed = Self::parse_and_validate_wire_request(wire_bytes)?;
        let pdf_data = if parsed.data_offset < wire_bytes.len() {
            &wire_bytes[parsed.data_offset..]
        } else {
            &[]
        };
        self.submit_ipp_job(parsed.attributes, pdf_data)
            .map_err(|status| IppWireError::AttributeError(format!("IPP error status: 0x{status:04X}")))
    }
}

// =========================================================================
// IPP 2.0 Wire Protocol Verification Types (RFC 8010 / RFC 8011)
// =========================================================================

/// Wire protocol error detected during IPP 2.0 message parsing or validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IppWireError {
    BufferTooShort { len: usize, expected: usize },
    InvalidVersion { major: u8, minor: u8 },
    UnsupportedOperation(u16),
    InvalidTagOctet(u8),
    UnexpectedEof { offset: usize, needed: usize },
    AttributeError(String),
}

/// Parsed IPP 2.0 wire request header and attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedIppRequest {
    pub version: (u8, u8),
    pub operation_id: u16,
    pub request_id: u32,
    pub attributes: HashMap<String, String>,
    pub data_offset: usize,
}

/// Parsed IPP 2.0 wire response header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedIppResponse {
    pub version: (u8, u8),
    pub status_code: u16,
    pub request_id: u32,
}

/// Creates a 0-margin virtual PDF printer mock (e.g. Microsoft Print to PDF).
pub fn create_mock_virtual_pdf_printer(name: &str) -> MockV4Driver {
    MockV4Driver::new(MockV4DriverConfig {
        printer_name: name.into(),
        supports_color: true,
        supports_monochrome: false, // Emulates omission of psk:Monochrome
        supports_duplex: false,     // Virtual printers lack duplex
        bounds: PrintableBounds::a4_borderless(),
    })
}
