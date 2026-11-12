//! Print Studio top-level layout, shared form helpers (`driver_options_panel`, `combo`,
//! `custom_paper_row`), emoji-free `failover_modal_window`, and diagnostic telemetry assembly.

use super::draft::{Handling, PrintDraft};
use super::preview_pane::PreviewContext;
use super::{Color32, Orientation, PREVIEW_W, Tokens, print, spool, still_looking, theme, widgets};

/// The narrowest the title is still worth cutting: an ellipsis and a couple of pixels at the title's
/// font. Given less room than this, [`egui::Sides::truncate`] cannot cut any further and the label
/// is painted back over the buttons, which is the defect of #5545; the title is left out instead.
const TITLE_MIN_WIDTH: f32 = 16.0;

/// Responsive inner width of the Print Studio modal: `1160 pt` on standard desktop viewports
/// (`>= 1240 px`), scaling down smoothly to `760 pt` on narrow screens.
pub(crate) fn dialog_width(ctx: &egui::Context) -> f32 {
    let vw = ctx.input(|i| i.viewport_rect().width());
    let raw = if vw.is_finite() && vw > 0.0 { vw - 80.0 } else { 1160.0 };
    raw.clamp(760.0, 1160.0)
}

/// Responsive height of the 3-pane Print Studio body: `560 pt` on desktop monitors, shrinking
/// cleanly on short displays (`480 px`) so the title and `Print`/`Cancel` footer stay on screen.
pub(crate) fn dialog_body_height(ctx: &egui::Context) -> f32 {
    let vh = ctx.input(|i| i.viewport_rect().height());
    let raw = if vh.is_finite() && vh > 0.0 { vh - 210.0 } else { 580.0 };
    raw.clamp(260.0, 610.0)
}

/// The panel's own header: "Printer properties" on the left, Done and Reset on the right.
fn driver_options_header(
    ui: &mut egui::Ui,
    show: &mut bool,
    can_reset: bool,
) -> (Option<egui::Response>, egui::Response) {
    egui::Sides::new().shrink_left().truncate().show(
        ui,
        |ui| {
            let room = ui.available_width();
            (room >= TITLE_MIN_WIDTH).then(|| {
                ui.add(egui::Label::new(
                    egui::RichText::new(tl!("Printer properties")).font(theme::semibold(13.0)),
                ))
            })
        },
        |ui| {
            if ui.button(tl!("Done")).clicked() {
                *show = false;
            }
            ui.add_enabled(can_reset, egui::Button::new(tl!("Reset to printer defaults")))
        },
    )
}

/// Properties… on CUPS: the driver's options for this print, grouped as the driver groups them.
/// Only choices away from the printer's defaults are kept, and sent with the job.
pub(super) fn driver_options_panel(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    let PrintDraft {
        driver_options: Some((_, options)),
        driver_choices: choices,
        show_driver_options: show,
        ..
    } = d
    else {
        return;
    };
    egui::Frame::new()
        .fill(t.hover)
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (_, reset) = driver_options_header(ui, show, !choices.is_empty());
            if reset.clicked() {
                choices.clear();
            }
            if options.is_empty() {
                ui.label(
                    egui::RichText::new(tl!("This printer offers no settings of its own."))
                        .small()
                        .color(t.text_muted),
                );
                return;
            }
            for (n, chunk) in options.chunk_by(|a, b| a.group == b.group).enumerate() {
                let group = chunk.first().map_or("", |o| o.group.as_str());
                if !group.is_empty() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(group).small().color(t.text_muted));
                }
                egui::Grid::new(("driver-group", n))
                    .num_columns(2)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        for o in chunk {
                            render_driver_option_row(ui, o, choices);
                        }
                    });
            }
        });
}

