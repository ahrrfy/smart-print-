//! Left column of the Print Studio: Target Printers Dock with instant search filter,
//! 1-click saved printer groups, non-floating always-visible scrollbar, and 46 pt
//! full-row interactive printer cards.

use egui::{Align, Color32, CornerRadius, Layout, Rect, Sense, Stroke, pos2, vec2};

use super::draft::PrintDraft;
use super::{Tokens, printer_label, spool, still_looking, theme, widgets};

const CARD_HEIGHT: f32 = 46.0;

/// Renders the Target Printers Dock in the spacious 3-pane desktop layout (`width ~ 348 pt`),
/// owning its own isolated vertical `ScrollArea` (zero nested scroll containers).
pub(super) fn show(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens, width: f32, body_h: f32) {
    let frame = egui::Frame::new()
        .fill(t.hover.gamma_multiply(0.38))
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10));

    frame.show(ui, |ui| {
        let inner_w = (width - 22.0).max(220.0);
        ui.set_width(inner_w);
        let start_y = ui.cursor().top();
        render_top_controls(ui, d, t, inner_w);
        ui.add_space(6.0);
        let used_h = (ui.cursor().top() - start_y).max(0.0);
        let list_h = (body_h - used_h - 22.0).max(110.0);
        render_isolated_printer_scroll(ui, d, t, list_h);
        render_selection_footer_notice(ui, d, t);
    });
}

/// Renders the Target Printers section inside the compact 2-column fallback without a nested
/// `ScrollArea` (since the compact left column already scrolls as one unit).
pub(super) fn show_compact(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    let frame = egui::Frame::new()
        .fill(t.hover.gamma_multiply(0.38))
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10));

    frame.show(ui, |ui| {
        let inner_w = ui.available_width();
        render_top_controls(ui, d, t, inner_w);
        ui.add_space(6.0);
        render_printer_cards_list(ui, d, t);
        render_selection_footer_notice(ui, d, t);
    });
}

fn render_top_controls(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens, inner_w: f32) {
    render_header_badge_row(ui, d, t);
    ui.add_space(6.0);
    render_single_printer_selector_row(ui, d, inner_w);
    ui.add_space(6.0);
    render_search_bar(ui, d, t);
    ui.add_space(6.0);
    render_selection_toolbar(ui, d);
    render_saved_groups_bar(ui, d, t);
    if d.new_group_name.is_some() {
        ui.add_space(4.0);
        save_group_inline_row(ui, d);
    }
}

fn render_header_badge_row(ui: &mut egui::Ui, d: &PrintDraft, t: &Tokens) {
    ui.horizontal(|ui| {
        ui.add(crate::icons::image("printer", 15.0, t.accent));
        ui.label(egui::RichText::new(tl!("Target Printers")).font(theme::semibold(13.5)).color(t.text));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let count = d.effective_target_count();
            let (badge_text, fill, fg) = if count > 1 {
                (format!("Selected ({count})"), t.accent_soft, t.accent_text)
            } else if count == 1 {
                (tl!("1 Printer").to_string(), t.selected, t.accent_text)
            } else if !d.multi_printer_mode && d.printer.is_none() && !still_looking(d) {
                (tl!("PDF File").to_string(), t.hover, t.text_muted)
            } else {
                (tl!("0 Selected").to_string(), t.hover, t.text_muted)
            };
            egui::Frame::new()
                .fill(fill)
                .corner_radius(CornerRadius::same(10))
                .inner_margin(egui::Margin::symmetric(8, 2))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(badge_text).font(theme::semibold(11.0)).color(fg));
                });
        });
    });
}

