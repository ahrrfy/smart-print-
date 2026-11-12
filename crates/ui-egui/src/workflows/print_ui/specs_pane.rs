//! Center column of the Print Studio: Unified Specifications & Broadcast Controls.
//! Set once -> automatically broadcast to all selected target printers (`0 ms` standard
//! paper pills, explicit Color vs. B&W cards, 1-click Two-Sided pills, Orientation,
//! Copies/Collate, Pages, Imposition Handling, Driver Options, and Advanced settings).

use egui::{Color32, CornerRadius, Stroke, vec2};

use super::body::{collect_ui_diagnostic_report, combo, custom_paper_row, driver_options_panel};
use super::draft::{CUSTOM_SCALE, ColorIntent, Handling, PrintDraft, Which};
use super::{
    Binding, BookletSubset, Content, Orientation, PageOrder, SizeMode, Subset, Tokens, print,
    spool, theme, widgets,
};

/// Quick standard paper pills with exact metric/imperial dimensions so labels are informative
/// and never collide with `Paper:` `ComboBox` option labels in AccessKit queries.
const QUICK_PAPER_PILLS: [(&str, &str); 4] = [
    ("A4", "A4 · 210×297"),
    ("A3", "A3 · 297×420"),
    ("US Letter", "Letter · 8.5×11"),
    ("US Legal", "Legal · 8.5×14"),
];

/// Renders the Center Unified Specs column inside its own isolated vertical `ScrollArea`.
pub(super) fn show(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    sizes: &[(f64, f64)],
    width: f32,
    body_h: f32,
) {
    ui.set_width(width);
    ui.scope(|ui| {
        ui.spacing_mut().scroll.floating = false;
        ui.spacing_mut().scroll.bar_width = 7.0;
        ui.spacing_mut().scroll.bar_inner_margin = 6.0;
        ui.spacing_mut().scroll.bar_outer_margin = 1.0;
        if ui.style().scroll_animation != egui::style::ScrollAnimation::none() {
            ui.style_mut().scroll_animation = egui::style::ScrollAnimation::duration(0.25);
        }
        egui::ScrollArea::vertical()
            .id_salt("print-specs-scroll")
            .max_height(body_h)
            .min_scrolled_height(body_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width((width - 16.0).max(280.0));
                show_contents(ui, d, t, sizes);
            });
    });
}

/// Renders the Unified Specifications controls directly (called inside `show`'s isolated scroll
/// or inside the compact `< 1020 pt` left-column scroll without nesting).
pub(super) fn show_contents(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    sizes: &[(f64, f64)],
) {
    ui.spacing_mut().interact_size.y = 24.0;
    ui.spacing_mut().item_spacing = vec2(5.0, 3.5);
    let browser = !d.multi_printer_mode && d.printer.as_deref() == Some(crate::web_print::BROWSER_PRINTER);

    render_broadcast_banner(ui, d, t);
    render_preset_and_template_bar(ui, d, browser);
    if d.new_preset_name.is_some() {
        save_preset_inline_row(ui, d);
    }

    render_driver_notices_and_options(ui, d, t);
    render_color_and_copies_section(ui, d, t, browser);
    render_paper_section(ui, d, t, sizes, browser);
    if d.handling != Handling::Booklet {
        render_two_sided_section(ui, d, browser);
    }

    render_orientation_row(ui, d);
    render_pages_to_print_section(ui, d, sizes);
    render_sizing_and_handling_section(ui, d, t);
    render_comments_and_advanced_section(ui, d, t);
}

fn render_broadcast_banner(ui: &mut egui::Ui, d: &PrintDraft, t: &Tokens) {
    let targets = d.effective_target_count();
    let (fill, border_col, icon_col, text_col, message) = if targets > 1 {
        (
            t.accent_soft,
            t.accent.gamma_multiply(0.35),
            t.accent_text,
            t.accent_text,
            format!(
                "Unified Broadcast Active — all specs below apply to {targets} selected printers"
            ),
        )
    } else if targets == 1 {
        (
            t.hover.gamma_multiply(0.75),
            t.divider,
            t.accent,
            t.text_muted,
            tl!("Unified Print Specs — check 2+ printers on the left to broadcast simultaneously")
                .to_string(),
        )
    } else {
        (
            t.hover.gamma_multiply(0.6),
            t.divider,
            t.icon,
            t.text_muted,
            tl!("Unified Print Specs — ready for PDF export or multi-printer selection").to_string(),
        )
    };

    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, border_col))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(crate::icons::image("layers", 14.0, icon_col));
                ui.label(egui::RichText::new(message).font(theme::medium(11.5)).color(text_col));
            });
        });
}