fn render_driver_option_row(
    ui: &mut egui::Ui,
    o: &spool::PrinterOption,
    choices: &mut std::collections::BTreeMap<String, String>,
) {
    let current = choices.get(&o.key).cloned().unwrap_or_else(|| o.default.clone());
    let label_of = |c: &str| {
        o.choices
            .iter()
            .find(|(k, _)| k == c)
            .map_or_else(|| c.to_string(), |(_, l)| l.clone())
    };
    let mut pick = current.clone();
    let option_label = ui.label(&o.label);
    craft_ui::ComboBox::from_id_salt(("driver-option", &o.key))
        .selected_text(label_of(&current))
        .width(190.0)
        .show_ui(ui, |ui| {
            for (k, l) in &o.choices {
                ui.selectable_value(&mut pick, k.clone(), l.as_str());
            }
        })
        .response
        .labelled_by(option_label.id);
    if pick != current {
        if pick == o.default {
            choices.remove(&o.key);
        } else {
            choices.insert(o.key.clone(), pick);
        }
    }
    ui.end_row();
}

/// Draw a combo box whose accessible name does not depend on the selected value.
pub(super) fn combo<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    id: &str,
    name: &str,
    value: &mut T,
    choices: &[(T, &str)],
    width: f32,
) {
    let shown = choices.iter().find(|c| c.0 == *value).map_or("", |c| c.1);
    craft_ui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .width(width)
        .show_ui(ui, |ui| {
            for (v, label) in choices {
                ui.selectable_value(value, *v, *label);
            }
        })
        .response
        .accessible_name(name);
}

/// Draw the 3-Pane Print Studio dialog. `thumb` gives a page's thumbnail texture when available.
/// Returns `(print, cancel)`.
pub(crate) fn body(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    sizes: &[(f64, f64)],
    labels: &[String],
    thumb: &mut dyn FnMut(usize) -> Option<egui::TextureId>,
) -> (bool, bool) {
    let width = dialog_width(ui.ctx());
    let body_h = dialog_body_height(ui.ctx());
    ui.set_width(width);

    let settings = d.settings(sizes.len(), labels);
    let laid = settings.as_ref().ok().map(|s| print::layout(sizes, s));
    let too_much_creep = d.handling == Handling::Booklet && d.creep > 0.0 && matches!(laid, Some(Err(_)));
    let sheets = laid.and_then(Result::ok).unwrap_or_default();
    d.sheet = d.sheet.min(sheets.len().saturating_sub(1));

    render_studio_header(ui, d, t, sizes.len(), sheets.len());
    ui.add_space(6.0);

    if width >= 1020.0 {
        render_three_pane_studio(
            ui,
            d,
            t,
            sizes,
            width,
            body_h,
            PreviewContext {
                settings: &settings,
                sheets: &sheets,
                sizes,
                too_much_creep,
                body_h,
            },
            thumb,
        );
    } else {
        render_compact_two_pane_studio(
            ui,
            d,
            t,
            sizes,
            width,
            body_h,
            PreviewContext {
                settings: &settings,
                sheets: &sheets,
                sizes,
                too_much_creep,
                body_h,
            },
            thumb,
        );
    }

    ui.add_space(10.0);
    let (go, cancel) = render_studio_footer(ui, d, settings.is_ok(), sheets.is_empty());

    failover_modal_window(ui.ctx(), d);

    (go, cancel)
}

