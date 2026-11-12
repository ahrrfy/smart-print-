use super::setups::{self, BroadcastSetup, PrinterSetup};
use super::{Binding, BookletSubset, Content, Layout, Orientation, PageOrder, SizeMode, Subset, print, spool};
use pdfcraft_engine::print::PAPERS;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    All,
    Current,
    Range,
    /// The pages picked in the Pages panel or the organize grid when the dialog opened.
    Selected,
    /// Only the area chosen with Snapshot (`PrintDraft::area`).
    Area,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handling {
    Size,
    Poster,
    Multiple,
    Booklet,
    /// Shipping label: cut to the printed area, turned upright, one per sheet.
    Label,
}

/// Explicit color intent enforced across all target printers during broadcast printing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColorIntent {
    #[default]
    Color,
    Monochrome,
}

/// Summary of the unified specifications and hardware adaptations applied to one target printer.
#[derive(Clone, Debug, PartialEq)]
pub struct PrinterBroadcastSummary {
    pub printer_name: String,
    pub is_installed: bool,
    pub is_virtual_save_printer: bool,
    pub is_default: bool,
    pub paper_name: String,
    pub paper_size_pt: (f64, f64),
    pub paper_size_um: (u32, u32),
    pub color_intent: ColorIntent,
    pub requested_duplex: Option<spool::Duplex>,
    pub effective_duplex: Option<spool::Duplex>,
    pub duplex_fallback_to_simplex: bool,
    pub orientation: Orientation,
    pub size_mode: SizeMode,
    pub copies: u32,
    pub collate: bool,
}

/// Standard paper names offered as 1-click quick pills in the Unified Broadcast Specs pane.
pub const QUICK_STANDARD_PAPERS: [&str; 5] = ["A4", "A3", "US Letter", "US Legal", "A5"];

/// The percent a custom scale can be (the dialog's field and a saved setting).
pub const CUSTOM_SCALE: std::ops::RangeInclusive<f64> = 1.0..=1000.0;

/// The resolutions the Print dialog offers for Print as image, in dots per inch.
pub const IMAGE_DPI: [f64; 3] = [150.0, 300.0, 600.0];

/// Converts Typographic points (`1/72 in`) to micrometers (`1 µm = 1/25_400 in`).
pub fn points_to_um(pt: f64) -> u32 {
    if !pt.is_finite() || pt <= 0.0 {
        return 0;
    }
    ((pt * 25_400.0) / 72.0).round() as u32
}

/// Converts micrometers (`1 µm = 1/25_400 in`) to Typographic points (`1/72 in`).
pub fn um_to_points(um: u32) -> f64 {
    (f64::from(um) * 72.0) / 25_400.0
}

type CanonicalPaperEntry = (&'static str, (f64, f64), (u32, u32));

fn canonical_paper_um(name: &str, size_pt: (f64, f64)) -> (u32, u32) {
    let known: &[CanonicalPaperEntry] = &[
        ("A4", (595.28, 841.89), (210_000, 297_000)),
        ("A3", (841.89, 1190.55), (297_000, 420_000)),
        ("A5", (419.53, 595.28), (148_000, 210_000)),
        ("B4", (708.66, 1000.63), (250_000, 353_000)),
        ("B5", (498.90, 708.66), (176_000, 250_000)),
        ("US Letter", (612.0, 792.0), (215_900, 279_400)),
        ("US Legal", (612.0, 1008.0), (215_900, 355_600)),
        ("Legal", (612.0, 1008.0), (215_900, 355_600)),
        ("Tabloid", (792.0, 1224.0), (279_400, 431_800)),
        ("Executive", (522.0, 756.0), (184_150, 266_700)),
        ("Statement", (396.0, 612.0), (139_700, 215_900)),
        ("4 x 6 in label (101.6 x 152.4 mm)", (288.0, 432.0), (101_600, 152_400)),
    ];
    for &(canon, ref_pt, exact_um) in known {
        if name == canon && (size_pt.0 - ref_pt.0).abs() <= 1.0 && (size_pt.1 - ref_pt.1).abs() <= 1.0 {
            return exact_um;
        }
    }
    (points_to_um(size_pt.0), points_to_um(size_pt.1))
}

#[derive(Clone, Debug, PartialEq)]
pub struct PrintDraft {
    pub printers: Vec<spool::Printer>,
    /// The unit the sheet size is shown in (Preferences).
    pub units: crate::units::Unit,
    /// The printers are still being listed in the background (see [`PrinterSource`]).
    pub printers_loading: bool,
    /// The user chose the printer, so a list arriving late doesn't move it to the default.
    pub printer_picked: bool,
    /// `None` = Save as PDF.
    pub printer: Option<String>,
    pub copies: u32,
    pub collate: bool,
    pub grayscale: bool,
    /// `None` = the printer's own default (its Properties), as Acrobat does.
    pub duplex: Option<spool::Duplex>,
    pub which: Which,
    pub range: String,
    /// The pages `Which::Selected` prints (0-based, in page order); empty when none were picked.
    pub selected: Vec<usize>,
    /// The area Snapshot's Print area… asked for: (page, rectangle in display space, points, y up).
    pub area: Option<(usize, [f64; 4])>,
    pub subset: Subset,
    pub reverse: bool,
    pub handling: Handling,
    pub size: SizeMode,
    pub custom_scale: f64,
    pub per_sheet: usize,
    /// Pages per sheet: Custom, `cols` across by `rows` down instead of `per_sheet`.
    pub custom_grid: bool,
    pub cols: usize,
    pub rows: usize,
    pub order: PageOrder,
    pub border: bool,
    pub auto_rotate: bool,
    pub booklet_subset: BookletSubset,
    pub binding: Binding,
    /// Booklet creep compensation, points per sheet.
    pub creep: f64,
    pub poster_scale: f64,
    pub overlap: f64,
    pub cut_marks: bool,
    pub orientation: Orientation,
    pub content: Content,
    /// Print as image (Acrobat's Advanced Print Setup) at `dpi`.
    pub as_image: bool,
    pub dpi: f64,
    /// Index into the paper list: the standard sizes, then `custom_papers`.
    pub paper: usize,
    /// Paper sizes added with Custom…, kept with the settings.
    pub custom_papers: Vec<crate::print_paper::CustomPaper>,
    /// The custom paper being added: name, width and height as typed (the preferences' unit).
    pub custom_new: Option<(String, String, String)>,
    /// The paper chosen before Label put label stock in, so leaving Label gives it back (#3564).
    pub paper_before_label: Option<usize>,
    /// Whether the user has explicitly chosen or overridden the paper size.
    pub paper_overridden: bool,
    /// The list of papers available for selection (standard PAPERS plus any detected from the driver).
    pub papers: Vec<(String, (f64, f64))>,
    /// The sheet shown in the preview (0-based).
    pub sheet: usize,
    pub current_page: usize,
    /// The printer driver's own options and the printer they were read for (Properties…, CUPS).
    pub driver_options: Option<(String, Vec<spool::PrinterOption>)>,
    /// Driver options set away from the printer's defaults, for `driver_options`' printer.
    pub driver_choices: BTreeMap<String, String>,
    pub show_driver_options: bool,
    /// Why the printer's preferences window didn't open (Windows).
    pub driver_error: Option<String>,
    /// Label: turn a further 180°.
    pub label_flip: bool,
    /// Label: each page's printed area and text direction, read when Label is chosen
    /// ([`PdfCraftApp::read_label_crops`]); empty until then.
    pub crops: Vec<Option<print::Crop>>,
    /// The pages `crops` were read for, so a changed page selection reads them again.
    pub crop_pages: Option<Vec<usize>>,
    /// The printer whose options are being read in the background (CUPS runs `lpoptions`,
    /// which a stuck CUPS can hold up; #1818). The panel says so meanwhile.
    pub driver_options_loading: Option<String>,
    /// Properties… asked for this printer's options; the app starts the read on the next frame.
    pub read_options_for: Option<String>,
    /// The printer whose papers and default paper are being read in the background.
    pub papers_loading: Option<String>,
    /// A printer change requested reading papers; the app starts the read on the next frame.
    pub read_papers_for: Option<String>,
    /// Whether each printer's driver takes two-sided settings, as read in the background for
    /// the printer picked (see [`TwoSidedSource`](super::TwoSidedSource)); `None` when it couldn't be read.
    pub two_sided: BTreeMap<String, Option<bool>>,
    /// The paper, sizing and two-sided choice each printer was last printed with, the printer
    /// printed to last first; kept across restarts (#2001).
    pub setups: Vec<(String, PrinterSetup)>,

    // --- Production Print Studio & Unified Broadcast Extensions ---
    /// Enable multi-printer simultaneous batch dispatch.
    pub multi_printer_mode: bool,
    /// The set of chosen printers when in multi-printer mode.
    pub selected_printers: BTreeSet<String>,
    /// Which printer's hardware printable area and margins are being previewed.
    pub active_preview_printer: Option<String>,
    /// Job presets and printer groups store.
    pub preset_store: crate::workflows::print_presets::PrintPresetStore,
    /// Physical hardware margins of the active preview printer (in points: [left, top, right, bottom]).
    pub preview_hardware_margins: Option<[f64; 4]>,
    /// Printable area of the active preview printer (width, height in points).
    pub preview_printable_area: Option<(f64, f64)>,
    /// Active clipping hazard detected (overflow amounts: [left, top, right, bottom] in points).
    pub clipping_hazard: Option<[f64; 4]>,
    /// Active failover modal dialog state, if any.
    pub failover_modal: Option<FailoverModalState>,
    /// Pending failover reroute request: (from_printer, to_printer).
    pub pending_failover: Option<(String, String)>,
    /// Single-slot legacy capability trigger kept for compatibility alongside `pending_caps_queue`.
    pub read_caps_for: Option<String>,
    /// Non-blocking FIFO queue of printers whose physical hardware capabilities should be queried off the UI thread.
    pub pending_caps_queue: VecDeque<String>,
    /// Cached hardware capabilities per printer for pre-flight checks, margin preview, and diagnostic export.
    pub cached_capabilities: BTreeMap<String, pdfcraft_platform::diagnostics::PrinterCapabilitiesTelemetry>,
    /// Recent execution traces (dispatch, stalls, hardware faults, failovers) for one-click diagnostic export.
    pub diagnostic_traces: Vec<pdfcraft_platform::diagnostics::TraceRecord>,
    /// Optional inline input when saving selected cluster printers as a reusable group.
    pub new_group_name: Option<String>,
    /// Optional inline input when saving current settings as a reusable job preset.
    pub new_preset_name: Option<String>,
    /// Persisted unified broadcast specifications across restarts for multi-printer workflows.
    pub broadcast_setup: Option<BroadcastSetup>,
    /// Whether unified broadcast specifications have been explicitly configured in this session,
    /// shielding them from being overwritten by single-printer `apply_setup` or async paper reads.
    pub unified_specs_edited: bool,
    /// Instant case-insensitive search filter for the Target Printers Dock.
    pub printer_search_query: String,
    /// Hide virtual document writers (`Microsoft Print to PDF`, `XPS`, `OneNote`, `Fax`) from the printer dock.
    pub hide_virtual_printers: bool,
}

/// State for the One-Click Print Failover modal dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct FailoverModalState {
    pub from_printer: String,
    pub to_printer: String,
    pub failed_sheet: usize,
    pub committed_sheets: usize,
    pub pending_sheets: usize,
    pub total_sheets: usize,
    pub error_message: String,
}

