//! crates/platform/src/win32_v3.rs — Win32 V3 DEVMODE Driver Adapter (Layer 7)
//!
//! Implements 2-pass DEVMODE allocation preserving vendor private `dmDriverExtra` data
//! (>3.7KB on Epson/HP drivers), in-place hardware flag mutation (DMCOLOR_MONOCHROME,
//! DMDUP_VERTICAL, DMDUP_HORIZONTAL), and GDI spooling.

#![allow(unsafe_code)]

use pdfcraft_print::driver::{
    ColorMode, DriverError, DriverGeneration, DuplexBinding, PreFlightReport,
    PrintDriverAdapter, PrintJobSettings, PrinterCapabilities, SpoolSummary,
};
use std::ptr::{null, null_mut};

pub const DM_ORIENTATION: u32 = 0x0000_0001;
pub const DM_PAPERSIZE: u32 = 0x0000_0002;
pub const DM_COPIES: u32 = 0x0000_0100;
pub const DM_COLOR: u32 = 0x0000_0800;
pub const DM_DUPLEX: u32 = 0x0000_1000;
pub const DM_COLLATE: u32 = 0x0000_8000;

pub const DMORIENT_PORTRAIT: i16 = 1;
pub const DMORIENT_LANDSCAPE: i16 = 2;

pub const DMCOLOR_MONOCHROME: i16 = 1;
pub const DMCOLOR_COLOR: i16 = 2;

pub const DMDUP_SIMPLEX: i16 = 1;
pub const DMDUP_VERTICAL: i16 = 2; // Flip on Long Edge
pub const DMDUP_HORIZONTAL: i16 = 3; // Flip on Short Edge

pub const DMCOLLATE_FALSE: i16 = 0;
pub const DMCOLLATE_TRUE: i16 = 1;

pub const DM_OUT_BUFFER: u32 = 2;
pub const DM_IN_PROMPT: u32 = 4;
pub const DM_IN_BUFFER: u32 = 8;
pub const IDOK: i32 = 1;

#[cfg(windows)]
#[allow(non_snake_case, dead_code, clippy::upper_case_acronyms)]
mod ffi {
    use std::ffi::c_void;

    pub type HDC = *mut c_void;
    pub type HANDLE = *mut c_void;

    #[repr(C)]
    pub struct PRINTER_HANDLE {
        pub value: HANDLE,
    }

    #[repr(C)]
    pub struct DOCINFOW {
        pub cbSize: i32,
        pub lpszDocName: *const u16,
        pub lpszOutput: *const u16,
        pub lpszDatatype: *const u16,
        pub fwType: u32,
    }

    #[link(name = "winspool")]
    unsafe extern "system" {
        pub fn OpenPrinterW(
            pPrinterName: *const u16,
            phPrinter: *mut HANDLE,
            pDefault: *const c_void,
        ) -> i32;
        pub fn ClosePrinter(hPrinter: HANDLE) -> i32;
        pub fn DocumentPropertiesW(
            hWnd: HANDLE,
            hPrinter: HANDLE,
            pDeviceName: *const u16,
            pDevModeOutput: *mut c_void,
            pDevModeInput: *const c_void,
            fMode: u32,
        ) -> i32;
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        pub fn CreateDCW(
            pwszDriver: *const u16,
            pwszDevice: *const u16,
            pwszPort: *const u16,
            pdm: *const c_void,
        ) -> HDC;
        pub fn DeleteDC(hdc: HDC) -> i32;
        pub fn StartDocW(hdc: HDC, lpdi: *const DOCINFOW) -> i32;
        pub fn StartPage(hdc: HDC) -> i32;
        pub fn EndPage(hdc: HDC) -> i32;
        pub fn EndDoc(hdc: HDC) -> i32;
    }
}

/// 8-byte aligned DEVMODE buffer preserving vendor private driver extra data.
#[derive(Clone, Debug)]
pub struct DevModeBuffer {
    data: Vec<u64>,
    pub len: usize,
}

impl DevModeBuffer {
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr().cast()
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr().cast()
    }
}