fn render_studio_header(
    ui: &mut egui::Ui,
    d: &PrintDraft,
    t: &Tokens,
    page_count: usize,
    sheet_count: usize,
) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(tl!("Print")).font(theme::semibold(18.0)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let targets = d.effective_target_count();
            let summary = if targets > 1 {
                format!("{page_count} pages · {sheet_count} sheets · {targets} target printers")
            } else {
                format!("{page_count} pages · {sheet_count} sheets")
            };
            ui.label(egui::RichText::new(summary).small().color(t.text_muted));
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn render_three_pane_studio(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    sizes: &[(f64, f64)],
    width: f32,
    body_h: f32,
    pctx: PreviewContext<'_>,
    thumb: &mut dyn FnMut(usize) -> Option<egui::TextureId>,
) {
    let left_w = 348.0;
    let center_w = (width - left_w - PREVIEW_W - 24.0).max(360.0);
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(left_w);
            ui.set_min_height(body_h);
            super::printers_pane::show(ui, d, t, left_w, body_h);
        });
        ui.add_space(12.0);
        ui.vertical(|ui| {
            ui.set_width(center_w);
            ui.set_min_height(body_h);
            super::specs_pane::show(ui, d, t, sizes, center_w, body_h);
        });
        ui.add_space(12.0);
        ui.vertical(|ui| {
            ui.set_width(PREVIEW_W);
            ui.set_min_height(body_h);
            super::preview_pane::show(ui, d, t, pctx, thumb);
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn render_compact_two_pane_studio(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    sizes: &[(f64, f64)],
    width: f32,
    body_h: f32,
    pctx: PreviewContext<'_>,
    thumb: &mut dyn FnMut(usize) -> Option<egui::TextureId>,
) {
    let left_w = (width - PREVIEW_W - 12.0).max(360.0);
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(left_w);
            ui.scope(|ui| {
                ui.spacing_mut().scroll.floating = false;
                ui.spacing_mut().scroll.bar_width = 7.0;
                ui.spacing_mut().scroll.bar_inner_margin = 6.0;
                egui::ScrollArea::vertical()
                    .id_salt("compact-studio-left")
                    .max_height(body_h)
                    .min_scrolled_height(body_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width((left_w - 16.0).max(260.0));
                        super::printers_pane::show_compact(ui, d, t);
                        ui.add_space(8.0);
                        super::specs_pane::show_contents(ui, d, t, sizes);
                    });
            });
        });
        ui.add_space(12.0);
        ui.vertical(|ui| {
            ui.set_width(PREVIEW_W);
            ui.set_min_height(body_h);
            super::preview_pane::show(ui, d, t, pctx, thumb);
        });
    });
}

fn render_studio_footer(
    ui: &mut egui::Ui,
    d: &PrintDraft,
    settings_ok: bool,
    sheets_empty: bool,
) -> (bool, bool) {
    let (mut go, mut cancel) = (false, false);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let looking = still_looking(d);
        let waiting_paper = d.papers_loading();
        let has_cluster_targets = !d.multi_printer_mode || !d.selected_printers.is_empty();
        let label = if d.multi_printer_mode || d.printer.is_some() || looking {
            tl!("Print")
        } else {
            tl!("Save as PDF")
        };
        if ui
            .add_enabled_ui(
                settings_ok && !sheets_empty && !looking && !waiting_paper && has_cluster_targets,
                |ui| widgets::pill_button(ui, label, true),
            )
            .inner
            .clicked()
        {
            go = true;
        }
        if widgets::pill_button(ui, tl!("Cancel"), false).clicked() {
            cancel = true;
        }
    });
    (go, cancel)
}