impl Default for PrintDraft {
    fn default() -> Self {
        PrintDraft {
            printers: Vec::new(),
            units: Default::default(),
            printers_loading: false,
            printer_picked: false,
            printer: None,
            copies: 1,
            collate: true,
            grayscale: false,
            duplex: None,
            which: Which::All,
            range: String::new(),
            selected: Vec::new(),
            area: None,
            subset: Subset::All,
            reverse: false,
            handling: Handling::Size,
            size: SizeMode::Fit,
            custom_scale: 100.0,
            per_sheet: 2,
            custom_grid: false,
            cols: 2,
            rows: 1,
            order: PageOrder::Horizontal,
            border: false,
            auto_rotate: true,
            booklet_subset: BookletSubset::BothSides,
            binding: Binding::Left,
            creep: 0.0,
            poster_scale: 200.0,
            overlap: 18.0,
            cut_marks: true,
            orientation: Orientation::Auto,
            content: Content::DocumentAndMarkups,
            as_image: false,
            dpi: 300.0,
            paper: 0,
            custom_papers: Vec::new(),
            custom_new: None,
            paper_before_label: None,
            paper_overridden: false,
            papers: default_papers(),
            sheet: 0,
            current_page: 0,
            driver_options: None,
            driver_choices: Default::default(),
            show_driver_options: false,
            driver_error: None,
            label_flip: false,
            crops: Vec::new(),
            crop_pages: None,
            driver_options_loading: None,
            read_options_for: None,
            papers_loading: None,
            read_papers_for: None,
            two_sided: Default::default(),
            setups: Vec::new(),
            multi_printer_mode: false,
            selected_printers: BTreeSet::new(),
            active_preview_printer: None,
            preset_store: crate::workflows::print_presets::PrintPresetStore::with_defaults(),
            preview_hardware_margins: None,
            preview_printable_area: None,
            clipping_hazard: None,
            failover_modal: None,
            pending_failover: None,
            read_caps_for: None,
            pending_caps_queue: VecDeque::new(),
            cached_capabilities: BTreeMap::new(),
            diagnostic_traces: Vec::new(),
            new_group_name: None,
            new_preset_name: None,
            broadcast_setup: None,
            unified_specs_edited: false,
            printer_search_query: String::new(),
            hide_virtual_printers: false,
        }
    }
}

impl PrintDraft {
    /// Whether multi-printer broadcast behavior is currently active.
    pub fn is_broadcast_active(&self) -> bool {
        self.multi_printer_mode || self.selected_printers.len() > 1
    }

    /// Marks unified specs as explicitly edited by the user, clearing any stale preset label
    /// and updating `broadcast_setup` when broadcasting.
    pub fn mark_unified_specs_edited(&mut self) {
        self.unified_specs_edited = true;
        self.preset_store.active_preset_id = None;
        if self.is_broadcast_active() {
            self.broadcast_setup = Some(self.snapshot_broadcast_setup());
        }
    }

    /// Current explicit color vs. monochrome choice.
    pub fn color_intent(&self) -> ColorIntent {
        if self.grayscale {
            ColorIntent::Monochrome
        } else {
            ColorIntent::Color
        }
    }

    /// Sets explicit color or monochrome mode and broadcasts it across all selected printers.
    pub fn set_color_intent(&mut self, intent: ColorIntent) {
        self.grayscale = matches!(intent, ColorIntent::Monochrome);
        self.mark_unified_specs_edited();
    }

    /// Convenience setter: `true` = Explicit Color (`DMCOLOR_COLOR`), `false` = Explicit B&W (`DMCOLOR_MONOCHROME`).
    pub fn set_explicit_color(&mut self, explicit_color: bool) {
        self.set_color_intent(if explicit_color { ColorIntent::Color } else { ColorIntent::Monochrome });
    }

    /// Current paper dimensions in micrometers `(width_um, height_um)`.
    pub fn paper_size_um(&self) -> (u32, u32) {
        let name = self.paper_name();
        canonical_paper_um(&name, self.paper_size())
    }

    /// Explicit landscape override (`Some(true)` = Landscape, `Some(false)` = Portrait, `None` = Auto).
    pub fn landscape_override(&self) -> Option<bool> {
        match self.orientation {
            Orientation::Portrait => Some(false),
            Orientation::Landscape => Some(true),
            Orientation::Auto => None,
        }
    }

