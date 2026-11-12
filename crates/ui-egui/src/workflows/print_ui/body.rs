use super::draft::{CUSTOM_SCALE, Handling, PrintDraft, Which};
use super::{
    Binding, BookletSubset, Color32, Content, Orientation, PREVIEW_BOX, PREVIEW_H, PREVIEW_W, PageOrder, Pos2, Rect, SizeMode, Stroke, Subset,
    Tokens, pos2, print, printer_label, spool, still_looking, theme, vec2, widgets,
};

/// The narrowest the title is still worth cutting: an ellipsis and a couple of pixels at the title's
/// font. Given less room than this, [`egui::Sides::truncate`] cannot cut any further and the label
/// is painted back over the buttons, which is the defect of #5545; the title is left out instead.
const TITLE_MIN_WIDTH: f32 = 16.0;

/// The panel's own header: "Printer properties" on the left, Done and Reset on the right.
///
/// The buttons take their room first, from the right (`shrink_left`), and the title gets what is
/// left of the row: [`egui::Sides::truncate`] cuts it with an ellipsis instead of letting a label wider than the row
/// spill over it. Laid out the other way round, the label is placed first at its natural width and
/// the buttons are painted back over it, which is what a translation of "Reset to printer
/// defaults" long enough to fill the row did in Italian (#5545).
///
/// The title comes back as `None` when the row is too narrow for even an ellipsis, which a catalog
/// whose two buttons nearly fill the row reaches (Bulgarian, Ukrainian, Spanish at about 450 px).
fn driver_options_header(ui: &mut egui::Ui, show: &mut bool, can_reset: bool) -> (Option<egui::Response>, egui::Response) {
    egui::Sides::new().shrink_left().truncate().show(
        ui,
        |ui| {
            let room = ui.available_width();
            (room >= TITLE_MIN_WIDTH).then(|| ui.add(egui::Label::new(egui::RichText::new(tl!("Printer properties")).font(theme::semibold(13.0)))))
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
fn driver_options_panel(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    let PrintDraft { driver_options: Some((_, options)), driver_choices: choices, show_driver_options: show, .. } = d else { return };
    egui::Frame::new().fill(t.hover).corner_radius(egui::CornerRadius::same(6)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let (_, reset) = driver_options_header(ui, show, !choices.is_empty());
        if reset.clicked() {
            choices.clear();
        }
        if options.is_empty() {
            ui.label(egui::RichText::new(tl!("This printer offers no settings of its own.")).small().color(t.text_muted));
            return;
        }
        egui::ScrollArea::vertical().id_salt("driver-options").max_height(240.0).show(ui, |ui| {
            for (n, chunk) in options.chunk_by(|a, b| a.group == b.group).enumerate() {
                let group = chunk.first().map_or("", |o| o.group.as_str());
                if !group.is_empty() {
                    ui.add_space(4.0);
                    // Option and group names are the driver's own text.
                    ui.label(egui::RichText::new(group).small().color(t.text_muted));
                }
                egui::Grid::new(("driver-group", n)).num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                    for o in chunk {
                        let current = choices.get(&o.key).cloned().unwrap_or_else(|| o.default.clone());
                        let label_of = |c: &str| o.choices.iter().find(|(k, _)| k == c).map_or_else(|| c.to_string(), |(_, l)| l.clone());
                        let mut pick = current.clone();
                        let option_label = ui.label(&o.label);
                        craft_ui::ComboBox::from_id_salt(("driver-option", &o.key))
                            .selected_text(label_of(&current))
                            .width(200.0)
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
                });
            }
        });
    });
}

/// Draw a combo box whose accessible name does not depend on the selected value.
fn combo<T: PartialEq + Copy>(ui: &mut egui::Ui, id: &str, name: &str, value: &mut T, choices: &[(T, &str)], width: f32) {
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

/// Draw the dialog. `thumb` gives a page's thumbnail texture when there is one. Returns
/// (print, cancel).
pub(crate) fn body(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    sizes: &[(f64, f64)],
    labels: &[String],
    thumb: &mut dyn FnMut(usize) -> Option<egui::TextureId>,
) -> (bool, bool) {
    ui.set_width(820.0);
    ui.label(egui::RichText::new(tl!("Print")).font(theme::semibold(18.0)));
    ui.add_space(8.0);
    let settings = d.settings(sizes.len(), labels);
    let laid = settings.as_ref().ok().map(|s| print::layout(sizes, s));
    // Creep within its field's range can still be too much for a thick booklet.
    let too_much_creep = d.handling == Handling::Booklet && d.creep > 0.0 && matches!(laid, Some(Err(_)));
    let sheets = laid.and_then(Result::ok).unwrap_or_default();
    d.sheet = d.sheet.min(sheets.len().saturating_sub(1));
    // Cap the settings+preview area to the screen's available height (#4252, #5317):
    // on a short screen, or with the driver's own Properties… panel expanded below, this row can
    // otherwise grow taller than the display and push the Print/Cancel buttons off both edges.
    // The title and the button row stay outside, so they are always reachable.
    // `viewport_rect()` is egui's own idea of the current viewport, kept from the latest
    // `RawInput::screen_rect` it was handed — unlike reading `raw.screen_rect` directly, it
    // does not go blank on a frame where the backend didn't resend it.
    let screen_h = ui.ctx().input(|i| i.viewport_rect().height());
    let max_h = (screen_h - 220.0).max(300.0);
    // The viewport is `max_h` itself, not what the modal offers this frame: when a different
    // dialog was shown before, the shared modal remeasures from a height of zero (#5118) and
    // offers less, which would size this area by the dialog shown before. `auto_shrink` still
    // shrinks it to contents that fit.
    egui::ScrollArea::vertical().max_height(max_h).min_scrolled_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
    ui.horizontal_top(|ui| {
        // Settings.
        ui.vertical(|ui| {
            ui.set_width(470.0);
            egui::Grid::new("print-top").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                // Milestone 4: Job Preset & Diagnostic Buttons Row
                ui.label(tl!("Job Preset:"));
                preset_and_diagnostics_row(ui, d);
                ui.end_row();
                if d.new_preset_name.is_some() {
                    ui.label("");
                    save_preset_inline_row(ui, d);
                    ui.end_row();
                }

                let printer_caption = ui.label(tl!("Printer:"));
                let before = d.printer.clone();
                // The browser's print dialog sets copies, two-sided printing and colour itself.
                let browser = d.printer.as_deref() == Some(crate::web_print::BROWSER_PRINTER);
                ui.horizontal(|ui| {
                    let shown = match d.printer.as_deref() {
                        Some(p) => printer_label(p),
                        None if still_looking(d) => tl!("Looking for printers…").to_string(),
                        None => tl!("Save as PDF").to_string(),
                    };
                    craft_ui::ComboBox::from_id_salt("printer").selected_text(shown).width(260.0).show_ui(ui, |ui| {
                        if d.printers_loading {
                            ui.add_enabled(false, egui::Label::new(tl!("Looking for printers…")));
                        }
                        for p in &d.printers {
                            let name = printer_label(&p.name);
                            let label = if p.default && p.name != crate::web_print::BROWSER_PRINTER {
                                crate::i18n::fmt(tl!("{name} (default)"), &[("name", &name)])
                            } else {
                                name
                            };
                            ui.selectable_value(&mut d.printer, Some(p.name.clone()), label);
                        }
                        // Chosen even when it was already the selection, so the list arriving
                        // later doesn't replace it with the default printer.
                        if ui.selectable_value(&mut d.printer, None, tl!("Save as PDF")).clicked() {
                            d.printer_picked = true;
                        }
                    })
                    .response
                    .labelled_by(printer_caption.id);
                    let tip = if spool::HAS_PRINTER_PREFERENCES {
                        tl!("Opens the printer driver's preferences. They are saved for every print from this computer; the copies, two-sided, grayscale and paper chosen here still apply.")
                    } else {
                        tl!("The printer driver's own settings for this print: paper tray, paper type, finishing and more.")
                    };
                    let properties = ui.add_enabled(d.printer.is_some() && !browser, egui::Button::new(tl!("Properties…"))).on_hover_text(tip);
                    if properties.clicked()
                        && let Some(p) = d.printer.clone()
                    {
                        if spool::HAS_PRINTER_PREFERENCES {
                            d.driver_error = spool::open_printer_preferences(&p).err().map(|e| e.to_string());
                        } else {
                            if d.driver_options.as_ref().is_none_or(|(read_for, _)| *read_for != p) && d.driver_options_loading.as_deref() != Some(p.as_str()) {
                                // Read off the GUI thread (#1818): `poll_print` starts it next frame.
                                d.driver_options = None;
                                d.driver_choices.clear();
                                d.driver_options_loading = Some(p.clone());
                                d.read_options_for = Some(p);
                            }
                            d.show_driver_options = !d.show_driver_options;
                        }
                    }
                });
                if d.printer != before {
                    d.printer_picked = true;
                    d.show_driver_options = false;
                    d.driver_error = None;
                    // What this printer was last printed with (#2001).
                    d.apply_setup();
                    d.reset_unoverridden_custom_paper();
                    d.apply_printer_papers(Vec::new());
                    if let Some(p) = d.printer.clone() {
                        d.read_papers_for = Some(p.clone());
                        d.read_caps_for = Some(p);
                    } else {
                        d.preview_hardware_margins = None;
                        d.preview_printable_area = None;
                    }
                }
                ui.end_row();

                // Milestone 4: Multi-Printer Mode Toggle & Cluster Selector
                ui.label(tl!("Cluster Mode:"));
                cluster_mode_header_row(ui, d);
                ui.end_row();
                if d.new_group_name.is_some() {
                    ui.label("");
                    save_group_inline_row(ui, d);
                    ui.end_row();
                }

                if d.multi_printer_mode {
                    ui.label(tl!("Target Printers:"));
                    cluster_printers_panel(ui, d, t);
                    ui.end_row();
                }

                // The spooler answered with no printer at all (#160): say so, rather than
                // leaving Save as PDF looking like the only thing PdfCraft can do.
                if !d.printers_loading && d.printers.is_empty() {
                    ui.label("");
                    ui.label(
                        egui::RichText::new(tl!("No printers were found. Check that the printer is installed and the print spooler is running, then open Print again."))
                            .small()
                            .color(t.text_muted),
                    );
                    ui.end_row();
                }
                let copies_label = ui.label(tl!("Copies:"));
                ui.add_enabled_ui(!browser, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut d.copies).range(1..=999)).labelled_by(copies_label.id);
                        ui.add(widgets::checkbox(&mut d.collate, tl!("Collate")));
                        ui.add(widgets::checkbox(&mut d.grayscale, tl!("Print in grayscale")));
                    });
                });
                ui.end_row();
                // A booklet decides two-sided itself (#2707), so the choice is hidden meanwhile, as
                // in Acrobat's Print dialog; it is kept for the other layouts.
                if d.handling != Handling::Booklet {
                    ui.label(tl!("Two-sided:"));
                    ui.add_enabled_ui(!browser, |ui| {
                        combo(
                            ui,
                            "duplex",
                            tl!("Two-sided:"),
                            &mut d.duplex,
                            &[
                                (None, tl!("Printer default")),
                                (Some(spool::Duplex::Off), tl!("Off")),
                                (Some(spool::Duplex::LongEdge), tl!("Flip on long edge")),
                                (Some(spool::Duplex::ShortEdge), tl!("Flip on short edge")),
                            ],
                            160.0,
                        );
                    });
                    ui.end_row();
                }
                let paper_label = ui.label(tl!("Paper:"));
                d.paper = d.paper.min(d.paper_count().saturating_sub(1));
                let before = d.paper;
                let mut paper_clicked = false;
                let detecting = d.papers_loading();
                ui.horizontal(|ui| {
                    // A standard size's name is translated; a custom paper keeps the name it was given.
                    let shown = if d.paper_is_custom() { d.paper_name().to_string() } else { tl!(&d.paper_name()).to_string() };
                    craft_ui::ComboBox::from_id_salt("paper").selected_text(shown).width(180.0).show_ui(ui, |ui| {
                        if detecting {
                            ui.add_enabled(false, egui::Label::new(tl!("Detecting paper…")));
                        }
                        for (i, (name, _)) in d.papers.iter().enumerate() {
                            if ui.selectable_value(&mut d.paper, i, tl!(name)).clicked() {
                                paper_clicked = true;
                            }
                        }
                        if !d.custom_papers.is_empty() {
                            ui.separator();
                        }
                        for (i, p) in d.custom_papers.iter().enumerate() {
                            let label = format!("{} ({})", p.name, d.units.size(p.width, p.height));
                            if ui.selectable_value(&mut d.paper, d.papers.len() + i, label).clicked() {
                                paper_clicked = true;
                            }
                        }
                    })
                    .response
                    .labelled_by(paper_label.id);
                    if detecting {
                        ui.label(egui::RichText::new(tl!("Detecting…")).small().color(t.text_muted));
                    }
                    if ui.button(tl!("Custom…")).on_hover_text(tl!("Add a paper size of your own to the list")).clicked() {
                        d.custom_new = match d.custom_new {
                            Some(_) => None,
                            None => Some((String::new(), String::new(), String::new())),
                        };
                    }
                    if d.paper_is_custom() && ui.button(tl!("Remove")).on_hover_text(tl!("Take this paper size off the list")).clicked() {
                        d.remove_custom_paper();
                        paper_clicked = true;
                    }
                    let page_setup_tip = if spool::HAS_PRINTER_PREFERENCES {
                        tl!("Selects paper size and orientation from the printer driver. For paper tray, print quality or other driver defaults, use Properties…")
                    } else {
                        tl!("Page setup dialog is not available on this system.")
                    };
                    let page_setup = ui
                        .add_enabled(d.printer.is_some() && !browser && spool::HAS_PRINTER_PREFERENCES, egui::Button::new(tl!("Page Setup…")))
                        .on_hover_text(page_setup_tip);
                    if page_setup.clicked()
                        && let Some(p) = d.printer.clone()
                    {
                        d.driver_error = None;
                        #[cfg(windows)]
                        let parent = winsafe::HWND::GetActiveWindow().map(|h| h.ptr() as isize);
                        #[cfg(not(windows))]
                        let parent = None;
                        match spool::show_printer_properties(&p, parent) {
                            Ok(Some((name, size))) => {
                                d.apply_driver_paper(name, size);
                            }
                            Ok(None) => {}
                            Err(e) => d.driver_error = Some(e.to_string()),
                        }
                    }
                });
                if paper_clicked || d.paper != before {
                    d.paper_overridden = true;
                }
                // Label stock gets the label layout (cut, turned upright, fitted).
                if paper_clicked && d.paper != before && d.paper == print::LABEL_PAPER && d.which != Which::Area {
                    d.paper_before_label = Some(before);
                    d.handling = Handling::Label;
                    d.sheet = 0;
                }
                ui.end_row();
                if d.custom_new.is_some() {
                    ui.label("");
                    custom_paper_row(ui, d, t);
                    ui.end_row();
                }
            });
            // Windows says which drivers can't print two-sided as they're asked; they keep their
            // own setting, even when Two-sided is Off, and for a booklet's both sides.
            if d.printer_two_sided() == Some(false) {
                ui.label(egui::RichText::new(tl!("This printer's driver doesn't take two-sided settings from other programs, so its own setting applies. Change it in Properties…")).small().color(t.text_muted));
            }
            if d.printer.as_deref() == Some(crate::web_print::BROWSER_PRINTER) {
                ui.label(egui::RichText::new(tl!("Copies, two-sided printing and colour are chosen in the browser's print dialog.")).small().color(t.text_muted));
            }
            if let Some(e) = &d.driver_error {
                ui.label(egui::RichText::new(crate::i18n::fmt(tl!("Couldn't open the printer's preferences: {e}"), &[("e", e)])).small().color(t.text_muted));
            }
            if d.show_driver_options {
                if d.driver_options.is_none() && d.driver_options_loading.is_some() {
                    ui.label(egui::RichText::new(tl!("Reading the printer's settings…")).small().color(t.text_muted));
                } else {
                    driver_options_panel(ui, d, t);
                }
            }
            if let Some(&source) = sizes.get(d.current_page) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(crate::i18n::fmt(tl!("PDF page: {size}"), &[("size", &d.units.size(source.0, source.1))]));
                    let matched = d.matching_paper(source);
                    if ui.add_enabled(matched.is_some(), egui::Button::new(tl!("Match page at 100%"))).clicked()
                        && let Some(paper) = matched
                    {
                        d.paper = paper;
                        d.paper_overridden = true;
                        // A paper picked outright: nothing for leaving Label to give back.
                        d.paper_before_label = None;
                        d.handling = Handling::Size;
                        d.size = SizeMode::Actual;
                        d.orientation = Orientation::Auto;
                        d.sheet = 0;
                    }
                });
            }
            ui.add_space(6.0);
            widgets::section_title(ui, tl!("Pages to Print"));
            ui.horizontal(|ui| {
                widgets::radio_value(ui, &mut d.which, Which::All, tl!("All"));
                widgets::radio_value(ui, &mut d.which, Which::Current, tl!("Current page"));
                let pages = widgets::radio_value(ui, &mut d.which, Which::Range, tl!("Pages"));
                let r = ui.add_enabled(
                    d.which == Which::Range,
                    crate::widgets::line(&mut d.range).hint_text(format!("1-{}", sizes.len())).desired_width(110.0),
                ).labelled_by(pages.id);
                if r.gained_focus() {
                    d.which = Which::Range;
                }
            });
            // Offered only when an area was chosen with Snapshot (its menu's Print area…).
            if let Some((page, a)) = d.area {
                let label = crate::i18n::fmt(
                    tl!("Selected area (page {page}, {size})"),
                    &[("page", &(page + 1).to_string()), ("size", &d.units.size(a[2] - a[0], a[3] - a[1]))],
                );
                if widgets::radio_value(ui, &mut d.which, Which::Area, label).clicked() {
                    // Leaving Label this way gives back the paper it replaced, as its tabs do.
                    d.set_handling(Handling::Size);
                }
            }
            // Offered only when pages were picked before the dialog opened.
            if !d.selected.is_empty() {
                let label = format!("{} ({})", tl!("Selected pages"), d.selected.len());
                widgets::radio_value(ui, &mut d.which, Which::Selected, label);
            }
            ui.horizontal(|ui| {
                ui.label(tl!("More options:"));
                combo(
                    ui,
                    "subset",
                    tl!("More options:"),
                    &mut d.subset,
                    &[(Subset::All, tl!("All pages in range")), (Subset::Odd, tl!("Odd pages only")), (Subset::Even, tl!("Even pages only"))],
                    150.0,
                );
                ui.add(widgets::checkbox(&mut d.reverse, tl!("Reverse pages")));
            });
            ui.add_space(6.0);
            widgets::section_title(ui, tl!("Page Sizing & Handling"));
            ui.horizontal_wrapped(|ui| {
                for (h, label) in [
                    (Handling::Size, tl!("Size")),
                    (Handling::Poster, tl!("Poster")),
                    (Handling::Multiple, tl!("Multiple")),
                    (Handling::Booklet, tl!("Booklet")),
                    (Handling::Label, tl!("Label")),
                ] {
                    // A selected area prints only with Size.
                    let offered = d.which != Which::Area || h == Handling::Size;
                    if ui.add_enabled_ui(offered, |ui| widgets::mode_tab(ui, label, d.handling == h)).inner.clicked() {
                        d.set_handling(h);
                    }
                }
            });
            ui.add_space(4.0);
            match d.handling {
                Handling::Size => {
                    ui.horizontal(|ui| {
                        widgets::radio_value(ui, &mut d.size, SizeMode::Fit, tl!("Fit"));
                        widgets::radio_value(ui, &mut d.size, SizeMode::Actual, tl!("Actual size"));
                        widgets::radio_value(ui, &mut d.size, SizeMode::Shrink, tl!("Shrink oversized pages"));
                    });
                    ui.horizontal(|ui| {
                        let custom = matches!(d.size, SizeMode::Custom(_));
                        if ui.add(widgets::radio(custom, tl!("Custom scale:"))).clicked() {
                            d.size = SizeMode::Custom(d.custom_scale);
                        }
                        ui.add_enabled(custom, egui::DragValue::new(&mut d.custom_scale).range(CUSTOM_SCALE).suffix(" %"))
                            .accessible_name(tl!("Custom scale"));
                    });
                }
                Handling::Label => {
                    ui.add(widgets::checkbox(&mut d.label_flip, tl!("Turn upside down (180°)")));
                    ui.label(
                        egui::RichText::new(tl!("Cuts each page to its printed area, turns it upright and fits it to the sheet without stretching. One label per sheet."))
                            .small()
                            .color(t.text_muted),
                    );
                }
                Handling::Poster => {
                    ui.horizontal(|ui| {
                        let scale_label = ui.label(tl!("Tile scale:"));
                        ui.add(egui::DragValue::new(&mut d.poster_scale).range(10.0..=1000.0).suffix(" %")).labelled_by(scale_label.id);
                        let overlap_label = ui.label(tl!("Overlap:"));
                        ui.add(egui::DragValue::new(&mut d.overlap).range(0.0..=144.0).suffix(" pt")).labelled_by(overlap_label.id);
                        ui.add(widgets::checkbox(&mut d.cut_marks, tl!("Cut marks")));
                    });
                }
                Handling::Multiple => {
                    let page_order = |ui: &mut egui::Ui, order: &mut PageOrder| {
                        ui.label(tl!("Page order:"));
                        combo(
                            ui,
                            "order",
                            tl!("Page order:"),
                            order,
                            &[
                                (PageOrder::Horizontal, tl!("Horizontal")),
                                (PageOrder::HorizontalReversed, tl!("Horizontal reversed")),
                                (PageOrder::Vertical, tl!("Vertical")),
                                (PageOrder::VerticalReversed, tl!("Vertical reversed")),
                                (PageOrder::CutStack, tl!("Cut and stack")),
                            ],
                            150.0,
                        );
                    };
                    ui.horizontal(|ui| {
                        ui.label(tl!("Pages per sheet:"));
                        // `None` is Custom: columns by rows, typed in.
                        let mut per_sheet = (!d.custom_grid).then_some(d.per_sheet);
                        combo(
                            ui,
                            "per-sheet",
                            tl!("Pages per sheet:"),
                            &mut per_sheet,
                            &[(Some(2), "2"), (Some(4), "4"), (Some(6), "6"), (Some(9), "9"), (Some(16), "16"), (None, tl!("Custom"))],
                            70.0,
                        );
                        d.custom_grid = per_sheet.is_none();
                        if let Some(n) = per_sheet {
                            d.per_sheet = n;
                            page_order(ui, &mut d.order);
                        } else {
                            let max = print::MAX_GRID;
                            ui.add(egui::DragValue::new(&mut d.cols).range(1..=max)).accessible_name(tl!("Columns"));
                            ui.label(tl!("by"));
                            ui.add(egui::DragValue::new(&mut d.rows).range(1..=max)).accessible_name(tl!("Rows"));
                        }
                    });
                    if d.custom_grid {
                        // The grid's fields take the room the page order had.
                        ui.horizontal(|ui| page_order(ui, &mut d.order));
                    }
                    ui.horizontal(|ui| {
                        ui.add(widgets::checkbox(&mut d.border, tl!("Print page border")));
                        ui.add(widgets::checkbox(&mut d.auto_rotate, tl!("Auto-rotate pages")));
                    });
                    if d.order == PageOrder::CutStack {
                        ui.label(tl!(
                            "Print single-sided. Keep the sheets in order, cut at the marks, then stack the piles left to right, top to bottom."
                        ));
                    }
                }
                Handling::Booklet => {
                    ui.horizontal(|ui| {
                        ui.label(tl!("Booklet subset:"));
                        combo(
                            ui,
                            "booklet",
                            tl!("Booklet subset:"),
                            &mut d.booklet_subset,
                            &[
                                (BookletSubset::BothSides, tl!("Both sides")),
                                (BookletSubset::FrontOnly, tl!("Front side only")),
                                (BookletSubset::BackOnly, tl!("Back side only")),
                            ],
                            130.0,
                        );
                        ui.label(tl!("Binding:"));
                        combo(ui, "binding", tl!("Binding:"), &mut d.binding, &[(Binding::Left, tl!("Left")), (Binding::Right, tl!("Right"))], 80.0);
                        let creep_label = ui.label(tl!("Creep:"));
                        ui.add(egui::DragValue::new(&mut d.creep).range(0.0..=print::MAX_CREEP).speed(0.05).suffix(" pt"))
                            .labelled_by(creep_label.id)
                            .on_hover_text(tl!("Moves each sheet's pages toward the fold by this much more than the sheet outside it, so the margins match once the booklet is folded and trimmed."));
                    });
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(tl!("Orientation:"));
                widgets::radio_value(ui, &mut d.orientation, Orientation::Auto, tl!("Auto portrait/landscape"));
                widgets::radio_value(ui, &mut d.orientation, Orientation::Portrait, tl!("Portrait"));
                widgets::radio_value(ui, &mut d.orientation, Orientation::Landscape, tl!("Landscape"));
            });
            ui.add_space(6.0);
            widgets::section_title(ui, tl!("Comments & Forms"));
            combo(
                ui,
                "content",
                tl!("Comments & Forms"),
                &mut d.content,
                &[
                    (Content::Document, tl!("Document")),
                    (Content::DocumentAndMarkups, tl!("Document and markups")),
                    (Content::DocumentAndStamps, tl!("Document and stamps")),
                    (Content::FormFieldsOnly, tl!("Form fields only")),
                ],
                220.0,
            );
            // Advanced (Acrobat's Advanced Print Setup): print as image. Folded unless it is on.
            ui.add_space(6.0);
            egui::CollapsingHeader::new(egui::RichText::new(crate::i18n::upper_label("Advanced")).font(theme::semibold(10.5)).color(t.text_faint))
                .id_salt("print-advanced")
                .default_open(d.as_image)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(widgets::checkbox(&mut d.as_image, tl!("Print as image")));
                        ui.add_enabled_ui(d.as_image, |ui| {
                            ui.label(tl!("Resolution:"));
                            let mut dpi = d.dpi.round() as u32;
                            combo(ui, "print-dpi", tl!("Resolution:"), &mut dpi, &[(150, "150 dpi"), (300, "300 dpi"), (600, "600 dpi")], 100.0);
                            d.dpi = f64::from(dpi);
                        });
                    });
                    ui.label(
                        egui::RichText::new(tl!(
                            "Use it when a printer skips text, shading or transparency. The print is no longer searchable text; with Print in grayscale, the pictures are grey."
                        ))
                        .small()
                        .color(t.text_muted),
                    );
                });
        });
        ui.add_space(12.0);
        // Preview.
        ui.vertical(|ui| {
            ui.set_width(PREVIEW_W);
            let (area, _) = ui.allocate_exact_size(vec2(PREVIEW_W, PREVIEW_H), egui::Sense::hover());
            ui.painter().rect_filled(area, 6.0, t.hover);
            match (&settings, sheets.get(d.sheet)) {
                (Err(e), _) => {
                    ui.put(area.shrink(16.0), egui::Label::new(egui::RichText::new(tl!(e)).color(t.text_muted)).wrap());
                }
                (Ok(_), None) if too_much_creep => {
                    let msg = tl!("The creep moves the innermost sheet's pages too far for this many pages. Reduce Creep.");
                    ui.put(area.shrink(16.0), egui::Label::new(egui::RichText::new(msg).color(t.text_muted)).wrap());
                }
                (Ok(_), Some(sheet)) => {
                    let k = (PREVIEW_BOX.0 / sheet.size.0 as f32).min(PREVIEW_BOX.1 / sheet.size.1 as f32);
                    let paper = Rect::from_center_size(area.center(), vec2(sheet.size.0 as f32 * k, sheet.size.1 as f32 * k));
                    ui.painter().rect_filled(paper, 0.0, Color32::WHITE);

                    // Milestone 4: Draw Hardware Margins (Shaded Unprintable Strips) & Printable Bounds
                    if let Some([m_left, m_top, m_right, m_bottom]) = d.preview_hardware_margins {
                        let strip_color = Color32::from_rgba_unmultiplied(215, 220, 230, 100);
                        let left_strip = Rect::from_min_max(paper.left_top(), pos2(paper.left() + m_left as f32 * k, paper.bottom()));
                        ui.painter().rect_filled(left_strip, 0.0, strip_color);
                        let right_strip = Rect::from_min_max(pos2(paper.right() - m_right as f32 * k, paper.top()), paper.right_bottom());
                        ui.painter().rect_filled(right_strip, 0.0, strip_color);
                        let top_strip = Rect::from_min_max(paper.left_top(), pos2(paper.right(), paper.top() + m_top as f32 * k));
                        ui.painter().rect_filled(top_strip, 0.0, strip_color);
                        let bottom_strip = Rect::from_min_max(pos2(paper.left(), paper.bottom() - m_bottom as f32 * k), paper.right_bottom());
                        ui.painter().rect_filled(bottom_strip, 0.0, strip_color);

                        let printable = Rect::from_min_max(
                            pos2(paper.left() + m_left as f32 * k, paper.top() + m_top as f32 * k),
                            pos2(paper.right() - m_right as f32 * k, paper.bottom() - m_bottom as f32 * k),
                        );
                        ui.painter().rect_stroke(printable, 0.0, Stroke::new(1.0, Color32::from_rgb(70, 130, 230)), egui::StrokeKind::Inside);
                    }

                    // Milestone 4: Clipping Hazard Detection
                    let mut has_clipping = false;
                    if let Some([m_left, m_top, m_right, m_bottom]) = d.preview_hardware_margins {
                        for pl in &sheet.placed {
                            let [x0, y0, x1, y1] = pl.clip;
                            let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| pl.matrix.apply(x, y));
                            for (cx, cy) in corners {
                                if cx < m_left - 0.5 || cx > (sheet.size.0 - m_right) + 0.5 || cy < m_bottom - 0.5 || cy > (sheet.size.1 - m_top) + 0.5 {
                                    has_clipping = true;
                                    break;
                                }
                            }
                        }
                    }
                    if has_clipping {
                        d.clipping_hazard = Some([1.0, 1.0, 1.0, 1.0]);
                        let pulse = 160 + ((ui.input(|i| i.time * 4.0).sin() * 70.0) as u8);
                        ui.painter().rect_stroke(paper, 0.0, Stroke::new(2.5, Color32::from_rgba_unmultiplied(235, 45, 35, pulse)), egui::StrokeKind::Outside);
                    } else {
                        d.clipping_hazard = None;
                        ui.painter().rect_stroke(paper, 0.0, Stroke::new(1.0, t.border), egui::StrokeKind::Outside);
                    }
                    let to_screen = |x: f64, y: f64| -> Pos2 { pos2(paper.left() + x as f32 * k, paper.bottom() - y as f32 * k) };
                    let painter = ui.painter().with_clip_rect(paper);
                    for pl in &sheet.placed {
                        let (dw, dh) = sizes[pl.page];
                        let [x0, y0, x1, y1] = pl.clip;
                        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| {
                            let (sx, sy) = pl.matrix.apply(x, y);
                            to_screen(sx, sy)
                        });
                        // UVs of the visible part (texture y runs down from the page top).
                        let uv = |x: f64, y: f64| pos2((x / dw) as f32, (1.0 - y / dh) as f32);
                        let uvs = [uv(x0, y0), uv(x1, y0), uv(x1, y1), uv(x0, y1)];
                        match thumb(pl.page) {
                            Some(tex) => {
                                let mut mesh = egui::Mesh::with_texture(tex);
                                for (p, u) in corners.iter().zip(uvs) {
                                    mesh.vertices.push(egui::epaint::Vertex {
                                        pos: *p,
                                        uv: u,
                                        color: if d.grayscale { Color32::from_gray(235) } else { Color32::WHITE },
                                    });
                                }
                                mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
                                painter.add(egui::Shape::mesh(mesh));
                            }
                            None => {
                                painter.add(egui::Shape::convex_polygon(
                                    corners.to_vec(),
                                    Color32::from_gray(245),
                                    Stroke::new(0.5, Color32::from_gray(180)),
                                ));
                                let c = corners.iter().fold(vec2(0.0, 0.0), |a, p| a + p.to_vec2()) / 4.0;
                                painter.text(
                                    c.to_pos2(),
                                    egui::Align2::CENTER_CENTER,
                                    (pl.page + 1).to_string(),
                                    theme::regular(11.0),
                                    Color32::from_gray(120),
                                );
                            }
                        }
                    }
                    for b in &sheet.borders {
                        painter.rect_stroke(
                            Rect::from_two_pos(to_screen(b[0], b[1]), to_screen(b[2], b[3])),
                            0.0,
                            Stroke::new(0.6, Color32::BLACK),
                            egui::StrokeKind::Middle,
                        );
                    }
                    for l in &sheet.lines {
                        painter.line_segment([to_screen(l[0], l[1]), to_screen(l[2], l[3])], Stroke::new(0.6, Color32::BLACK));
                    }
                }
                _ => {}
            }
            ui.horizontal(|ui| {
                let n = sheets.len();
                if ui.add_enabled(d.sheet > 0, egui::Button::new("‹")).on_hover_text(tl!("Previous sheet")).clicked() {
                    d.sheet -= 1;
                }
                ui.label(if n == 0 {
                    tl!("No sheets").to_string()
                } else {
                    crate::i18n::fmt(tl!("Sheet {s} of {n}"), &[("s", &(d.sheet + 1).to_string()), ("n", &n.to_string())])
                });
                if ui.add_enabled(d.sheet + 1 < n, egui::Button::new("›")).on_hover_text(tl!("Next sheet")).clicked() {
                    d.sheet += 1;
                }
            });
            if let Some(s) = sheets.get(d.sheet) {
                ui.label(egui::RichText::new(d.units.size(s.size.0, s.size.1)).small().color(t.text_muted));
                if d.handling == Handling::Size
                    && let Some(p) = s.placed.first()
                {
                    let scale = p.matrix.0[0].hypot(p.matrix.0[1]) * 100.0;
                    ui.label(crate::i18n::fmt(tl!("PDF scale: {scale}%"), &[("scale", &format!("{scale:.2}"))]));
                    ui.label(egui::RichText::new(tl!("Printer paper and scaling may differ. Check driver settings.")).small().color(t.text_muted));
                    if (scale - 100.0).abs() > 0.01 {
                        ui.label(egui::RichText::new(tl!("Drawing scale changes. Use Actual size for full-scale plans.")).small().color(t.text_muted));
                    }
                }
            }

            // Milestone 4: Clipping Hazard Banner & Auto-Fit Action
            if d.clipping_hazard.is_some() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⚠️ Clipping hazard: content extends beyond printable area").color(Color32::from_rgb(225, 45, 35)).small());
                    if ui.small_button(tl!("Auto-Fit to Printable Area")).clicked() {
                        d.auto_fit_to_printable();
                    }
                });
            }
        });
    });
    }); // end ScrollArea (#4252, #5317)
    ui.add_space(12.0);
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
                settings.is_ok() && !sheets.is_empty() && !looking && !waiting_paper && has_cluster_targets,
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

    // Milestone 4: One-Click Print Failover Modal
    failover_modal_window(ui.ctx(), d);

    (go, cancel)
}