pub(crate) fn failover_modal_window(ctx: &egui::Context, d: &mut PrintDraft) {
    let copy_diag_clicked = std::cell::Cell::new(false);
    let candidates = d
        .failover_modal
        .as_ref()
        .map(|m| d.failover_candidates(&m.from_printer))
        .unwrap_or_default();

    if let Some(modal) = &mut d.failover_modal {
        let mut modal_open = true;
        let mut close_modal = false;
        egui::Window::new(tl!("One-Click Print Failover"))
            .open(&mut modal_open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(400.0);
                ui.horizontal(|ui| {
                    ui.add(crate::icons::image("triangle-alert", 18.0, Color32::from_rgb(225, 45, 35)));
                    ui.heading(tl!("Printer Hardware / Spooler Fault"));
                });
                ui.add_space(4.0);
                ui.label(format!("Faulted Printer: {}", modal.from_printer));
                ui.label(format!("Failure Cause: {}", modal.error_message));
                ui.label(format!("Faulted Sheet Index: #{}", modal.failed_sheet + 1));
                ui.label(format!(
                    "Committed: {} sheets | Pending: {} sheets",
                    modal.committed_sheets, modal.pending_sheets
                ));
                ui.separator();
                let target_lbl = ui.label(tl!("Select Failover Target Printer:"));
                craft_ui::ComboBox::from_id_salt("failover-target-printer")
                    .selected_text(if modal.to_printer.is_empty() {
                        "Pick Target Printer"
                    } else {
                        &modal.to_printer
                    })
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for name in &candidates {
                            ui.selectable_value(&mut modal.to_printer, name.clone(), name.as_str());
                        }
                    })
                    .response
                    .labelled_by(target_lbl.id);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let can_reroute = !modal.to_printer.is_empty() && modal.to_printer != modal.from_printer;
                    if ui
                        .add_enabled(
                            can_reroute,
                            egui::Button::new(tl!("Reroute Remaining Sheets (Zero Duplicates)")),
                        )
                        .clicked()
                    {
                        d.pending_failover = Some((modal.from_printer.clone(), modal.to_printer.clone()));
                        close_modal = true;
                    }
                    if ui.button(tl!("Copy Diagnostics")).clicked() {
                        copy_diag_clicked.set(true);
                    }
                    if ui.button(tl!("Dismiss")).clicked() {
                        close_modal = true;
                    }
                });
            });
        if !modal_open || close_modal {
            d.failover_modal = None;
        }
    }
    if copy_diag_clicked.get() {
        let report = collect_ui_diagnostic_report(d);
        ctx.copy_text(report.to_markdown_table());
    }
}

/// The row that adds a custom paper: name, width and height in the preferences' unit.
pub(super) fn custom_paper_row(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    let unit = d.units;
    let Some((name, width, height)) = d.custom_new.as_mut() else { return };
    let side = |s: &str| s.trim().replace(',', ".").parse::<f64>().ok().map(|v| v / unit.per_point());
    let paper = match (side(width), side(height)) {
        (Some(w), Some(h)) => crate::print_paper::CustomPaper::new(name, w, h),
        _ => None,
    };
    let (mut add, mut cancel) = (false, false);
    ui.horizontal_wrapped(|ui| {
        ui.add(egui::TextEdit::singleline(name).hint_text(tl!("Name")).desired_width(105.0))
            .accessible_name(tl!("Name"));
        ui.add(egui::TextEdit::singleline(width).hint_text(tl!("Width")).desired_width(50.0))
            .accessible_name(tl!("Width"));
        ui.label("×");
        ui.add(egui::TextEdit::singleline(height).hint_text(tl!("Height")).desired_width(50.0))
            .accessible_name(tl!("Height"));
        ui.label(egui::RichText::new(unit.label()).color(t.text_muted));
        add = ui.add_enabled(paper.is_some(), egui::Button::new(tl!("Add"))).clicked();
        cancel = ui.button(tl!("Cancel")).clicked();
    });
    if add && let Some(paper) = paper {
        d.add_custom_paper(paper);
        d.custom_new = None;
        d.mark_unified_specs_edited();
    } else if cancel {
        d.custom_new = None;
    }
}

