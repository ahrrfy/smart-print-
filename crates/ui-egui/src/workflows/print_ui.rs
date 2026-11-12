//! The Print dialog (Acrobat's File ▸ Print, execution plan M10.5): printer, copies, grayscale;
//! pages to print (all, current, range with labels, the selected pages; odd/even, reverse); page sizing & handling
//! (Size, Poster, Multiple, Booklet); orientation; comments & forms; and a live preview of the
//! sheets. Printing sends the print-ready PDF to the system spooler; "Save as PDF" writes it.

#[cfg(not(target_arch = "wasm32"))]
use crate::file_filter::AddFilterAnyCase as _;
use egui::Color32;
use pdfcraft_engine::print::{self, Binding, BookletSubset, Content, Layout, Orientation, PageOrder, SizeMode, Subset, spool};

use crate::theme::{self, Tokens};
use crate::{PdfCraftApp, widgets};

mod body;
mod draft;
mod preview_pane;
mod printers_pane;
mod setups;
mod specs_pane;

pub(crate) use body::{body, dialog_width, failover_modal_window};
#[allow(unused_imports)]
pub use draft::{
    CUSTOM_SCALE, ColorIntent, Handling, IMAGE_DPI, PrintDraft, PrinterBroadcastSummary,
    QUICK_STANDARD_PAPERS, Which, points_to_um, um_to_points,
};
#[allow(unused_imports)]
pub use setups::{BroadcastSetup, PrinterSetup};
pub(crate) use setups::{
    broadcast_from_json, broadcast_to_json, from_json as setups_from_json, to_json as setups_to_json,
};

/// The preview pane, and the paper inside it (12 pt inset on each side).
const PREVIEW_W: f32 = 320.0;
const PREVIEW_H: f32 = 380.0;
const PREVIEW_INSET: f32 = 12.0;
pub(crate) const PREVIEW_BOX: (f32, f32) = (PREVIEW_W - 2.0 * PREVIEW_INSET, PREVIEW_H - 2.0 * PREVIEW_INSET);
/// Label printing reads the printed area of at most this many pages (each is a quick render on
/// the UI thread); further pages print whole.
pub(crate) const LABEL_CROP_PAGES: usize = 50;
/// Two-sided reads running at once. A driver Windows can't reach holds its read for about 48 s,
/// so picking printer after printer could otherwise start a thread for each; past this, the
/// printer picked is asked once a read finishes.
#[cfg(not(target_arch = "wasm32"))]
const TWO_SIDED_READS: usize = 4;

/// Lists the printers off the UI thread (the desktop app sets [`PdfCraftApp::printer_source`]):
/// CUPS' driverless discovery can wait on the network. Without a source the dialog reads them as
/// it opens, so tests stay deterministic.
pub type PrinterSource = std::sync::Arc<dyn Fn() -> Vec<spool::Printer> + Send + Sync>;

/// Where a printer's driver options come from: the spooler's (`spool::printer_options_in`, in the
/// UI language) unless a test stands in. Called off the GUI thread.
pub type OptionsSource = std::sync::Arc<dyn Fn(&str) -> Vec<spool::PrinterOption> + Send + Sync>;

/// Where a printer's configured default paper comes from: the spooler's DEVMODE / driver settings
/// unless a test stands in.
pub type DefaultPaperSource = std::sync::Arc<dyn Fn(&str) -> Option<(String, (f64, f64))> + Send + Sync>;

/// Where a printer's supported paper sizes come from: the spooler driver capabilities
/// unless a test stands in.
pub type PrinterPapersSource = std::sync::Arc<dyn Fn(&str) -> Vec<spool::Paper> + Send + Sync>;

/// What takes a print job in place of the system spooler: a test's stand-in, which gets the
/// print-ready PDF and the job and answers as the spooler would. Called off the GUI thread.
pub type Spooler = std::sync::Arc<dyn Fn(&[u8], &spool::Job) -> Result<String, String> + Send + Sync>;

/// The system spooler's printers, as the desktop app's [`PrinterSource`]: its queues only, with
/// no driver asked anything, so the list never waits on a printer (#2192).
pub fn system_printers() -> PrinterSource {
    #[cfg(not(target_arch = "wasm32"))]
    pdfcraft_platform::install_print_bridge();
    static SYSTEM_PRINTERS: std::sync::OnceLock<PrinterSource> = std::sync::OnceLock::new();
    SYSTEM_PRINTERS
        .get_or_init(|| std::sync::Arc::new(spool::printer_queues))
        .clone()
}

/// Reads whether one printer's driver takes two-sided settings, off the UI thread, for the
/// printer picked (the desktop app sets [`PdfCraftApp::two_sided_source`]). The printer list
/// leaves it unknown: reading a driver Windows can't reach takes it about 48 s, so the list
/// doesn't wait for every printer's (#2192).
pub type TwoSidedSource = std::sync::Arc<dyn Fn(&str) -> Option<bool> + Send + Sync>;

/// The system spooler's answer, as the desktop app's [`TwoSidedSource`].
pub fn system_two_sided() -> TwoSidedSource {
    #[cfg(not(target_arch = "wasm32"))]
    pdfcraft_platform::install_print_bridge();
    static SYSTEM_TWO_SIDED: std::sync::OnceLock<TwoSidedSource> = std::sync::OnceLock::new();
    SYSTEM_TWO_SIDED
        .get_or_init(|| std::sync::Arc::new(spool::two_sided))
        .clone()
}

#[cfg(not(target_arch = "wasm32"))]
type PaperInfo = (Vec<spool::Paper>, Option<spool::Paper>);
#[cfg(not(target_arch = "wasm32"))]
type PendingPaperInfo = (String, std::sync::mpsc::Receiver<PaperInfo>);

/// The Print dialog's work in the background: the printer list being read, and the jobs being
/// spooled, oldest first.
#[derive(Default)]
pub(crate) struct PrintTasks {
    #[cfg(not(target_arch = "wasm32"))]
    printers: Option<std::sync::mpsc::Receiver<Vec<spool::Printer>>>,
    /// Two-sided support being read, by printer.
    #[cfg(not(target_arch = "wasm32"))]
    two_sided: Vec<(String, std::sync::mpsc::Receiver<Option<bool>>)>,
    #[cfg(not(target_arch = "wasm32"))]
    jobs: Vec<PrintJob>,
    /// The progress card is showing a print job (it is shared with other long jobs).
    #[cfg(not(target_arch = "wasm32"))]
    showing_progress: bool,
    /// The driver options being read, per printer (`read_options_in_background`).
    #[cfg(not(target_arch = "wasm32"))]
    options: Vec<(String, std::sync::mpsc::Receiver<Vec<spool::PrinterOption>>)>,
    /// The papers and default paper being read in the background, per printer (`read_papers_in_background`).
    #[cfg(not(target_arch = "wasm32"))]
    papers: Vec<PendingPaperInfo>,
    /// Cached papers and default paper per printer so switching printers avoids re-querying.
    #[cfg(not(target_arch = "wasm32"))]
    papers_cache: std::collections::HashMap<String, PaperInfo>,
    /// Milestone 4: active multi-printer coordinator handles.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) coordinator_handles: Vec<pdfcraft_engine::print_coordinator::MultiJobHandle>,
    /// Milestone 4: live stream health telemetry per printer for status badge rendering.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) stream_telemetry: std::collections::BTreeMap<String, pdfcraft_engine::print_coordinator::PrinterStreamTelemetry>,
    /// Hardware capabilities (margins, printable area, DPI, duplex, color) read in background per printer.
    #[cfg(not(target_arch = "wasm32"))]
    caps: Vec<(
        String,
        std::sync::mpsc::Receiver<Option<pdfcraft_platform::diagnostics::PrinterCapabilitiesTelemetry>>,
    )>,
    /// The browser print being drawn, a sheet a frame (`web_print`).
    #[cfg(target_arch = "wasm32")]
    web_job: Option<crate::web_print::WebPrintJob>,
    /// What went wrong after the browser print page was handed over, with the print-ready PDF
    /// to download instead.
    #[cfg(target_arch = "wasm32")]
    web_failed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// The last browser print's PDF and name, for that fallback.
    #[cfg(target_arch = "wasm32")]
    web_last: Option<(String, std::sync::Arc<Vec<u8>>)>,
}

/// The printers to offer when no [`PrinterSource`] is set: the spooler's, or in a browser its
/// print dialog.
fn listed_printers() -> Vec<spool::Printer> {
    #[cfg(target_arch = "wasm32")]
    {
        vec![spool::Printer { name: crate::web_print::BROWSER_PRINTER.to_string(), default: true, two_sided: None, description: String::new() }]
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        pdfcraft_platform::install_print_bridge();
        spool::printer_queues()
    }
}

/// The first list is still being read and the user hasn't chosen yet (#160): no printer is
/// selected only because none is known yet, so the dialog says it is looking rather than
/// offering Save as PDF, and its button waits. Printing then would have saved a PDF.
fn still_looking(d: &PrintDraft) -> bool {
    if d.is_broadcast_active() && !d.selected_printers.is_empty() {
        return false;
    }
    d.printers_loading && d.printer.is_none() && !d.printer_picked
}

/// A printer's name as the dialog shows it.
fn printer_label(name: &str) -> String {
    if name == crate::web_print::BROWSER_PRINTER { crate::web_print::browser_printer_label().to_string() } else { name.to_string() }
}

/// A job being spooled: its printer, the sheets sent so far of how many, and its worker's news.
#[cfg(not(target_arch = "wasm32"))]
struct PrintJob {
    printer: String,
    sheets: Option<(usize, usize)>,
    news: std::sync::mpsc::Receiver<JobNews>,
}

#[cfg(not(target_arch = "wasm32"))]
enum JobNews {
    /// `(sent, sheets)`: the spooler has this many of the job's sheets.
    Sent(usize, usize),
    /// The spooler's answer.
    Done(Result<String, String>),
}

/// The printer the dialog preselects: the one printed to last while it is listed (#2001), else
/// the system default, else the first.
fn default_printer(d: &PrintDraft, printers: &[spool::Printer]) -> Option<String> {
    d.last_printer(printers).or_else(|| printers.iter().find(|p| p.default)).or(printers.first()).map(|p| p.name.clone())
}

/// Whether `printer` can still be chosen from `printers`: Save as PDF (`None`) always can.
fn offered(printer: Option<&str>, printers: &[spool::Printer]) -> bool {
    printer.is_none_or(|name| printers.iter().any(|p| p.name == name))
}

/// Asks for a frame when a worker thread ends, also when it panics: [`PdfCraftApp::poll_print`]
/// then sees the answer, or the disconnected channel of a worker that died, without waiting
/// for some other event to draw (#2108 review). Declared before the worker's sender, so the
/// sender is dropped first and the frame finds the channel closed.
#[cfg(not(target_arch = "wasm32"))]
struct WakeOnExit(Option<egui::Context>);