fn render_preset_and_template_bar(ui: &mut egui::Ui, d: &mut PrintDraft, browser: bool) {
    ui.horizontal_wrapped(|ui| {
        let preset_lbl = ui.label(tl!("Preset:"));
        let active_name = d.preset_store.active_preset().map_or("Custom Setup", |p| p.name.as_str());
        let mut chosen_preset = None;
        craft_ui::ComboBox::from_id_salt("job-preset-picker")
            .selected_text(active_name)
            .width(155.0)
            .show_ui(ui, |ui| {
                for preset in d.preset_store.list_presets() {
                    let is_cur = d.preset_store.active_preset_id.as_deref() == Some(preset.id.as_str());
                    if ui.selectable_label(is_cur, &preset.name).clicked() {
                        chosen_preset = Some(preset.clone());
                    }
                }
            })
            .response
            .labelled_by(preset_lbl.id);

        if let Some(p) = chosen_preset {
            d.apply_preset(&p);
        }

        if ui
            .small_button(tl!("+ Save"))
            .on_hover_text(tl!("Save current unified specifications as a reusable preset"))
            .clicked()
        {
            d.new_preset_name = match d.new_preset_name {
                Some(_) => None,
                None => Some(String::new()),
            };
        }

        let ref_printer = if d.multi_printer_mode {
            d.active_preview_printer
                .clone()
                .or_else(|| d.selected_printers.iter().next().cloned())
        } else {
            d.printer.clone()
        };
        let can_capture = ref_printer.is_some() && !browser && spool::HAS_PRINTER_PREFERENCES;
        if ui
            .add_enabled(can_capture, egui::Button::new(tl!("Capture Template…")).small())
            .on_hover_text(tl!(
                "Open native driver preferences once and broadcast captured paper, color, duplex, orientation & copies to all selected printers"
            ))
            .clicked()
            && let Some(p) = ref_printer
        {
            let _ = d.capture_driver_template_from(&p);
        }
    });
}

fn save_preset_inline_row(ui: &mut egui::Ui, d: &mut PrintDraft) {
    let Some(mut name_buf) = d.new_preset_name.clone() else { return };
    let mut save = false;
    let mut dismiss = false;
    ui.horizontal(|ui| {
        ui.add(
            widgets::line(&mut name_buf)
                .hint_text(tl!("Preset name"))
                .desired_width(175.0)
                .id_salt("new-preset-name"),
        )
        .accessible_name(tl!("Preset name"));
        save = ui
            .add_enabled(!name_buf.trim().is_empty(), egui::Button::new(tl!("Save Preset")).small())
            .clicked();
        dismiss = crate::icons::labeled_button(
            ui,
            "x",
            24.0,
            false,
            tl!("Close preset form"),
            tl!("Close preset form"),
        )
        .clicked();
    });
    if save {
        let clean = name_buf.trim().to_string();
        let id = format!("user-preset-{}", d.preset_store.presets.len() + 1);
        d.save_current_as_preset(id, clean);
        d.new_preset_name = None;
    } else if dismiss {
        d.new_preset_name = None;
    } else {
        d.new_preset_name = Some(name_buf);
    }
}

fn render_driver_notices_and_options(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    if !d.multi_printer_mode && d.printer_two_sided() == Some(false) {
        ui.label(
            egui::RichText::new(tl!(
                "This printer's driver doesn't take two-sided settings from other programs, so its own setting applies. Change it in Properties…"
            ))
            .small()
            .color(t.text_muted),
        );
    }
    if !d.multi_printer_mode && d.printer.as_deref() == Some(crate::web_print::BROWSER_PRINTER) {
        ui.label(
            egui::RichText::new(tl!(
                "Copies, two-sided printing and colour are chosen in the browser's print dialog."
            ))
            .small()
            .color(t.text_muted),
        );
    }
    if let Some(e) = &d.driver_error {
        ui.label(
            egui::RichText::new(crate::i18n::fmt(
                tl!("Couldn't open the printer's preferences: {e}"),
                &[("e", e)],
            ))
            .small()
            .color(t.text_muted),
        );
    }
    if d.show_driver_options {
        if d.driver_options.is_none() && d.driver_options_loading.is_some() {
            ui.label(egui::RichText::new(tl!("Reading the printer's settings…")).small().color(t.text_muted));
        } else {
            driver_options_panel(ui, d, t);
        }
    }
}