fn preset_and_diagnostics_row(ui: &mut egui::Ui, d: &mut PrintDraft) {
    ui.horizontal(|ui| {
        let active_name = d.preset_store.active_preset().map_or("Custom Setup", |p| &p.name);
        let mut chosen_preset = None;
        craft_ui::ComboBox::from_id_salt("job-preset-picker")
            .selected_text(active_name)
            .width(175.0)
            .show_ui(ui, |ui| {
                for preset in d.preset_store.list_presets() {
                    if ui
                        .selectable_label(d.preset_store.active_preset_id.as_deref() == Some(&preset.id), &preset.name)
                        .clicked()
                    {
                        chosen_preset = Some(preset.clone());
                    }
                }
            });
        if let Some(p) = chosen_preset {
            d.preset_store.active_preset_id = Some(p.id.clone());
            d.apply_preset(&p);
        }

        if ui
            .small_button(tl!("+ Save"))
            .on_hover_text(tl!("Save current print settings as a reusable preset"))
            .clicked()
        {
            d.new_preset_name = match d.new_preset_name {
                Some(_) => None,
                None => Some(String::new()),
            };
        }

        if ui
            .small_button(tl!("📋 Copy Diagnostics"))
            .on_hover_text(tl!("Copy full GFM Markdown diagnostic report to clipboard"))
            .clicked()
        {
            let report = collect_ui_diagnostic_report(d);
            ui.ctx().copy_text(report.to_markdown_table());
        }

        if ui
            .small_button(tl!("💾 Export JSON"))
            .on_hover_text(tl!("Export diagnostic report as JSON to file (and copy to clipboard)"))
            .clicked()
        {
            export_diagnostics_json(ui.ctx(), d);
        }
    });
}

