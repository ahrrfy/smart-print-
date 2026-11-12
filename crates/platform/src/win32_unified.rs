//! Unified Windows `DEVMODEW` + `PrintTicket` Driver Bridge (Layer 7).
//!
//! Connects `pdfcraft-platform` (`L7`) to `pdfcraft-print::spool` (`L4`) via
//! [`install_print_bridge`], enforcing unified broadcast specifications across every selected
//! Windows printer:
//! - 2-pass `DocumentPropertiesW` allocation with Chromium headroom preserving vendor `dmDriverExtra`.
//! - Explicit hardware Color (`DMCOLOR_COLOR = 2`) vs. Monochrome (`DMCOLOR_MONOCHROME = 1`).
//! - Orientation-independent `1.0 mm` paper matching against `DC_PAPERS` / `DC_PAPERSIZE`,
//!   `DMPAPER_USER (256)` custom dimensions in `0.1 mm`, and automatic `ShrinkToFit` adaptation
//!   when fixed-tray drivers substitute paper.
//! - Hardware duplex capability detection (`DC_DUPLEX`) with automatic `Simplex` fallback.
//! - `PTConvertDevModeToPrintTicket` (`kPTJobScope = 2`) conversion with full COM `IStream` / `BSTR`
//!   RAII cleanup and shared process-wide `PRINT_TICKET_PROVIDER` synchronization.
//! - Interactive `capture_driver_template` via per-job `DocumentPropertiesW(DM_IN_PROMPT | DM_OUT_BUFFER)`.

#![allow(unsafe_code)]

use std::sync::Once;

use pdfcraft_print::PrintError;
#[cfg(windows)]
use pdfcraft_print::driver::{
    ColorMode, DriverError, DuplexBinding, PageFitMode, PrintJobSettings, SpoolSummary,
};
use pdfcraft_print::spool::{CapturedDriverTemplate, Job, WindowsUnifiedBridge};
#[cfg(windows)]
use pdfcraft_print::spool::{
    Duplex, Scaling, register_windows_unified_bridge, set_job_broadcast_specs,
};

static INSTALL_BRIDGE_ONCE: Once = Once::new();

/// Bridge singleton registered with `pdfcraft_print::spool`.
pub struct Win32UnifiedBridge;

#[cfg(windows)]
static WIN32_UNIFIED_BRIDGE: Win32UnifiedBridge = Win32UnifiedBridge;

/// Idempotently installs the native Windows unified print bridge into `pdfcraft-print`.
/// Safe no-op on non-Windows targets.
pub fn install_print_bridge() {
    INSTALL_BRIDGE_ONCE.call_once(|| {
        #[cfg(windows)]
        register_windows_unified_bridge(&WIN32_UNIFIED_BRIDGE);
    });
}

impl WindowsUnifiedBridge for Win32UnifiedBridge {
    fn submit_unified(
        &self,
        pdf_bytes: &[u8],
        job: &Job,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<String, PrintError> {
        #[cfg(windows)]
        {
            imp::submit_unified_windows(pdf_bytes, job, progress)
        }
        #[cfg(not(windows))]
        {
            let _ = (pdf_bytes, job, progress);
            Err(PrintError::Spool(
                "Win32 unified print bridge is only available on Windows".into(),
            ))
        }
    }

    fn capture_driver_template(
        &self,
        printer: &str,
    ) -> Result<Option<CapturedDriverTemplate>, PrintError> {
        #[cfg(windows)]
        {
            imp::capture_driver_template_windows(printer)
        }
        #[cfg(not(windows))]
        {
            let _ = printer;
            Ok(None)
        }
    }
}

/// Spool a [`PrintJobSettings`] job through the unified Windows `DEVMODEW` + `PrintTicket` pipeline.
#[cfg(windows)]
pub(crate) fn spool_driver_job(
    settings: &PrintJobSettings,
    pdf_bytes: &[u8],
) -> Result<SpoolSummary, DriverError> {
    install_print_bridge();
    let duplex = match settings.duplex_mode {
        DuplexBinding::Simplex => Duplex::Off,
        DuplexBinding::FlipLongEdge => Duplex::LongEdge,
        DuplexBinding::FlipShortEdge => Duplex::ShortEdge,
    };
    let scaling = match settings.fit_mode {
        PageFitMode::ActualSize => Scaling::Exact,
        PageFitMode::FitPrintableArea | PageFitMode::ShrinkOversized => Scaling::ShrinkToFit,
    };
    let mut job = Job {
        printer: (!settings.printer_name.is_empty()).then(|| settings.printer_name.clone()),
        title: settings.job_title.clone(),
        copies: settings.copies.max(1),
        collate: true,
        duplex: Some(duplex),
        grayscale: settings.color_mode == ColorMode::MonochromeKOnly,
        scaling,
        ..Job::default()
    };
    let w_um =
        pdfcraft_print::spool::windows_unified::pts_to_um(settings.page_bounds.0).unwrap_or(210_000);
    let h_um =
        pdfcraft_print::spool::windows_unified::pts_to_um(settings.page_bounds.1).unwrap_or(297_000);
    set_job_broadcast_specs(
        &mut job,
        settings.color_mode == ColorMode::Color,
        (w_um, h_um),
        Some(settings.landscape),
    );

    let mut pages_spooled = 0usize;
    pdfcraft_print::spool::submit_with_progress(pdf_bytes, &job, &mut |sent, total| {
        pages_spooled = pages_spooled.max(total).max(sent);
    })
    .map_err(|e| DriverError::SpoolFailed(e.to_string()))?;

    Ok(SpoolSummary {
        job_id: 1,
        pages_spooled: pages_spooled.max(1),
        bytes_written: pdf_bytes.len(),
    })
}

/// Merge `base_xml` and `delta_xml` via `prntvpt.dll` (`kPTJobScope = 2`) and return the validated
/// result `PrintTicket` XML string without leaking COM streams or `BSTR` error messages.
#[cfg(windows)]
pub(crate) fn merge_and_read_ticket_xml(
    printer_name: &str,
    base_xml: &[u8],
    delta_xml: &[u8],
) -> Result<String, DriverError> {
    imp::merge_and_read_ticket_xml_impl(printer_name, base_xml, delta_xml)
}

#[cfg(windows)]
#[allow(non_snake_case, clippy::upper_case_acronyms)]
mod imp {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::sync::OnceLock;