    /// Standard paper names offered as 1-click quick pills in the Unified Broadcast Specs pane.
    pub fn quick_standard_papers() -> &'static [&'static str] {
        &QUICK_STANDARD_PAPERS
    }

    /// Instant (`0 ms`) standard paper selection by canonical name (e.g. `"A4"`, `"A3"`, `"US Letter"`, `"Letter"`, `"Legal"`, `"A5"`).
    /// Sets `paper_overridden = true` so background driver queries never block `Print` or overwrite the choice.
    pub fn select_standard_paper(&mut self, name: &str) -> bool {
        let normalized = if name.eq_ignore_ascii_case("letter") {
            "US Letter"
        } else if name.eq_ignore_ascii_case("legal") {
            "US Legal"
        } else {
            name
        };
        let found = self
            .papers
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(normalized))
            .or_else(|| {
                PAPERS
                    .iter()
                    .find(|(canon, _)| canon.eq_ignore_ascii_case(normalized))
                    .and_then(|(canon, sz)| self.find_or_add_paper((*canon).to_string(), *sz))
            });
        let Some(pos) = found else { return false };
        if self.handling == Handling::Label {
            self.paper_before_label = Some(pos);
        } else {
            self.paper = pos;
        }
        self.paper_overridden = true;
        self.sheet = 0;
        self.mark_unified_specs_edited();
        true
    }

    /// Selects a paper entry by index with explicit override protection.
    pub fn select_paper_index(&mut self, index: usize) {
        if index < self.paper_count() {
            if self.handling == Handling::Label {
                self.paper_before_label = Some(index);
            } else {
                self.paper = index;
            }
            self.paper_overridden = true;
            self.sheet = 0;
            self.mark_unified_specs_edited();
        }
    }

    /// Sets unified duplex binding across all selected printers.
    pub fn set_unified_duplex(&mut self, duplex: Option<spool::Duplex>) {
        self.duplex = duplex;
        self.mark_unified_specs_edited();
    }

    /// Sets unified page orientation across all selected printers.
    pub fn set_unified_orientation(&mut self, orientation: Orientation) {
        self.orientation = orientation;
        self.sheet = 0;
        self.mark_unified_specs_edited();
    }

    /// Sets unified page sizing mode across all selected printers.
    pub fn set_unified_size_mode(&mut self, size: SizeMode) {
        self.handling = Handling::Size;
        if let SizeMode::Custom(scale) = size {
            self.custom_scale = scale.clamp(*CUSTOM_SCALE.start(), *CUSTOM_SCALE.end());
            self.size = SizeMode::Custom(self.custom_scale);
        } else {
            self.size = size;
        }
        self.sheet = 0;
        self.mark_unified_specs_edited();
    }

    /// Sets unified copy count (`>= 1`) across all selected printers.
    pub fn set_unified_copies(&mut self, copies: u32) {
        self.copies = copies.max(1);
        self.mark_unified_specs_edited();
    }

    /// Sets unified collation flag across all selected printers.
    pub fn set_unified_collate(&mut self, collate: bool) {
        self.collate = collate;
        self.mark_unified_specs_edited();
    }

    /// Enqueues a non-blocking background hardware capability query for `printer`.
    pub fn enqueue_caps_query(&mut self, printer: impl Into<String>) {
        let name = printer.into();
        if name.is_empty()
            || name == crate::web_print::BROWSER_PRINTER
            || self.cached_capabilities.contains_key(&name)
            || self.pending_caps_queue.iter().any(|queued| queued == &name)
        {
            return;
        }
        self.pending_caps_queue.push_back(name);
    }

    /// Enqueues background capability queries for all selected printers and the preview target.
    pub fn enqueue_caps_for_selected(&mut self) {
        let targets: Vec<String> = self
            .selected_printers
            .iter()
            .chain(self.active_preview_printer.iter())
            .cloned()
            .collect();
        for printer in targets {
            self.enqueue_caps_query(printer);
        }
    }

    /// Returns printers filtered by `printer_search_query` and `hide_virtual_printers`.
    pub fn filtered_printers(&self) -> Vec<&spool::Printer> {
        let query = self.printer_search_query.trim().to_lowercase();
        self.printers
            .iter()
            .filter(|p| {
                if self.multi_printer_mode && p.name == crate::web_print::BROWSER_PRINTER {
                    return false;
                }
                if self.hide_virtual_printers && spool::is_virtual_save_printer(&p.name) {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                p.name.to_lowercase().contains(&query) || p.description.to_lowercase().contains(&query)
            })
            .collect()
    }

    /// Enables or disables multi-printer broadcast mode without clobbering unified specifications.
    pub fn set_multi_printer_mode(&mut self, enabled: bool) {
        let was_multi = self.multi_printer_mode;
        self.multi_printer_mode = enabled;
        if enabled && !was_multi {
            if self.selected_printers.is_empty()
                && let Some(ref primary) = self.printer
                && primary != crate::web_print::BROWSER_PRINTER
                && (!self.hide_virtual_printers || !spool::is_virtual_save_printer(primary))
            {
                self.selected_printers.insert(primary.clone());
            }
            if self.active_preview_printer.is_none() {
                self.active_preview_printer = self.selected_printers.iter().next().cloned().or_else(|| self.printer.clone());
            }
            if let Some(ref bs) = self.broadcast_setup.clone()
                && !self.unified_specs_edited
            {
                self.apply_broadcast_setup(bs);
            }
            self.enqueue_caps_for_selected();
        }
    }

    /// Selects or deselects a printer card in the Target Printers Dock, synchronizing mode and preview
    /// without invoking destructive `apply_setup()`.
    pub fn set_printer_selected(&mut self, printer_name: &str, selected: bool) {
        if printer_name.is_empty() || printer_name == crate::web_print::BROWSER_PRINTER {
            return;
        }
        if selected {
            self.selected_printers.insert(printer_name.to_string());
            self.enqueue_caps_query(printer_name);
        } else {
            self.selected_printers.remove(printer_name);
        }
        self.preset_store.active_group_id = None;

        if self.selected_printers.len() > 1 {
            self.multi_printer_mode = true;
            self.unified_specs_edited = true;
            self.broadcast_setup = Some(self.snapshot_broadcast_setup());
        } else if !self.multi_printer_mode && self.selected_printers.len() == 1 {
            self.printer = self.selected_printers.iter().next().cloned();
            self.printer_picked = true;
        }

        if self
            .active_preview_printer
            .as_ref()
            .is_none_or(|cur| !self.selected_printers.contains(cur))
        {
            self.active_preview_printer = self.selected_printers.iter().next().cloned();
            if let Some(preview) = self.active_preview_printer.clone() {
                self.set_preview_printer(preview);
            }
        }
    }

    /// Toggles selection of `printer_name` and returns its new selection state.
    pub fn toggle_printer_selection(&mut self, printer_name: &str) -> bool {
        let next = !self.selected_printers.contains(printer_name);
        self.set_printer_selected(printer_name, next);
        next
    }

    /// Selects all printers currently visible under `filtered_printers()`.
    pub fn select_all_filtered_printers(&mut self) {
        let names: Vec<String> = self
            .filtered_printers()
            .into_iter()
            .filter(|p| p.name != crate::web_print::BROWSER_PRINTER)
            .map(|p| p.name.clone())
            .collect();
        for name in names {
            self.selected_printers.insert(name);
        }
        self.preset_store.active_group_id = None;
        if self.selected_printers.len() > 1 {
            self.multi_printer_mode = true;
            self.unified_specs_edited = true;
        } else if self.selected_printers.len() == 1 && !self.multi_printer_mode {
            self.printer = self.selected_printers.iter().next().cloned();
            self.printer_picked = true;
        }
        if self.active_preview_printer.is_none() {
            self.active_preview_printer = self.selected_printers.iter().next().cloned();
        }
        self.enqueue_caps_for_selected();
    }

    /// Clears all selected cluster printers.
    pub fn clear_selected_printers(&mut self) {
        self.selected_printers.clear();
        self.preset_store.active_group_id = None;
    }

    /// Clears all printer cards in the Target Printers Dock so zero queues are checked.
    pub fn clear_printer_cards(&mut self) {
        self.multi_printer_mode = true;
        self.selected_printers.clear();
        self.preset_store.active_group_id = None;
    }

    /// Whether `printer_name`'s card is currently selected as an active print target.
    pub fn is_printer_card_selected(&self, printer_name: &str) -> bool {
        if self.multi_printer_mode || !self.selected_printers.is_empty() {
            self.selected_printers.contains(printer_name)
        } else {
            self.printer.as_deref() == Some(printer_name)
        }
    }

    /// Number of active physical/queue targets that will receive unified specifications.
    pub fn effective_target_count(&self) -> usize {
        if self.multi_printer_mode {
            self.dispatchable_target_printers().len()
        } else {
            usize::from(self.printer.is_some())
        }
    }

    /// Toggles a full-row printer card cleanly: seeds the active single printer when transitioning
    /// into multi-printer selection and preserves all unified broadcast specifications.
    pub fn click_printer_card(&mut self, printer_name: &str) -> bool {
        if printer_name.is_empty() || printer_name == crate::web_print::BROWSER_PRINTER {
            return false;
        }
        if !self.multi_printer_mode
            && self.selected_printers.is_empty()
            && let Some(ref cur) = self.printer
            && cur != crate::web_print::BROWSER_PRINTER
            && (!self.hide_virtual_printers || !spool::is_virtual_save_printer(cur))
        {
            self.selected_printers.insert(cur.clone());
        }
        let now_selected = if self.selected_printers.contains(printer_name) {
            self.selected_printers.remove(printer_name);
            false
        } else {
            self.selected_printers.insert(printer_name.to_string());
            self.enqueue_caps_query(printer_name);
            true
        };
        self.preset_store.active_group_id = None;

        match self.selected_printers.len() {
            0 => {
                self.multi_printer_mode = true;
            }
            1 => {
                let only = self.selected_printers.iter().next().cloned();
                self.printer = only.clone();
                self.printer_picked = true;
                if self.multi_printer_mode {
                    self.broadcast_setup = Some(self.snapshot_broadcast_setup());
                }
                if let Some(p) = only {
                    self.read_papers_for = Some(p);
                }
            }
            _ => {
                self.multi_printer_mode = true;
                self.unified_specs_edited = true;
                self.broadcast_setup = Some(self.snapshot_broadcast_setup());
            }
        }

        if self
            .active_preview_printer
            .as_ref()
            .is_none_or(|cur| !self.selected_printers.contains(cur))
            && let Some(preview) = self.selected_printers.iter().next().cloned()
        {
            self.set_preview_printer(preview);
        }
        now_selected
    }

    /// Switches immediately to "Save as PDF" mode.
    pub fn select_save_as_pdf(&mut self) {
        self.multi_printer_mode = false;
        self.selected_printers.clear();
        self.preset_store.active_group_id = None;
        self.printer = None;
        self.printer_picked = true;
    }

    /// Sets which printer's physical margins are rendered in the live sheet preview.
    pub fn set_preview_printer(&mut self, printer_name: impl Into<String>) {
        let name = printer_name.into();
        self.active_preview_printer = Some(name.clone());
        if let Some(caps) = self.cached_capabilities.get(&name).cloned() {
            self.apply_capabilities_telemetry(name, caps);
        } else {
            self.enqueue_caps_query(name);
        }
    }

    /// Appends a diagnostic trace record, capping the buffer at 250 entries.
    pub fn push_trace(&mut self, record: pdfcraft_platform::diagnostics::TraceRecord) {
        if self.diagnostic_traces.len() >= 250 {
            self.diagnostic_traces.remove(0);
        }
        self.diagnostic_traces.push(record);
    }

    /// Stores queried hardware capabilities for `printer_name`, updating live preview margins
    /// when `printer_name` matches the active preview or primary selected printer.
    pub fn apply_capabilities_telemetry(
        &mut self,
        printer_name: String,
        mut caps: pdfcraft_platform::diagnostics::PrinterCapabilitiesTelemetry,
    ) {
        if let Some(listed) = self.printers.iter().find(|p| p.name == printer_name) {
            caps.is_default = listed.default;
        }
        caps.supported_paper_count = self.papers.len();
        self.two_sided
            .entry(printer_name.clone())
            .or_insert(Some(caps.is_duplex_supported));

        let preview_target = if self.multi_printer_mode {
            self.active_preview_printer
                .as_deref()
                .or(self.printer.as_deref())
        } else {
            self.printer.as_deref()
        };
        if preview_target == Some(printer_name.as_str()) {
            self.preview_hardware_margins = Some([
                caps.hardware_margins_pt.left,
                caps.hardware_margins_pt.top,
                caps.hardware_margins_pt.right,
                caps.hardware_margins_pt.bottom,
            ]);
            self.preview_printable_area =
                Some((caps.printable_area_pt.width, caps.printable_area_pt.height));
        }
        self.cached_capabilities.insert(printer_name, caps);
    }

    /// Creates a `PrintJobPreset` snapshot from the current dialog choices (including paper label & dimensions).
    pub fn snapshot_as_preset(&self, id: String, name: String) -> crate::workflows::print_presets::PrintJobPreset {
        let color_mode = if self.grayscale {
            crate::workflows::print_presets::PresetColorMode::Monochrome
        } else {
            crate::workflows::print_presets::PresetColorMode::Color
        };
        let duplex = match self.duplex {
            Some(spool::Duplex::Off) => crate::workflows::print_presets::PresetDuplex::Simplex,
            Some(spool::Duplex::LongEdge) => crate::workflows::print_presets::PresetDuplex::LongEdge,
            Some(spool::Duplex::ShortEdge) => crate::workflows::print_presets::PresetDuplex::ShortEdge,
            None => crate::workflows::print_presets::PresetDuplex::PrinterDefault,
        };
        let orientation = match self.orientation {
            Orientation::Portrait => crate::workflows::print_presets::PresetOrientation::Portrait,
            Orientation::Landscape => crate::workflows::print_presets::PresetOrientation::Landscape,
            Orientation::Auto => crate::workflows::print_presets::PresetOrientation::Auto,
        };
        let fit_mode = match self.size {
            SizeMode::Fit => crate::workflows::print_presets::PresetFitMode::FitToPrintable,
            SizeMode::Actual => crate::workflows::print_presets::PresetFitMode::ActualSize100,
            SizeMode::Shrink => crate::workflows::print_presets::PresetFitMode::ShrinkOversized,
            SizeMode::Custom(_) => crate::workflows::print_presets::PresetFitMode::CustomScale(self.custom_scale as f32),
        };
        let paper_idx = if self.handling == Handling::Label {
            self.paper_before_label.unwrap_or(self.paper)
        } else {
            self.paper
        };
        let paper_name = self.paper_name_at(paper_idx).to_string();
        let paper_pt = self
            .papers
            .get(paper_idx)
            .map(|(_, sz)| *sz)
            .unwrap_or_else(|| self.paper_size());
        crate::workflows::print_presets::PrintJobPreset {
            id,
            name,
            description: "User-saved print preset".to_string(),
            is_built_in: false,
            copies: self.copies.max(1),
            color_mode,
            duplex,
            orientation,
            fit_mode,
            custom_scale_pct: matches!(self.size, SizeMode::Custom(_)).then_some(self.custom_scale as f32),
            collate: self.collate,
            paper_label: Some(paper_name.clone()),
            paper_size_um: Some(canonical_paper_um(&paper_name, paper_pt)),
        }
    }

    /// Saves the current unified draft choices (including paper label & micrometer dimensions)
    /// into `self.preset_store` and activates the saved preset.
    pub fn save_current_as_preset(
        &mut self,
        id: String,
        name: String,
    ) -> crate::workflows::print_presets::PrintJobPreset {
        let preset = self.snapshot_as_preset(id.clone(), name);
        self.preset_store.upsert_preset(preset.clone());
        self.preset_store.set_active_preset(Some(id));
        preset
    }

    /// Applies a saved job preset to the current draft configuration (including optional paper size).
    pub fn apply_preset(&mut self, preset: &crate::workflows::print_presets::PrintJobPreset) {
        self.copies = preset.copies.max(1);
        self.collate = preset.collate;
        self.grayscale = matches!(preset.color_mode, crate::workflows::print_presets::PresetColorMode::Monochrome);
        self.duplex = match preset.duplex {
            crate::workflows::print_presets::PresetDuplex::Simplex => Some(spool::Duplex::Off),
            crate::workflows::print_presets::PresetDuplex::LongEdge => Some(spool::Duplex::LongEdge),
            crate::workflows::print_presets::PresetDuplex::ShortEdge => Some(spool::Duplex::ShortEdge),
            crate::workflows::print_presets::PresetDuplex::PrinterDefault => None,
        };
        self.orientation = match preset.orientation {
            crate::workflows::print_presets::PresetOrientation::Auto => Orientation::Auto,
            crate::workflows::print_presets::PresetOrientation::Portrait => Orientation::Portrait,
            crate::workflows::print_presets::PresetOrientation::Landscape => Orientation::Landscape,
        };
        self.apply_preset_fit_mode(&preset.fit_mode, preset.custom_scale_pct);
        self.apply_preset_paper(preset.paper_label.as_deref(), preset.paper_size_um);
        self.unified_specs_edited = true;
        self.preset_store.active_preset_id = Some(preset.id.clone());
        if self.is_broadcast_active() {
            self.broadcast_setup = Some(self.snapshot_broadcast_setup());
        }
    }

    /// Applies a preset from `self.preset_store` by ID. Returns `true` if found and applied.
    pub fn apply_preset_by_id(&mut self, id: &str) -> bool {
        let Some(preset) = self.preset_store.get_preset(id).cloned() else {
            return false;
        };
        self.apply_preset(&preset);
        true
    }

    fn apply_preset_fit_mode(
        &mut self,
        fit_mode: &crate::workflows::print_presets::PresetFitMode,
        custom_scale_pct: Option<f32>,
    ) {
        match fit_mode {
            crate::workflows::print_presets::PresetFitMode::FitToPrintable => {
                self.handling = Handling::Size;
                self.size = SizeMode::Fit;
            }
            crate::workflows::print_presets::PresetFitMode::ActualSize100 => {
                self.handling = Handling::Size;
                self.size = SizeMode::Actual;
                self.custom_scale = 100.0;
            }
            crate::workflows::print_presets::PresetFitMode::ShrinkOversized => {
                self.handling = Handling::Size;
                self.size = SizeMode::Shrink;
            }
            crate::workflows::print_presets::PresetFitMode::CustomScale(s) => {
                self.handling = Handling::Size;
                let clamped = f64::from(*s).clamp(*CUSTOM_SCALE.start(), *CUSTOM_SCALE.end());
                self.size = SizeMode::Custom(clamped);
                self.custom_scale = clamped;
            }
        }
        if let Some(s) = custom_scale_pct {
            let clamped = f64::from(s).clamp(*CUSTOM_SCALE.start(), *CUSTOM_SCALE.end());
            self.custom_scale = clamped;
            if matches!(self.size, SizeMode::Custom(_)) {
                self.size = SizeMode::Custom(clamped);
            }
        }
    }

    fn apply_preset_paper(&mut self, label: Option<&str>, size_um: Option<(u32, u32)>) {
        if let Some(lbl) = label
            && let Some(idx) = self.paper_index(lbl)
        {
            self.paper = idx;
            self.paper_overridden = true;
            return;
        }
        if let Some((w_um, h_um)) = size_um {
            let size_pt = (um_to_points(w_um), um_to_points(h_um));
            let name = label.unwrap_or_default().to_string();
            if let Some(pos) = self.find_or_add_paper(name, size_pt) {
                self.paper = pos;
                self.paper_overridden = true;
            }
        }
    }

    /// Selects all available physical printers associated with a named printer group,
    /// updating `active_group_id` and filtering out disconnected or browser-only queues.
    pub fn select_printer_group(&mut self, group_id: &str) {
        let Some(raw_names) = self.preset_store.get_group(group_id).map(|g| g.printer_names.clone()) else {
            return;
        };
        let filtered: BTreeSet<String> = raw_names
            .into_iter()
            .filter(|name| {
                if name == crate::web_print::BROWSER_PRINTER {
                    return false;
                }
                if self.hide_virtual_printers && spool::is_virtual_save_printer(name) {
                    return false;
                }
                self.printers.is_empty() || self.printers.iter().any(|p| p.name == *name)
            })
            .collect();
        self.preset_store.set_active_group(Some(group_id.to_string()));
        self.selected_printers = filtered;
        if !self.selected_printers.is_empty() {
            self.multi_printer_mode = true;
            self.unified_specs_edited = true;
        }
        if self
            .active_preview_printer
            .as_ref()
            .is_none_or(|cur| !self.selected_printers.contains(cur))
        {
            self.active_preview_printer = self.selected_printers.iter().next().cloned();
        }
        self.enqueue_caps_for_selected();
    }

    /// Returns printers listed in `group_id` that are not currently installed/online in `self.printers`.
    pub fn missing_group_printers(&self, group_id: &str) -> Vec<String> {
        let Some(group) = self.preset_store.get_group(group_id) else {
            return Vec::new();
        };
        if self.printers.is_empty() {
            return Vec::new();
        }
        group
            .printer_names
            .iter()
            .filter(|name| !self.printers.iter().any(|p| p.name == **name))
            .cloned()
            .collect()
    }

    /// Evaluates if content bounds extend outside the printable area.
    /// Bounds are `[x0, y0, x1, y1]` in sheet coordinates.
    pub fn detect_clipping_hazard(&mut self, sheet_size: (f64, f64), placed_bounds: [f64; 4]) -> bool {
        let Some(margins) = self.preview_hardware_margins else {
            self.clipping_hazard = None;
            return false;
        };

        let [left, top, right, bottom] = margins;
        let printable_x0 = left;
        let printable_y0 = bottom;
        let printable_x1 = sheet_size.0 - right;
        let printable_y1 = sheet_size.1 - top;

        let [bx0, by0, bx1, by1] = placed_bounds;

        let ov_left = (printable_x0 - bx0).max(0.0);
        let ov_bottom = (printable_y0 - by0).max(0.0);
        let ov_right = (bx1 - printable_x1).max(0.0);
        let ov_top = (by1 - printable_y1).max(0.0);

        if ov_left > 0.5 || ov_bottom > 0.5 || ov_right > 0.5 || ov_top > 0.5 {
            self.clipping_hazard = Some([ov_left, ov_top, ov_right, ov_bottom]);
            true
        } else {
            self.clipping_hazard = None;
            false
        }
    }

    /// Automatically sets sizing to fit within the printable area, resolving clipping.
    pub fn auto_fit_to_printable(&mut self) {
        self.handling = Handling::Size;
        self.size = SizeMode::Fit;
        self.clipping_hazard = None;
    }
}