fn export_diagnostics_json(ctx: &egui::Context, d: &PrintDraft) {
    let report = collect_ui_diagnostic_report(d);
    if let Ok(json) = report.to_json_pretty() {
        ctx.copy_text(json.clone());
        #[cfg(not(target_arch = "wasm32"))]
        {
            std::thread::spawn(move || {
                let file = pollster::block_on(
                    rfd::AsyncFileDialog::new()
                        .set_title("Export Diagnostic Report")
                        .set_file_name("pdfcraft-diagnostics.json")
                        .add_filter("JSON", &["json"])
                        .save_file(),
                );
                if let Some(file) = file {
                    let _ = std::fs::write(file.path(), json);
                }
            });
        }
    }
}

fn save_preset_inline_row(ui: &mut egui::Ui, d: &mut PrintDraft) {
    let Some(mut name_buf) = d.new_preset_name.clone() else { return };
    let mut save = false;
    let mut cancel = false;
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut name_buf).hint_text(tl!("Preset name")).desired_width(180.0));
        save = ui.add_enabled(!name_buf.trim().is_empty(), egui::Button::new(tl!("Save Preset"))).clicked();
        cancel = ui.button(tl!("Cancel")).clicked();
    });
    if save {
        let clean = name_buf.trim().to_string();
        let id = format!("user-preset-{}", d.preset_store.presets.len() + 1);
        let preset = d.snapshot_as_preset(id.clone(), clean);
        d.preset_store.upsert_preset(preset);
        d.preset_store.set_active_preset(Some(id));
        d.new_preset_name = None;
    } else if cancel {
        d.new_preset_name = None;
    } else {
        d.new_preset_name = Some(name_buf);
    }
}