fn render_color_and_copies_section(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    browser: bool,
) {
    ui.add_enabled_ui(!browser, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(tl!("Color:")).color(t.text_muted));
            let is_color = d.color_intent() == ColorIntent::Color;
            if color_intent_pill(ui, t, "palette", tl!("Explicit Color"), is_color).clicked() {
                d.set_color_intent(ColorIntent::Color);
            }
            if color_intent_pill(ui, t, "contrast", tl!("Explicit B&W"), !is_color).clicked() {
                d.set_color_intent(ColorIntent::Monochrome);
            }
            ui.add_space(4.0);
            let copies_label = ui.label(tl!("Copies:"));
            let prev_copies = d.copies;
            ui.add(egui::DragValue::new(&mut d.copies).range(1..=999))
                .labelled_by(copies_label.id);
            if d.copies != prev_copies {
                d.set_unified_copies(d.copies);
            }
            let prev_collate = d.collate;
            ui.add(widgets::checkbox(&mut d.collate, tl!("Collate")));
            if d.collate != prev_collate {
                d.set_unified_collate(d.collate);
            }
        });
    });
}

fn color_intent_pill(
    ui: &mut egui::Ui,
    t: &Tokens,
    icon_name: &str,
    label: &str,
    selected: bool,
) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::semibold(11.0),
        if selected { t.accent_text } else { t.text },
    );
    let size = vec2(galley.size().x + 25.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::RadioButton, selected, true, label)
    });
    let fill = if selected {
        t.selected
    } else if resp.hovered() {
        t.hover
    } else {
        t.card
    };
    let border = if selected {
        Stroke::new(1.4, t.accent)
    } else {
        Stroke::new(1.0, t.border)
    };
    ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
    ui.painter().rect_stroke(rect, CornerRadius::same(6), border, egui::StrokeKind::Inside);
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        vec2(13.0, 13.0),
    );
    crate::icons::paint(
        ui,
        icon_rect,
        icon_name,
        12.5,
        if selected { t.accent_text } else { t.icon },
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 22.0, rect.center().y - galley.size().y * 0.5),
        galley,
        if selected { t.accent_text } else { t.text },
    );
    theme::hand(resp)
}

fn render_paper_section(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    sizes: &[(f64, f64)],
    browser: bool,
) {
    // 0 ms 1-click standard paper pills (immediate override, never blocked by slow driver queries)
    let cur_paper = d.paper_name().to_string();
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(tl!("Quick Paper:")).small().color(t.text_muted));
        for (canon, pill_label) in QUICK_PAPER_PILLS {
            let active = cur_paper.eq_ignore_ascii_case(canon);
            if ui.selectable_label(active, pill_label).clicked() {
                d.select_standard_paper(canon);
            }
        }
    });

    render_paper_combo_and_setup_row(ui, d, t, browser);

    if d.custom_new.is_some() {
        custom_paper_row(ui, d, t);
    }

    if let Some(&source) = sizes.get(d.current_page) {
        ui.horizontal_wrapped(|ui| {
            ui.label(crate::i18n::fmt(
                tl!("PDF page: {size}"),
                &[("size", &d.units.size(source.0, source.1))],
            ));
            let matched = d.matching_paper(source);
            if ui
                .add_enabled(matched.is_some(), egui::Button::new(tl!("Match page at 100%")).small())
                .clicked()
                && let Some(paper) = matched
            {
                d.paper = paper;
                d.paper_overridden = true;
                d.paper_before_label = None;
                d.handling = Handling::Size;
                d.size = SizeMode::Actual;
                d.orientation = Orientation::Auto;
                d.sheet = 0;
                d.mark_unified_specs_edited();
            }
        });
    }
}