/// Standard fallback paper presets.
pub fn default_papers() -> Vec<(String, (f64, f64))> {
    PAPERS.iter().map(|(n, s)| (n.to_string(), *s)).collect()
}

impl PrintDraft {
    /// Find the index of a paper in `self.papers` matching `size`, allowing for orientation.
    pub fn matching_paper(&self, size: (f64, f64)) -> Option<usize> {
        print::matching_paper(size).or_else(|| {
            self.papers.iter().position(|(_, p)| {
                ((p.0 - size.0).abs() <= 0.5 && (p.1 - size.1).abs() <= 0.5) || ((p.0 - size.1).abs() <= 0.5 && (p.1 - size.0).abs() <= 0.5)
            })
        })
    }

    /// Whether automatic paper detection for the selected printer is in progress and unresolved.
    ///
    /// Never blocks submission when multi-printer broadcast mode is active.
    pub fn papers_loading(&self) -> bool {
        if self.is_broadcast_active() {
            return false;
        }
        let Some(p) = self.printer.as_deref() else { return false };
        !self.paper_overridden && (self.papers_loading.as_deref() == Some(p) || self.read_papers_for.as_deref() == Some(p))
    }

    /// Reset an automatically selected custom paper to a standard preset on printer change.
    ///
    /// If the currently selected paper does not match any standard preset and `paper_overridden` is false,
    /// resets `paper` to 0 (the standard default preset) so printer A's custom paper does not
    /// carry over when switching to printer B. Also resets `paper_before_label` if it refers to
    /// an unoverridden custom paper. Explicit user overrides (`paper_overridden == true`) are preserved.
    pub fn reset_unoverridden_custom_paper(&mut self) {
        if !self.paper_overridden {
            if print::matching_paper(self.paper_size()).is_none() {
                self.paper = 0;
            }
            if let Some(idx) = self.paper_before_label {
                let is_custom = self.papers.get(idx).is_none_or(|(_, s)| print::matching_paper(*s).is_none());
                if is_custom {
                    self.paper_before_label = Some(0);
                }
            }
        }
    }

