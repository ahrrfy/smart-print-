//! crates/platform/src/caps.rs — GDI Device Capabilities & Physical Margin Querying (Layer 7)
//!
//! Queries physical printer dimensions, asymmetric hardware margins, duplex, color,
//! and copies capabilities using native Win32 GDI calls with RAII handle safety.

#![allow(unsafe_code)]

use pdfcraft_print::driver::{
    DriverError, DriverGeneration, DuplexBinding, HardwareMargins, PrintableBounds,
    PrinterCapabilities,
};

#[cfg(windows)]
#[allow(non_snake_case, clippy::upper_case_acronyms)]
mod win32 {
    use std::ffi::c_void;

    pub type HDC = *mut c_void;

    pub const HORZRES: i32 = 8;
    pub const VERTRES: i32 = 10;
    pub const LOGPIXELSX: i32 = 88;
    pub const LOGPIXELSY: i32 = 90;
    pub const PHYSICALWIDTH: i32 = 110;
    pub const PHYSICALHEIGHT: i32 = 111;
    pub const PHYSICALOFFSETX: i32 = 112;
    pub const PHYSICALOFFSETY: i32 = 113;

    pub const DC_DUPLEX: u16 = 7;
    pub const DC_COPIES: u16 = 18;
    pub const DC_COLORDEVICE: u16 = 32;

    #[link(name = "gdi32")]
    unsafe extern "system" {
        pub fn CreateDCW(
            pwszDriver: *const u16,
            pwszDevice: *const u16,
            pwszPort: *const u16,
            pdm: *const c_void,
        ) -> HDC;
        pub fn GetDeviceCaps(hdc: HDC, nIndex: i32) -> i32;
        pub fn DeleteDC(hdc: HDC) -> i32;
    }

    #[link(name = "winspool")]
    unsafe extern "system" {
        pub fn DeviceCapabilitiesW(
            pDevice: *const u16,
            pPort: *const u16,
            fwCapability: u16,
            pOutput: *mut u8,
            pDevMode: *const c_void,
        ) -> i32;
    }

    /// RAII guard ensuring DeleteDC is deterministically called to prevent GDI leaks.
    pub struct DcGuard(pub HDC);

    impl Drop for DcGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: self.0 is a valid HDC created by CreateDCW.
                #[allow(unsafe_code)]
                unsafe {
                    DeleteDC(self.0);
                }
            }
        }
    }

    pub fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

/// Raw GDI queried values in device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawGdiCaps {
    pub dpi_x: u32,
    pub dpi_y: u32,
    pub horz_res: u32,
    pub vert_res: u32,
    pub phys_width: u32,
    pub phys_height: u32,
    pub phys_offset_x: u32,
    pub phys_offset_y: u32,
    pub is_duplex: bool,
    pub is_color: bool,
    pub max_copies: u32,
}