fn render_single_printer_selector_row(ui: &mut egui::Ui, d: &mut PrintDraft, inner_w: f32) {
    let browser = !d.multi_printer_mode && d.printer.as_deref() == Some(crate::web_print::BROWSER_PRINTER);
    let mut picked_single: Option<Option<String>> = None;

    ui.horizontal(|ui| {
        let printer_caption = ui.label(tl!("Printer:"));
        let shown = if d.multi_printer_mode && d.selected_printers.len() > 1 {
            format!("{} ({})", tl!("Broadcast"), d.selected_printers.len())
        } else if d.multi_printer_mode && d.selected_printers.is_empty() {
            tl!("Select target printers…").to_string()
        } else {
            match d.printer.as_deref() {
                Some(p) => printer_label(p),
                None if still_looking(d) => tl!("Looking for printers…").to_string(),
                None => tl!("Save as PDF").to_string(),
            }
        };
        let combo_w = (inner_w - 148.0).clamp(120.0, 220.0);
        craft_ui::ComboBox::from_id_salt("printer")
            .selected_text(shown)
            .width(combo_w)
            .show_ui(ui, |ui| {
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
                    let active = !d.multi_printer_mode && d.printer.as_deref() == Some(p.name.as_str());
                    if ui.selectable_label(active, label).clicked() {
                        picked_single = Some(Some(p.name.clone()));
                    }
                }
                let pdf_active = !d.multi_printer_mode && d.printer.is_none() && !still_looking(d);
                if ui.selectable_label(pdf_active, tl!("Save as PDF")).clicked() {
                    picked_single = Some(None);
                }
            })
            .response
            .labelled_by(printer_caption.id);

        render_properties_button(ui, d, browser);
    });

    if let Some(choice) = picked_single {
        apply_single_printer_choice(d, choice);
    }
}

fn render_properties_button(ui: &mut egui::Ui, d: &mut PrintDraft, browser: bool) {
    let tip = if spool::HAS_PRINTER_PREFERENCES {
        tl!("Opens the printer driver's preferences. They are saved for every print from this computer; the copies, two-sided, grayscale and paper chosen here still apply.")
    } else {
        tl!("The printer driver's own settings for this print: paper tray, paper type, finishing and more.")
    };
    let target = if d.multi_printer_mode {
        d.active_preview_printer
            .clone()
            .or_else(|| d.selected_printers.iter().next().cloned())
    } else {
        d.printer.clone()
    };
    let enabled = target.is_some() && !browser;
    let properties = ui.add_enabled(enabled, egui::Button::new(tl!("Properties…"))).on_hover_text(tip);
    if properties.clicked()
        && let Some(p) = target
    {
        if spool::HAS_PRINTER_PREFERENCES {
            d.driver_error = spool::open_printer_preferences(&p).err().map(|e| e.to_string());
        } else {
            if d.driver_options.as_ref().is_none_or(|(read_for, _)| *read_for != p)
                && d.driver_options_loading.as_deref() != Some(p.as_str())
            {
                d.driver_options = None;
                d.driver_choices.clear();
                d.driver_options_loading = Some(p.clone());
                d.read_options_for = Some(p);
            }
            d.show_driver_options = !d.show_driver_options;
        }
    }
}

fn apply_single_printer_choice(d: &mut PrintDraft, choice: Option<String>) {
    let changed = d.multi_printer_mode || d.printer != choice;
    d.multi_printer_mode = false;
    d.selected_printers.clear();
    d.preset_store.active_group_id = None;
    d.printer = choice;
    d.printer_picked = true;
    if changed {
        d.show_driver_options = false;
        d.driver_error = None;
        d.apply_setup();
        d.reset_unoverridden_custom_paper();
        d.apply_printer_papers(Vec::new());
        if let Some(p) = d.printer.clone() {
            d.read_papers_for = Some(p.clone());
            d.read_caps_for = Some(p.clone());
            d.active_preview_printer = Some(p);
        } else {
            d.preview_hardware_margins = None;
            d.preview_printable_area = None;
        }
    }
}

fn render_search_bar(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    ui.horizontal(|ui| {
        ui.add(crate::icons::image("search", 14.0, t.text_muted));
        let has_query = !d.printer_search_query.is_empty();
        let edit_w = (ui.available_width() - if has_query { 28.0 } else { 0.0 }).max(100.0);
        ui.add(
            widgets::line(&mut d.printer_search_query)
                .hint_text(tl!("Filter printers by name…"))
                .desired_width(edit_w)
                .id_salt("printer-search-filter"),
        )
        .accessible_name(tl!("Filter printers by name"));
        if has_query
            && crate::icons::labeled_button(
                ui,
                "x",
                24.0,
                false,
                tl!("Clear printer filter"),
                tl!("Clear printer filter"),
            )
            .clicked()
        {
            d.printer_search_query.clear();
        }
    });
}