    /// Refresh `self.papers` with driver-reported media sizes, preserving the current selection.
    pub fn update_printer_papers(&mut self) {
        self.update_printer_papers_with(None);
    }

    /// Refresh `self.papers` with driver-reported media sizes from an optional source or the driver.
    pub fn update_printer_papers_with(&mut self, source: Option<&super::PrinterPapersSource>) {
        let detected = if let Some(ref printer) = self.printer {
            if let Some(source) = source { source(printer) } else { spool::printer_papers(printer) }
        } else {
            Vec::new()
        };
        self.apply_printer_papers(detected);
    }

    /// Apply driver-reported media sizes to `self.papers`, preserving the current selection.
    pub fn apply_printer_papers(&mut self, detected: Vec<(String, (f64, f64))>) {
        let custom_paper_idx = self.paper.checked_sub(self.papers.len());
        let before_label_custom = self.paper_before_label.and_then(|idx| idx.checked_sub(self.papers.len()));
        let before_label = if before_label_custom.is_some() { None } else { self.paper_before_label.and_then(|idx| self.papers.get(idx).cloned()) };
        let current_name = self.paper_name().to_string();
        let current_size = self.paper_size();
        let mut papers = default_papers();
        for (name, size) in detected {
            if !(size.0 >= 72.0 && size.1 >= 72.0 && size.0.is_finite() && size.1.is_finite()) {
                continue;
            }
            let norm = (size.0.min(size.1), size.0.max(size.1));
            if !papers.iter().any(|(_, s)| {
                let s_norm = (s.0.min(s.1), s.0.max(s.1));
                (s_norm.0 - norm.0).abs() <= 0.5 && (s_norm.1 - norm.1).abs() <= 0.5
            }) {
                papers.push((name, size));
            }
        }
        self.papers = papers;
        if let Some(c_idx) = custom_paper_idx {
            if c_idx < self.custom_papers.len() {
                self.paper = self.papers.len() + c_idx;
            } else {
                self.paper = 0;
            }
        } else if let Some(pos) =
            self.papers.iter().position(|(n, s)| n == &current_name && (s.0 - current_size.0).abs() <= 0.5 && (s.1 - current_size.1).abs() <= 0.5)
        {
            self.paper = pos;
        } else if let Some(pos) = self.papers.iter().position(|(_, s)| (s.0 - current_size.0).abs() <= 0.5 && (s.1 - current_size.1).abs() <= 0.5) {
            self.paper = pos;
        } else {
            self.papers.push((current_name, current_size));
            self.paper = self.papers.len() - 1;
        }
        if let Some(c_idx) = before_label_custom {
            if c_idx < self.custom_papers.len() {
                self.paper_before_label = Some(self.papers.len() + c_idx);
            } else {
                self.paper_before_label = None;
            }
        } else {
            self.paper_before_label = before_label.map(|(name, size)| {
                if let Some(pos) = self.papers.iter().position(|(n, s)| n == &name && (s.0 - size.0).abs() <= 0.5 && (s.1 - size.1).abs() <= 0.5) {
                    pos
                } else if let Some(pos) = self.papers.iter().position(|(_, s)| (s.0 - size.0).abs() <= 0.5 && (s.1 - size.1).abs() <= 0.5) {
                    pos
                } else {
                    self.papers.push((name, size));
                    self.papers.len() - 1
                }
            });
        }
    }