fn cluster_mode_header_row(ui: &mut egui::Ui, d: &mut PrintDraft) {
    let was_multi = d.multi_printer_mode;
    ui.horizontal(|ui| {
        ui.add(widgets::checkbox(&mut d.multi_printer_mode, tl!("Multi-Printer Cluster Dispatch")));
        if d.multi_printer_mode && !was_multi {
            if d.selected_printers.is_empty()
                && let Some(ref primary) = d.printer
                && primary != crate::web_print::BROWSER_PRINTER
            {
                d.selected_printers.insert(primary.clone());
            }
            if d.active_preview_printer.is_none() {
                d.active_preview_printer = d.selected_printers.iter().next().cloned().or_else(|| d.printer.clone());
            }
            if let Some(ref preview) = d.active_preview_printer {
                d.read_caps_for = Some(preview.clone());
            }
        }
        if d.multi_printer_mode {
            if !d.preset_store.groups.is_empty() {
                let active_group = d.preset_store.active_group().map_or("Select Group", |g| &g.name);
                let mut chosen_group = None;
                craft_ui::ComboBox::from_id_salt("printer-group-picker")
                    .selected_text(active_group)
                    .width(135.0)
                    .show_ui(ui, |ui| {
                        for group in d.preset_store.list_groups() {
                            if ui
                                .selectable_label(d.preset_store.active_group_id.as_deref() == Some(&group.id), &group.name)
                                .clicked()
                            {
                                chosen_group = Some(group.id.clone());
                            }
                        }
                    });
                if let Some(gid) = chosen_group {
                    d.select_printer_group(&gid);
                    if let Some(ref preview) = d.active_preview_printer {
                        d.read_caps_for = Some(preview.clone());
                    }
                }
            }
            if ui
                .small_button(tl!("+ Save Group"))
                .on_hover_text(tl!("Save selected printers as a reusable printer group"))
                .clicked()
            {
                d.new_group_name = match d.new_group_name {
                    Some(_) => None,
                    None => Some(String::new()),
                };
            }
        }
    });
}