#[cfg(windows)]
pub fn get_modified_devmode(
    printer_name: &str,
    color_mode: ColorMode,
    duplex: DuplexBinding,
    landscape: bool,
    copies: u32,
) -> Result<DevModeBuffer, DriverError> {
    use ffi::{ClosePrinter, DocumentPropertiesW, HANDLE, OpenPrinterW};

    let wide_name: Vec<u16> = printer_name.encode_utf16().chain(Some(0)).collect();

    // 1. Open printer handle
    let mut h_printer: HANDLE = null_mut();
    let ok = unsafe { OpenPrinterW(wide_name.as_ptr(), &mut h_printer, null()) };
    if ok == 0 || h_printer.is_null() {
        return Err(DriverError::PrinterNotFound(printer_name.to_string()));
    }

    struct PrinterGuard(HANDLE);
    impl Drop for PrinterGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    ClosePrinter(self.0);
                }
            }
        }
    }
    let _guard = PrinterGuard(h_printer);

    // 2. Pass 1: Query required buffer size
    let needed = unsafe {
        DocumentPropertiesW(
            null_mut(),
            h_printer,
            wide_name.as_ptr(),
            null_mut(),
            null(),
            0,
        )
    };
    if needed <= 0 {
        return Err(DriverError::DevModeError(format!(
            "Failed to query DEVMODE size for '{}'",
            printer_name
        )));
    }

    // Headroom safety formula (Chromium win_helper.cc):
    // Preserves vendor private data (>3.7KB on Epson) across DM_IN_BUFFER | DM_OUT_BUFFER
    let alloc_bytes = ((needed as usize).max(220))
        .saturating_mul(2)
        .saturating_add(8192);
    let u64_count = alloc_bytes.div_ceil(8);
    let mut buf_u64 = vec![0u64; u64_count];

    // 3. Fetch default printer DEVMODE
    let res = unsafe {
        DocumentPropertiesW(
            null_mut(),
            h_printer,
            wide_name.as_ptr(),
            buf_u64.as_mut_ptr().cast(),
            null(),
            DM_OUT_BUFFER,
        )
    };
    if res != IDOK {
        return Err(DriverError::DevModeError(format!(
            "Failed to fetch default DEVMODE for '{}'",
            printer_name
        )));
    }

    // 4. Mutate fields in-place:
    // Offsets per verified Windows DEVMODEW specification:
    // Offset 68: dmSize (u16)
    // Offset 70: dmDriverExtra (u16)
    // Offset 72: dmFields (u32)
    // Offset 76: dmOrientation (i16)
    // Offset 86: dmCopies (i16)
    // Offset 92: dmColor (i16)
    // Offset 94: dmDuplex (i16)
    // Offset 100: dmCollate (i16)
    let raw = buf_u64.as_mut_ptr() as *mut u8;
    unsafe {
        let dm_fields_ptr = raw.add(72) as *mut u32;
        let dm_orientation_ptr = raw.add(76) as *mut i16;
        let dm_copies_ptr = raw.add(86) as *mut i16;
        let dm_color_ptr = raw.add(92) as *mut i16;
        let dm_duplex_ptr = raw.add(94) as *mut i16;
        let dm_collate_ptr = raw.add(100) as *mut i16;

        // Color Mode (Layer 1 Hardware Driver Assertion)
        match color_mode {
            ColorMode::MonochromeKOnly => {
                *dm_color_ptr = DMCOLOR_MONOCHROME;
                *dm_fields_ptr |= DM_COLOR;
            }
            ColorMode::Color => {
                *dm_color_ptr = DMCOLOR_COLOR;
                *dm_fields_ptr |= DM_COLOR;
            }
        }

        // Duplex Binding
        match duplex {
            DuplexBinding::Simplex => {
                *dm_duplex_ptr = DMDUP_SIMPLEX;
                *dm_fields_ptr |= DM_DUPLEX;
            }
            DuplexBinding::FlipLongEdge => {
                *dm_duplex_ptr = DMDUP_VERTICAL;
                *dm_fields_ptr |= DM_DUPLEX;
            }
            DuplexBinding::FlipShortEdge => {
                *dm_duplex_ptr = DMDUP_HORIZONTAL;
                *dm_fields_ptr |= DM_DUPLEX;
            }
        }

        // Orientation
        if landscape {
            *dm_orientation_ptr = DMORIENT_LANDSCAPE;
            *dm_fields_ptr |= DM_ORIENTATION;
        } else {
            *dm_orientation_ptr = DMORIENT_PORTRAIT;
            *dm_fields_ptr |= DM_ORIENTATION;
        }

        // Copies
        if copies > 1 {
            *dm_copies_ptr = (copies.min(i16::MAX as u32)) as i16;
            *dm_fields_ptr |= DM_COPIES;
            *dm_collate_ptr = DMCOLLATE_TRUE;
            *dm_fields_ptr |= DM_COLLATE;
        }
    }

    // 5. Pass 2: Merge and Validate with DM_IN_BUFFER | DM_OUT_BUFFER
    let mut merged_u64 = vec![0u64; u64_count];
    let merge_res = unsafe {
        DocumentPropertiesW(
            null_mut(),
            h_printer,
            wide_name.as_ptr(),
            merged_u64.as_mut_ptr().cast(),
            buf_u64.as_ptr().cast(),
            DM_IN_BUFFER | DM_OUT_BUFFER,
        )
    };
    if merge_res != IDOK {
        return Err(DriverError::DevModeError(format!(
            "Driver '{}' rejected modified DEVMODE",
            printer_name
        )));
    }

    let raw_merged = merged_u64.as_ptr() as *const u8;
    let (dm_size, dm_extra) = unsafe {
        let s = *(raw_merged.add(68) as *const u16) as usize;
        let e = *(raw_merged.add(70) as *const u16) as usize;
        (s, e)
    };
    let total_len = (dm_size.saturating_add(dm_extra)).min(alloc_bytes);

    Ok(DevModeBuffer {
        data: merged_u64,
        len: total_len,
    })
}

#[cfg(not(windows))]
pub fn get_modified_devmode(
    printer_name: &str,
    _color_mode: ColorMode,
    _duplex: DuplexBinding,
    _landscape: bool,
    _copies: u32,
) -> Result<DevModeBuffer, DriverError> {
    let _ = printer_name;
    Ok(DevModeBuffer {
        data: vec![0u64; 220.div_ceil(8)],
        len: 220,
    })
}

/// Win32 V3 DEVMODE Print Driver Adapter.
#[derive(Clone, Debug, Default)]
pub struct Win32V3Adapter;

impl Win32V3Adapter {
    pub fn new() -> Self {
        Self
    }
}

impl PrintDriverAdapter for Win32V3Adapter {
    fn query_capabilities(&self, printer_name: &str) -> Result<PrinterCapabilities, DriverError> {
        let mut caps = crate::caps::query_gdi_capabilities(printer_name)?;
        caps.driver_generation = DriverGeneration::Win32V3DevMode;
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