    /// Auto-select the printer's configured default paper (from DEVMODE / driver settings).
    pub fn auto_select_default_paper(&mut self) {
        self.auto_select_default_paper_with(None);
    }

    /// Auto-select the printer's default paper using an optional source (or DEVMODE / driver settings).
    pub fn auto_select_default_paper_with(&mut self, source: Option<&super::DefaultPaperSource>) {
        if let Some(ref printer) = self.printer {
            let detected = if let Some(source) = source { source(printer) } else { spool::printer_default_paper(printer) };
            self.apply_default_paper(detected);
        }
    }

    /// Apply the detected default paper.
    pub fn apply_default_paper(&mut self, detected: Option<(String, (f64, f64))>) {
        if let Some((name, size)) = detected
            && let Some(pos) = self.find_or_add_paper(name, size)
        {
            if self.handling == Handling::Label {
                if !self.paper_overridden {
                    self.paper_before_label = Some(pos);
                }
            } else {
                self.paper = pos;
            }
        }
    }

    /// Locate `(name, size)` in `self.papers`, or append it if missing. Returns the paper index.
    pub fn find_or_add_paper(&mut self, name: String, size: (f64, f64)) -> Option<usize> {
        if !(size.0 >= 72.0 && size.1 >= 72.0 && size.0.is_finite() && size.1.is_finite()) {
            return None;
        }
        if let Some(pos) = print::matching_paper(size) {
            Some(pos)
        } else if !name.is_empty() {
            let norm = (size.0.min(size.1), size.0.max(size.1));
            if let Some(pos) = self.papers.iter().position(|(n, s)| {
                n.eq_ignore_ascii_case(&name) && {
                    let s_norm = (s.0.min(s.1), s.0.max(s.1));
                    (s_norm.0 - norm.0).abs() <= 1.0 && (s_norm.1 - norm.1).abs() <= 1.0
                }
            }) {
                Some(pos)
            } else if let Some(pos) = self.matching_paper(size) {
                Some(pos)
            } else {
                self.papers.push((name, size));
                Some(self.papers.len() - 1)
            }
        } else if let Some(pos) = self.matching_paper(size) {
            Some(pos)
        } else {
            let custom_name = format!("{:.1} x {:.1} pt", size.0, size.1);
            self.papers.push((custom_name, size));
            Some(self.papers.len() - 1)
        }
    }

    /// Apply paper size and orientation chosen in printer properties or page setup.
    pub fn apply_driver_paper(&mut self, name: String, size: (f64, f64)) {
        if let Some(pos) = self.find_or_add_paper(name, size) {
            self.paper_overridden = true;
            self.paper = pos;
            if size.0 > size.1 {
                self.orientation = Orientation::Landscape;
            } else if size.1 > size.0 {
                self.orientation = Orientation::Portrait;
            }
        }
    }

    /// Choose Page Sizing & Handling. Label loads label stock and remembers the paper it
    /// replaced; leaving Label gives that paper back, unless another one was picked meanwhile.
    pub fn set_handling(&mut self, handling: Handling) {
        let leaving_label = self.handling == Handling::Label && handling != Handling::Label;
        self.handling = handling;
        self.sheet = 0;
        if handling == Handling::Label {
            // A label goes on label stock.
            if self.paper != print::LABEL_PAPER {
                self.paper_before_label = Some(self.paper);
                self.paper = print::LABEL_PAPER;
            }
        } else if leaving_label
            && let Some(paper) = self.paper_before_label.take()
            && self.paper == print::LABEL_PAPER
        {
            self.paper = paper;
        }
    }

    /// Remember this draft's paper, sizing and two-sided choice for its printer, after printing
    /// to it (#2001). Save as PDF has none to remember.
    pub fn remember_setup(&mut self) {
        let Some(printer) = self.printer.clone() else { return };
        // Label's stock is that job's; the printer keeps the paper Label replaced.
        let paper = if self.handling == Handling::Label { self.paper_before_label.unwrap_or(self.paper) } else { self.paper };
        // By name, so a custom paper is found again however the list changes.
        let setup =
            PrinterSetup { paper: self.paper_name_at(paper).to_string(), size: self.size, custom_scale: self.custom_scale, duplex: self.duplex };
        setups::record(&mut self.setups, &printer, setup);
    }

    /// Captures the current unified broadcast specifications for session/restart persistence.
    pub fn snapshot_broadcast_setup(&self) -> BroadcastSetup {
        let paper_idx = if self.handling == Handling::Label {
            self.paper_before_label.unwrap_or(self.paper)
        } else {
            self.paper
        };
        let paper_name = self.paper_name_at(paper_idx).to_string();
        let paper_size_pt = self
            .papers
            .get(paper_idx)
            .map(|(_, sz)| *sz)
            .unwrap_or_else(|| self.paper_size());
        let keep_specs_across_docs = self
            .broadcast_setup
            .as_ref()
            .is_some_and(|prev| prev.keep_specs_across_docs);
        BroadcastSetup {
            paper: paper_name,
            paper_size_pt,
            explicit_color: !self.grayscale,
            duplex: self.duplex,
            orientation: self.orientation,
            size: self.size,
            custom_scale: self.custom_scale,
            copies: self.copies.max(1),
            collate: self.collate,
            keep_specs_across_docs,
        }
    }

    /// Restores unified broadcast specifications onto this draft without per-printer clobbering.
    pub fn apply_broadcast_setup(&mut self, setup: &BroadcastSetup) {
        self.grayscale = !setup.explicit_color;
        self.duplex = setup.duplex;
        self.orientation = setup.orientation;
        self.custom_scale = setup.custom_scale;
        self.size = match setup.size {
            SizeMode::Custom(_) => SizeMode::Custom(setup.custom_scale),
            other => other,
        };
        self.copies = setup.copies.max(1);
        self.collate = setup.collate;
        let matched = self
            .paper_index(&setup.paper)
            .or_else(|| self.find_or_add_paper(setup.paper.clone(), setup.paper_size_pt));
        if let Some(paper) = matched {
            if self.handling == Handling::Label {
                self.paper_before_label = Some(paper);
            } else {
                self.paper = paper;
            }
            self.paper_overridden = true;
        }
        self.sheet = 0;
    }

    /// Records the unified setup for every selected printer in a multi-printer cluster dispatch
    /// AND updates `self.broadcast_setup` so restarts restore the full unified configuration.
    pub fn remember_cluster_setup(&mut self) {
        let paper_idx = if self.handling == Handling::Label {
            self.paper_before_label.unwrap_or(self.paper)
        } else {
            self.paper
        };
        let per_printer = PrinterSetup {
            paper: self.paper_name_at(paper_idx).to_string(),
            size: self.size,
            custom_scale: self.custom_scale,
            duplex: self.duplex,
        };
        let targets = self.dispatchable_target_printers();
        setups::record_many(&mut self.setups, targets.iter().map(String::as_str), &per_printer);
        self.broadcast_setup = Some(self.snapshot_broadcast_setup());
    }