fn save_group_inline_row(ui: &mut egui::Ui, d: &mut PrintDraft) {
    let Some(mut name_buf) = d.new_group_name.clone() else { return };
    let mut save = false;
    let mut cancel = false;
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut name_buf).hint_text(tl!("Printer group name")).desired_width(180.0));
        let can_save = !name_buf.trim().is_empty() && !d.selected_printers.is_empty();
        save = ui.add_enabled(can_save, egui::Button::new(tl!("Save Group"))).clicked();
        cancel = ui.button(tl!("Cancel")).clicked();
    });
    if save {
        let clean = name_buf.trim().to_string();
        let id = format!("group-{}", d.preset_store.groups.len() + 1);
        let group = crate::workflows::print_presets::PrinterGroup::with_printers(
            id.clone(),
            clean,
            d.selected_printers.iter().cloned(),
            false,
        );
        d.preset_store.upsert_group(group);
        d.preset_store.set_active_group(Some(id));
        d.new_group_name = None;
    } else if cancel {
        d.new_group_name = None;
    } else {
        d.new_group_name = Some(name_buf);
    }
}

fn cluster_printers_panel(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            if ui.small_button(tl!("Select All")).clicked() {
                for p in &d.printers {
                    if p.name != crate::web_print::BROWSER_PRINTER {
                        d.selected_printers.insert(p.name.clone());
                    }
                }
                if d.active_preview_printer.is_none() {
                    d.active_preview_printer = d.selected_printers.iter().next().cloned();
                    d.read_caps_for = d.active_preview_printer.clone();
                }
            }
            if ui.small_button(tl!("Clear")).clicked() {
                d.selected_printers.clear();
            }
            ui.label(
                egui::RichText::new(format!("({} selected)", d.selected_printers.len()))
                    .small()
                    .color(t.text_muted),
            );
        });
        egui::ScrollArea::vertical().id_salt("printer-cluster-grid").max_height(105.0).show(ui, |ui| {
            let printer_list = d.printers.clone();
            for p in &printer_list {
                ui.horizontal(|ui| {
                    let mut is_selected = d.selected_printers.contains(&p.name);
                    if ui.add(widgets::checkbox(&mut is_selected, p.name.as_str())).changed() {
                        if is_selected {
                            d.selected_printers.insert(p.name.clone());
                            d.read_caps_for = Some(p.name.clone());
                        } else {
                            d.selected_printers.remove(&p.name);
                        }
                    }
                    if p.default {
                        ui.label(egui::RichText::new("(default)").small().color(t.text_muted));
                    }
                    let is_preview = d.active_preview_printer.as_deref() == Some(&p.name);
                    if ui.selectable_label(is_preview, "👁 Preview").clicked() {
                        d.active_preview_printer = Some(p.name.clone());
                        if let Some(caps) = d.cached_capabilities.get(&p.name).cloned() {
                            d.apply_capabilities_telemetry(p.name.clone(), caps);
                        } else {
                            d.read_caps_for = Some(p.name.clone());
                        }
                    }
                });
            }
        });
        if d.selected_printers.is_empty() {
            ui.label(
                egui::RichText::new(tl!("Select at least one target printer to dispatch the batch."))
                    .small()
                    .color(Color32::from_rgb(220, 80, 35)),
            );
        } else if matches!(d.duplex, Some(spool::Duplex::LongEdge | spool::Duplex::ShortEdge)) {
            let simplex_only: Vec<&str> = d
                .selected_printers
                .iter()
                .filter(|name| d.printer_two_sided_for(name) == Some(false))
                .map(String::as_str)
                .collect();
            if !simplex_only.is_empty() {
                ui.label(
                    egui::RichText::new(format!(
                        "⚠️ Simplex hardware detected ({}) — two-sided printers will print duplex while simplex devices fall back safely.",
                        simplex_only.join(", ")
                    ))
                    .small()
                    .color(Color32::from_rgb(205, 130, 20)),
                );
            }
        }
    });
}