fn render_selection_toolbar(ui: &mut egui::Ui, d: &mut PrintDraft) {
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!d.printers.is_empty(), egui::Button::new(tl!("Select All")).small())
            .on_hover_text(tl!("Select all visible printers for simultaneous broadcast"))
            .clicked()
        {
            d.select_all_filtered_printers();
        }
        if ui
            .add_enabled(d.effective_target_count() > 0, egui::Button::new(tl!("Clear")).small())
            .on_hover_text(tl!("Uncheck all target printers"))
            .clicked()
        {
            d.clear_printer_cards();
        }
        let prev_hide = d.hide_virtual_printers;
        ui.add(widgets::checkbox(&mut d.hide_virtual_printers, tl!("Hide virtual")));
        if d.hide_virtual_printers && !prev_hide {
            d.selected_printers.retain(|n| !spool::is_virtual_save_printer(n));
        }
        if ui
            .small_button(tl!("+ Save Group"))
            .on_hover_text(tl!("Save selected printers as a 1-click printer group"))
            .clicked()
        {
            d.new_group_name = match d.new_group_name {
                Some(_) => None,
                None => Some(String::new()),
            };
        }
    });
}

fn render_saved_groups_bar(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    let groups: Vec<(String, String, usize)> = d
        .preset_store
        .list_groups()
        .into_iter()
        .filter(|g| !g.is_built_in || !g.printer_names.is_empty())
        .map(|g| (g.id.clone(), g.name.clone(), g.printer_names.len()))
        .collect();
    if groups.is_empty() {
        return;
    }
    ui.add_space(4.0);
    let mut clicked_group = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(tl!("Groups:")).small().color(t.text_muted));
        for (gid, gname, len) in &groups {
            let is_active = d.preset_store.active_group_id.as_deref() == Some(gid.as_str());
            let chip = format!("{gname} ({len})");
            if ui.selectable_label(is_active, chip).clicked() {
                clicked_group = Some(gid.clone());
            }
        }
    });
    if let Some(gid) = clicked_group {
        if d.multi_printer_mode && d.selected_printers.is_empty() && d.printer.is_some() {
            d.set_multi_printer_mode(true);
        }
        d.select_printer_group(&gid);
    }
}

fn save_group_inline_row(ui: &mut egui::Ui, d: &mut PrintDraft) {
    let Some(mut name_buf) = d.new_group_name.clone() else { return };
    let mut save = false;
    let mut dismiss = false;
    ui.horizontal(|ui| {
        ui.add(
            widgets::line(&mut name_buf)
                .hint_text(tl!("Printer group name"))
                .desired_width(145.0)
                .id_salt("new-group-name"),
        )
        .accessible_name(tl!("Printer group name"));
        let targets = d.target_printers();
        let can_save = !name_buf.trim().is_empty() && !targets.is_empty();
        save = ui.add_enabled(can_save, egui::Button::new(tl!("Save Group")).small()).clicked();
        dismiss = crate::icons::labeled_button(
            ui,
            "x",
            24.0,
            false,
            tl!("Close group form"),
            tl!("Close group form"),
        )
        .clicked();
    });
    if save {
        let clean = name_buf.trim().to_string();
        let id = format!("group-{}", d.preset_store.groups.len() + 1);
        let targets = d.target_printers();
        let group = crate::workflows::print_presets::PrinterGroup::with_printers(id.clone(), clean, targets, false);
        d.preset_store.upsert_group(group);
        d.preset_store.set_active_group(Some(id));
        d.new_group_name = None;
    } else if dismiss {
        d.new_group_name = None;
    } else {
        d.new_group_name = Some(name_buf);
    }
}

fn render_isolated_printer_scroll(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens, list_h: f32) {
    ui.scope(|ui| {
        ui.spacing_mut().scroll.floating = false;
        ui.spacing_mut().scroll.bar_width = 8.0;
        ui.spacing_mut().scroll.bar_inner_margin = 6.0;
        ui.spacing_mut().scroll.bar_outer_margin = 2.0;
        if ui.style().scroll_animation != egui::style::ScrollAnimation::none() {
            ui.style_mut().scroll_animation = egui::style::ScrollAnimation::duration(0.25);
        }
        egui::ScrollArea::vertical()
            .id_salt("printer-studio-list")
            .max_height(list_h)
            .min_scrolled_height(list_h)
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .show(ui, |ui| {
                render_printer_cards_list(ui, d, t);
            });
    });
}