fn build_printer_telemetry(
    d: &PrintDraft,
    p: &spool::Printer,
) -> pdfcraft_platform::diagnostics::PrinterCapabilitiesTelemetry {
    use pdfcraft_platform::diagnostics::{
        DuplexBindingKind, MarginsPt, PrinterCapabilitiesTelemetry, ResolutionDpi, SizePt,
    };

    if let Some(mut cached) = d.cached_capabilities.get(&p.name).cloned() {
        cached.is_default = p.default;
        cached.supported_paper_count = d.papers.len();
        return cached;
    }

    let preview_target = if d.multi_printer_mode {
        d.active_preview_printer.as_deref().or(d.printer.as_deref())
    } else {
        d.printer.as_deref()
    };
    let is_active = preview_target == Some(p.name.as_str());
    let margins = if is_active {
        d.preview_hardware_margins.unwrap_or([0.0, 0.0, 0.0, 0.0])
    } else {
        [0.0, 0.0, 0.0, 0.0]
    };
    let printable = if is_active {
        d.preview_printable_area.unwrap_or((612.0, 792.0))
    } else {
        (612.0, 792.0)
    };
    let is_duplex = d.printer_two_sided_for(&p.name).unwrap_or(false);
    let generation = resolve_driver_generation(&p.name);

    PrinterCapabilitiesTelemetry {
        printer_name: p.name.clone(),
        driver_name: p.name.clone(),
        driver_version: "System Queue".to_string(),
        driver_generation: generation,
        port_name: "SystemPort".to_string(),
        is_default: p.default,
        is_color_supported: true,
        is_duplex_supported: is_duplex,
        supported_duplex_modes: if is_duplex {
            vec![
                DuplexBindingKind::Simplex,
                DuplexBindingKind::FlipLongEdge,
                DuplexBindingKind::FlipShortEdge,
            ]
        } else {
            vec![DuplexBindingKind::Simplex]
        },
        max_copies: 999,
        hardware_margins_pt: MarginsPt {
            left: margins[0],
            top: margins[1],
            right: margins[2],
            bottom: margins[3],
        },
        printable_area_pt: SizePt { width: printable.0, height: printable.1 },
        physical_sheet_pt: SizePt {
            width: printable.0 + margins[0] + margins[2],
            height: printable.1 + margins[1] + margins[3],
        },
        resolution_dpi: ResolutionDpi { dpi_x: 600, dpi_y: 600 },
        supported_paper_count: d.papers.len(),
    }
}

fn resolve_driver_generation(printer_name: &str) -> pdfcraft_platform::diagnostics::DriverGenerationKind {
    use pdfcraft_platform::diagnostics::DriverGenerationKind;
    if cfg!(windows) {
        if spool::xps_driver(printer_name) == Some(true) {
            DriverGenerationKind::Win32V4PrintTicket
        } else {
            DriverGenerationKind::Win32V3DevMode
        }
    } else {
        DriverGenerationKind::CupsIpp
    }
}

fn build_applied_ticket(
    d: &PrintDraft,
    printer_name: &str,
) -> pdfcraft_platform::diagnostics::AppliedTicketTelemetry {
    use pdfcraft_platform::diagnostics::{
        AppliedColorMode, AppliedOrientation, AppliedTicketTelemetry, DuplexBindingKind,
    };

    let job = d.job_for_printer(Some(printer_name), "diagnostic-preview.pdf");
    let color_mode = if d.grayscale {
        AppliedColorMode::MonochromeKOnly
    } else {
        AppliedColorMode::Color
    };
    let duplex_mode = match job.duplex {
        Some(spool::Duplex::LongEdge) => DuplexBindingKind::FlipLongEdge,
        Some(spool::Duplex::ShortEdge) => DuplexBindingKind::FlipShortEdge,
        _ => DuplexBindingKind::Simplex,
    };
    let orientation = match d.orientation {
        Orientation::Landscape => AppliedOrientation::Landscape,
        _ => AppliedOrientation::Portrait,
    };
    let paper_name = d.paper_name().to_string();
    let generation = d
        .cached_capabilities
        .get(printer_name)
        .map(|c| c.driver_generation)
        .unwrap_or_else(|| resolve_driver_generation(printer_name));

    let details = build_ticket_details(
        generation,
        d.grayscale,
        duplex_mode,
        orientation,
        &paper_name,
        &job,
    );

    AppliedTicketTelemetry {
        printer_name: printer_name.to_string(),
        generation,
        color_mode,
        duplex_mode,
        orientation,
        paper_size_name: paper_name,
        copies: job.copies,
        details,
    }
}