impl RawGdiCaps {
    /// Convert device pixels to exact 1/72-inch points and calculate asymmetric margins.
    pub fn to_printable_bounds(&self) -> PrintableBounds {
        let dx = (self.dpi_x.max(1)) as f64;
        let dy = (self.dpi_y.max(1)) as f64;

        let scale_x = 72.0 / dx;
        let scale_y = 72.0 / dy;

        let right_px = self
            .phys_width
            .saturating_sub(self.horz_res.saturating_add(self.phys_offset_x));
        let bottom_px = self
            .phys_height
            .saturating_sub(self.vert_res.saturating_add(self.phys_offset_y));

        let margins = HardwareMargins {
            left_pt: (self.phys_offset_x as f64) * scale_x,
            top_pt: (self.phys_offset_y as f64) * scale_y,
            right_pt: (right_px as f64) * scale_x,
            bottom_pt: (bottom_px as f64) * scale_y,
        };

        PrintableBounds {
            printable_width_pt: (self.horz_res as f64) * scale_x,
            printable_height_pt: (self.vert_res as f64) * scale_y,
            physical_width_pt: (self.phys_width as f64) * scale_x,
            physical_height_pt: (self.phys_height as f64) * scale_y,
            dpi_x: self.dpi_x,
            dpi_y: self.dpi_y,
            margins,
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn query_raw_gdi_caps(printer_name: &str) -> Result<RawGdiCaps, DriverError> {
    use win32::{
        CreateDCW, DC_COLORDEVICE, DC_COPIES, DC_DUPLEX, DcGuard, DeviceCapabilitiesW,
        GetDeviceCaps, HORZRES, LOGPIXELSX, LOGPIXELSY, PHYSICALHEIGHT, PHYSICALOFFSETX,
        PHYSICALOFFSETY, PHYSICALWIDTH, VERTRES, to_wide,
    };

    if !printer_name.starts_with(r"\\")
        && !pdfcraft_print::spool::printer_queues()
            .iter()
            .any(|q| q.name.eq_ignore_ascii_case(printer_name))
    {
        return Err(DriverError::DeviceContextFailed(format!(
            "CreateDCW failed for printer '{}'",
            printer_name
        )));
    }

    let wide_name = to_wide(printer_name);
    let driver_name = to_wide("WINSPOOL");

    // 1. Create GDI Device Context
    let hdc = unsafe {
        CreateDCW(
            driver_name.as_ptr(),
            wide_name.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };

    if hdc.is_null() {
        return Err(DriverError::DeviceContextFailed(format!(
            "CreateDCW failed for printer '{}'",
            printer_name
        )));
    }

    // Wrap in RAII guard for deterministic handle release
    let _guard = DcGuard(hdc);

    // 2. Query basic resolution & dimensions in device pixels
    let dpi_x = (unsafe { GetDeviceCaps(hdc, LOGPIXELSX) }).max(72) as u32;
    let dpi_y = (unsafe { GetDeviceCaps(hdc, LOGPIXELSY) }).max(72) as u32;
    let horz_res = (unsafe { GetDeviceCaps(hdc, HORZRES) }).max(0) as u32;
    let vert_res = (unsafe { GetDeviceCaps(hdc, VERTRES) }).max(0) as u32;

    let mut phys_w = (unsafe { GetDeviceCaps(hdc, PHYSICALWIDTH) }).max(0) as u32;
    let mut phys_h = (unsafe { GetDeviceCaps(hdc, PHYSICALHEIGHT) }).max(0) as u32;
    let off_x = (unsafe { GetDeviceCaps(hdc, PHYSICALOFFSETX) }).max(0) as u32;
    let off_y = (unsafe { GetDeviceCaps(hdc, PHYSICALOFFSETY) }).max(0) as u32;

    // Fallback for virtual printer drivers reporting 0 physical dimensions
    if phys_w < horz_res {
        phys_w = horz_res.saturating_add(off_x.saturating_mul(2));
    }
    if phys_h < vert_res {
        phys_h = vert_res.saturating_add(off_y.saturating_mul(2));
    }

    // 3. Query hardware feature flags via DeviceCapabilitiesW
    let duplex = unsafe {
        DeviceCapabilitiesW(
            wide_name.as_ptr(),
            std::ptr::null(),
            DC_DUPLEX,
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    let color = unsafe {
        DeviceCapabilitiesW(
            wide_name.as_ptr(),
            std::ptr::null(),
            DC_COLORDEVICE,
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    let copies = unsafe {
        DeviceCapabilitiesW(
            wide_name.as_ptr(),
            std::ptr::null(),
            DC_COPIES,
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };

    Ok(RawGdiCaps {
        dpi_x,
        dpi_y,
        horz_res,
        vert_res,
        phys_width: phys_w,
        phys_height: phys_h,
        phys_offset_x: off_x,
        phys_offset_y: off_y,
        is_duplex: duplex == 1,
        is_color: color == 1,
        max_copies: (copies.max(1)) as u32,
    })
}

#[cfg(not(windows))]
pub fn query_raw_gdi_caps(printer_name: &str) -> Result<RawGdiCaps, DriverError> {
    let _ = printer_name;
    // Cross-platform standard letter/A4 fallback for non-Windows platforms
    Ok(RawGdiCaps {
        dpi_x: 600,
        dpi_y: 600,
        horz_res: 4820,
        vert_res: 6874,
        phys_width: 4960,
        phys_height: 7014,
        phys_offset_x: 70,
        phys_offset_y: 70,
        is_duplex: true,
        is_color: true,
        max_copies: 999,
    })
}

/// Query printable boundaries for a printer.
pub fn query_gdi_caps(printer_name: &str) -> Result<PrintableBounds, DriverError> {
    let raw = query_raw_gdi_caps(printer_name)?;
    Ok(raw.to_printable_bounds())
}

/// Query full PrinterCapabilities for a printer.
pub fn query_gdi_capabilities(printer_name: &str) -> Result<PrinterCapabilities, DriverError> {
    let raw = query_raw_gdi_caps(printer_name)?;
    let bounds = raw.to_printable_bounds();

    let supported_duplex = if raw.is_duplex {
        vec![
            DuplexBinding::Simplex,
            DuplexBinding::FlipLongEdge,
            DuplexBinding::FlipShortEdge,
        ]
    } else {
        vec![DuplexBinding::Simplex]
    };

    let generation = if pdfcraft_print::spool::xps_driver(printer_name) == Some(true) {
        DriverGeneration::Win32V4PrintTicket
    } else {
        DriverGeneration::Win32V3DevMode
    };

    Ok(PrinterCapabilities {
        name: printer_name.to_owned(),
        driver_generation: generation,
        is_duplex_supported: raw.is_duplex,
        supported_duplex,
        is_color_supported: raw.is_color,
        max_copies: raw.max_copies,
        bounds,
    })
}