    /// Bring back what the chosen printer was last printed with, when it has been printed to
    /// (#2001); a printer never printed to keeps the dialog's choices.
    ///
    /// In multi-printer broadcast mode, single-printer history never overwrites the unified
    /// broadcast specifications.
    pub fn apply_setup(&mut self) {
        if self.is_broadcast_active() {
            if !self.unified_specs_edited
                && let Some(bs) = self.broadcast_setup.clone()
            {
                self.apply_broadcast_setup(&bs);
            }
            return;
        }
        let Some(printer) = self.printer.as_deref() else { return };
        let Some(setup) = self.setups.iter().find(|(name, _)| name == printer).map(|(_, s)| s.clone()) else { return };
        self.size = setup.size;
        self.custom_scale = setup.custom_scale;
        self.duplex = setup.duplex;
        if let Some(paper) = self.paper_index(&setup.paper) {
            if self.handling == Handling::Label {
                self.paper_before_label = Some(paper);
            } else {
                self.paper = paper;
            }
            // The paper this printer was last printed on is a choice, as one made in the dialog:
            // the driver's default paper, read in the background (#5861), must not replace it.
            self.paper_overridden = true;
        }
        self.sheet = 0;
    }

    /// Applies a `CapturedDriverTemplate` (from a reference printer's native `DocumentPropertiesW` dialog)
    /// directly onto the unified broadcast draft so all selected printers inherit the exact specs.
    pub fn apply_captured_driver_template(&mut self, tpl: spool::CapturedDriverTemplate) {
        if let Some((w_um, h_um)) = tpl.paper_um
            && w_um > 0
            && h_um > 0
        {
            let raw_w = um_to_points(w_um);
            let raw_h = um_to_points(h_um);
            let portrait_pt = (raw_w.min(raw_h), raw_w.max(raw_h));
            let label = tpl.paper_name.unwrap_or_default();
            if let Some(pos) = self.find_or_add_paper(label, portrait_pt) {
                if self.handling == Handling::Label {
                    self.paper_before_label = Some(pos);
                } else {
                    self.paper = pos;
                }
                self.paper_overridden = true;
            }
        } else if let Some(ref label) = tpl.paper_name
            && let Some(pos) = self.paper_index(label)
        {
            self.paper = pos;
            self.paper_overridden = true;
        }

        if let Some(gray) = tpl.grayscale {
            self.grayscale = gray;
        }
        if let Some(duplex) = tpl.duplex {
            self.duplex = Some(duplex);
        }
        if let Some(is_landscape) = tpl.landscape {
            self.orientation = if is_landscape {
                Orientation::Landscape
            } else {
                Orientation::Portrait
            };
        }
        if let Some(copies) = tpl.copies {
            self.copies = copies.max(1);
        }
        if let Some(collate) = tpl.collate {
            self.collate = collate;
        }
        self.driver_error = None;
        self.sheet = 0;
        self.mark_unified_specs_edited();
    }