fn render_paper_combo_and_setup_row(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    browser: bool,
) {
    d.paper = d.paper.min(d.paper_count().saturating_sub(1));
    let before = d.paper;
    let mut paper_clicked = false;
    let detecting = d.papers_loading();

    ui.horizontal_wrapped(|ui| {
        let paper_label = ui.label(tl!("Paper:"));
        let shown = if d.paper_is_custom() {
            d.paper_name().to_string()
        } else {
            tl!(&d.paper_name()).to_string()
        };
        craft_ui::ComboBox::from_id_salt("paper")
            .selected_text(shown)
            .width(136.0)
            .show_ui(ui, |ui| {
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
        if ui
            .button(tl!("Custom…"))
            .on_hover_text(tl!("Add a paper size of your own to the list"))
            .clicked()
        {
            d.custom_new = match d.custom_new {
                Some(_) => None,
                None => Some((String::new(), String::new(), String::new())),
            };
        }
        if d.paper_is_custom()
            && ui
                .button(tl!("Remove"))
                .on_hover_text(tl!("Take this paper size off the list"))
                .clicked()
        {
            d.remove_custom_paper();
            paper_clicked = true;
        }
        render_page_setup_button(ui, d, browser);
    });

    if paper_clicked || d.paper != before {
        d.paper_overridden = true;
        d.mark_unified_specs_edited();
    }
    if paper_clicked && d.paper != before && d.paper == print::LABEL_PAPER && d.which != Which::Area {
        d.paper_before_label = Some(before);
        d.handling = Handling::Label;
        d.sheet = 0;
    }
}

fn render_page_setup_button(ui: &mut egui::Ui, d: &mut PrintDraft, browser: bool) {
    let page_setup_tip = if spool::HAS_PRINTER_PREFERENCES {
        tl!(
            "Selects paper size and orientation from the printer driver. For paper tray, print quality or other driver defaults, use Properties…"
        )
    } else {
        tl!("Page setup dialog is not available on this system.")
    };
    let target = if d.multi_printer_mode {
        d.active_preview_printer
            .clone()
            .or_else(|| d.selected_printers.iter().next().cloned())
    } else {
        d.printer.clone()
    };
    let page_setup = ui
        .add_enabled(
            target.is_some() && !browser && spool::HAS_PRINTER_PREFERENCES,
            egui::Button::new(tl!("Page Setup…")),
        )
        .on_hover_text(page_setup_tip);
    if page_setup.clicked()
        && let Some(p) = target
    {
        d.driver_error = None;
        #[cfg(windows)]
        let parent = winsafe::HWND::GetActiveWindow().map(|h| h.ptr() as isize);
        #[cfg(not(windows))]
        let parent = None;
        match spool::show_printer_properties(&p, parent) {
            Ok(Some((name, size))) => {
                d.apply_driver_paper(name, size);
                d.mark_unified_specs_edited();
            }
            Ok(None) => {}
            Err(e) => d.driver_error = Some(e.to_string()),
        }
    }
}

fn render_two_sided_section(ui: &mut egui::Ui, d: &mut PrintDraft, browser: bool) {
    ui.add_enabled_ui(!browser, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(tl!("Two-sided:"));
            let before = d.duplex;
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
                122.0,
            );
            for (val, pill) in [
                (Some(spool::Duplex::Off), "1-Sided"),
                (Some(spool::Duplex::LongEdge), "Long-Edge"),
                (Some(spool::Duplex::ShortEdge), "Short-Edge"),
            ] {
                if ui.selectable_label(d.duplex == val, pill).clicked() {
                    d.duplex = val;
                }
            }
            if d.duplex != before {
                d.set_unified_duplex(d.duplex);
            }
        });
    });

    if d.multi_printer_mode {
        let fallbacks = d.duplex_simplex_fallback_printers();
        if !fallbacks.is_empty() {
            ui.horizontal(|ui| {
                ui.add(crate::icons::image("triangle-alert", 13.0, Color32::from_rgb(205, 130, 20)));
                ui.label(
                    egui::RichText::new(format!(
                        "Simplex fallback active for: {} (other printers print two-sided)",
                        fallbacks.join(", ")
                    ))
                    .small()
                    .color(Color32::from_rgb(205, 130, 20)),
                );
            });
        }
    }
}

fn render_orientation_row(ui: &mut egui::Ui, d: &mut PrintDraft) {
    let before = d.orientation;
    ui.horizontal_wrapped(|ui| {
        ui.label(tl!("Orientation:"));
        widgets::radio_value(ui, &mut d.orientation, Orientation::Auto, tl!("Auto portrait/landscape"));
        widgets::radio_value(ui, &mut d.orientation, Orientation::Portrait, tl!("Portrait"));
        widgets::radio_value(ui, &mut d.orientation, Orientation::Landscape, tl!("Landscape"));
    });
    if d.orientation != before {
        d.set_unified_orientation(d.orientation);
    }
}