    use pdfcraft_print::PrintError;
    use pdfcraft_print::driver::DriverError;
    use pdfcraft_print::spool::windows_unified::{
        ColorIntent, JobSheetTarget, WindowsNormalizedSeed, inspect_job_target,
        paper_matches_retained, parse_devmode_template_bytes, submit_with_seed,
    };
    use pdfcraft_print::spool::{
        CapturedDriverTemplate, Duplex, Job, open_printer_preferences, printer_papers,
        with_ticket_provider_lock,
    };

    type HANDLE = *mut c_void;
    type HDC = *mut c_void;
    type HPTPROVIDER = isize;
    type HRESULT = i32;

    const DM_ORIENTATION: u32 = 0x0000_0001;
    const DM_PAPERSIZE: u32 = 0x0000_0002;
    const DM_PAPERLENGTH: u32 = 0x0000_0004;
    const DM_PAPERWIDTH: u32 = 0x0000_0008;
    const DM_COPIES: u32 = 0x0000_0100;
    const DM_COLOR: u32 = 0x0000_0800;
    const DM_DUPLEX: u32 = 0x0000_1000;
    const DM_COLLATE: u32 = 0x0000_8000;
    const DM_FORMNAME: u32 = 0x0001_0000;

    const DMORIENT_PORTRAIT: i16 = 1;
    const DMORIENT_LANDSCAPE: i16 = 2;
    const DMPAPER_USER: i16 = 256;
    const DMCOLOR_MONOCHROME: i16 = 1;
    const DMCOLOR_COLOR: i16 = 2;
    const DMDUP_SIMPLEX: i16 = 1;
    const DMDUP_VERTICAL: i16 = 2;
    const DMDUP_HORIZONTAL: i16 = 3;
    const DMCOLLATE_FALSE: i16 = 0;
    const DMCOLLATE_TRUE: i16 = 1;

    const DM_OUT_BUFFER: u32 = 2;
    const DM_IN_PROMPT: u32 = 4;
    const DM_IN_BUFFER: u32 = 8;
    const IDOK: i32 = 1;

    const DC_PAPERS: u16 = 2;
    const DC_PAPERSIZE: u16 = 3;
    const DC_DUPLEX: u16 = 7;
    const DC_COLORDEVICE: u16 = 32;

    const LOGPIXELSX: i32 = 88;
    const LOGPIXELSY: i32 = 90;
    const PHYSICALWIDTH: i32 = 110;
    const PHYSICALHEIGHT: i32 = 111;
    const HORZRES: i32 = 8;
    const VERTRES: i32 = 10;

    /// `EPrintTicketScope::kPTJobScope` (`prntvpt.h`: Page=0, Document=1, Job=2).
    const K_PT_JOB_SCOPE: i32 = 2;
    const STREAM_SEEK_SET: u32 = 0;
    const MAX_TICKET_STREAM_BYTES: usize = 8 << 20;
    /// `1.0 mm` tolerance in `0.1 mm` Win32 `DEVMODEW` units.
    const PAPER_TOLERANCE_TMM: i32 = 10;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct PointI32 {
        pub(super) x: i32,
        pub(super) y: i32,
    }