fn render_printer_cards_list(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens) {
    if d.printers_loading && d.printers.is_empty() {
        ui.add_space(12.0);
        ui.label(egui::RichText::new(tl!("Scanning system print queues…")).color(t.text_muted));
        return;
    }
    if d.printers.is_empty() {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(tl!(
                "No printers were found. Check that the printer is installed and the print spooler is running, then open Print again."
            ))
            .small()
            .color(t.text_muted),
        );
        return;
    }

    let visible: Vec<spool::Printer> = d.filtered_printers().into_iter().cloned().collect();
    if visible.is_empty() {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(tl!("No printers match the current filter."))
                .small()
                .color(t.text_muted),
        );
        return;
    }

    ui.spacing_mut().item_spacing.y = 6.0;
    for p in &visible {
        render_printer_card(ui, d, t, p);
    }
}

fn render_printer_card(ui: &mut egui::Ui, d: &mut PrintDraft, t: &Tokens, p: &spool::Printer) {
    let row_w = ui.available_width().max(180.0);
    let is_selected = d.is_printer_card_selected(&p.name);
    let is_preview = d
        .active_preview_printer
        .as_deref()
        .or(d.printer.as_deref())
        == Some(p.name.as_str());
    let two_sided = d.printer_two_sided_for(&p.name);
    let is_virtual = spool::is_virtual_save_printer(&p.name);

    let bg_slot = ui.painter().add(egui::Shape::Noop);
    let mut card_hovered = false;
    let mut toggle_clicked = false;
    let mut preview_clicked = false;

    let outer = ui.allocate_ui_with_layout(vec2(row_w, CARD_HEIGHT), Layout::right_to_left(Align::Center), |ui| {
        ui.add_space(6.0);
        let eye_label = format!("Preview margins: {}", p.name);
        let eye_tip = format!("Preview hardware margins for {}", p.name);
        if crate::icons::labeled_button(ui, "eye", 28.0, is_preview, &eye_label, &eye_tip).clicked() {
            preview_clicked = true;
        }
        let main_w = ui.available_width().max(120.0);
        let (main_rect, resp) = ui.allocate_exact_size(vec2(main_w, CARD_HEIGHT), Sense::click());
        resp.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::Role::CheckBox,
                is_selected,
                true,
                format!("Target printer: {}", p.name),
            )
        });
        card_hovered = resp.hovered();
        if resp.clicked() {
            toggle_clicked = true;
        }
        let resp = theme::hand(resp);
        resp.on_hover_text(build_card_tooltip(p, two_sided, is_virtual, is_selected));
        paint_printer_card_interior(ui, main_rect, t, p, is_selected, two_sided, is_virtual);
    });

    let card_rect = outer.response.rect;
    let fill = if is_selected {
        t.selected
    } else if card_hovered {
        t.hover
    } else {
        t.card
    };
    let border = if is_selected {
        Stroke::new(1.5, t.accent)
    } else {
        Stroke::new(1.0, t.border)
    };
    ui.painter().set(
        bg_slot,
        egui::Shape::rect_filled(card_rect, CornerRadius::same(6), fill),
    );
    ui.painter().rect_stroke(card_rect, CornerRadius::same(6), border, egui::StrokeKind::Inside);

    if toggle_clicked {
        if p.name == crate::web_print::BROWSER_PRINTER {
            apply_single_printer_choice(d, Some(p.name.clone()));
        } else {
            d.click_printer_card(&p.name);
        }
    }
    if preview_clicked {
        d.set_preview_printer(p.name.clone());
    }
}