pub(crate) fn failover_modal_window(ctx: &egui::Context, d: &mut PrintDraft) {
    let copy_diag_clicked = std::cell::Cell::new(false);
    if let Some(modal) = &mut d.failover_modal {
        let mut modal_open = true;
        let mut close_modal = false;
        egui::Window::new(tl!("One-Click Print Failover"))
            .open(&mut modal_open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(390.0);
                ui.heading("⚠️ Printer Hardware / Spooler Fault");
                ui.add_space(4.0);
                ui.label(format!("Faulted Printer: {}", modal.from_printer));
                ui.label(format!("Failure Cause: {}", modal.error_message));
                ui.label(format!("Faulted Sheet Index: #{}", modal.failed_sheet + 1));
                ui.label(format!(
                    "Committed: {} sheets | Pending: {} sheets",
                    modal.committed_sheets, modal.pending_sheets
                ));
                ui.separator();
                ui.label(tl!("Select Failover Target Printer:"));
                craft_ui::ComboBox::from_id_salt("failover-target-printer")
                    .selected_text(if modal.to_printer.is_empty() {
                        "Pick Target Printer"
                    } else {
                        &modal.to_printer
                    })
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for p in &d.printers {
                            if p.name != modal.from_printer {
                                ui.selectable_value(&mut modal.to_printer, p.name.clone(), &p.name);
                            }
                        }
                    });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let can_reroute = !modal.to_printer.is_empty() && modal.to_printer != modal.from_printer;
                    if ui
                        .add_enabled(can_reroute, egui::Button::new(tl!("Reroute Remaining Sheets (Zero Duplicates)")))
                        .clicked()
                    {
                        d.pending_failover = Some((modal.from_printer.clone(), modal.to_printer.clone()));
                        close_modal = true;
                    }
                    if ui.button(tl!("📋 Copy Diagnostics")).clicked() {
                        copy_diag_clicked.set(true);
                    }
                    if ui.button(tl!("Cancel")).clicked() {
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
fn custom_paper_row(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    let unit = d.units;
    let Some((name, width, height)) = d.custom_new.as_mut() else { return };
    let side = |s: &str| s.trim().replace(',', ".").parse::<f64>().ok().map(|v| v / unit.per_point());
    let paper = match (side(width), side(height)) {
        (Some(w), Some(h)) => crate::print_paper::CustomPaper::new(name, w, h),
        _ => None,
    };
    let (mut add, mut cancel) = (false, false);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(name).hint_text(tl!("Name")).desired_width(110.0)).accessible_name(tl!("Name"));
        ui.add(egui::TextEdit::singleline(width).hint_text(tl!("Width")).desired_width(52.0)).accessible_name(tl!("Width"));
        ui.label("×");
        ui.add(egui::TextEdit::singleline(height).hint_text(tl!("Height")).desired_width(52.0)).accessible_name(tl!("Height"));
        ui.label(egui::RichText::new(unit.label()).color(t.text_muted));
        add = ui.add_enabled(paper.is_some(), egui::Button::new(tl!("Add"))).clicked();
        cancel = ui.button(tl!("Cancel")).clicked();
    });
    if add && let Some(paper) = paper {
        d.add_custom_paper(paper);
        d.custom_new = None;
    } else if cancel {
        d.custom_new = None;
    }
}