fn build_ticket_details(
    generation: pdfcraft_platform::diagnostics::DriverGenerationKind,
    grayscale: bool,
    duplex_mode: pdfcraft_platform::diagnostics::DuplexBindingKind,
    orientation: pdfcraft_platform::diagnostics::AppliedOrientation,
    paper_name: &str,
    job: &spool::Job,
) -> pdfcraft_platform::diagnostics::TicketDetails {
    use pdfcraft_platform::diagnostics::{
        AppliedOrientation, DriverGenerationKind, DuplexBindingKind, TicketDetails,
    };

    match generation {
        DriverGenerationKind::Win32V4PrintTicket => TicketDetails::PrintTicketV4 {
            feature_color: if grayscale {
                "psk:Grayscale (+ K-Only Filter)".to_string()
            } else {
                "psk:Color".to_string()
            },
            feature_duplex: match duplex_mode {
                DuplexBindingKind::Simplex => "psk:OneSided".to_string(),
                DuplexBindingKind::FlipLongEdge => "psk:TwoSidedLongEdge".to_string(),
                DuplexBindingKind::FlipShortEdge => "psk:TwoSidedShortEdge".to_string(),
            },
            feature_orientation: format!("{orientation:?}"),
            feature_media_size: paper_name.to_string(),
            raw_xml_snippet: None,
        },
        DriverGenerationKind::CupsIpp => TicketDetails::CupsIpp {
            print_color_mode: if grayscale {
                "monochrome".to_string()
            } else {
                "color".to_string()
            },
            sides: match duplex_mode {
                DuplexBindingKind::Simplex => "one-sided".to_string(),
                DuplexBindingKind::FlipLongEdge => "two-sided-long-edge".to_string(),
                DuplexBindingKind::FlipShortEdge => "two-sided-short-edge".to_string(),
            },
            media: paper_name.to_string(),
            copies: job.copies,
            raw_options: job.options.clone(),
        },
        _ => TicketDetails::DevModeV3 {
            dm_fields_bitmask: 0x0001 | 0x0002 | 0x0004 | 0x0800 | 0x1000,
            dm_size: 220,
            dm_driver_extra_bytes: 1024,
            dm_color: if grayscale { 1 } else { 2 },
            dm_duplex: match duplex_mode {
                DuplexBindingKind::Simplex => 1,
                DuplexBindingKind::FlipLongEdge => 2,
                DuplexBindingKind::FlipShortEdge => 3,
            },
            dm_orientation: match orientation {
                AppliedOrientation::Landscape => 2,
                AppliedOrientation::Portrait => 1,
            },
            dm_copies: job.copies as i16,
            dm_papersize: 1,
            dm_collate: if job.collate { 1 } else { 0 },
        },
    }
}

/// Harvests active print studio state into a complete platform `DiagnosticReport`.
pub(crate) fn collect_ui_diagnostic_report(
    d: &PrintDraft,
) -> pdfcraft_platform::diagnostics::DiagnosticReport {
    use pdfcraft_platform::diagnostics::{
        DiagnosticReport, TraceCategory, TraceLevel, TraceRecord, collect_diagnostic_report,
    };

    let now_ts = DiagnosticReport::harvest_system().generated_at;
    let printers = d.printers.iter().map(|p| build_printer_telemetry(d, p)).collect();
    let applied_tickets = d
        .target_printers()
        .iter()
        .map(|name| build_applied_ticket(d, name))
        .collect();

    let mut traces = d.diagnostic_traces.clone();
    let mut ctx = std::collections::BTreeMap::new();
    ctx.insert("copies".to_string(), d.copies.to_string());
    ctx.insert("grayscale".to_string(), d.grayscale.to_string());
    ctx.insert("multi_printer_mode".to_string(), d.multi_printer_mode.to_string());
    ctx.insert("selected_printers".to_string(), d.selected_printers.len().to_string());
    if let Some(preset) = d.preset_store.active_preset() {
        ctx.insert("active_preset".to_string(), preset.name.clone());
    }
    traces.push(TraceRecord {
        timestamp: now_ts,
        level: TraceLevel::Info,
        category: TraceCategory::DriverNegotiation,
        printer_name: d.printer.clone(),
        error_code: None,
        message: format!("Print studio pre-flight snapshot (targets: {:?})", d.target_printers()),
        context: ctx,
    });
    append_hazard_and_failover_traces(d, now_ts, &mut traces);

    collect_diagnostic_report(printers, applied_tickets, traces)
}