    #[repr(C)]
    struct IStreamVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        read: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *mut u32) -> HRESULT,
        write: unsafe extern "system" fn(*mut c_void, *const c_void, u32, *mut u32) -> HRESULT,
        seek: unsafe extern "system" fn(*mut c_void, i64, u32, *mut u64) -> HRESULT,
    }

    #[repr(C)]
    struct ComIStream {
        lp_vtbl: *const IStreamVtbl,
    }

    type FnPTOpenProvider = unsafe extern "system" fn(
        pszPrinterName: *const u16,
        dwVersion: u32,
        phProvider: *mut HPTPROVIDER,
    ) -> HRESULT;
    type FnPTCloseProvider = unsafe extern "system" fn(hProvider: HPTPROVIDER) -> HRESULT;
    type FnPTConvertDevModeToPrintTicket = unsafe extern "system" fn(
        hProvider: HPTPROVIDER,
        cbDevmode: u32,
        pDevmode: *const c_void,
        scope: i32,
        pPrintTicket: *mut c_void,
    ) -> HRESULT;
    type FnPTMergeAndValidatePrintTicket = unsafe extern "system" fn(
        hProvider: HPTPROVIDER,
        pBaseTicket: *mut c_void,
        pDeltaTicket: *mut c_void,
        scope: i32,
        pResultTicket: *mut c_void,
        pbstrErrorMessage: *mut *mut u16,
    ) -> HRESULT;
    type FnSHCreateMemStream =
        unsafe extern "system" fn(pInit: *const u8, cbInit: u32) -> *mut c_void;

    #[derive(Clone, Copy)]
    struct PrntVptApi {
        open_provider: FnPTOpenProvider,
        close_provider: FnPTCloseProvider,
        convert_devmode_to_ticket: FnPTConvertDevModeToPrintTicket,
        merge_ticket: FnPTMergeAndValidatePrintTicket,
        create_mem_stream: FnSHCreateMemStream,
    }

    // SAFETY: Function pointers live in pinned system DLL modules for the lifetime of the process.
    unsafe impl Send for PrntVptApi {}
    unsafe impl Sync for PrntVptApi {}

    #[link(name = "winspool")]
    unsafe extern "system" {
        fn OpenPrinterW(
            pPrinterName: *const u16,
            phPrinter: *mut HANDLE,
            pDefault: *const c_void,
        ) -> i32;
        fn ClosePrinter(hPrinter: HANDLE) -> i32;
        fn DocumentPropertiesW(
            hWnd: HANDLE,
            hPrinter: HANDLE,
            pDeviceName: *const u16,
            pDevModeOutput: *mut c_void,
            pDevModeInput: *const c_void,
            fMode: u32,
        ) -> i32;
        fn DeviceCapabilitiesW(
            pDevice: *const u16,
            pPort: *const u16,
            fwCapability: u16,
            pOutput: *mut u8,
            pDevMode: *const c_void,
        ) -> i32;
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn CreateDCW(
            pwszDriver: *const u16,
            pwszDevice: *const u16,
            pwszPort: *const u16,
            pdm: *const c_void,
        ) -> HDC;
        fn GetDeviceCaps(hdc: HDC, nIndex: i32) -> i32;
        fn DeleteDC(hdc: HDC) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;
        fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *mut c_void;
    }

    #[link(name = "oleaut32")]
    unsafe extern "system" {
        fn SysFreeString(bstrString: *mut u16);
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetActiveWindow() -> HANDLE;
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

    struct DcGuard(HDC);
    impl Drop for DcGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    DeleteDC(self.0);
                }
            }
        }
    }

    struct ComStreamGuard(*mut c_void);
    impl Drop for ComStreamGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let stream = self.0 as *mut ComIStream;
                    let vtbl = (*stream).lp_vtbl;
                    if !vtbl.is_null() {
                        ((*vtbl).release)(self.0);
                    }
                }
            }
        }
    }

    struct BstrGuard(*mut u16);
    impl Drop for BstrGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    SysFreeString(self.0);
                }
            }
        }
    }

    struct ProviderGuard {
        provider: HPTPROVIDER,
        close_fn: FnPTCloseProvider,
    }
    impl Drop for ProviderGuard {
        fn drop(&mut self) {
            if self.provider != 0 {
                unsafe {
                    (self.close_fn)(self.provider);
                }
            }
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn prntvpt_api() -> Result<PrntVptApi, DriverError> {
        static API: OnceLock<Result<PrntVptApi, DriverError>> = OnceLock::new();
        API.get_or_init(load_prntvpt_api).clone()
    }

    fn load_prntvpt_api() -> Result<PrntVptApi, DriverError> {
        let prntvpt_w = to_wide("prntvpt.dll");
        let shlwapi_w = to_wide("shlwapi.dll");
        // Intentionally pinned for process lifetime so background COM/provider threads never
        // race DLL unloading.
        let h_prntvpt = unsafe { LoadLibraryW(prntvpt_w.as_ptr()) };
        if h_prntvpt.is_null() {
            return Err(DriverError::DllNotFound(
                "prntvpt.dll is not available on this system".into(),
            ));
        }
        let h_shlwapi = unsafe { LoadLibraryW(shlwapi_w.as_ptr()) };
        if h_shlwapi.is_null() {
            return Err(DriverError::DllNotFound(
                "shlwapi.dll is not available on this system".into(),
            ));
        }
        unsafe {
            let load_sym = |module: *mut c_void, name: &[u8], label: &str| {
                let ptr = GetProcAddress(module, name.as_ptr());
                if ptr.is_null() {
                    Err(DriverError::PrintTicketError(format!(
                        "{label} symbol not found"
                    )))
                } else {
                    Ok(ptr)
                }
            };
            let open_ptr = load_sym(h_prntvpt, b"PTOpenProvider\0", "PTOpenProvider")?;
            let close_ptr = load_sym(h_prntvpt, b"PTCloseProvider\0", "PTCloseProvider")?;
            let conv_ptr = load_sym(
                h_prntvpt,
                b"PTConvertDevModeToPrintTicket\0",
                "PTConvertDevModeToPrintTicket",
            )?;
            let merge_ptr = load_sym(
                h_prntvpt,
                b"PTMergeAndValidatePrintTicket\0",
                "PTMergeAndValidatePrintTicket",
            )?;
            let stream_ptr = load_sym(h_shlwapi, b"SHCreateMemStream\0", "SHCreateMemStream")?;
            Ok(PrntVptApi {
                open_provider: std::mem::transmute::<*mut c_void, FnPTOpenProvider>(open_ptr),
                close_provider: std::mem::transmute::<*mut c_void, FnPTCloseProvider>(close_ptr),
                convert_devmode_to_ticket: std::mem::transmute::<
                    *mut c_void,
                    FnPTConvertDevModeToPrintTicket,
                >(conv_ptr),
                merge_ticket: std::mem::transmute::<*mut c_void, FnPTMergeAndValidatePrintTicket>(
                    merge_ptr,
                ),
                create_mem_stream: std::mem::transmute::<*mut c_void, FnSHCreateMemStream>(
                    stream_ptr,
                ),
            })
        }
    }

    fn read_com_stream_bytes(stream_ptr: *mut c_void) -> Result<Vec<u8>, DriverError> {
        if stream_ptr.is_null() {
            return Err(DriverError::PrintTicketError("null COM stream".into()));
        }
        unsafe {
            let stream = stream_ptr as *mut ComIStream;
            let vtbl = (*stream).lp_vtbl;
            if vtbl.is_null() {
                return Err(DriverError::PrintTicketError(
                    "null COM stream vtable".into(),
                ));
            }
            let seek_hr = ((*vtbl).seek)(stream_ptr, 0, STREAM_SEEK_SET, null_mut());
            if seek_hr < 0 {
                return Err(DriverError::PrintTicketError(format!(
                    "IStream::Seek failed with HRESULT 0x{:08X}",
                    seek_hr as u32
                )));
            }
            let mut out = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let mut read_len: u32 = 0;
                let hr = ((*vtbl).read)(
                    stream_ptr,
                    chunk.as_mut_ptr().cast(),
                    chunk.len() as u32,
                    &mut read_len,
                );
                if hr < 0 {
                    return Err(DriverError::PrintTicketError(format!(
                        "IStream::Read failed with HRESULT 0x{:08X}",
                        hr as u32
                    )));
                }
                if read_len == 0 {
                    break;
                }
                let take = (read_len as usize).min(chunk.len());
                if out.len().saturating_add(take) > MAX_TICKET_STREAM_BYTES {
                    return Err(DriverError::PrintTicketError(
                        "PrintTicket stream exceeded 8 MiB safety cap".into(),
                    ));
                }
                out.extend_from_slice(&chunk[..take]);
            }
            Ok(out)
        }
    }

    pub(crate) fn merge_and_read_ticket_xml_impl(
        printer_name: &str,
        base_xml: &[u8],
        delta_xml: &[u8],
    ) -> Result<String, DriverError> {
        with_ticket_provider_lock(|| {
            let api = prntvpt_api()?;
            let wide_printer = to_wide(printer_name);
            let mut provider: HPTPROVIDER = 0;
            let open_hr = unsafe { (api.open_provider)(wide_printer.as_ptr(), 1, &mut provider) };
            if open_hr < 0 || provider == 0 {
                return Err(DriverError::PrintTicketError(format!(
                    "PTOpenProvider failed with HRESULT 0x{:08X}",
                    open_hr as u32
                )));
            }
            let _provider_guard = ProviderGuard {
                provider,
                close_fn: api.close_provider,
            };

            let p_base = unsafe { (api.create_mem_stream)(base_xml.as_ptr(), base_xml.len() as u32) };
            let _base_guard = ComStreamGuard(p_base);
            if p_base.is_null() {
                return Err(DriverError::PrintTicketError(
                    "Failed to create base PrintTicket stream".into(),
                ));
            }

            let p_delta =
                unsafe { (api.create_mem_stream)(delta_xml.as_ptr(), delta_xml.len() as u32) };
            let _delta_guard = ComStreamGuard(p_delta);
            if p_delta.is_null() {
                return Err(DriverError::PrintTicketError(
                    "Failed to create delta PrintTicket stream".into(),
                ));
            }

            let p_result = unsafe { (api.create_mem_stream)(null(), 0) };
            let _result_guard = ComStreamGuard(p_result);
            if p_result.is_null() {
                return Err(DriverError::PrintTicketError(
                    "Failed to create result PrintTicket stream".into(),
                ));
            }

            let mut err_bstr: *mut u16 = null_mut();
            let merge_hr = unsafe {
                (api.merge_ticket)(
                    provider,
                    p_base,
                    p_delta,
                    K_PT_JOB_SCOPE,
                    p_result,
                    &mut err_bstr,
                )
            };
            let _bstr_guard = BstrGuard(err_bstr);

            if merge_hr < 0 {
                return Err(DriverError::PrintTicketError(format!(
                    "PTMergeAndValidatePrintTicket failed with HRESULT 0x{:08X}",
                    merge_hr as u32
                )));
            }

            let merged_bytes = read_com_stream_bytes(p_result)?;
            if merged_bytes.is_empty() {
                return String::from_utf8(delta_xml.to_vec())
                    .map_err(|e| DriverError::PrintTicketError(e.to_string()));
            }
            String::from_utf8(merged_bytes).map_err(|e| DriverError::PrintTicketError(e.to_string()))
        })
    }

    pub(super) struct DriverCapsSnapshot {
        pub(super) supports_duplex: Option<bool>,
        pub(super) supports_color: Option<bool>,
        pub(super) papers: Vec<(u16, PointI32)>,
    }

    fn query_driver_caps_snapshot(wide_printer: &[u16]) -> DriverCapsSnapshot {
        let duplex_raw = unsafe {
            DeviceCapabilitiesW(wide_printer.as_ptr(), null(), DC_DUPLEX, null_mut(), null())
        };
        let color_raw = unsafe {
            DeviceCapabilitiesW(
                wide_printer.as_ptr(),
                null(),
                DC_COLORDEVICE,
                null_mut(),
                null(),
            )
        };
        let supports_duplex = (duplex_raw >= 0).then_some(duplex_raw == 1);
        let supports_color = (color_raw >= 0).then_some(color_raw == 1);

        let count_ids = unsafe {
            DeviceCapabilitiesW(wide_printer.as_ptr(), null(), DC_PAPERS, null_mut(), null())
        };
        let count_sizes = unsafe {
            DeviceCapabilitiesW(
                wide_printer.as_ptr(),
                null(),
                DC_PAPERSIZE,
                null_mut(),
                null(),
            )
        };
        let papers = if count_ids > 0 && count_ids == count_sizes && count_ids <= 2048 {
            let n = count_ids as usize;
            let mut ids = vec![0u16; n];
            let mut sizes = vec![PointI32::default(); n];
            let got_ids = unsafe {
                DeviceCapabilitiesW(
                    wide_printer.as_ptr(),
                    null(),
                    DC_PAPERS,
                    ids.as_mut_ptr().cast(),
                    null(),
                )
            };
            let got_sizes = unsafe {
                DeviceCapabilitiesW(
                    wide_printer.as_ptr(),
                    null(),
                    DC_PAPERSIZE,
                    sizes.as_mut_ptr().cast(),
                    null(),
                )
            };
            if got_ids == count_ids && got_sizes == count_sizes {
                ids.into_iter().zip(sizes).collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        DriverCapsSnapshot {
            supports_duplex,
            supports_color,
            papers,
        }
    }

    pub(super) fn match_gdi_paper_id(
        papers: &[(u16, PointI32)],
        target_um: (u32, u32),
    ) -> Option<u16> {
        let w_tmm = ((target_um.0.min(target_um.1) + 50) / 100) as i32;
        let h_tmm = ((target_um.0.max(target_um.1) + 50) / 100) as i32;
        papers
            .iter()
            .filter_map(|&(id, pt)| {
                if id == 0 || pt.x <= 0 || pt.y <= 0 {
                    return None;
                }
                let (pw, ph) = (pt.x.min(pt.y), pt.x.max(pt.y));
                let dw = (pw - w_tmm).abs();
                let dh = (ph - h_tmm).abs();
                (dw <= PAPER_TOLERANCE_TMM && dh <= PAPER_TOLERANCE_TMM).then_some((dw + dh, id))
            })
            .min_by_key(|&(dist, _)| dist)
            .map(|(_, id)| id)
    }

    pub(super) fn apply_devmode_core_fields(
        buf_u64: &mut [u64],
        job: &Job,
        target: &JobSheetTarget,
        caps: &DriverCapsSnapshot,
        effective_duplex: Option<Duplex>,
    ) {
        let raw = buf_u64.as_mut_ptr() as *mut u8;
        unsafe {
            let dm_fields_ptr = raw.add(72) as *mut u32;
            let dm_orientation_ptr = raw.add(76) as *mut i16;
            let dm_copies_ptr = raw.add(86) as *mut i16;
            let dm_color_ptr = raw.add(92) as *mut i16;
            let dm_duplex_ptr = raw.add(94) as *mut i16;
            let dm_collate_ptr = raw.add(100) as *mut i16;

            match target.color_intent {
                ColorIntent::ExplicitColor => {
                    *dm_color_ptr = if caps.supports_color == Some(false) {
                        DMCOLOR_MONOCHROME
                    } else {
                        DMCOLOR_COLOR
                    };
                    *dm_fields_ptr |= DM_COLOR;
                }
                ColorIntent::ExplicitMonochrome => {
                    *dm_color_ptr = DMCOLOR_MONOCHROME;
                    *dm_fields_ptr |= DM_COLOR;
                }
                ColorIntent::DriverDefault => {}
            }

            if let Some(duplex) = effective_duplex {
                *dm_duplex_ptr = match duplex {
                    Duplex::Off => DMDUP_SIMPLEX,
                    Duplex::LongEdge => DMDUP_VERTICAL,
                    Duplex::ShortEdge => DMDUP_HORIZONTAL,
                };
                *dm_fields_ptr |= DM_DUPLEX;
            }

            *dm_orientation_ptr = if target.landscape {
                DMORIENT_LANDSCAPE
            } else {
                DMORIENT_PORTRAIT
            };
            *dm_fields_ptr |= DM_ORIENTATION;

            let copies = job.copies.clamp(1, 999) as i16;
            *dm_copies_ptr = copies;
            *dm_fields_ptr |= DM_COPIES;
            *dm_collate_ptr = if job.collate {
                DMCOLLATE_TRUE
            } else {
                DMCOLLATE_FALSE
            };
            *dm_fields_ptr |= DM_COLLATE;
        }
    }

    pub(super) fn apply_devmode_paper_fields(
        buf_u64: &mut [u64],
        alloc_bytes: usize,
        papers: &[(u16, PointI32)],
        target_um: (u32, u32),
    ) {
        let raw = buf_u64.as_mut_ptr() as *mut u8;
        unsafe {
            let dm_size = *(raw.add(68) as *const u16) as usize;
            let dm_fields_ptr = raw.add(72) as *mut u32;
            let dm_paper_size_ptr = raw.add(78) as *mut i16;
            let dm_paper_len_ptr = raw.add(80) as *mut i16;
            let dm_paper_wid_ptr = raw.add(82) as *mut i16;

            if let Some(paper_id) = match_gdi_paper_id(papers, target_um) {
                *dm_paper_size_ptr = paper_id as i16;
                *dm_fields_ptr =
                    (*dm_fields_ptr | DM_PAPERSIZE) & !(DM_PAPERLENGTH | DM_PAPERWIDTH | DM_FORMNAME);
            } else {
                let min_tmm = ((target_um.0.min(target_um.1) + 50) / 100).clamp(1, 32_767) as i16;
                let max_tmm = ((target_um.0.max(target_um.1) + 50) / 100).clamp(1, 32_767) as i16;
                *dm_paper_size_ptr = DMPAPER_USER;
                *dm_paper_wid_ptr = min_tmm;
                *dm_paper_len_ptr = max_tmm;
                *dm_fields_ptr =
                    (*dm_fields_ptr | DM_PAPERSIZE | DM_PAPERWIDTH | DM_PAPERLENGTH) & !DM_FORMNAME;
            }

            if dm_size >= 166 && alloc_bytes >= 166 {
                *(raw.add(102) as *mut u16) = 0;
            }
        }
    }

    fn devmode_byte_len(buf_u64: &[u64], alloc_bytes: usize) -> usize {
        let raw = buf_u64.as_ptr() as *const u8;
        let (dm_size, dm_extra) = unsafe {
            let s = *(raw.add(68) as *const u16) as usize;
            let e = *(raw.add(70) as *const u16) as usize;
            (s, e)
        };
        dm_size.saturating_add(dm_extra).min(alloc_bytes)
    }

    fn query_devmode_paper_um(wide_printer: &[u16], devmode_ptr: *const c_void) -> Option<(u32, u32)> {
        let driver_w = to_wide("WINSPOOL");
        let hdc = unsafe { CreateDCW(driver_w.as_ptr(), wide_printer.as_ptr(), null(), devmode_ptr) };
        if hdc.is_null() {
            return None;
        }
        let _dc_guard = DcGuard(hdc);
        let dpi_x = (unsafe { GetDeviceCaps(hdc, LOGPIXELSX) }).max(1) as u64;
        let dpi_y = (unsafe { GetDeviceCaps(hdc, LOGPIXELSY) }).max(1) as u64;
        let phys_w = (unsafe { GetDeviceCaps(hdc, PHYSICALWIDTH) }).max(0) as u64;
        let phys_h = (unsafe { GetDeviceCaps(hdc, PHYSICALHEIGHT) }).max(0) as u64;
        let horz_w = (unsafe { GetDeviceCaps(hdc, HORZRES) }).max(0) as u64;
        let vert_h = (unsafe { GetDeviceCaps(hdc, VERTRES) }).max(0) as u64;
        let px_w = phys_w.max(horz_w);
        let px_h = phys_h.max(vert_h);
        if px_w == 0 || px_h == 0 {
            return None;
        }
        let w_um = ((px_w * 25_400 + dpi_x / 2) / dpi_x) as u32;
        let h_um = ((px_h * 25_400 + dpi_y / 2) / dpi_y) as u32;
        (w_um > 0 && h_um > 0).then_some((w_um, h_um))
    }

    fn convert_devmode_to_ticket(
        wide_printer: &[u16],
        devmode_bytes: &[u8],
    ) -> Option<Vec<u8>> {
        let api = prntvpt_api().ok()?;
        let mut provider: HPTPROVIDER = 0;
        let open_hr = unsafe { (api.open_provider)(wide_printer.as_ptr(), 1, &mut provider) };
        if open_hr < 0 || provider == 0 {
            return None;
        }
        let _provider_guard = ProviderGuard {
            provider,
            close_fn: api.close_provider,
        };
        let p_stream = unsafe { (api.create_mem_stream)(null(), 0) };
        let _stream_guard = ComStreamGuard(p_stream);
        if p_stream.is_null() {
            return None;
        }
        let hr = unsafe {
            (api.convert_devmode_to_ticket)(
                provider,
                devmode_bytes.len() as u32,
                devmode_bytes.as_ptr().cast(),
                K_PT_JOB_SCOPE,
                p_stream,
            )
        };
        if hr < 0 {
            return None;
        }
        read_com_stream_bytes(p_stream).ok().filter(|b| !b.is_empty())
    }

    fn resolve_target_printer_name(requested: Option<&str>) -> Option<String> {
        requested.map(str::to_string).or_else(|| {
            pdfcraft_print::spool::printer_queues()
                .into_iter()
                .find(|p| p.default)
                .map(|p| p.name)
        })
    }

    fn build_normalized_seed(
        printer_name: &str,
        job: &Job,
        target: &JobSheetTarget,
    ) -> Option<WindowsNormalizedSeed> {
        with_ticket_provider_lock(|| {
            let wide_name = to_wide(printer_name);
            let mut h_printer: HANDLE = null_mut();
            let ok = unsafe { OpenPrinterW(wide_name.as_ptr(), &mut h_printer, null()) };
            if ok == 0 || h_printer.is_null() {
                return None;
            }
            let _printer_guard = PrinterGuard(h_printer);

            let needed = unsafe {
                DocumentPropertiesW(null_mut(), h_printer, wide_name.as_ptr(), null_mut(), null(), 0)
            };
            if needed <= 0 {
                return None;
            }
            let alloc_bytes = ((needed as usize).max(220))
                .saturating_mul(2)
                .saturating_add(8192);
            let u64_count = alloc_bytes.div_ceil(8);
            let mut base_u64 = vec![0u64; u64_count];
            let res = unsafe {
                DocumentPropertiesW(
                    null_mut(),
                    h_printer,
                    wide_name.as_ptr(),
                    base_u64.as_mut_ptr().cast(),
                    null(),
                    DM_OUT_BUFFER,
                )
            };
            if res != IDOK {
                return None;
            }

            let caps = query_driver_caps_snapshot(&wide_name);
            let effective_duplex = match job.duplex {
                Some(Duplex::LongEdge | Duplex::ShortEdge)
                    if caps.supports_duplex == Some(false) =>
                {
                    Some(Duplex::Off)
                }
                other => other,
            };

            let mut candidate_u64 = base_u64.clone();
            apply_devmode_core_fields(&mut candidate_u64, job, target, &caps, effective_duplex);
            apply_devmode_paper_fields(
                &mut candidate_u64,
                alloc_bytes,
                &caps.papers,
                target.target_paper_um,
            );

            let mut merged_u64 = vec![0u64; u64_count];
            let mut adapted_shrink_to_fit = false;
            let merge_ok = unsafe {
                DocumentPropertiesW(
                    null_mut(),
                    h_printer,
                    wide_name.as_ptr(),
                    merged_u64.as_mut_ptr().cast(),
                    candidate_u64.as_ptr().cast(),
                    DM_IN_BUFFER | DM_OUT_BUFFER,
                )
            };
            if merge_ok != IDOK {
                // Retry keeping core specifications while leaving fixed-tray paper untouched.
                let mut fallback_u64 = base_u64;
                apply_devmode_core_fields(&mut fallback_u64, job, target, &caps, effective_duplex);
                let retry_ok = unsafe {
                    DocumentPropertiesW(
                        null_mut(),
                        h_printer,
                        wide_name.as_ptr(),
                        merged_u64.as_mut_ptr().cast(),
                        fallback_u64.as_ptr().cast(),
                        DM_IN_BUFFER | DM_OUT_BUFFER,
                    )
                };
                if retry_ok != IDOK {
                    return None;
                }
                adapted_shrink_to_fit = true;
            }

            let total_len = devmode_byte_len(&merged_u64, alloc_bytes);
            let raw_bytes = unsafe {
                std::slice::from_raw_parts(merged_u64.as_ptr() as *const u8, total_len)
            }
            .to_vec();

            let retained_paper_um = query_devmode_paper_um(&wide_name, merged_u64.as_ptr().cast());
            if let Some(actual_um) = retained_paper_um
                && !paper_matches_retained(actual_um, target.target_paper_um)
            {
                adapted_shrink_to_fit = true;
            }

            let base_ticket_xml = convert_devmode_to_ticket(&wide_name, &raw_bytes);

            Some(WindowsNormalizedSeed {
                base_ticket_xml,
                devmode_bytes: raw_bytes,
                effective_duplex,
                supports_duplex: caps.supports_duplex,
                supports_color: caps.supports_color,
                retained_paper_um,
                adapted_shrink_to_fit,
            })
        })
    }

    pub(super) fn submit_unified_windows(
        pdf_bytes: &[u8],
        job: &Job,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<String, PrintError> {
        let target = inspect_job_target(pdf_bytes, job)?;
        let seed = resolve_target_printer_name(job.printer.as_deref())
            .and_then(|name| build_normalized_seed(&name, job, &target));
        submit_with_seed(pdf_bytes, job, seed.as_ref(), progress)
    }

    pub(super) fn capture_driver_template_windows(
        printer: &str,
    ) -> Result<Option<CapturedDriverTemplate>, PrintError> {
        let wide_name = to_wide(printer);
        let mut h_printer: HANDLE = null_mut();
        let ok = unsafe { OpenPrinterW(wide_name.as_ptr(), &mut h_printer, null()) };
        if ok == 0 || h_printer.is_null() {
            return Err(PrintError::Spool(format!(
                "the printer is not available: {printer}"
            )));
        }
        let printer_guard = PrinterGuard(h_printer);

        let needed = unsafe {
            DocumentPropertiesW(null_mut(), h_printer, wide_name.as_ptr(), null_mut(), null(), 0)
        };
        if needed <= 0 {
            drop(printer_guard);
            open_printer_preferences(printer)?;
            return Ok(None);
        }

        let alloc_bytes = ((needed as usize).max(220))
            .saturating_mul(2)
            .saturating_add(8192);
        let u64_count = alloc_bytes.div_ceil(8);
        let mut base_u64 = vec![0u64; u64_count];
        let base_ok = unsafe {
            DocumentPropertiesW(
                null_mut(),
                h_printer,
                wide_name.as_ptr(),
                base_u64.as_mut_ptr().cast(),
                null(),
                DM_OUT_BUFFER,
            )
        };

        let hwnd = unsafe { GetActiveWindow() };
        let mut out_u64 = vec![0u64; u64_count];
        let prompt_mode = if base_ok == IDOK {
            DM_IN_BUFFER | DM_IN_PROMPT | DM_OUT_BUFFER
        } else {
            DM_IN_PROMPT | DM_OUT_BUFFER
        };
        let in_ptr = if base_ok == IDOK {
            base_u64.as_ptr().cast()
        } else {
            null()
        };
        let prompt_res = unsafe {
            DocumentPropertiesW(
                hwnd,
                h_printer,
                wide_name.as_ptr(),
                out_u64.as_mut_ptr().cast(),
                in_ptr,
                prompt_mode,
            )
        };
        drop(printer_guard);

        if prompt_res < 0 {
            open_printer_preferences(printer)?;
            return Ok(None);
        }
        if prompt_res != IDOK {
            // User clicked Cancel in the DocumentPropertiesW dialog.
            return Ok(None);
        }

        let total_len = devmode_byte_len(&out_u64, alloc_bytes);
        let dm_bytes =
            unsafe { std::slice::from_raw_parts(out_u64.as_ptr() as *const u8, total_len) };
        let phys_pts = query_devmode_paper_um(&wide_name, out_u64.as_ptr().cast()).map(|(w, h)| {
            (
                pdfcraft_print::spool::windows_unified::um_to_pts(w),
                pdfcraft_print::spool::windows_unified::um_to_pts(h),
            )
        });
        let known_papers = printer_papers(printer);
        Ok(parse_devmode_template_bytes(
            dm_bytes,
            phys_pts,
            &known_papers,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_print_bridge_is_idempotent() {
        install_print_bridge();
        install_print_bridge();
    }

    #[cfg(windows)]
    #[test]
    fn devmode_normalization_enforces_explicit_color_monochrome_and_paper_while_preserving_extra() {
        use imp::{
            DriverCapsSnapshot, PointI32, apply_devmode_core_fields, apply_devmode_paper_fields,
        };
        use pdfcraft_print::spool::windows_unified::{
            ColorIntent, JobSheetTarget, parse_devmode_template_bytes,
        };

        let alloc_bytes = 4224usize;
        let mut buf_u64 = vec![0u64; alloc_bytes / 8];
        let raw = buf_u64.as_mut_ptr() as *mut u8;
        unsafe {
            *(raw.add(68) as *mut u16) = 220; // dmSize
            *(raw.add(70) as *mut u16) = 4000; // dmDriverExtra (>3.7 KB vendor data)
            // Start with stale queue defaults: DM_PAPERSIZE | DM_FORMNAME | DM_COLOR (Monochrome = 1)
            *(raw.add(72) as *mut u32) = 0x0000_0002 | 0x0001_0000 | 0x0000_0800;
            *(raw.add(78) as *mut i16) = 1; // DMPAPER_LETTER
            *(raw.add(92) as *mut i16) = 1; // Stale DMCOLOR_MONOCHROME queue default!
            *(raw.add(102) as *mut u16) = b'L' as u16; // Stale dmFormName = "L..."
            for i in 220..4220 {
                *raw.add(i) = ((i * 13) & 0xFF) as u8;
            }
        }

        let caps = DriverCapsSnapshot {
            supports_duplex: Some(true),
            supports_color: Some(true),
            papers: vec![
                (1, PointI32 { x: 2159, y: 2794 }), // Letter
                (9, PointI32 { x: 2100, y: 2970 }), // A4
            ],
        };
        let job = Job {
            copies: 3,
            collate: true,
            duplex: Some(Duplex::LongEdge),
            ..Job::default()
        };
        let target = JobSheetTarget {
            sheet_count: 4,
            first_sheet_pt: (595.28, 841.89),
            target_paper_um: (210_400, 296_700), // Within 1.0 mm of A4!
            landscape: false,
            color_intent: ColorIntent::ExplicitColor,
        };

        apply_devmode_core_fields(&mut buf_u64, &job, &target, &caps, job.duplex);
        apply_devmode_paper_fields(&mut buf_u64, alloc_bytes, &caps.papers, target.target_paper_um);

        let bytes = unsafe { std::slice::from_raw_parts(buf_u64.as_ptr() as *const u8, 4220) };
        let dm_fields = u32::from_le_bytes([bytes[72], bytes[73], bytes[74], bytes[75]]);
        let dm_paper = i16::from_le_bytes([bytes[78], bytes[79]]);
        let dm_color = i16::from_le_bytes([bytes[92], bytes[93]]);
        let dm_form0 = u16::from_le_bytes([bytes[102], bytes[103]]);

        assert_eq!(dm_paper, 9, "must snap to DMPAPER_A4 (9) within 1.0 mm");
        assert_eq!(dm_color, 2, "ExplicitColor must override stale Monochrome default to DMCOLOR_COLOR (2)");
        assert_eq!(dm_fields & 0x0001_0000, 0, "DM_FORMNAME must be cleared");
        assert_eq!(dm_form0, 0, "dmFormName[0] must be zeroed");

        let parsed = parse_devmode_template_bytes(bytes, Some((595.28, 841.89)), &[]).unwrap();
        assert_eq!(parsed.grayscale, Some(false));
        assert_eq!(parsed.duplex, Some(Duplex::LongEdge));
        assert_eq!(parsed.copies, Some(3));
        assert_eq!(parsed.collate, Some(true));

        // Verify custom paper (148.5 x 200.0 mm -> DMPAPER_USER = 256)
        apply_devmode_paper_fields(&mut buf_u64, alloc_bytes, &caps.papers, (148_500, 200_000));
        let bytes = unsafe { std::slice::from_raw_parts(buf_u64.as_ptr() as *const u8, 4220) };
        assert_eq!(i16::from_le_bytes([bytes[78], bytes[79]]), 256);
        assert_eq!(i16::from_le_bytes([bytes[82], bytes[83]]), 1485);
        assert_eq!(i16::from_le_bytes([bytes[80], bytes[81]]), 2000);

        // Vendor dmDriverExtra canary bytes remain 100% intact
        for (idx, &b) in bytes[220..4220].iter().enumerate() {
            let offset = 220 + idx;
            assert_eq!(b, ((offset * 13) & 0xFF) as u8);
        }
    }

    #[cfg(windows)]
    #[test]
    fn merge_print_ticket_reads_validated_com_stream_on_installed_queue() {
        let queues = pdfcraft_print::spool::printer_queues();
        let Some(first) = queues.first() else {
            return;
        };
        let adapter = crate::win32_v4::Win32V4Adapter::new();
        let settings = PrintJobSettings {
            printer_name: first.name.clone(),
            color_mode: ColorMode::Color,
            duplex_mode: DuplexBinding::Simplex,
            landscape: true,
            copies: 2,
            ..PrintJobSettings::default()
        };
        if let Ok(merged_xml) = adapter.merge_print_ticket(&first.name, &settings) {
            assert!(merged_xml.contains("PrintTicket"));
            assert!(merged_xml.contains("Landscape"));
        }
    }
}