fn render_pages_to_print_section(ui: &mut egui::Ui, d: &mut PrintDraft, sizes: &[(f64, f64)]) {
    widgets::section_title(ui, tl!("Pages to Print"));
    ui.horizontal_wrapped(|ui| {
        widgets::radio_value(ui, &mut d.which, Which::All, tl!("All"));
        widgets::radio_value(ui, &mut d.which, Which::Current, tl!("Current page"));
        let pages = widgets::radio_value(ui, &mut d.which, Which::Range, tl!("Pages"));
        let r = ui
            .add_enabled(
                d.which == Which::Range,
                widgets::line(&mut d.range)
                    .hint_text(format!("1-{}", sizes.len()))
                    .desired_width(105.0),
            )
            .labelled_by(pages.id);
        if r.gained_focus() {
            d.which = Which::Range;
        }
    });
    if let Some((page, a)) = d.area {
        let label = crate::i18n::fmt(
            tl!("Selected area (page {page}, {size})"),
            &[
                ("page", &(page + 1).to_string()),
                ("size", &d.units.size(a[2] - a[0], a[3] - a[1])),
            ],
        );
        if widgets::radio_value(ui, &mut d.which, Which::Area, label).clicked() {
            d.set_handling(Handling::Size);
        }
    }
    if !d.selected.is_empty() {
        let label = format!("{} ({})", tl!("Selected pages"), d.selected.len());
        widgets::radio_value(ui, &mut d.which, Which::Selected, label);
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(tl!("More options:"));
        combo(
            ui,
            "subset",
            tl!("More options:"),
            &mut d.subset,
            &[
                (Subset::All, tl!("All pages in range")),
                (Subset::Odd, tl!("Odd pages only")),
                (Subset::Even, tl!("Even pages only")),
            ],
            145.0,
        );
        ui.add(widgets::checkbox(&mut d.reverse, tl!("Reverse pages")));
    });
}

fn render_sizing_and_handling_section(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    widgets::section_title(ui, tl!("Page Sizing & Handling"));
    ui.horizontal_wrapped(|ui| {
        for (h, label) in [
            (Handling::Size, tl!("Size")),
            (Handling::Poster, tl!("Poster")),
            (Handling::Multiple, tl!("Multiple")),
            (Handling::Booklet, tl!("Booklet")),
            (Handling::Label, tl!("Label")),
        ] {
            let offered = d.which != Which::Area || h == Handling::Size;
            if ui
                .add_enabled_ui(offered, |ui| widgets::mode_tab(ui, label, d.handling == h))
                .inner
                .clicked()
            {
                d.set_handling(h);
            }
        }
    });
    ui.add_space(3.0);
    match d.handling {
        Handling::Size => render_handling_size(ui, d),
        Handling::Label => render_handling_label(ui, d, t),
        Handling::Poster => render_handling_poster(ui, d),
        Handling::Multiple => render_handling_multiple(ui, d),
        Handling::Booklet => render_handling_booklet(ui, d),
    }
}

fn render_handling_size(ui: &mut egui::Ui, d: &mut PrintDraft) {
    let prev_size = d.size;
    let prev_scale = d.custom_scale;
    ui.horizontal_wrapped(|ui| {
        widgets::radio_value(ui, &mut d.size, SizeMode::Fit, tl!("Fit"));
        widgets::radio_value(ui, &mut d.size, SizeMode::Actual, tl!("Actual size"));
        widgets::radio_value(ui, &mut d.size, SizeMode::Shrink, tl!("Shrink oversized pages"));
    });
    ui.horizontal(|ui| {
        let custom = matches!(d.size, SizeMode::Custom(_));
        if ui.add(widgets::radio(custom, tl!("Custom scale:"))).clicked() {
            d.size = SizeMode::Custom(d.custom_scale);
        }
        ui.add_enabled(
            custom,
            egui::DragValue::new(&mut d.custom_scale).range(CUSTOM_SCALE).suffix(" %"),
        )
        .accessible_name(tl!("Custom scale"));
    });
    if matches!(d.size, SizeMode::Custom(_)) {
        d.size = SizeMode::Custom(d.custom_scale);
    }
    if d.size != prev_size || (d.custom_scale - prev_scale).abs() > f64::EPSILON {
        d.mark_unified_specs_edited();
    }
}

fn render_handling_label(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    ui.add(widgets::checkbox(&mut d.label_flip, tl!("Turn upside down (180°)")));
    ui.label(
        egui::RichText::new(tl!(
            "Cuts each page to its printed area, turns it upright and fits it to the sheet without stretching. One label per sheet."
        ))
        .small()
        .color(t.text_muted),
    );
}