fn append_hazard_and_failover_traces(
    d: &PrintDraft,
    now_ts: chrono::DateTime<chrono::Utc>,
    traces: &mut Vec<pdfcraft_platform::diagnostics::TraceRecord>,
) {
    use pdfcraft_platform::diagnostics::{TraceCategory, TraceLevel, TraceRecord};

    if let Some(hazard) = d.clipping_hazard {
        let mut h_ctx = std::collections::BTreeMap::new();
        h_ctx.insert("left_pt".to_string(), format!("{:.2}", hazard[0]));
        h_ctx.insert("top_pt".to_string(), format!("{:.2}", hazard[1]));
        h_ctx.insert("right_pt".to_string(), format!("{:.2}", hazard[2]));
        h_ctx.insert("bottom_pt".to_string(), format!("{:.2}", hazard[3]));
        traces.push(TraceRecord {
            timestamp: now_ts,
            level: TraceLevel::Warning,
            category: TraceCategory::MarginAdaptation,
            printer_name: d.active_preview_printer.clone().or_else(|| d.printer.clone()),
            error_code: Some("CLIPPING_HAZARD".to_string()),
            message: format!(
                "Clipping hazard detected: overflow [L:{:.1}, T:{:.1}, R:{:.1}, B:{:.1}] pt",
                hazard[0], hazard[1], hazard[2], hazard[3]
            ),
            context: h_ctx,
        });
    }
    if let Some(modal) = &d.failover_modal {
        let mut f_ctx = std::collections::BTreeMap::new();
        f_ctx.insert("from_printer".to_string(), modal.from_printer.clone());
        f_ctx.insert("to_printer".to_string(), modal.to_printer.clone());
        f_ctx.insert("failed_sheet".to_string(), modal.failed_sheet.to_string());
        traces.push(TraceRecord {
            timestamp: now_ts,
            level: TraceLevel::Error,
            category: TraceCategory::Failover,
            printer_name: Some(modal.from_printer.clone()),
            error_code: Some("HARDWARE_FAILURE".to_string()),
            message: format!(
                "Printer failure: {} at sheet #{}",
                modal.error_message,
                modal.failed_sheet + 1
            ),
            context: f_ctx,
        });
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::i18n::Lang;
    use crate::{Dialog, PdfCraftApp};

    /// The row the settings column gives the header (`center_w ~ 468 px`, less frame margins).
    const PANEL_ROW: f32 = 450.0;

    fn header_frame(lang: &str, width: f32, hover: bool) -> (Option<egui::Rect>, egui::Rect, bool) {
        crate::i18n::set_current(Lang::from_code(lang).expect("a catalogued language"));
        let ctx = egui::Context::default();
        ctx.set_fonts(theme::font_definitions());
        ctx.all_styles_mut(|style| style.interaction.tooltip_delay = 0.0);
        let row = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 400.0));
        let pointer = std::cell::Cell::new(None);
        let seen = std::cell::Cell::new((None, egui::Rect::NOTHING));
        for _ in 0..3 {
            let mut input = egui::RawInput { screen_rect: Some(row), ..Default::default() };
            if let Some(at) = hover.then(|| pointer.get()).flatten() {
                input.events.push(egui::Event::PointerMoved(at));
            }
            let mut output = ctx.run_ui(input, |ui| {
                let mut show = true;
                let (title, reset) = driver_options_header(ui, &mut show, true);
                let title = title.map(|title| title.rect);
                pointer.set(title.map(|title| title.center()));
                seen.set((title, reset.rect));
            });
            output.textures_delta.clear();
        }
        let tooltip = ctx.memory(|memory| {
            memory
                .areas()
                .visible_layer_ids()
                .iter()
                .any(|layer| layer.order == egui::Order::Tooltip)
        });
        crate::i18n::set_current(Lang::EN);
        let (title, reset) = seen.get();
        (title, reset, tooltip)
    }

    fn header_at(lang: &str, width: f32) -> (Option<egui::Rect>, egui::Rect) {
        let (title, reset, _) = header_frame(lang, width, false);
        (title, reset)
    }

    #[test]
    fn the_reset_button_never_covers_the_title_in_any_language() {
        if !crate::i18n::needs_catalogs() {
            return;
        }
        for lang in Lang::all() {
            let (title, reset) = header_at(lang.code(), PANEL_ROW);
            assert!(
                title.is_none_or(|title| !title.intersects(reset)),
                "{} at {PANEL_ROW}px: the title {title:?} is drawn on top of the reset button {reset:?}",
                lang.code()
            );
            assert!(
                reset.right() <= PANEL_ROW,
                "{}: the reset button {reset:?} runs out of the row",
                lang.code()
            );
        }
    }

    #[test]
    fn a_row_without_room_for_a_title_leaves_it_out() {
        if !crate::i18n::needs_catalogs() {
            return;
        }
        for width in [449.0, 440.0, 400.0, 300.0] {
            for lang in ["bg", "uk", "es", "it", "en"] {
                let (title, reset) = header_at(lang, width);
                assert!(
                    title.is_none_or(|title| !title.intersects(reset)),
                    "{lang} at {width}px: the title {title:?} is drawn on top of the reset button {reset:?}"
                );
            }
        }
        let (title, reset) = header_at("bg", 440.0);
        assert!(
            title.is_none(),
            "the title is dropped, not squeezed to a sliver: {:?} next to {reset:?}",
            title
        );
    }

    #[test]
    fn the_title_truncates_and_leaves_the_buttons_whole() {
        let (wide_title, wide_reset) = header_at("en", 620.0);
        let (narrow_title, narrow_reset) = header_at("en", 300.0);
        let (wide_title, narrow_title) = (
            wide_title.expect("the wide row keeps the title"),
            narrow_title.expect("the narrow row keeps a title"),
        );
        assert!(narrow_title.width() < wide_title.width(), "the title shrinks in a narrow row");
        assert_eq!(narrow_reset.width(), wide_reset.width(), "the buttons keep their size");
        assert!(narrow_reset.right() <= 301.0, "the reset button stays inside the row");
    }

    #[test]
    fn the_truncated_title_is_reachable_through_a_tooltip() {
        if !crate::i18n::needs_catalogs() {
            return;
        }
        let (whole, _, _) = header_frame("it", 620.0, false);
        let whole = whole.expect("a wide row keeps the title");
        let (cut, _, tooltip) = header_frame("it", PANEL_ROW, true);
        let cut = cut.expect("the Italian title is elided, not dropped");
        assert!(
            cut.width() < whole.width(),
            "the Italian title is cut at {PANEL_ROW}px: {} of {}",
            cut.width(),
            whole.width()
        );
        assert!(tooltip, "hovering the elided title opens a tooltip with the full string");
        let (_, _, tooltip) = header_frame("en", 620.0, true);
        assert!(!tooltip, "a title that fits whole needs no tooltip");
    }

    #[test]
    fn the_print_dialog_keeps_its_buttons_within_a_short_screen() {
        let screen = egui::vec2(1000.0, 480.0);
        let mut h = Harness::builder().with_size(screen).build_eframe(|_cc| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            let bytes = app.session.create_blank(612.0, 792.0, 1).unwrap();
            app.open_bytes("one-page.pdf", None, bytes.as_ref().clone()).unwrap();
            app.dialog = Some(Dialog::Print);
            app
        });
        h.run_steps(4);

        let cancel = h.get_by_label("Cancel").rect();
        assert!(
            cancel.min.y >= 0.0 && cancel.max.y <= screen.y,
            "Cancel must stay within the {}×{} screen, got {cancel:?}",
            screen.x,
            screen.y
        );
    }
}