    /// Opens the native driver template dialog for `printer`, captures all normalized fields,
    /// and applies them onto this unified draft. Returns `Ok(true)` if applied, `Ok(false)` if cancelled.
    pub fn capture_driver_template_from(&mut self, printer: &str) -> Result<bool, String> {
        #[cfg(not(target_arch = "wasm32"))]
        pdfcraft_platform::install_print_bridge();
        match spool::capture_driver_template(printer) {
            Ok(Some(tpl)) => {
                self.apply_captured_driver_template(tpl);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(err) => {
                let msg = err.to_string();
                self.driver_error = Some(msg.clone());
                Err(msg)
            }
        }
    }

    /// The printer printed to last, while `printers` still lists it (#2001).
    pub fn last_printer<'a>(&self, printers: &'a [spool::Printer]) -> Option<&'a spool::Printer> {
        let (last, _) = self.setups.first()?;
        printers.iter().find(|p| p.name == *last)
    }

    /// The engine settings for this draft (page count and labels from the document).
    pub fn settings(&self, count: usize, labels: &[String]) -> Result<print::Settings, String> {
        let size = Layout::Size(match self.size {
            SizeMode::Custom(_) => SizeMode::Custom(self.custom_scale),
            m => m,
        });
        let paper = self.paper_size();
        let raster = self.raster();
        // A selected area prints alone, its one page sized like a page of its own: Size is its
        // only handling (the dialog offers no other while it is chosen).
        if self.which == Which::Area {
            let Some((page, area)) = self.area.filter(|(p, _)| *p < count) else {
                return Err("The selected area is no longer on a page of this document.".into());
            };
            return Ok(print::Settings {
                pages: vec![page],
                paper,
                orientation: self.orientation,
                layout: size,
                content: self.content,
                raster,
                crops: Vec::new(),
                area: Some(area),
            });
        }
        if self.cut_and_stack() && matches!(self.chosen_duplex(), Some(spool::Duplex::LongEdge | spool::Duplex::ShortEdge)) {
            return Err("Cut and stack needs Two-sided: Off. Print single-sided sheets.".into());
        }
        let range = match self.which {
            Which::All | Which::Selected | Which::Current | Which::Area => None,
            Which::Range => Some(self.range.clone()),
        };
        // Selected and Current are positions, not a typed range: a range would read numbers as
        // page labels first.
        let pages = match self.which {
            Which::Selected => print::select_listed(count, &self.selected, self.subset, self.reverse),
            Which::Current => print::select_listed(count, &[self.current_page], self.subset, self.reverse),
            Which::All | Which::Range | Which::Area => print::select_pages(count, range.as_deref(), labels, self.subset, self.reverse),
        }
        // What the person got wrong about what to print is said in their language: the engine hands
        // the wording over whole and the page numbers apart from it.
        .map_err(|e| e.said(&crate::i18n::words))?;
        let layout = match self.handling {
            Handling::Size => size,
            Handling::Multiple => {
                let grid = if self.custom_grid {
                    Layout::grid(self.cols, self.rows).ok_or_else(|| tl!("Columns and rows must each be 1 to 99.").to_string())?
                } else {
                    Layout::multiple(self.per_sheet)
                };
                match grid {
                    Layout::Multiple { cols, rows, per_sheet, custom, .. } => {
                        Layout::Multiple { cols, rows, per_sheet, order: self.order, border: self.border, auto_rotate: self.auto_rotate, custom }
                    }
                    other => other,
                }
            }
            Handling::Booklet => Layout::Booklet { subset: self.booklet_subset, binding: self.binding, creep: self.creep },
            Handling::Poster => Layout::Poster { scale: self.poster_scale, overlap: self.overlap, cut_marks: self.cut_marks },
            Handling::Label => Layout::Label { flip: self.label_flip },
        };
        Ok(print::Settings {
            pages,
            paper,
            orientation: self.orientation,
            layout,
            content: self.content,
            raster: self.raster(),
            crops: self.crops.clone(),
            area: None,
        })
    }

    /// Print as image: pictures at the chosen resolution, grey when grayscale is asked for.
    fn raster(&self) -> Option<print::Raster> {
        self.as_image.then_some(print::Raster { dpi: self.dpi, grayscale: self.grayscale, colour: None })
    }

    /// Whether multiple pages per sheet should be cut and stacked into sorted piles.
    fn cut_and_stack(&self) -> bool {
        self.handling == Handling::Multiple && self.order == PageOrder::CutStack
    }

    /// Whether `printer_name`'s driver accepts two-sided settings from external applications.
    pub fn printer_two_sided_for(&self, printer_name: &str) -> Option<bool> {
        let from_list = self
            .printers
            .iter()
            .find(|p| p.name == printer_name)
            .and_then(|p| p.two_sided);
        from_list
            .or_else(|| self.two_sided.get(printer_name).copied().flatten())
            .or_else(|| {
                self.cached_capabilities
                    .get(printer_name)
                    .map(|c| c.is_duplex_supported)
            })
    }

    /// Whether the chosen printer's driver takes two-sided settings from other programs:
    /// `Some(false)` for one that keeps its own (no two-sided printing, or only manual two-sided in
    /// its Properties); `None` when unknown, and for Save as PDF. The printer list says when it
    /// knows; on Windows it lists the queues without asking their drivers (#3554), and the answer
    /// for the printer picked arrives in [`PrintDraft::two_sided`].
    pub fn printer_two_sided(&self) -> Option<bool> {
        let p = self.printers.iter().find(|p| self.printer.as_ref() == Some(&p.name))?;
        self.printer_two_sided_for(&p.name)
    }

    /// The Two-sided choice adapted for a specific printer (`printer_name`), or for Save as PDF (`None`).
    fn chosen_duplex_for(&self, printer_name: Option<&str>) -> Option<spool::Duplex> {
        let two_sided = printer_name.and_then(|name| self.printer_two_sided_for(name));
        match self.duplex {
            Some(spool::Duplex::LongEdge | spool::Duplex::ShortEdge) if two_sided == Some(false) => None,
            chosen => chosen,
        }
    }

    /// The Two-sided choice as the chosen printer can take it: long or short edge is left out for
    /// a printer whose driver doesn't take two-sided settings from other programs, so its own
    /// setting applies instead of the job being refused (#3208). Off and the printer's default
    /// stay as chosen: one-sided is accepted by every driver.
    fn chosen_duplex(&self) -> Option<spool::Duplex> {
        self.chosen_duplex_for(self.printer.as_deref())
    }

    /// Returns the list of target printers for the current mode (cluster selection in multi-printer mode,
    /// or the single chosen printer otherwise).
    pub fn target_printers(&self) -> Vec<String> {
        if self.multi_printer_mode {
            self.selected_printers.iter().cloned().collect()
        } else {
            self.printer.iter().cloned().collect()
        }
    }

    /// Returns sanitized, currently installed physical/OS target printers ready for batch dispatch,
    /// stripping `BROWSER_PRINTER` and any uninstalled ghost queues left over from previous sessions.
    pub fn dispatchable_target_printers(&self) -> Vec<String> {
        let candidates = if self.multi_printer_mode {
            self.selected_printers.iter().cloned().collect::<Vec<_>>()
        } else {
            self.printer.iter().cloned().collect::<Vec<_>>()
        };
        candidates
            .into_iter()
            .filter(|name| {
                if name.is_empty() || name == crate::web_print::BROWSER_PRINTER {
                    return false;
                }
                if self.hide_virtual_printers && spool::is_virtual_save_printer(name) {
                    return false;
                }
                self.printers.is_empty() || self.printers.iter().any(|p| p.name == *name)
            })
            .collect()
    }

    /// Returns eligible automatic failover target printers for `faulted_printer`, excluding
    /// `faulted_printer`, `BROWSER_PRINTER`, virtual file-save queues (`spool::is_virtual_save_printer`),
    /// and printers already busy in `active_in_batch`.
    pub fn eligible_failover_printers(
        &self,
        faulted_printer: &str,
        active_in_batch: &BTreeSet<String>,
    ) -> Vec<String> {
        let mut idle_physical = Vec::new();
        let mut fallback_physical = Vec::new();
        for p in &self.printers {
            if p.name == faulted_printer
                || p.name == crate::web_print::BROWSER_PRINTER
                || spool::is_virtual_save_printer(&p.name)
            {
                continue;
            }
            if !active_in_batch.contains(&p.name) {
                idle_physical.push(p.name.clone());
            } else {
                fallback_physical.push(p.name.clone());
            }
        }
        if !idle_physical.is_empty() {
            idle_physical
        } else {
            fallback_physical
        }
    }

    /// Returns eligible failover printers for interactive modal selection (excluding `faulted_printer`
    /// and virtual save-to-file printers).
    pub fn failover_candidates(&self, faulted_printer: &str) -> Vec<String> {
        self.eligible_failover_printers(faulted_printer, &self.selected_printers)
    }

    /// Returns selected printers that are known simplex-only (`two_sided == Some(false)`) while a
    /// two-sided mode (`LongEdge` or `ShortEdge`) is requested.
    pub fn duplex_simplex_fallback_printers(&self) -> Vec<String> {
        let wants_two_sided = if self.handling == Handling::Booklet {
            self.booklet_subset == BookletSubset::BothSides
        } else {
            matches!(self.duplex, Some(spool::Duplex::LongEdge | spool::Duplex::ShortEdge))
        };
        if !wants_two_sided {
            return Vec::new();
        }
        self.target_printers()
            .into_iter()
            .filter(|name| self.printer_two_sided_for(name) == Some(false))
            .collect()
    }

    /// Builds a structured pre-flight broadcast summary for `printer_name` (used by `preview_pane.rs`).
    pub fn broadcast_summary_for(&self, printer_name: &str) -> PrinterBroadcastSummary {
        let listed = self.printers.iter().find(|p| p.name == printer_name);
        let is_installed = self.printers.is_empty() || listed.is_some();
        let is_default = listed.is_some_and(|p| p.default);
        let two_sided = self.printer_two_sided_for(printer_name);
        let requested_duplex = if self.cut_and_stack() {
            Some(spool::Duplex::Off)
        } else if self.handling == Handling::Booklet {
            match self.booklet_subset {
                BookletSubset::BothSides => Some(spool::Duplex::ShortEdge),
                BookletSubset::FrontOnly | BookletSubset::BackOnly => Some(spool::Duplex::Off),
            }
        } else {
            self.duplex
        };
        let effective_duplex = if self.cut_and_stack() {
            Some(spool::Duplex::Off)
        } else if self.handling == Handling::Booklet {
            print::booklet_job_duplex(self.booklet_subset, two_sided)
        } else {
            self.chosen_duplex_for(Some(printer_name))
        };
        let duplex_fallback_to_simplex = matches!(
            requested_duplex,
            Some(spool::Duplex::LongEdge | spool::Duplex::ShortEdge)
        ) && two_sided == Some(false);

        PrinterBroadcastSummary {
            printer_name: printer_name.to_string(),
            is_installed,
            is_virtual_save_printer: spool::is_virtual_save_printer(printer_name),
            is_default,
            paper_name: self.paper_name().to_string(),
            paper_size_pt: self.paper_size(),
            paper_size_um: self.paper_size_um(),
            color_intent: self.color_intent(),
            requested_duplex,
            effective_duplex,
            duplex_fallback_to_simplex,
            orientation: self.orientation,
            size_mode: self.size,
            copies: self.copies.max(1),
            collate: self.collate,
        }
    }

    /// Returns broadcast summaries for all active target printers.
    pub fn broadcast_summaries(&self) -> Vec<PrinterBroadcastSummary> {
        self.target_printers()
            .iter()
            .map(|name| self.broadcast_summary_for(name))
            .collect()
    }

    /// Stamps explicit unified broadcast metadata (`OPT_COLOR_INTENT`, `OPT_PAPER_UM`, `OPT_ORIENTATION`)
    /// onto `job` via `spool::set_job_broadcast_specs` for deterministic Win32 `DEVMODEW + PrintTicket` enforcement.
    pub fn apply_broadcast_specs_to_job(&self, job: &mut spool::Job) {
        spool::set_job_broadcast_specs(
            job,
            !self.grayscale,
            self.paper_size_um(),
            self.landscape_override(),
        );
    }

    /// Builds the unified spooler job tailored to `target_printer` (applying unified copies, collate,
    /// explicit color/monochrome intent, paper media & micrometer dimensions, and per-printer duplex safety,
    /// without leaking vendor-specific driver options or proprietary paper names across printers).
    pub fn job_for_printer(&self, target_printer: Option<&str>, title: &str) -> spool::Job {
        let two_sided = target_printer.and_then(|name| self.printer_two_sided_for(name));
        let paper_name = self.paper_name();
        let shareable_media_name = (!self.paper_is_custom()
            && (!self.multi_printer_mode || PAPERS.iter().any(|(canon, _)| *canon == paper_name)))
        .then(|| paper_name.to_string());

        let mut job = spool::Job {
            printer: target_printer.map(str::to_owned),
            copies: self.copies.max(1),
            collate: self.collate,
            // Cut and stack needs single-sided sheets, whatever the printer's default; a booklet
            // decides two-sided itself (#2707), as far as the driver takes it (#5772), leaving the
            // Two-sided choice as it was.
            duplex: if self.cut_and_stack() {
                Some(spool::Duplex::Off)
            } else if self.handling == Handling::Booklet {
                print::booklet_job_duplex(self.booklet_subset, two_sided)
            } else {
                self.chosen_duplex_for(target_printer)
            },
            scaling: spool::Scaling::Exact,
            grayscale: print::driver_needs_grayscale(self.grayscale, self.raster().as_ref()),
            // Set when the copy sent is made grey (`spooler_copy`, #5806).
            grey_pages: false,
            title: title.to_string(),
            // Only for the printer the options were read from (never leaked across printers).
            options: match (target_printer, &self.driver_options) {
                (Some(p), Some((read_for, _))) if p == read_for => {
                    self.driver_choices.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                }
                _ => Vec::new(),
            }
            .into_iter()
            // The sheet the PDF was laid out on, not the queue's default paper.
            // Custom or vendor-specific paper names from another printer fall back to exact dimensions.
            .chain(Some(print::paper_media_option(
                shareable_media_name.as_deref(),
                self.paper_size(),
            )))
            .collect(),
        };

        if self.multi_printer_mode {
            self.apply_broadcast_specs_to_job(&mut job);
        }

        job
    }

    /// The spooler job for this print: the dialog's copies, two-sided and colour settings, the
    /// driver options changed from the printer's defaults, and the chosen paper.
    pub fn job(&self, title: &str) -> spool::Job {
        self.job_for_printer(self.printer.as_deref(), title)
    }
}