fn render_handling_poster(ui: &mut egui::Ui, d: &mut PrintDraft) {
    ui.horizontal_wrapped(|ui| {
        let scale_label = ui.label(tl!("Tile scale:"));
        ui.add(egui::DragValue::new(&mut d.poster_scale).range(10.0..=1000.0).suffix(" %"))
            .labelled_by(scale_label.id);
        let overlap_label = ui.label(tl!("Overlap:"));
        ui.add(egui::DragValue::new(&mut d.overlap).range(0.0..=144.0).suffix(" pt"))
            .labelled_by(overlap_label.id);
        ui.add(widgets::checkbox(&mut d.cut_marks, tl!("Cut marks")));
    });
}

fn render_handling_multiple(ui: &mut egui::Ui, d: &mut PrintDraft) {
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
            145.0,
        );
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(tl!("Pages per sheet:"));
        let mut per_sheet = (!d.custom_grid).then_some(d.per_sheet);
        combo(
            ui,
            "per-sheet",
            tl!("Pages per sheet:"),
            &mut per_sheet,
            &[
                (Some(2), "2"),
                (Some(4), "4"),
                (Some(6), "6"),
                (Some(9), "9"),
                (Some(16), "16"),
                (None, tl!("Custom")),
            ],
            70.0,
        );
        d.custom_grid = per_sheet.is_none();
        if let Some(n) = per_sheet {
            d.per_sheet = n;
            page_order(ui, &mut d.order);
        } else {
            let max = print::MAX_GRID;
            ui.add(egui::DragValue::new(&mut d.cols).range(1..=max))
                .accessible_name(tl!("Columns"));
            ui.label(tl!("by"));
            ui.add(egui::DragValue::new(&mut d.rows).range(1..=max))
                .accessible_name(tl!("Rows"));
        }
    });
    if d.custom_grid {
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

fn render_handling_booklet(ui: &mut egui::Ui, d: &mut PrintDraft) {
    ui.horizontal_wrapped(|ui| {
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
            125.0,
        );
        ui.label(tl!("Binding:"));
        combo(
            ui,
            "binding",
            tl!("Binding:"),
            &mut d.binding,
            &[(Binding::Left, tl!("Left")), (Binding::Right, tl!("Right"))],
            78.0,
        );
        let creep_label = ui.label(tl!("Creep:"));
        ui.add(
            egui::DragValue::new(&mut d.creep)
                .range(0.0..=print::MAX_CREEP)
                .speed(0.05)
                .suffix(" pt"),
        )
        .labelled_by(creep_label.id)
        .on_hover_text(tl!(
            "Moves each sheet's pages toward the fold by this much more than the sheet outside it, so the margins match once the booklet is folded and trimmed."
        ));
    });
}

fn render_comments_and_advanced_section(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(tl!("Comments & Forms")).font(theme::semibold(11.5)).color(t.text_muted));
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
            210.0,
        );
    });

    ui.add_space(3.0);
    egui::CollapsingHeader::new(
        egui::RichText::new(crate::i18n::upper_label("Advanced"))
            .font(theme::semibold(10.5))
            .color(t.text_faint),
    )
    .id_salt("print-advanced")
    .default_open(d.as_image)
    .show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.add(widgets::checkbox(&mut d.as_image, tl!("Print as image")));
            ui.add_enabled_ui(d.as_image, |ui| {
                ui.label(tl!("Resolution:"));
                let mut dpi = d.dpi.round() as u32;
                combo(
                    ui,
                    "print-dpi",
                    tl!("Resolution:"),
                    &mut dpi,
                    &[(150, "150 dpi"), (300, "300 dpi"), (600, "600 dpi")],
                    96.0,
                );
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
        ui.horizontal(|ui| {
            if ui
                .small_button(tl!("Copy Diagnostics"))
                .on_hover_text(tl!("Copy GFM Markdown diagnostic report to clipboard"))
                .clicked()
            {
                let report = collect_ui_diagnostic_report(d);
                ui.ctx().copy_text(report.to_markdown_table());
            }
            if ui
                .small_button(tl!("Export Diagnostics JSON"))
                .on_hover_text(tl!("Export diagnostic report as JSON to file and clipboard"))
                .clicked()
            {
                export_diagnostics_json(ui.ctx(), d);
            }
        });
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