#[cfg(not(target_arch = "wasm32"))]
impl Drop for WakeOnExit {
    fn drop(&mut self) {
        if let Some(ctx) = &self.0 {
            ctx.request_repaint();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn make_trace(
    level: pdfcraft_platform::diagnostics::TraceLevel,
    category: pdfcraft_platform::diagnostics::TraceCategory,
    printer_name: Option<String>,
    message: String,
    error_code: Option<String>,
) -> pdfcraft_platform::diagnostics::TraceRecord {
    let duration = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    let timestamp = chrono::DateTime::from_timestamp(duration.as_secs() as i64, duration.subsec_nanos())
        .unwrap_or_default();
    pdfcraft_platform::diagnostics::TraceRecord {
        timestamp,
        level,
        category,
        printer_name,
        message,
        error_code,
        context: std::collections::BTreeMap::new(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn resolve_failover_target(
    draft: &PrintDraft,
    faulted_printer: &str,
    suggested: Option<String>,
    active_in_batch: &std::collections::BTreeSet<String>,
) -> String {
    if let Some(candidate) = suggested.filter(|name| {
        !name.is_empty()
            && name != faulted_printer
            && name != crate::web_print::BROWSER_PRINTER
            && !spool::is_virtual_save_printer(name)
            && !active_in_batch.contains(name)
    }) {
        return candidate;
    }
    draft
        .eligible_failover_printers(faulted_printer, active_in_batch)
        .into_iter()
        .next()
        .unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
fn coordinator_progress_summary(
    handle: &pdfcraft_engine::print_coordinator::MultiJobHandle,
) -> Option<(String, f32)> {
    let tel_map = handle.all_telemetry();
    if tel_map.is_empty() {
        return None;
    }
    let printer_count = tel_map.len();
    let mut done_sheets = 0usize;
    let mut total_sheets = 0usize;
    for tel in tel_map.values() {
        done_sheets = done_sheets.saturating_add(tel.ledger.committed_count());
        total_sheets = total_sheets.saturating_add(tel.total_sheets);
    }
    let fraction = if total_sheets > 0 {
        (done_sheets as f32 / total_sheets as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let label = format!(
        "Cluster printing: {done_sheets}/{total_sheets} sheets across {printer_count} printers…"
    );
    Some((label, fraction))
}

impl PdfCraftApp {
    /// Open the Print dialog, preselecting the default printer and reading papers in the background.
    pub fn open_print(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        pdfcraft_platform::install_print_bridge();
        let Some((i, _)) = self.active_ids() else { return };
        let (printers, printers_loading) = match self.printer_source.clone() {
            // The list from the last time the dialog opened, until the fresh one arrives.
            #[cfg(not(target_arch = "wasm32"))]
            Some(source) => {
                self.list_printers_in_background(source);
                (self.print_draft.printers.clone(), true)
            }
            _ => (listed_printers(), false),
        };
        let current = self.views[i].current;
        let selected: Vec<usize> = self.views[i].selected.iter().copied().collect();
        let mut keep = std::mem::take(&mut self.print_draft);
        // Leaving Label gives back the paper it replaced (#3564), before the layout resets below.
        if keep.handling == Handling::Label {
            keep.set_handling(Handling::Size);
        }
        let default = default_printer(&keep, &printers);
        // The printer picked last time, Save as PDF included, while it is still listed (#3537).
        let picked = keep.printer_picked && offered(keep.printer.as_deref(), &printers);
        let printer = if picked { keep.printer.clone() } else { default };
        // A printer other than last time's brings back what it was last printed with (#2001);
        // the same one keeps this session's choices.
        let changed = printer != keep.printer;
        // Picked pages are what Print is for (Acrobat's "Selected pages"); without any, the whole
        // document.
        let which = if selected.is_empty() { Which::All } else { Which::Selected };
        // Copies, pages, the layout with its own options and orientation are this job's, and start
        // from the defaults each time the dialog opens: ISO 32000-2 Table 147 initialises them per
        // document when the print dialog opens (NumCopies, "typically 1"; PrintPageRange), unless
        // unified broadcast specs explicitly opt to carry across documents.
        let fresh = PrintDraft::default();
        let preserve_broadcast_job_fields = keep.is_broadcast_active()
            && keep.broadcast_setup.as_ref().is_some_and(|b| b.keep_specs_across_docs);
        let next_copies = if preserve_broadcast_job_fields { keep.copies } else { fresh.copies };
        let next_orientation = if preserve_broadcast_job_fields { keep.orientation } else { fresh.orientation };
        self.print_draft = PrintDraft {
            copies: next_copies,
            range: fresh.range,
            subset: fresh.subset,
            reverse: fresh.reverse,
            handling: fresh.handling,
            per_sheet: fresh.per_sheet,
            custom_grid: fresh.custom_grid,
            cols: fresh.cols,
            rows: fresh.rows,
            order: fresh.order,
            border: fresh.border,
            auto_rotate: fresh.auto_rotate,
            booklet_subset: fresh.booklet_subset,
            poster_scale: fresh.poster_scale,
            overlap: fresh.overlap,
            cut_marks: fresh.cut_marks,
            label_flip: fresh.label_flip,
            orientation: next_orientation,
            crops: Vec::new(),
            crop_pages: None,
            printers,
            printers_loading,
            printer_picked: picked,
            units: self.units,
            printer: printer.clone(),
            current_page: current,
            sheet: 0,
            selected,
            which,
            area: None,
            custom_new: None,
            unified_specs_edited: keep.is_broadcast_active() && keep.unified_specs_edited,
            ..keep
        };
        if self.print_draft.is_broadcast_active() {
            if !self.print_draft.unified_specs_edited
                && let Some(bs) = self.print_draft.broadcast_setup.clone()
            {
                self.print_draft.apply_broadcast_setup(&bs);
            }
        } else if changed {
            self.print_draft.apply_setup();
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.start_initial_print_queries(printer);
        self.dialog = Some(crate::Dialog::Print);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_initial_print_queries(&mut self, printer: Option<String>) {
        if let Some(p) = printer {
            self.read_papers_in_background(p.clone());
            self.print_draft.enqueue_caps_query(p);
        }
        if self.print_draft.multi_printer_mode {
            self.print_draft.enqueue_caps_for_selected();
        }
        self.poll_caps();
    }

    /// Label printing: find each printed page's printed area (a quick low-resolution render, as
    /// Fit Visible does) and which way its text runs. Only the pages being printed are read, at
    /// most [`LABEL_CROP_PAGES`] of them (the rest print whole); read again when they change.
    pub(crate) fn read_label_crops(&mut self, id: pdfcraft_engine::DocId) {
        let Some(doc) = self.session.get(id) else { return };
        let labels: Vec<String> = doc.info.pages.iter().map(|p| p.label.clone()).collect();
        let mut pages = self.print_draft.settings(doc.info.pages.len(), &labels).map(|s| s.pages).unwrap_or_default();
        pages.sort_unstable();
        pages.dedup();
        pages.truncate(LABEL_CROP_PAGES);
        if self.print_draft.crop_pages.as_ref() == Some(&pages) {
            return;
        }
        let up = self.session.print_text_up(id, &pages);
        let config = pdfcraft_render::RenderConfig { password: doc.password.as_deref().map(std::sync::Arc::from), ..Default::default() };
        let mut r = pdfcraft_render::PageRenderer::new(doc.bytes.clone(), config);
        self.print_draft.crops = doc
            .info
            .pages
            .iter()
            .enumerate()
            .map(|(page, p)| {
                let (w, h) = (f64::from(p.width), f64::from(p.height));
                if pages.binary_search(&page).is_err() || w <= 0.0 || h <= 0.0 {
                    return None;
                }
                let out = r.render(pdfcraft_render::RenderRequest {
                    page,
                    kind: pdfcraft_render::RequestKind::Pixels,
                    tile: None,
                    scale: (1000.0 / w.max(h) as f32).clamp(0.1, 4.0),
                    tag: 0,
                    preview: false,
                });
                let [u0, v0, u1, v1] = crate::zoom_snap::ink_bounds(out.width as usize, out.height as usize, &out.rgba)?;
                // Image rows run down from the top; display space runs up. Half a millimetre of slack.
                let pad = 1.5;
                let rect = [f64::from(u0) * w - pad, (1.0 - f64::from(v1)) * h - pad, f64::from(u1) * w + pad, (1.0 - f64::from(v0)) * h + pad];
                Some(print::Crop { rect, text_up: up.get(page).copied().unwrap_or(false) })
            })
            .collect();
        self.print_draft.crop_pages = Some(pages);
    }

    /// Read the printer list in the background off the GUI thread; `poll_print` picks it up.
    #[cfg(not(target_arch = "wasm32"))]
    fn list_printers_in_background(&mut self, source: PrinterSource) {
        // A list already being read is as fresh as a new one would be.
        if self.print_tasks.printers.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = WakeOnExit(self.ctx.clone());
        std::thread::spawn(move || {
            let _wake = wake;
            let tx = tx;
            // The receiver may be gone (the app quit): nothing to report to then.
            let _ = tx.send(source());
        });
        self.print_tasks.printers = Some(rx);
    }

    /// Read `printer`'s driver options off the GUI thread (#1818); `poll_print` picks them up.
    /// A read already under way for the same printer is left to finish.
    #[cfg(not(target_arch = "wasm32"))]
    fn read_options_in_background(&mut self, printer: String) {
        if self.print_tasks.options.iter().any(|(p, _)| *p == printer) {
            return;
        }
        let source: OptionsSource = self.options_source.clone().unwrap_or_else(|| {
            // The driver's own names in the UI language, as it is when Properties… is clicked.
            let lang = crate::i18n::current().code();
            std::sync::Arc::new(move |p: &str| spool::printer_options_in(p, lang))
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = WakeOnExit(self.ctx.clone());
        let name = printer.clone();
        let spawned = std::thread::Builder::new().name("printer options".into()).spawn(move || {
            let _wake = wake;
            let tx = tx;
            // The receiver may be gone (the app quit): nothing to report to then.
            let _ = tx.send(source(&name));
        });
        match spawned {
            Ok(_) => self.print_tasks.options.push((printer, rx)),
            // No thread to read them on: the printer offers nothing this time.
            Err(_) => self.receive_options(printer, Vec::new()),
        }
    }

    /// The options read for `printer` arrived, or the read died. A read that a later click
    /// superseded (another printer's is awaited) is dropped.
    #[cfg(not(target_arch = "wasm32"))]
    fn receive_options(&mut self, printer: String, options: Vec<spool::PrinterOption>) {
        let d = &mut self.print_draft;
        if d.driver_options_loading.as_deref() == Some(printer.as_str()) {
            d.driver_options_loading = None;
            d.driver_options = Some((printer, options));
        }
    }

    /// Read `printer`'s papers and default paper off the GUI thread (#5861); `poll_print` picks them up.
    /// A read already under way for the same printer is left to finish.
    #[cfg(not(target_arch = "wasm32"))]
    fn read_papers_in_background(&mut self, printer: String) {
        if let Some((papers, default_paper)) = self.print_tasks.papers_cache.get(&printer).cloned() {
            let d = &mut self.print_draft;
            if d.printer.as_deref() == Some(printer.as_str()) {
                d.apply_printer_papers(papers);
                if !d.paper_overridden && !d.is_broadcast_active() {
                    d.apply_default_paper(default_paper);
                }
            }
        }
        self.print_draft.papers_loading = Some(printer.clone());
        if self.print_tasks.papers.iter().any(|(p, _)| *p == printer) {
            return;
        }
        let papers_source = self.printer_papers_source.clone();
        let default_source = self.default_paper_source.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = WakeOnExit(self.ctx.clone());
        let name = printer.clone();
        let spawned = std::thread::Builder::new().name("printer papers".into()).spawn(move || {
            let _wake = wake;
            let tx = tx;
            let info = match (&papers_source, &default_source) {
                (None, None) => spool::printer_paper_info(&name),
                (papers_s, def_s) => {
                    let papers = match papers_s {
                        Some(s) => s(&name),
                        None => spool::printer_papers(&name),
                    };
                    let def = match def_s {
                        Some(s) => s(&name),
                        None => spool::printer_default_paper(&name),
                    };
                    (papers, def)
                }
            };
            let _ = tx.send(info);
        });
        match spawned {
            Ok(_) => self.print_tasks.papers.push((printer, rx)),
            Err(_) => {
                let d = &mut self.print_draft;
                if d.papers_loading.as_deref() == Some(printer.as_str()) {
                    d.papers_loading = None;
                }
            }
        }
    }

    /// The papers and default paper read for `printer` arrived, or the read died.
    #[cfg(not(target_arch = "wasm32"))]
    fn receive_papers(&mut self, printer: String, papers: Vec<spool::Paper>, default_paper: Option<spool::Paper>) {
        if !papers.is_empty() || default_paper.is_some() {
            self.print_tasks.papers_cache.insert(printer.clone(), (papers.clone(), default_paper.clone()));
        }
        let d = &mut self.print_draft;
        if d.papers_loading.as_deref() == Some(printer.as_str()) {
            d.papers_loading = None;
        }
        if d.printer.as_deref() == Some(printer.as_str()) {
            if !papers.is_empty() {
                d.apply_printer_papers(papers);
            }
            if !d.paper_overridden && !d.is_broadcast_active() && default_paper.is_some() {
                d.apply_default_paper(default_paper);
            }
        }
    }

    /// Returns `true` when any test stand-in source (`spooler`, `printer_papers_source`,
    /// `default_paper_source`, `options_source`, or a custom non-system `printer_source` /
    /// `two_sided_source`) is active, so mock printer names never hit live OS GDI `CreateDCW`
    /// calls during parallel test runs.
    #[cfg(not(target_arch = "wasm32"))]
    fn has_test_print_mock(&self) -> bool {
        let custom_printers = self
            .printer_source
            .as_ref()
            .is_some_and(|src| !std::sync::Arc::ptr_eq(src, &system_printers()));
        let custom_two_sided = self
            .two_sided_source
            .as_ref()
            .is_some_and(|src| !std::sync::Arc::ptr_eq(src, &system_two_sided()));
        self.spooler.is_some()
            || self.printer_papers_source.is_some()
            || self.default_paper_source.is_some()
            || self.options_source.is_some()
            || custom_printers
            || custom_two_sided
    }

    /// Read `printer`'s hardware capabilities (physical margins, printable area, DPI, duplex, color)
    /// off the GUI thread so live clipping-hazard preview and pre-flight telemetry stay responsive.
    #[cfg(not(target_arch = "wasm32"))]
    fn read_caps_in_background(&mut self, printer: String) {
        if printer == crate::web_print::BROWSER_PRINTER {
            return;
        }
        if let Some(cached) = self.print_draft.cached_capabilities.get(&printer).cloned() {
            self.print_draft.apply_capabilities_telemetry(printer, cached);
            return;
        }
        // Skip OS GDI DC queries when running with an injected test stand-in.
        if self.has_test_print_mock()
            || self.print_tasks.caps.iter().any(|(p, _)| *p == printer)
            || self.print_tasks.caps.len() >= TWO_SIDED_READS
        {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = WakeOnExit(self.ctx.clone());
        let name = printer.clone();
        let spawned = std::thread::Builder::new().name("printer caps".into()).spawn(move || {
            let _wake = wake;
            let tel = pdfcraft_platform::caps::query_gdi_capabilities(&name)
                .ok()
                .map(|c| pdfcraft_platform::diagnostics::PrinterCapabilitiesTelemetry::from_capabilities(&c));
            let _ = tx.send(tel);
        });
        if spawned.is_ok() {
            self.print_tasks.caps.push((printer, rx));
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn poll_caps(&mut self) {
        if let Some(printer) = self.print_draft.read_caps_for.take() {
            self.print_draft.enqueue_caps_query(printer);
        }
        let mut arrived = Vec::new();
        self.print_tasks.caps.retain(|(printer, rx)| match rx.try_recv() {
            Ok(Some(caps)) => {
                arrived.push((printer.clone(), caps));
                false
            }
            Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
        });
        for (printer, caps) in arrived {
            self.print_draft.apply_capabilities_telemetry(printer, caps);
        }
        let is_mock = self.has_test_print_mock();
        while self.print_tasks.caps.len() < TWO_SIDED_READS {
            let Some(printer) = self.print_draft.pending_caps_queue.pop_front() else {
                break;
            };
            if is_mock && !self.print_draft.cached_capabilities.contains_key(&printer) {
                continue;
            }
            self.read_caps_in_background(printer);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn poll_coordinator(&mut self) {
        use pdfcraft_engine::print_coordinator::CoordinatorEvent;
        use pdfcraft_platform::diagnostics::{TraceCategory, TraceLevel};

        let mut completed_notices: Vec<(String, usize)> = Vec::new();
        let mut traces = Vec::new();

        self.print_tasks.coordinator_handles.retain_mut(|handle| {
            let active_in_batch: std::collections::BTreeSet<String> =
                handle.all_telemetry().keys().cloned().collect();
            let events = handle.poll_events();
            for event in events {
                match event {
                    CoordinatorEvent::ProgressUpdate(tel) => {
                        self.print_tasks.stream_telemetry.insert(tel.printer_name.clone(), tel);
                    }
                    CoordinatorEvent::PrinterCompleted { printer_name, total_sheets, duration_secs } => {
                        if let Some(t) = handle.get_telemetry(&printer_name) {
                            self.print_tasks.stream_telemetry.insert(printer_name.clone(), t.clone());
                        }
                        traces.push(make_trace(
                            TraceLevel::Info,
                            TraceCategory::Spooler,
                            Some(printer_name.clone()),
                            format!("Completed {total_sheets} sheets in {duration_secs:.2}s"),
                            None,
                        ));
                        completed_notices.push((printer_name, total_sheets));
                    }
                    CoordinatorEvent::PrinterFailed {
                        printer_name,
                        error,
                        committed_sheets,
                        pending_sheets,
                        suggested_failover,
                    } => {
                        if let Some(t) = handle.get_telemetry(&printer_name) {
                            self.print_tasks.stream_telemetry.insert(printer_name.clone(), t.clone());
                        }
                        let committed = committed_sheets.len();
                        let total = committed_sheets.len() + pending_sheets.len();
                        let pending = pending_sheets.len();
                        let failed_sheet = committed;
                        let fallback = resolve_failover_target(
                            &self.print_draft,
                            &printer_name,
                            suggested_failover,
                            &active_in_batch,
                        );
                        traces.push(make_trace(
                            TraceLevel::Error,
                            TraceCategory::Failover,
                            Some(printer_name.clone()),
                            format!(
                                "Stream fault at sheet #{}: {error} (committed={committed}, pending={pending})",
                                failed_sheet + 1
                            ),
                            Some("PRINTER_STREAM_FAULT".to_string()),
                        ));
                        self.print_draft.failover_modal = Some(draft::FailoverModalState {
                            from_printer: printer_name,
                            to_printer: fallback,
                            failed_sheet,
                            committed_sheets: committed,
                            pending_sheets: pending,
                            total_sheets: total,
                            error_message: error,
                        });
                    }
                    CoordinatorEvent::StreamStalled { printer_name, stalled_duration_secs } => {
                        if let Some(t) = handle.get_telemetry(&printer_name) {
                            self.print_tasks.stream_telemetry.insert(printer_name.clone(), t.clone());
                        }
                        traces.push(make_trace(
                            TraceLevel::Warning,
                            TraceCategory::Spooler,
                            Some(printer_name),
                            format!("Spool stream stalled for {stalled_duration_secs}s"),
                            Some("STREAM_STALLED".to_string()),
                        ));
                    }
                    CoordinatorEvent::FailoverDispatched { from_printer, to_printer, rerouted_sheets } => {
                        if let Some(t) = handle.get_telemetry(&to_printer) {
                            self.print_tasks.stream_telemetry.insert(to_printer.clone(), t.clone());
                        }
                        traces.push(make_trace(
                            TraceLevel::Info,
                            TraceCategory::Failover,
                            Some(to_printer.clone()),
                            format!("Rerouted {rerouted_sheets} uncommitted sheets from '{from_printer}' to '{to_printer}'"),
                            None,
                        ));
                    }
                    _ => {}
                }
            }
            !handle.is_all_completed()
        });

        for trace in traces {
            self.print_draft.push_trace(trace);
        }
        for (printer, sheets) in completed_notices {
            self.notify(format!("Sent to {printer} ({sheets} sheets)"));
        }

        // Milestone 4: process pending failover rerouting request triggered from modal
        if let Some((from_printer, to_printer)) = self.print_draft.pending_failover.take() {
            self.execute_failover_reroute(&from_printer, &to_printer);
        }
    }

    /// Pick up a printer list, driver options or a spooled job that finished (each frame).
    pub(crate) fn poll_print(&mut self) {
        #[cfg(target_arch = "wasm32")]
        self.poll_web_print();
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(printer) = self.print_draft.read_papers_for.take() {
                self.read_papers_in_background(printer);
            }
            let mut arrived_papers = Vec::new();
            self.print_tasks.papers.retain(|(printer, rx)| match rx.try_recv() {
                Ok((papers, def)) => {
                    arrived_papers.push((printer.clone(), papers, def));
                    false
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    arrived_papers.push((printer.clone(), Vec::new(), None));
                    false
                }
            });
            for (printer, papers, def) in arrived_papers {
                self.receive_papers(printer, papers, def);
            }
            if let Some(printer) = self.print_draft.read_options_for.take() {
                self.read_options_in_background(printer);
            }
            let mut arrived = Vec::new();
            self.print_tasks.options.retain(|(printer, rx)| match rx.try_recv() {
                Ok(options) => {
                    arrived.push((printer.clone(), options));
                    false
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => true,
                // A read that died offers nothing; the panel says so rather than wait.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    arrived.push((printer.clone(), Vec::new()));
                    false
                }
            });
            for (printer, options) in arrived {
                self.receive_options(printer, options);
            }
            let arrived_printers = self.print_tasks.printers.as_ref().and_then(|rx| match rx.try_recv() {
                Ok(p) => Some(p),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                // A source that died lists nothing new; the dialog keeps what it has.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(self.print_draft.printers.clone()),
            });
            if let Some(printers) = arrived_printers {
                self.apply_arrived_printers(printers);
            }
            self.poll_caps();
            self.poll_two_sided();
            let mut finished = Vec::new();
            self.print_tasks.jobs.retain_mut(|job| {
                loop {
                    match job.news.try_recv() {
                        Ok(JobNews::Sent(sent, sheets)) => job.sheets = Some((sent, sheets)),
                        Ok(JobNews::Done(r)) => {
                            finished.push((job.printer.clone(), r));
                            break false;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break true,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            finished.push((job.printer.clone(), Err("the print job stopped unexpectedly".into())));
                            break false;
                        }
                    }
                }
            });
            for (printer, result) in finished {
                self.report_print_job(&printer, result);
            }
            self.poll_coordinator();
            self.show_print_progress();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn apply_arrived_printers(&mut self, printers: Vec<spool::Printer>) {
        self.print_tasks.printers = None;
        let d = &mut self.print_draft;
        // A picked printer that is no longer listed falls back to the default too.
        let changed = if !d.printer_picked || !offered(d.printer.as_deref(), &printers) {
            d.printer_picked = false;
            let printer = default_printer(d, &printers);
            let changed = printer != d.printer;
            if changed {
                d.printer = printer;
                if !d.is_broadcast_active() {
                    d.apply_setup();
                }
            }
            changed
        } else {
            false
        };
        d.printers = printers;
        d.printers_loading = false;
        if d.multi_printer_mode && !d.selected_printers.is_empty() {
            if d.active_preview_printer
                .as_ref()
                .is_none_or(|cur| !d.selected_printers.contains(cur))
            {
                d.active_preview_printer = d.selected_printers.iter().next().cloned();
            }
            d.enqueue_caps_for_selected();
        }
        if changed {
            if !d.is_broadcast_active() {
                d.reset_unoverridden_custom_paper();
            }
            d.apply_printer_papers(Vec::new());
            if let Some(p) = d.printer.clone() {
                self.read_papers_in_background(p.clone());
                self.print_draft.enqueue_caps_query(p);
            }
        }
    }

    /// The oldest job being spooled on the progress card, counting its sheets as they reach the
    /// spooler, while no other long job holds the card.
    #[cfg(not(target_arch = "wasm32"))]
    fn show_print_progress(&mut self) {
        let progress_pair = if let Some(job) = self.print_tasks.jobs.first() {
            let printer = job.printer.as_str();
            let pair = match job.sheets {
                Some((sent, sheets)) if sheets > 0 => {
                    // The sheet on its way: one past those sent, until the last is.
                    let sheet = sent.saturating_add(1).min(sheets).to_string();
                    let label = crate::i18n::fmt(
                        tl!("Sending sheet {s} of {n} to {printer}…"),
                        &[("s", &sheet), ("n", &sheets.to_string()), ("printer", printer)],
                    );
                    (label, sent as f32 / sheets as f32)
                }
                _ => (crate::i18n::fmt(tl!("Sending to {printer}…"), &[("printer", printer)]), 0.0),
            };
            Some(pair)
        } else {
            self.print_tasks
                .coordinator_handles
                .first()
                .and_then(coordinator_progress_summary)
        };

        let Some((label, fraction)) = progress_pair else {
            if std::mem::take(&mut self.print_tasks.showing_progress) {
                self.progress_notice = None;
            }
            return;
        };
        if self.progress_notice.is_some() && !self.print_tasks.showing_progress {
            return;
        }
        self.progress_notice = Some(crate::widgets::ProgressNotice { label, fraction, cancellable: false });
        self.print_tasks.showing_progress = true;
        // The bar's sweep keeps moving between sheets.
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// Executes one-click failover: cancels the faulted worker on `from_printer` and reroutes
    /// all remaining unprinted sheets to `to_printer` with zero duplicate pages.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn execute_failover_reroute(&mut self, from_printer: &str, to_printer: &str) -> bool {
        let mut rerouted = false;
        for handle in &mut self.print_tasks.coordinator_handles {
            if handle.all_telemetry().contains_key(from_printer) {
                // Halt the faulted/stalled worker thread to guarantee zero duplicate sheets
                handle.cancel_printer(from_printer);
                match handle.failover_reroute(from_printer, to_printer) {
                    Ok(()) => {
                        if let Some(tel) = handle.get_telemetry(to_printer) {
                            self.print_tasks.stream_telemetry.insert(to_printer.to_string(), tel.clone());
                        }
                        self.notify(format!("Rerouted remaining sheets to {to_printer}"));
                        rerouted = true;
                        break;
                    }
                    Err(err) => {
                        self.print_draft.push_trace(make_trace(
                            pdfcraft_platform::diagnostics::TraceLevel::Error,
                            pdfcraft_platform::diagnostics::TraceCategory::Failover,
                            Some(to_printer.to_string()),
                            format!("Failover rerouting from '{from_printer}' failed: {err}"),
                            Some("FAILOVER_FAILED".to_string()),
                        ));
                        self.notify_error(format!("Failover rerouting failed: {err}"));
                        return false;
                    }
                }
            }
        }
        rerouted
    }

    /// Pick up two-sided answers that arrived, and ask for the picked printer's (and selected
    /// cluster printers') when the list doesn't say, nobody has asked yet and fewer than
    /// [`TWO_SIDED_READS`] reads are running.
    #[cfg(not(target_arch = "wasm32"))]
    fn poll_two_sided(&mut self) {
        let d = &mut self.print_draft;
        self.print_tasks.two_sided.retain(|(printer, rx)| match rx.try_recv() {
            Ok(two_sided) => {
                d.two_sided.insert(printer.clone(), two_sided);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                d.two_sided.insert(printer.clone(), None);
                false
            }
        });
        if self.dialog != Some(crate::Dialog::Print) {
            return;
        }
        let Some(source) = self.two_sided_source.clone() else { return };
        let candidates: Vec<String> = d
            .printer
            .iter()
            .chain(d.selected_printers.iter().filter(|_| d.multi_printer_mode))
            .cloned()
            .collect();
        for candidate in candidates {
            if self.print_tasks.two_sided.len() >= TWO_SIDED_READS {
                break;
            }
            let Some(printer) = d.printers.iter().find(|p| p.name == candidate) else { continue };
            if printer.two_sided.is_some()
                || d.two_sided.contains_key(&printer.name)
                || self.print_tasks.two_sided.iter().any(|(n, _)| *n == printer.name)
            {
                continue;
            }
            let name = printer.name.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let ctx = self.ctx.clone();
            let asked = name.clone();
            let src = source.clone();
            std::thread::spawn(move || {
                // The receiver may be gone (the app quit): nothing to report to then.
                let _ = tx.send(src(&asked));
                if let Some(ctx) = ctx {
                    ctx.request_repaint();
                }
            });
            self.print_tasks.two_sided.push((name, rx));
        }
    }

    /// Report the outcome of a finished print job to the user.
    fn report_print_job(&mut self, printer: &str, result: Result<String, String>) {
        match result {
            Ok(msg) if msg.is_empty() => {
                #[cfg(not(target_arch = "wasm32"))]
                self.print_draft.push_trace(make_trace(
                    pdfcraft_platform::diagnostics::TraceLevel::Info,
                    pdfcraft_platform::diagnostics::TraceCategory::Spooler,
                    Some(printer.to_string()),
                    "Spooler accepted print job".to_string(),
                    None,
                ));
                self.notify(crate::i18n::fmt(tl!("Sent to {printer}"), &[("printer", printer)]));
            }
            Ok(msg) => {
                #[cfg(not(target_arch = "wasm32"))]
                self.print_draft.push_trace(make_trace(
                    pdfcraft_platform::diagnostics::TraceLevel::Info,
                    pdfcraft_platform::diagnostics::TraceCategory::Spooler,
                    Some(printer.to_string()),
                    format!("Spooler accepted print job: {msg}"),
                    None,
                ));
                self.notify(crate::i18n::fmt(tl!("Sent to {printer}: {msg}"), &[("printer", printer), ("msg", &msg)]));
            }
            Err(e) => {
                #[cfg(not(target_arch = "wasm32"))]
                self.print_draft.push_trace(make_trace(
                    pdfcraft_platform::diagnostics::TraceLevel::Error,
                    pdfcraft_platform::diagnostics::TraceCategory::Spooler,
                    Some(printer.to_string()),
                    e.clone(),
                    Some("SPOOL_SUBMIT_ERROR".to_string()),
                ));
                self.notify_error(e);
            }
        }
    }

    /// Pre-flight spooler disk headroom check before dispatching native print jobs.
    /// Skipped when an injected test `spooler` stand-in is active.
    #[cfg(not(target_arch = "wasm32"))]
    fn check_spooler_disk_headroom(&mut self, estimated_required_bytes: u64) -> bool {
        if self.spooler.is_some() {
            return true;
        }
        let spool_tel = pdfcraft_platform::diagnostics::harvest_spooler_telemetry();
        let insufficient_batch_space =
            spool_tel.available_bytes > 0 && spool_tel.available_bytes < estimated_required_bytes;
        if spool_tel.total_bytes > 0
            && (spool_tel.disk_verdict == pdfcraft_platform::diagnostics::SpoolDiskVerdict::CriticalExhaustion
                || insufficient_batch_space)
        {
            let free_mb = spool_tel.available_bytes / (1024 * 1024);
            let msg = format!(
                "Spooler disk space critically low ({free_mb} MB free in {}). Free disk space before printing.",
                spool_tel.spool_directory
            );
            self.print_draft.push_trace(make_trace(
                pdfcraft_platform::diagnostics::TraceLevel::Error,
                pdfcraft_platform::diagnostics::TraceCategory::Spooler,
                None,
                msg.clone(),
                Some("SPOOL_DISK_CRITICAL".to_string()),
            ));
            self.notify_error(msg);
            return false;
        }
        true
    }

    /// Dispatches a unified print batch simultaneously across all selected cluster printers,
    /// running imposition, rasterization, monochrome K-only filtering, and GDI flattening off the UI thread.
    #[cfg(not(target_arch = "wasm32"))]
    fn dispatch_cluster_batch(
        &mut self,
        id: pdfcraft_engine::DocId,
        name: String,
        settings: print::Settings,
        prepared: pdfcraft_engine::print_coordinator::PreparedPrintSource,
    ) -> bool {
        use pdfcraft_engine::print_coordinator::{
            BatchDispatchConfig, PrintOrchestrationCoordinator, SharedPdfSource, SpoolSubmitFn,
        };
        use pdfcraft_platform::diagnostics::{TraceCategory, TraceLevel};

        pdfcraft_platform::install_print_bridge();
        let printers = self.print_draft.dispatchable_target_printers();
        if printers.is_empty() {
            self.notify_error("Select at least one target printer to dispatch the batch.");
            return false;
        }
        let doc_bytes_len = self.session.get(id).map_or(0, |d| d.bytes.len() as u64);
        let estimated_spool_bytes = doc_bytes_len
            .saturating_mul(printers.len() as u64)
            .saturating_mul(4);
        if !self.check_spooler_disk_headroom(estimated_spool_bytes) {
            return false;
        }
        let Some(doc) = self.session.get(id) else { return false };
        let sizes: Vec<(f64, f64)> = doc
            .info
            .pages
            .iter()
            .map(|p| (f64::from(p.width), f64::from(p.height)))
            .collect();
        let sheets = match print::layout(&sizes, &settings) {
            Ok(s) => s,
            Err(e) => {
                self.notify_error(e);
                return false;
            }
        };

        // Zero-copy document source: memory-map clean files on disk, or share Arc<Vec<u8>> for edited state.
        let doc_source = if !doc.dirty
            && let Some(ref path) = doc.path
            && let Ok(mmap_src) = SharedPdfSource::from_file(path)
        {
            mmap_src
        } else {
            SharedPdfSource::from_arc(doc.bytes.clone())
        };

        let mut printer_jobs = std::collections::HashMap::with_capacity(printers.len());
        for p in &printers {
            let mut job = self.print_draft.job_for_printer(Some(p.as_str()), &name);
            print::add_cups_orientation(&mut job.options, &sheets);
            printer_jobs.insert(p.clone(), job);
        }
        let mut default_job = self.print_draft.job_for_printer(None, &name);
        print::add_cups_orientation(&mut default_job.options, &sheets);

        let wakeup_ctx = self.ctx.clone();
        let on_event_wakeup: Option<std::sync::Arc<dyn Fn() + Send + Sync>> = wakeup_ctx.map(|c| {
            let cb: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
                c.request_repaint();
            });
            cb
        });

        let stand_in_ctx = self.ctx.clone();
        let custom_spooler: Option<SpoolSubmitFn> = self.spooler.clone().map(|stand_in| {
            let sheet_count = sheets.len().max(1);
            let cb: SpoolSubmitFn = std::sync::Arc::new(move |pdf_bytes, job, on_progress| {
                on_progress(sheet_count, sheet_count);
                let res = stand_in(pdf_bytes, job);
                if let Some(ref c) = stand_in_ctx {
                    c.request_repaint();
                }
                res
            });
            cb
        });

        let config = BatchDispatchConfig {
            printers: printers.clone(),
            settings,
            doc_source,
            page_sizes: sizes,
            printer_jobs,
            default_job,
            pre_imposed_pdf: None,
            custom_spooler,
            on_event_wakeup,
        };

        match PrintOrchestrationCoordinator::dispatch_batch_prepared(config, prepared) {
            Ok(handle) => {
                for (p_name, tel) in handle.all_telemetry() {
                    self.print_tasks.stream_telemetry.insert(p_name.clone(), tel.clone());
                }
                self.print_draft.push_trace(make_trace(
                    TraceLevel::Info,
                    TraceCategory::Spooler,
                    None,
                    format!(
                        "Dispatched cluster batch '{}' ({} sheets) across {} printers: [{}]",
                        name,
                        sheets.len(),
                        printers.len(),
                        printers.join(", ")
                    ),
                    None,
                ));
                self.print_tasks.coordinator_handles.push(handle);
                self.show_print_progress();
                true
            }
            Err(err) => {
                let msg = format!("Cluster dispatch failed: {err}");
                self.print_draft.push_trace(make_trace(
                    TraceLevel::Error,
                    TraceCategory::Spooler,
                    None,
                    msg.clone(),
                    Some("CLUSTER_DISPATCH_ERR".to_string()),
                ));
                self.notify_error(msg);
                false
            }
        }
    }

    /// Print (or save) with the dialog's settings. Returns `true` on success. Save as PDF without
    /// a preset path returns `true` once the save picker is showing; the file is written on a
    /// later frame, when the user has chosen where.
    pub fn print_now(&mut self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        pdfcraft_platform::install_print_bridge();
        if still_looking(&self.print_draft) || self.print_draft.papers_loading() {
            return false;
        }
        // What's typed in a form field is part of what's printed (#166).
        if !self.commit_form_typing() {
            return false;
        }
        let Some((_, id)) = self.active_ids() else { return false };
        // A form's Will Print script runs first: what it changes is printed.
        if self.session.get(id).is_some_and(|d| d.allows_printing()) {
            let before = self.session.get(id).map(|d| d.edit_generation());
            self.run_document_actions(id, pdfcraft_engine::js::DocumentScriptEvent::WillPrint);
            // What it showed may lie outside the printed areas Label printing found: find them again.
            if self.session.get(id).map(|d| d.edit_generation()) != before && self.print_draft.handling == Handling::Label {
                self.print_draft.crop_pages = None;
                self.read_label_crops(id);
            }
        }
        let Some(doc) = self.session.get(id) else { return false };
        let labels: Vec<String> = doc.info.pages.iter().map(|p| p.label.clone()).collect();
        let sizes: Vec<(f64, f64)> = doc.info.pages.iter().map(|p| (f64::from(p.width), f64::from(p.height))).collect();
        let page_count = doc.info.pages.len();
        let name = doc.name.clone();
        let settings = match self.print_draft.settings(page_count, &labels) {
            Ok(s) => s,
            Err(e) => {
                self.notify_tr(&e);
                return false;
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        if self.print_draft.multi_printer_mode {
            let prepared = match self.session.prepare_print_pdf(id) {
                Ok(p) => p,
                Err(e) => {
                    self.notify_error(e);
                    return false;
                }
            };
            let sent = self.dispatch_cluster_batch(id, name, settings, prepared);
            if sent {
                self.print_draft.remember_cluster_setup();
            }
            return sent;
        }
        let bytes = match self.session.print_pdf(id, &settings) {
            Ok(b) => b,
            Err(e) => {
                self.notify_error(e);
                return false;
            }
        };
        let to_printer = self.print_draft.printer.is_some();
        let sent = match self.print_draft.printer.clone() {
            #[cfg(target_arch = "wasm32")]
            Some(printer) if printer == crate::web_print::BROWSER_PRINTER => {
                match print::layout(&sizes, &settings) {
                    Ok(sheets) => self.print_in_browser(name, bytes, sheets.iter().map(|s| s.size).collect()),
                    Err(e) => {
                        self.notify_error(e);
                        false
                    }
                }
            }
            Some(printer) => {
                #[cfg(not(target_arch = "wasm32"))]
                if !self.check_spooler_disk_headroom((bytes.len() as u64).saturating_mul(4)) {
                    return false;
                }
                let mut job = self.print_draft.job(&name);
                #[cfg(not(target_arch = "wasm32"))]
                if self.spooler.is_none() {
                    self.print_draft.apply_broadcast_specs_to_job(&mut job);
                }
                // CUPS is told which way the sheets run (#2713), unless a driver choice already
                // says; Windows' print ticket already is.
                if let Ok(sheets) = print::layout(&sizes, &settings) {
                    print::add_cups_orientation(&mut job.options, &sheets);
                }
                self.send_to_printer(printer, bytes, job)
            }
            None => self.save_print_pdf(&name, bytes),
        };
        // What this printer printed with is what it starts with next time (#2001).
        if sent && to_printer {
            self.print_draft.remember_setup();
        }
        sent
    }

    /// Spool a job off the UI thread: on Windows every sheet is rendered at the printer's
    /// resolution first, which can take minutes for a long document. The dialog closes at once,
    /// the progress card counts the sheets as they reach the spooler, and the spooler's answer
    /// arrives as a notice.
    #[cfg(not(target_arch = "wasm32"))]
    fn send_to_printer(&mut self, printer: String, bytes: Vec<u8>, mut job: spool::Job) -> bool {
        pdfcraft_platform::install_print_bridge();
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = self.ctx.clone();
        let spooler = self.spooler.clone();
        std::thread::spawn(move || {
            let repaint = || {
                if let Some(ctx) = &ctx {
                    ctx.request_repaint();
                }
            };
            // On Windows grayscale makes the pages grey (#5806) and large pictures become tiles
            // (#5131): work on the whole file, kept off the UI thread with the rest.
            // A GDI (v3) driver gets transparent sheets flattened (#6155, #5838, #5940); XPS (v4)
            // drivers print them as they are.
            let flatten = spooler.is_none() && job.printer.as_deref().is_some_and(spool::gdi_driver);
            let bytes = pdfcraft_engine::spooler::spooler_copy(bytes, &mut job, flatten);
            // The receiver may be gone (the app quit): nothing to report to then.
            let result = match &spooler {
                Some(submit) => submit(&bytes, &job),
                None => spool::submit_with_progress(&bytes, &job, &mut |sent, sheets| {
                    let _ = tx.send(JobNews::Sent(sent, sheets));
                    repaint();
                })
                .map_err(|e| e.to_string()),
            };
            let _ = tx.send(JobNews::Done(result));
            repaint();
        });
        self.print_tasks.jobs.push(PrintJob { printer, sheets: None, news: rx });
        self.show_print_progress();
        true
    }

    /// A browser has no spooler: `submit` answers at once.
    #[cfg(target_arch = "wasm32")]
    fn send_to_printer(&mut self, printer: String, bytes: Vec<u8>, job: spool::Job) -> bool {
        let result = spool::submit(&bytes, &job).map_err(|e| e.to_string());
        let ok = result.is_ok();
        self.report_print_job(&printer, result);
        ok
    }

    /// Print through the browser's print dialog: start drawing `sheets` (the pages of the
    /// print-ready `bytes`), a sheet a frame in [`PdfCraftApp::poll_print`]. A print still being
    /// prepared is replaced. The dialog closes at once.
    #[cfg(target_arch = "wasm32")]
    fn print_in_browser(&mut self, name: String, bytes: Vec<u8>, sheets: Vec<(f64, f64)>) -> bool {
        match crate::web_print::WebPrintJob::new(bytes, sheets, name) {
            Ok(job) => {
                let n = job.total().to_string();
                self.print_tasks.web_job = Some(job);
                self.notify_fmt("Preparing {n} sheets for printing…", &[("n", &n)]);
                if let Some(ctx) = &self.ctx {
                    ctx.request_repaint();
                }
                true
            }
            Err(e) => {
                self.notify_error(e);
                false
            }
        }
    }

    /// Draw the next sheet of the browser print, and hand the finished page to the browser.
    #[cfg(target_arch = "wasm32")]
    fn poll_web_print(&mut self) {
        let failed: Vec<String> = self.print_tasks.web_failed.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        if let Some(e) = failed.into_iter().next() {
            self.web_print_failed(&e);
        }
        let Some(job) = self.print_tasks.web_job.as_mut() else { return };
        if !job.finished() {
            if let Err(e) = job.step(&mut crate::web_print::blob_url) {
                self.print_tasks.web_job = None;
                self.notify_error(e);
                return;
            }
            let (done, total) = (job.done().to_string(), job.total().to_string());
            self.notify_fmt("Preparing sheet {done} of {total} for printing…", &[("done", &done), ("total", &total)]);
            if let Some(ctx) = &self.ctx {
                ctx.request_repaint();
            }
            return;
        }
        let Some(mut job) = self.print_tasks.web_job.take() else { return };
        let html = job.html();
        let urls = job.take_urls();
        self.print_tasks.web_last = Some((job.name.clone(), job.pdf.clone()));
        match crate::web_print::print_in_iframe(&html, urls, self.print_tasks.web_failed.clone(), self.ctx.clone()) {
            Ok(()) => self.notify_tr("Opening the browser's print dialog…"),
            Err(e) => self.web_print_failed(&e),
        }
    }

    /// The browser wouldn't print (a sandboxed frame, a picture it couldn't decode): offer the
    /// print-ready PDF as a download instead, so the work isn't lost.
    #[cfg(target_arch = "wasm32")]
    fn web_print_failed(&mut self, error: &str) {
        let Some((name, pdf)) = self.print_tasks.web_last.take() else {
            self.notify_error(error);
            return;
        };
        let stem = name.trim_end_matches(".pdf").trim_end_matches(".PDF");
        let file = format!("{stem} (print).pdf");
        match crate::editing::download(&file, &pdf) {
            Ok(()) => self.notify_fmt("Couldn't print from the browser ({e}); downloaded {name} to print instead", &[("e", error), ("name", &file)]),
            Err(_) => self.notify_error(error),
        }
    }

    /// Print ▸ Save as PDF on the desktop: write to `save_override` (tests and automation), or
    /// ask where and write on a later frame. Returns `true` once written, or once the save picker
    /// is showing.
    #[cfg(not(target_arch = "wasm32"))]
    fn save_print_pdf(&mut self, name: &str, bytes: Vec<u8>) -> bool {
        let write = move |app: &mut Self, path: std::path::PathBuf| match crate::editing::write_atomically(&path.to_string_lossy(), &bytes) {
            Ok(()) => {
                app.notify_fmt("Saved the print-ready PDF to {path}", &[("path", &path.display().to_string())]);
                true
            }
            Err(e) => {
                app.notify_fmt("Could not save: {e}", &[("e", &e.to_string())]);
                false
            }
        };
        match self.save_override.clone() {
            Some(p) => write(self, p.into()),
            None => {
                let stem = name.trim_end_matches(".pdf").trim_end_matches(".PDF");
                let dialog = rfd::AsyncFileDialog::new().add_filter_any_case("PDF", &["pdf"]).set_file_name(format!("{stem} (print).pdf"));
                self.ask_one(crate::pickers::Ask::Save(dialog), None, move |app, path| {
                    write(app, path);
                })
            }
        }
    }

    /// Print ▸ Save as PDF in a browser: download it, as Save does (#170). There is no folder to
    /// choose and no file system to write to, so `save_override` doesn't apply.
    #[cfg(target_arch = "wasm32")]
    fn save_print_pdf(&mut self, name: &str, bytes: Vec<u8>) -> bool {
        let stem = name.trim_end_matches(".pdf").trim_end_matches(".PDF");
        let file = format!("{stem} (print).pdf");
        match crate::editing::download(&file, &bytes) {
            Ok(()) => {
                self.notify_fmt("Downloaded {name}", &[("name", &file)]);
                true
            }
            Err(e) => {
                self.notify_fmt("Couldn't download {name}: {e}", &[("name", &file), ("e", &e)]);
                false
            }
        }
    }
}

/// The longest side, in pixels, of one page's print-preview raster (also kept within the GPU's
/// `max_texture_side`): an Actual-size print of a large page would otherwise ask for a raster
/// bigger than a texture can be.
pub(crate) const PREVIEW_MAX_SIDE: f32 = 4096.0;
/// The most bytes of print-preview rasters kept at once. The current sheet comes first, then the
/// next and the previous one; a neighbour that doesn't fit isn't rendered ahead.
pub(crate) const PREVIEW_BYTES: f64 = 128.0 * 1024.0 * 1024.0;

/// Pages on the current sheet and its neighbours, each with the device pixels per point that
/// fill the preview pane (so the picture is not a stretched thumbnail), nearest sheet first and
/// within [`PREVIEW_MAX_SIDE`] (and `max_side`, the GPU's texture limit) and [`PREVIEW_BYTES`].
pub(crate) fn preview_rasters(d: &PrintDraft, sizes: &[(f64, f64)], labels: &[String], ppp: f32, max_side: f32) -> Vec<(usize, f32)> {
    let Ok(settings) = d.settings(sizes.len(), labels) else { return Vec::new() };
    let Ok(sheets) = print::layout(sizes, &settings) else { return Vec::new() };
    if sheets.is_empty() {
        return Vec::new();
    }
    let ppp = if ppp.is_finite() { ppp.max(1.0) } else { 1.0 };
    let side = if max_side.is_finite() && max_side >= 1.0 { max_side.min(PREVIEW_MAX_SIDE) } else { PREVIEW_MAX_SIDE };
    let i = d.sheet.min(sheets.len() - 1);
    // Nearest first: the sheet on screen, then the next one, then the previous one.
    let order = [Some(i), i.checked_add(1).filter(|n| *n < sheets.len()), i.checked_sub(1)];
    let mut out: Vec<(usize, f32, f64)> = Vec::new();
    let mut bytes = 0.0f64;
    for sheet in order.into_iter().flatten().filter_map(|s| sheets.get(s)) {
        let (sw, sh) = (sheet.size.0 as f32, sheet.size.1 as f32);
        if !(sw.is_finite() && sh.is_finite()) || sw < 1.0 || sh < 1.0 {
            continue;
        }
        let k = (PREVIEW_BOX.0 / sw).min(PREVIEW_BOX.1 / sh);
        for pl in &sheet.placed {
            let [a, b, _, _, _, _] = pl.matrix.0;
            let placed = (a.hypot(b) as f32) * k * ppp;
            let Some(&(pw, ph)) = sizes.get(pl.page) else { continue };
            let long_pt = pw.max(ph) as f32;
            if !placed.is_finite() || placed < 0.05 || !long_pt.is_finite() || long_pt < 1.0 {
                continue;
            }
            let scale = placed.min(64.0).min(side / long_pt);
            let cost = (pw * f64::from(scale)).ceil() * (ph * f64::from(scale)).ceil() * 4.0;
            if let Some(slot) = out.iter_mut().find(|(page, _, _)| *page == pl.page) {
                if scale > slot.1 && bytes - slot.2 + cost <= PREVIEW_BYTES {
                    bytes += cost - slot.2;
                    (slot.1, slot.2) = (scale, cost);
                }
            } else if out.len() < 48 && (out.is_empty() || bytes + cost <= PREVIEW_BYTES) {
                bytes += cost;
                out.push((pl.page, scale, cost));
            }
        }
    }
    out.into_iter().map(|(page, scale, _)| (page, scale)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_page_preview_uses_positions_and_selection_guards() {
        for labels in [["i", "ii", "iii", "1", "2"], ["1", "1", "1", "1", "1"], ["1", "2", "3", "4", "5"]] {
            let labels: Vec<String> = labels.into_iter().map(str::to_owned).collect();
            for current_page in 0..5 {
                let draft = PrintDraft { which: Which::Current, current_page, ..Default::default() };
                let rasters = preview_rasters(&draft, &[(200.0, 300.0); 5], &labels, 1.0, 8192.0);
                assert_eq!(rasters.iter().map(|(page, _)| *page).collect::<Vec<_>>(), vec![current_page]);
            }
        }
        let labels: Vec<String> = ["i", "ii", "iii", "1", "2"].into_iter().map(str::to_owned).collect();
        let mut draft = PrintDraft { which: Which::Selected, selected: vec![0, 3], ..Default::default() };
        assert_eq!(draft.settings(5, &labels).unwrap().pages, vec![0, 3]);
        draft.which = Which::All;
        assert_eq!(draft.settings(5, &labels).unwrap().pages, vec![0, 1, 2, 3, 4]);
        draft.which = Which::Current;
        draft.reverse = true;
        draft.subset = Subset::Odd;
        assert_eq!(draft.settings(5, &labels).unwrap().pages, vec![0]);
        draft.subset = Subset::Even;
        assert!(draft.settings(5, &labels).is_err());
        draft.subset = Subset::All;
        for (count, current) in [(0, 0), (5, 5), (5, usize::MAX)] {
            draft.current_page = current;
            assert!(draft.settings(count, &labels).is_err());
        }
    }

    /// #5806: with Print as image, grayscale makes the pictures grey, so the job doesn't ask the
    /// driver for it as well; a driver that keeps no grayscale setting then still prints.
    #[test]
    fn grey_pictures_dont_ask_the_driver_for_grayscale() {
        let vector = PrintDraft { grayscale: true, ..PrintDraft::default() };
        assert!(vector.job("a.pdf").grayscale, "vector pages: the driver makes them grey");
        let pictures = PrintDraft { grayscale: true, as_image: true, ..PrintDraft::default() };
        assert!(!pictures.job("a.pdf").grayscale, "grey pictures: nothing for the driver to do");
        assert!(pictures.settings(1, &[]).unwrap().raster.is_some_and(|r| r.grayscale), "grey pictures: the pages are rendered grey");
        assert!(!PrintDraft::default().job("a.pdf").grayscale);
    }

    #[test]
    fn composed_sheets_keep_exact_scaling_and_the_dialogs_duplex_choice() {
        let draft = PrintDraft { duplex: Some(spool::Duplex::LongEdge), ..PrintDraft::default() };
        let job = draft.job("report.pdf");
        assert_eq!(job.scaling, spool::Scaling::Exact);
        assert_eq!(job.duplex, Some(spool::Duplex::LongEdge));
        let off = PrintDraft { duplex: Some(spool::Duplex::Off), ..PrintDraft::default() };
        assert_eq!(off.job("report.pdf").duplex, Some(spool::Duplex::Off));
    }

    /// Untouched, Two-sided sends nothing, so the duplex set in the printer's Properties applies
    /// (#2422); cut and stack still asks for single-sided sheets.
    #[test]
    fn two_sided_follows_the_printer_until_chosen() {
        assert_eq!(PrintDraft::default().duplex, None);
        assert_eq!(PrintDraft::default().job("report.pdf").duplex, None);
        let mut cut = PrintDraft { handling: Handling::Multiple, order: PageOrder::CutStack, ..PrintDraft::default() };
        assert_eq!(cut.job("report.pdf").duplex, Some(spool::Duplex::Off));
        assert!(cut.settings(4, &[]).is_ok(), "the printer's default doesn't block cut and stack");
        cut.duplex = Some(spool::Duplex::ShortEdge);
        assert!(cut.settings(4, &[]).is_err());
    }

    /// #3208: long or short edge goes to a printer whose driver doesn't take two-sided settings
    /// as no duplex setting at all, so the driver keeps its own instead of refusing the job; Off
    /// still reaches it, a printer that can or whose support is unknown still gets the choice,
    /// and a stale long-edge choice doesn't block cut and stack. The printers are listed as the
    /// dialog lists them (#3554), drivers unasked, with each answer in `two_sided`.
    #[test]
    fn duplex_is_not_sent_to_a_printer_without_two_sided() {
        let draft = |name: &str, duplex| PrintDraft {
            printers: ["Office", "Duplexer", "Network"]
                .map(|n| spool::Printer { name: n.into(), default: n == "Office", two_sided: None, description: String::new() })
                .to_vec(),
            two_sided: [("Office".to_string(), Some(false)), ("Duplexer".to_string(), Some(true))].into_iter().collect(),
            printer: Some(name.into()),
            duplex,
            ..PrintDraft::default()
        };
        for duplex in [spool::Duplex::LongEdge, spool::Duplex::ShortEdge] {
            assert_eq!(draft("Office", Some(duplex)).job("report.pdf").duplex, None, "{duplex:?} to a printer without two-sided");
            assert_eq!(draft("Duplexer", Some(duplex)).job("report.pdf").duplex, Some(duplex));
            assert_eq!(draft("Network", Some(duplex)).job("report.pdf").duplex, Some(duplex), "unknown support keeps the choice");
        }
        assert_eq!(draft("Office", Some(spool::Duplex::Off)).job("report.pdf").duplex, Some(spool::Duplex::Off), "Off still reaches it");
        assert_eq!(draft("Office", None).job("report.pdf").duplex, None);
        // A long-edge choice left from another printer doesn't block cut and stack on this one.
        let mut cut = draft("Office", Some(spool::Duplex::LongEdge));
        (cut.handling, cut.order) = (Handling::Multiple, PageOrder::CutStack);
        assert!(cut.settings(4, &[]).is_ok());
        assert_eq!(cut.job("report.pdf").duplex, Some(spool::Duplex::Off));
        cut.printer = Some("Duplexer".into());
        assert!(cut.settings(4, &[]).is_err(), "still refused where it would print two-sided");
    }

    /// #5772: a booklet's both sides ask the printer for the short edge, unless its driver doesn't
    /// take two-sided settings: it would refuse the job, so its own setting (manual two-sided, say)
    /// prints it. One side on its own stays one-sided.
    #[test]
    fn a_booklet_leaves_two_sided_to_a_driver_that_keeps_its_own() {
        // Listed as the dialog lists them (#3554), drivers unasked; the answers for the printers
        // picked arrive in `two_sided`, and the network printer's never came.
        let printer = |name: &str| spool::Printer { name: name.into(), default: false, two_sided: None, description: String::new() };
        let mut d = PrintDraft {
            printers: vec![printer("Epson"), printer("Laser"), printer("Network")],
            two_sided: [("Epson".to_string(), Some(false)), ("Laser".to_string(), Some(true))].into_iter().collect(),
            handling: Handling::Booklet,
            ..PrintDraft::default()
        };
        for (name, both) in [("Epson", None), ("Laser", Some(spool::Duplex::ShortEdge)), ("Network", Some(spool::Duplex::ShortEdge))] {
            d.printer = Some(name.into());
            d.booklet_subset = BookletSubset::BothSides;
            assert_eq!(d.job("a.pdf").duplex, both, "{name}");
            d.booklet_subset = BookletSubset::FrontOnly;
            assert_eq!(d.job("a.pdf").duplex, Some(spool::Duplex::Off), "{name}");
        }
        // Save as PDF has no driver: as before.
        d.printer = None;
        d.booklet_subset = BookletSubset::BothSides;
        assert_eq!(d.job("a.pdf").duplex, Some(spool::Duplex::ShortEdge));
    }

    /// A job being spooled holds the progress card, counting its sheets as the spooler reports
    /// them; the card goes when the job ends and its answer arrives as a notice. A card another
    /// long job is showing is left alone.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_progress_card_counts_the_sheets_of_a_print_job() {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        app.print_tasks.jobs.push(PrintJob { printer: "Office".into(), sheets: None, news: rx });
        app.poll_print();
        assert_eq!(app.progress_notice.as_ref().map(|p| p.label.as_str()), Some("Sending to Office…"));

        tx.send(JobNews::Sent(0, 3)).unwrap();
        tx.send(JobNews::Sent(1, 3)).unwrap();
        app.poll_print();
        let shown = app.progress_notice.clone().unwrap();
        assert_eq!(shown.label, "Sending sheet 2 of 3 to Office…");
        assert!((shown.fraction - 1.0 / 3.0).abs() < 1e-4 && !shown.cancellable, "{shown:?}");

        tx.send(JobNews::Sent(3, 3)).unwrap();
        tx.send(JobNews::Done(Ok(String::new()))).unwrap();
        app.poll_print();
        assert!(app.progress_notice.is_none() && app.print_tasks.jobs.is_empty());
        assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("Sent to Office"));

        // Another job's card stays; a worker that vanished is reported, not left spinning.
        let other = crate::widgets::ProgressNotice { label: "Optimizing…".into(), fraction: 0.5, cancellable: true };
        app.progress_notice = Some(other.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        app.print_tasks.jobs.push(PrintJob { printer: "Office".into(), sheets: None, news: rx });
        tx.send(JobNews::Sent(1, 2)).unwrap();
        app.poll_print();
        assert_eq!(app.progress_notice, Some(other));
        drop(tx);
        app.poll_print();
        assert!(app.print_tasks.jobs.is_empty() && app.progress_notice.is_some());
        assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("the print job stopped unexpectedly"));
    }

    /// Issue #1308: Current page is the page on screen, even when a later page is labelled with
    /// its number. A typed range still reads labels first.
    #[test]
    fn current_page_is_the_viewed_page_not_a_label() {
        let labels: Vec<String> = ["i", "ii", "iii", "1", "2"].map(String::from).to_vec();
        for (current, want) in [(0, vec![0]), (1, vec![1]), (3, vec![3])] {
            let d = PrintDraft { which: Which::Current, current_page: current, ..PrintDraft::default() };
            assert_eq!(d.settings(5, &labels).map(|s| s.pages), Ok(want), "viewing page {}", current + 1);
        }
        let typed = PrintDraft { which: Which::Range, range: "1".into(), ..PrintDraft::default() };
        assert_eq!(typed.settings(5, &labels).map(|s| s.pages), Ok(vec![3]));
    }

    #[test]
    fn print_preview_rasters_stay_within_the_texture_limit() {
        // Actual size on a 14,400 pt (200 in) page would ask for a raster far past any texture.
        let d = PrintDraft { size: SizeMode::Actual, ..PrintDraft::default() };
        let sizes = vec![(14_400.0, 14_400.0); 2];
        let labels = vec!["1".into(), "2".into()];
        for max_side in [2048.0, 8192.0, 16384.0, f32::NAN, 0.0] {
            for (page, scale) in preview_rasters(&d, &sizes, &labels, 4.0, max_side) {
                let long = 14_400.0 * scale;
                let cap = if max_side.is_finite() && max_side >= 1.0 { max_side.min(PREVIEW_MAX_SIDE) } else { PREVIEW_MAX_SIDE };
                assert!(long <= cap + 1.0, "page {page} at {max_side}: {long} px");
            }
        }
    }

    #[test]
    fn print_preview_rasters_keep_the_current_sheet_within_the_byte_budget() {
        // Many large pages: the current sheet always gets its raster; neighbours only while the
        // budget lasts, and the total stays within it.
        let d = PrintDraft { sheet: 5, ..PrintDraft::default() };
        let sizes = vec![(2_000.0, 2_000.0); 12];
        let labels: Vec<String> = (1..=12).map(|n| n.to_string()).collect();
        let rasters = preview_rasters(&d, &sizes, &labels, 4.0, 16384.0);
        assert_eq!(rasters.first().map(|r| r.0), Some(5), "the sheet on screen comes first: {rasters:?}");
        let total: f64 = rasters.iter().map(|(_, s)| (2_000.0 * f64::from(*s)).ceil().powi(2) * 4.0).sum();
        assert!(total <= PREVIEW_BYTES || rasters.len() == 1, "{total} bytes for {rasters:?}");
    }

    #[test]
    fn print_preview_is_sharper_than_a_thumbnail_on_a_high_dpi_screen() {
        let d = PrintDraft::default();
        let sizes = vec![(612.0, 792.0); 3];
        let labels = vec!["1".into(), "2".into(), "3".into()];
        let rasters = preview_rasters(&d, &sizes, &labels, 2.0, 8192.0);
        let scale = rasters.iter().find(|(page, _)| *page == 0).map(|(_, s)| *s).unwrap();
        // A 132 pt thumbnail at 2× is about 0.43 px/pt. The preview pane needs roughly twice that.
        assert!(scale > 0.7, "letter page in the preview at 2 px/pt: {scale}");
        assert!(rasters.iter().any(|(page, _)| *page == 1), "the next sheet is ready");
        let mut later = d.clone();
        later.sheet = 2;
        let rasters = preview_rasters(&later, &sizes, &labels, 2.0, 8192.0);
        assert!(rasters.iter().any(|(page, _)| *page == 2));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn multi_printer_cluster_dispatches_unified_settings_and_k_only_grayscale_without_mutating_source() {
        use pdfcraft_cos::{Dict, Document, Object, SaveOptions, Stream, write_full};
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        let mut cos = Document::new_empty();
        let pages_ref = cos
            .root()
            .and_then(|r| cos.get(r).as_dict().and_then(|d| d.reference(b"Pages")))
            .unwrap();
        let mut kids = Vec::new();
        for _ in 0..2 {
            let stream = cos.add(Object::Stream(Stream::from_raw(
                Dict::new(),
                b"1 0 0 rg 40 40 150 150 re f".to_vec(),
            )));
            let mut page = Dict::new();
            page.set("Type", Object::name("Page"));
            page.set("Parent", Object::Ref(pages_ref));
            page.set("MediaBox", Object::Array([0, 0, 595, 842].map(Object::Int).to_vec()));
            page.set("Contents", Object::Ref(stream));
            kids.push(Object::Ref(cos.add(Object::Dict(page))));
        }
        cos.update_dict(pages_ref, |d| {
            d.set("Count", Object::Int(2));
            d.set("Kids", Object::Array(kids));
        })
        .unwrap();
        let original_pdf = write_full(&cos, &SaveOptions::default()).unwrap();

        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.open_bytes("invoice.pdf", None, original_pdf.clone()).unwrap();

        type CapturedJobs = HashMap<String, (spool::Job, Vec<u8>)>;
        let captured: Arc<Mutex<CapturedJobs>> = Arc::new(Mutex::new(HashMap::new()));
        let captured_cb = captured.clone();
        app.spooler = Some(Arc::new(move |bytes, job| {
            let printer = job.printer.clone().unwrap_or_default();
            captured_cb
                .lock()
                .unwrap()
                .insert(printer, (job.clone(), bytes.to_vec()));
            Ok("ok".to_string())
        }));

        app.print_draft.printers = vec![
            spool::Printer { name: "Office_HP".into(), default: true, two_sided: Some(true), description: String::new() },
            spool::Printer { name: "Finance_Canon".into(), default: false, two_sided: Some(true), description: String::new() },
            spool::Printer { name: "Warehouse_Simplex".into(), default: false, two_sided: Some(false), description: String::new() },
        ];
        app.print_draft.multi_printer_mode = true;
        app.print_draft.selected_printers.insert("Office_HP".into());
        app.print_draft.selected_printers.insert("Finance_Canon".into());
        app.print_draft.selected_printers.insert("Warehouse_Simplex".into());
        app.print_draft.copies = 3;
        app.print_draft.collate = true;
        app.print_draft.grayscale = true;
        app.print_draft.duplex = Some(spool::Duplex::LongEdge);

        assert!(app.print_now(), "cluster print_now must succeed");
        assert_eq!(app.print_tasks.coordinator_handles.len(), 1);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app.print_tasks.coordinator_handles.is_empty() && std::time::Instant::now() < deadline {
            app.poll_print();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.print_tasks.coordinator_handles.is_empty(), "all cluster workers must complete");

        let records = captured.lock().unwrap();
        assert_eq!(records.len(), 3, "all 3 selected printers must receive the batch simultaneously");
        for name in ["Office_HP", "Finance_Canon", "Warehouse_Simplex"] {
            let (job, spooled_bytes) = records.get(name).expect(name);
            assert_eq!(job.copies, 3);
            assert!(job.collate);
            assert!(job.grayscale);
            assert!(job.grey_pages);
            assert_eq!(job.title, "invoice.pdf");
            assert!(!spooled_bytes.is_empty());
        }
        assert_eq!(records["Office_HP"].0.duplex, Some(spool::Duplex::LongEdge));
        assert_eq!(records["Finance_Canon"].0.duplex, Some(spool::Duplex::LongEdge));
        assert_eq!(
            records["Warehouse_Simplex"].0.duplex, None,
            "simplex-only printer must strip incompatible duplex flag safely"
        );

        // Verify original document bytes in session remain 100% untouched
        let (_, doc_id) = app.active_ids().unwrap();
        let active_doc = app.session.get(doc_id).unwrap();
        assert_eq!(active_doc.bytes.as_slice(), original_pdf.as_slice());

        // Verify diagnostic report contains all 3 printers and execution traces
        let diag = body::collect_ui_diagnostic_report(&app.print_draft);
        assert_eq!(diag.printers.len(), 3);
        assert_eq!(diag.applied_tickets.len(), 3);
        assert!(!diag.execution_traces.is_empty());
        let md = diag.to_markdown_table();
        assert!(md.contains("Office_HP") && md.contains("Finance_Canon") && md.contains("Warehouse_Simplex"));
    }

    #[test]
    fn broadcast_mode_stamps_unified_specs_without_leaking_driver_choices_or_vendor_paper_names() {
        let mut d = PrintDraft {
            printers: vec![
                spool::Printer { name: "Canon_Main".into(), default: true, two_sided: Some(true), description: String::new() },
                spool::Printer { name: "HP_Floor2".into(), default: false, two_sided: Some(true), description: String::new() },
                spool::Printer { name: "Brother_Simplex".into(), default: false, two_sided: Some(false), description: String::new() },
            ],
            ..PrintDraft::default()
        };
        d.set_printer_selected("Canon_Main", true);
        d.set_printer_selected("HP_Floor2", true);
        d.set_printer_selected("Brother_Simplex", true);
        assert!(d.is_broadcast_active());

        // Simulate vendor-specific driver choices read from Canon_Main
        d.driver_options = Some(("Canon_Main".into(), Vec::new()));
        d.driver_choices.insert("CanonStaple".into(), "TopLeft".into());

        // Select standard A4 + Explicit Color + LongEdge + Landscape + 4 copies
        assert!(d.select_standard_paper("A4"));
        d.set_color_intent(ColorIntent::Color);
        d.set_unified_duplex(Some(spool::Duplex::LongEdge));
        d.set_unified_orientation(Orientation::Landscape);
        d.set_unified_copies(4);
        d.set_unified_collate(true);

        let canon_job = d.job_for_printer(Some("Canon_Main"), "batch.pdf");
        let hp_job = d.job_for_printer(Some("HP_Floor2"), "batch.pdf");
        let brother_job = d.job_for_printer(Some("Brother_Simplex"), "batch.pdf");

        // Canon keeps its own driver_choices; HP and Brother never receive Canon's proprietary option
        assert!(canon_job.options.iter().any(|(k, v)| k == "CanonStaple" && v == "TopLeft"));
        assert!(!hp_job.options.iter().any(|(k, _)| k == "CanonStaple"));
        assert!(!brother_job.options.iter().any(|(k, _)| k == "CanonStaple"));

        // Every printer receives explicit Color intent, exact A4 micrometer dimensions, and Landscape override
        for job in [&canon_job, &hp_job, &brother_job] {
            assert_eq!(job.copies, 4);
            assert!(job.collate);
            assert!(!job.grayscale);
            assert!(
                job.options
                    .iter()
                    .any(|(k, v)| k == spool::OPT_COLOR_INTENT && v == "color")
            );
            assert!(
                job.options
                    .iter()
                    .any(|(k, v)| k == spool::OPT_PAPER_UM && v == "210000x297000")
            );
            assert!(
                job.options
                    .iter()
                    .any(|(k, v)| k == spool::OPT_ORIENTATION && v == "landscape")
            );
        }
        assert_eq!(canon_job.duplex, Some(spool::Duplex::LongEdge));
        assert_eq!(hp_job.duplex, Some(spool::Duplex::LongEdge));
        assert_eq!(brother_job.duplex, None, "simplex printer strips incompatible duplex safely");

        // Vendor-specific custom paper on Canon falls back to exact dimensions (no proprietary keyword) on all cluster printers
        d.papers.push(("Canon_Tray3_Custom_9x13".into(), (648.0, 936.0)));
        let custom_idx = d.papers.len() - 1;
        d.select_paper_index(custom_idx);
        let hp_custom_job = d.job_for_printer(Some("HP_Floor2"), "batch.pdf");
        let media_val = hp_custom_job
            .options
            .iter()
            .find(|(k, _)| k == "media")
            .map(|(_, v)| v.as_str())
            .unwrap_or_default();
        assert!(
            !media_val.contains("Canon_Tray3"),
            "vendor-specific paper keyword must not leak to another printer: {media_val}"
        );
    }

    #[test]
    fn broadcast_mode_protects_unified_specs_from_single_printer_apply_setup_and_papers_loading() {
        let mut d = PrintDraft {
            printers: vec![
                spool::Printer { name: "Label_Zebra".into(), default: true, two_sided: Some(false), description: String::new() },
                spool::Printer { name: "Office_Laser".into(), default: false, two_sided: Some(true), description: String::new() },
            ],
            setups: vec![(
                "Label_Zebra".into(),
                PrinterSetup {
                    paper: "4 x 6 in".into(),
                    size: SizeMode::Actual,
                    custom_scale: 100.0,
                    duplex: Some(spool::Duplex::Off),
                },
            )],
            ..PrintDraft::default()
        };

        // Configure unified broadcast specs first, then select multiple printers
        assert!(d.select_standard_paper("A3"));
        d.set_color_intent(ColorIntent::Monochrome);
        d.set_unified_duplex(Some(spool::Duplex::LongEdge));
        d.set_printer_selected("Label_Zebra", true);
        d.set_printer_selected("Office_Laser", true);
        d.papers_loading = Some("Label_Zebra".into());

        // Broadcast mode never blocks Print on papers_loading and never clobbers A3 with Label_Zebra's old 4x6 setup
        assert!(!d.papers_loading());
        d.printer = Some("Label_Zebra".into());
        d.apply_setup();
        assert_eq!(d.paper_name(), "A3");
        assert_eq!(d.color_intent(), ColorIntent::Monochrome);
        assert_eq!(d.duplex, Some(spool::Duplex::LongEdge));

        // Recording cluster setup updates both per-printer setups for ALL targets and BroadcastSetup
        d.remember_cluster_setup();
        assert!(d.broadcast_setup.is_some());
        assert_eq!(d.setups.len(), 2);
        for (_, setup) in &d.setups {
            assert_eq!(setup.paper, "A3");
            assert_eq!(setup.duplex, Some(spool::Duplex::LongEdge));
        }
    }

    #[test]
    fn failover_candidates_exclude_virtual_save_printers_and_busy_batch_workers() {
        let mut d = PrintDraft {
            printers: vec![
                spool::Printer { name: "Floor1_HP".into(), default: true, two_sided: Some(true), description: String::new() },
                spool::Printer { name: "Floor1_Canon".into(), default: false, two_sided: Some(true), description: String::new() },
                spool::Printer { name: "Microsoft Print to PDF".into(), default: false, two_sided: Some(false), description: String::new() },
                spool::Printer { name: "Microsoft XPS Document Writer".into(), default: false, two_sided: Some(false), description: String::new() },
                spool::Printer { name: "Floor2_Backup".into(), default: false, two_sided: Some(true), description: String::new() },
            ],
            ..PrintDraft::default()
        };
        d.set_printer_selected("Floor1_HP", true);
        d.set_printer_selected("Floor1_Canon", true);

        let active: std::collections::BTreeSet<String> =
            ["Floor1_HP".to_string(), "Floor1_Canon".to_string()].into_iter().collect();
        let candidates = d.eligible_failover_printers("Floor1_HP", &active);
        assert_eq!(
            candidates,
            vec!["Floor2_Backup".to_string()],
            "idle physical backup comes first; virtual PDF/XPS queues and busy workers excluded"
        );
    }

    #[test]
    fn broadcast_setup_and_cluster_selection_round_trip_across_persist_and_restore() {
        let mut app = PdfCraftApp::new();
        app.print_draft.set_printer_selected("Office_Canon", true);
        app.print_draft.set_printer_selected("Finance_HP", true);
        app.print_draft.hide_virtual_printers = true;
        assert!(app.print_draft.select_standard_paper("A3"));
        app.print_draft.set_color_intent(ColorIntent::Color);
        app.print_draft.set_unified_duplex(Some(spool::Duplex::ShortEdge));
        app.print_draft.set_unified_orientation(Orientation::Landscape);
        app.print_draft.set_unified_size_mode(SizeMode::Custom(85.0));
        app.print_draft.set_unified_copies(6);
        app.print_draft.set_unified_collate(false);

        let saved = app.persist();
        let mut restored = PdfCraftApp::new();
        restored.restore(&saved);

        let d = &restored.print_draft;
        assert!(d.multi_printer_mode);
        assert!(d.hide_virtual_printers);
        assert_eq!(d.selected_printers.len(), 2);
        assert_eq!(d.paper_name(), "A3");
        assert_eq!(d.color_intent(), ColorIntent::Color);
        assert_eq!(d.duplex, Some(spool::Duplex::ShortEdge));
        assert_eq!(d.orientation, Orientation::Landscape);
        assert_eq!(d.size, SizeMode::Custom(85.0));
        assert_eq!(d.copies, 6);
        assert!(!d.collate);
        assert!(d.broadcast_setup.is_some());
    }
}