fn build_printer_telemetry(
    d: &PrintDraft,
    p: &spool::Printer,
) -> pdfcraft_platform::diagnostics::PrinterCapabilitiesTelemetry {
    use pdfcraft_platform::diagnostics::{
        DriverGenerationKind, DuplexBindingKind, MarginsPt, PrinterCapabilitiesTelemetry, ResolutionDpi, SizePt,
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
    let generation = if cfg!(windows) {
        if spool::xps_driver(&p.name) == Some(true) {
            DriverGenerationKind::Win32V4PrintTicket
        } else {
            DriverGenerationKind::Win32V3DevMode
        }
    } else {
        DriverGenerationKind::CupsIpp
    };
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
        printable_area_pt: SizePt {
            width: printable.0,
            height: printable.1,
        },
        physical_sheet_pt: SizePt {
            width: printable.0 + margins[0] + margins[2],
            height: printable.1 + margins[1] + margins[3],
        },
        resolution_dpi: ResolutionDpi { dpi_x: 600, dpi_y: 600 },
        supported_paper_count: d.papers.len(),
    }
}

fn build_applied_ticket(
    d: &PrintDraft,
    printer_name: &str,
) -> pdfcraft_platform::diagnostics::AppliedTicketTelemetry {
    use pdfcraft_platform::diagnostics::{
        AppliedColorMode, AppliedOrientation, AppliedTicketTelemetry, DriverGenerationKind, DuplexBindingKind,
        TicketDetails,
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
        .unwrap_or_else(|| {
            if cfg!(windows) {
                if spool::xps_driver(printer_name) == Some(true) {
                    DriverGenerationKind::Win32V4PrintTicket
                } else {
                    DriverGenerationKind::Win32V3DevMode
                }
            } else {
                DriverGenerationKind::CupsIpp
            }
        });

    let details = match generation {
        DriverGenerationKind::Win32V4PrintTicket => TicketDetails::PrintTicketV4 {
            feature_color: if d.grayscale {
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
            feature_media_size: paper_name.clone(),
            raw_xml_snippet: None,
        },
        DriverGenerationKind::CupsIpp => TicketDetails::CupsIpp {
            print_color_mode: if d.grayscale {
                "monochrome".to_string()
            } else {
                "color".to_string()
            },
            sides: match duplex_mode {
                DuplexBindingKind::Simplex => "one-sided".to_string(),
                DuplexBindingKind::FlipLongEdge => "two-sided-long-edge".to_string(),
                DuplexBindingKind::FlipShortEdge => "two-sided-short-edge".to_string(),
            },
            media: paper_name.clone(),
            copies: job.copies,
            raw_options: job.options,
        },
        _ => TicketDetails::DevModeV3 {
            dm_fields_bitmask: 0x0001 | 0x0002 | 0x0004 | 0x0800 | 0x1000,
            dm_size: 220,
            dm_driver_extra_bytes: 1024,
            dm_color: if d.grayscale { 1 } else { 2 },
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
    };

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

/// Harvests active print studio state into a complete platform DiagnosticReport.
pub(crate) fn collect_ui_diagnostic_report(d: &PrintDraft) -> pdfcraft_platform::diagnostics::DiagnosticReport {
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

    collect_diagnostic_report(printers, applied_tickets, traces)
}

#[cfg(test)]
mod tests {
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::i18n::Lang;
    use crate::{Dialog, PdfCraftApp};

    /// The row the settings column really gives the header: `body` sets it to 470 px, less the
    /// frame's 10 px margins.
    const PANEL_ROW: f32 = 450.0;

    /// One frame of the header at `width`, drawn in the app's own fonts; with `hover`, the pointer
    /// is parked on the title and the frames after the first are run so that a tooltip can come up.
    /// Returns the title's rectangle (none when the title was left out), the reset button's, and
    /// whether a tooltip layer is open.
    fn header_frame(lang: &str, width: f32, hover: bool) -> (Option<egui::Rect>, egui::Rect, bool) {
        crate::i18n::set_current(Lang::from_code(lang).expect("a catalogued language"));
        let ctx = egui::Context::default();
        ctx.set_fonts(theme::font_definitions());
        // Without this the tooltip would need half a second of frames to show.
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
            // Nothing draws the frame, but epaint's deltas must be taken anyway.
            output.textures_delta.clear();
        }
        let tooltip = ctx.memory(|memory| memory.areas().visible_layer_ids().iter().any(|layer| layer.order == egui::Order::Tooltip));
        crate::i18n::set_current(Lang::EN);
        let (title, reset) = seen.get();
        (title, reset, tooltip)
    }

    fn header_at(lang: &str, width: f32) -> (Option<egui::Rect>, egui::Rect) {
        let (title, reset, _) = header_frame(lang, width, false);
        (title, reset)
    }

    /// #5545: in a long locale the reset label is wider than the row, and it used to be painted
    /// over the title. Every catalogued language is checked at the row the panel really gives the
    /// header, so the longest translation of "Reset to printer defaults" is covered by the run
    /// rather than by a guess at which locale is the worst one.
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
            assert!(reset.right() <= PANEL_ROW, "{}: the reset button {reset:?} runs out of the row", lang.code());
        }
    }

    /// A row narrower than the buttons plus one ellipsis cannot be cut any further: the title is
    /// left out instead of being painted back over the buttons. Bulgarian, Ukrainian and Spanish
    /// are the catalogs that get there first, at the row's own width.
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
        assert!(title.is_none(), "the title is dropped, not squeezed to a sliver: {:?} next to {reset:?}", title);
    }

    /// The title is what gives way: it truncates in a narrow row while the buttons keep their
    /// size, and a wide row keeps it whole instead of cutting it for nothing.
    #[test]
    fn the_title_truncates_and_leaves_the_buttons_whole() {
        let (wide_title, wide_reset) = header_at("en", 620.0);
        let (narrow_title, narrow_reset) = header_at("en", 300.0);
        let (wide_title, narrow_title) = (wide_title.expect("the wide row keeps the title"), narrow_title.expect("the narrow row keeps a title"));
        assert!(narrow_title.width() < wide_title.width(), "the title shrinks in a narrow row");
        assert_eq!(narrow_reset.width(), wide_reset.width(), "the buttons keep their size");
        assert!(narrow_reset.right() <= 301.0, "the reset button stays inside the row");
    }

    /// The cut title stays reachable: hovering it shows the full string in a tooltip (egui's
    /// `Label` does that by itself when it elides), and a title that fits whole shows none.
    #[test]
    fn the_truncated_title_is_reachable_through_a_tooltip() {
        if !crate::i18n::needs_catalogs() {
            return;
        }
        let (whole, _, _) = header_frame("it", 620.0, false);
        let whole = whole.expect("a wide row keeps the title");
        let (cut, _, tooltip) = header_frame("it", PANEL_ROW, true);
        let cut = cut.expect("the Italian title is elided, not dropped");
        assert!(cut.width() < whole.width(), "the Italian title is cut at {PANEL_ROW}px: {} of {}", cut.width(), whole.width());
        assert!(tooltip, "hovering the elided title opens a tooltip with the full string");
        let (_, _, tooltip) = header_frame("en", 620.0, true);
        assert!(!tooltip, "a title that fits whole needs no tooltip");
    }

    /// #4252, #5317: on a short screen the settings+preview area used to grow past
    /// both the top and bottom of the screen (centered modals overflow both edges equally),
    /// taking the Cancel button with it. It must now stay reachable, scrolling instead.
    #[test]
    fn the_print_dialog_keeps_its_buttons_within_a_short_screen() {
        let screen = egui::vec2(1000.0, 480.0); // shorter than the settings+preview column alone
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
        assert!(cancel.min.y >= 0.0 && cancel.max.y <= screen.y, "Cancel must stay within the {}×{} screen, got {cancel:?}", screen.x, screen.y);
    }
}