fn paint_printer_card_interior(
    ui: &egui::Ui,
    rect: Rect,
    t: &Tokens,
    p: &spool::Printer,
    is_selected: bool,
    two_sided: Option<bool>,
    is_virtual: bool,
) {
    let check_rect = Rect::from_center_size(pos2(rect.left() + 16.0, rect.center().y), vec2(16.0, 16.0));
    if is_selected {
        ui.painter().rect_filled(check_rect, CornerRadius::same(4), t.accent);
        crate::icons::paint(ui, check_rect, "check", 11.0, Color32::WHITE);
    } else {
        ui.painter().rect_filled(check_rect, CornerRadius::same(4), t.field);
        ui.painter()
            .rect_stroke(check_rect, CornerRadius::same(4), Stroke::new(1.2, t.border), egui::StrokeKind::Inside);
    }

    let icon_rect = Rect::from_center_size(pos2(rect.left() + 36.0, rect.top() + 15.0), vec2(14.0, 14.0));
    let icon_tint = if is_selected { t.accent_text } else { t.icon };
    crate::icons::paint(ui, icon_rect, "printer", 13.0, icon_tint);

    let text_left = rect.left() + 48.0;
    let text_right = (rect.right() - 4.0).max(text_left + 40.0);
    let name_str = printer_label(&p.name);
    let name_color = if is_selected { t.accent_text } else { t.text };
    let clip = Rect::from_min_max(pos2(text_left, rect.top() + 3.0), pos2(text_right, rect.top() + 24.0));
    ui.painter().with_clip_rect(clip).text(
        pos2(text_left, rect.top() + 14.0),
        egui::Align2::LEFT_CENTER,
        name_str,
        theme::semibold(12.5),
        name_color,
    );

    // Second line inside the 46 pt card: crisp capability badges without crowding the printer name
    let mut x = text_left;
    let badge_y = rect.bottom() - 11.0;
    if p.default && p.name != crate::web_print::BROWSER_PRINTER {
        x = paint_badge_pill(ui, pos2(x, badge_y), tl!("Default"), t.accent_soft, t.accent_text, text_right);
    }
    match two_sided {
        Some(true) => {
            x = paint_badge_pill(ui, pos2(x, badge_y), tl!("2-Sided"), t.hover, t.text_muted, text_right);
        }
        Some(false) => {
            x = paint_badge_pill(ui, pos2(x, badge_y), tl!("1-Sided"), t.hover, t.text_faint, text_right);
        }
        None => {}
    }
    if is_virtual {
        let _ = paint_badge_pill(ui, pos2(x, badge_y), tl!("Virtual"), t.hover, t.text_faint, text_right);
    }
}

fn paint_badge_pill(
    ui: &egui::Ui,
    left_center: egui::Pos2,
    text: &str,
    fill: Color32,
    fg: Color32,
    max_right: f32,
) -> f32 {
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::medium(10.0), fg);
    let pill_w = galley.size().x + 10.0;
    let pill_h = 15.0;
    if left_center.x + pill_w > max_right {
        return left_center.x;
    }
    let pill_rect = Rect::from_min_size(
        pos2(left_center.x, left_center.y - pill_h * 0.5),
        vec2(pill_w, pill_h),
    );
    ui.painter().rect_filled(pill_rect, CornerRadius::same(4), fill);
    ui.painter().galley(
        pos2(pill_rect.left() + 5.0, pill_rect.center().y - galley.size().y * 0.5),
        galley,
        fg,
    );
    pill_rect.right() + 5.0
}

fn build_card_tooltip(
    p: &spool::Printer,
    two_sided: Option<bool>,
    is_virtual: bool,
    is_selected: bool,
) -> String {
    let mut lines = vec![printer_label(&p.name)];
    if !p.description.is_empty() {
        lines.push(p.description.clone());
    }
    let duplex_str = match two_sided {
        Some(true) => "Supports automatic two-sided printing",
        Some(false) => "Simplex / manual two-sided driver",
        None => "Click to include in unified print broadcast",
    };
    lines.push(duplex_str.to_string());
    if is_virtual {
        lines.push("Virtual document writer queue".to_string());
    }
    lines.push(if is_selected {
        "Click anywhere on card to unselect".to_string()
    } else {
        "Click anywhere on card to select".to_string()
    });
    lines.join("\n")
}

fn render_selection_footer_notice(ui: &mut egui::Ui, d: &PrintDraft, _t: &Tokens) {
    if d.multi_printer_mode && d.selected_printers.is_empty() {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add(crate::icons::image("triangle-alert", 13.0, Color32::from_rgb(220, 80, 35)));
            ui.label(
                egui::RichText::new(tl!("Select at least one target printer to dispatch the batch."))
                    .small()
                    .color(Color32::from_rgb(220, 80, 35)),
            );
        });
    }
}
