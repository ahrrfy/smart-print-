//! Right column of the Print Studio: Live Sheet Preview with hardware margin overlays,
//! clipping-hazard detection & 1-click Auto-Fit, sheet navigation, and Pre-Flight Broadcast Summary.

use egui::{Color32, CornerRadius, Pos2, Rect, Sense, Stroke, pos2, vec2};

use super::draft::{ColorIntent, Handling, PrintDraft, PrinterBroadcastSummary};
use super::{PREVIEW_H, PREVIEW_INSET, PREVIEW_W, Tokens, print, spool, theme};

/// Context passed from `body.rs` to render the live sheet preview.
pub(super) struct PreviewContext<'a> {
    pub settings: &'a Result<print::Settings, String>,
    pub sheets: &'a [print::Sheet],
    pub sizes: &'a [(f64, f64)],
    pub too_much_creep: bool,
    pub body_h: f32,
}

/// Renders the non-scrolling Right Preview & Pre-Flight Summary column (`PREVIEW_W = 320 pt`),
/// scaling canvas height gracefully so short displays (`480 px`) never overflow `body_h`.
pub(super) fn show(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    pctx: PreviewContext<'_>,
    thumb: &mut dyn FnMut(usize) -> Option<egui::TextureId>,
) {
    ui.set_width(PREVIEW_W);
    let show_summary = pctx.body_h >= 460.0;
    let reserved = if show_summary { 195.0 } else { 112.0 };
    let canvas_h = (pctx.body_h - reserved).clamp(120.0, PREVIEW_H);

    if pctx.body_h >= 340.0 {
        render_preview_header(ui, d, t);
        ui.add_space(4.0);
    }

    render_sheet_canvas(ui, d, t, &pctx, canvas_h, thumb);
    ui.add_space(4.0);
    render_stepper_and_scale(ui, d, t, pctx.sheets);

    if d.clipping_hazard.is_some() {
        ui.add_space(3.0);
        render_clipping_banner(ui, d);
    }

    if show_summary {
        ui.add_space(5.0);
        render_broadcast_preflight_card(ui, d, t);
    }
}

fn render_preview_header(ui: &mut egui::Ui, d: &PrintDraft, t: &Tokens) {
    ui.horizontal(|ui| {
        ui.add(crate::icons::image("eye", 13.5, t.accent));
        let preview_name = if d.multi_printer_mode {
            d.active_preview_printer
                .as_deref()
                .or(d.printer.as_deref())
                .unwrap_or("Sheet Preview")
        } else {
            d.printer.as_deref().unwrap_or("PDF Output")
        };
        ui.label(
            egui::RichText::new(format!("Preview · {preview_name}"))
                .font(theme::medium(11.5))
                .color(t.text_muted),
        );
    });
}

fn render_sheet_canvas(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    pctx: &PreviewContext<'_>,
    canvas_h: f32,
    thumb: &mut dyn FnMut(usize) -> Option<egui::TextureId>,
) {
    let (area, _) = ui.allocate_exact_size(vec2(PREVIEW_W, canvas_h), Sense::hover());
    ui.painter().rect_filled(area, 6.0, t.hover);

    match (pctx.settings, pctx.sheets.get(d.sheet)) {
        (Err(e), _) => {
            d.clipping_hazard = None;
            ui.put(
                area.shrink(16.0),
                egui::Label::new(egui::RichText::new(tl!(e)).color(t.text_muted)).wrap(),
            );
        }
        (Ok(_), None) if pctx.too_much_creep => {
            d.clipping_hazard = None;
            let msg = tl!("The creep moves the innermost sheet's pages too far for this many pages. Reduce Creep.");
            ui.put(
                area.shrink(16.0),
                egui::Label::new(egui::RichText::new(msg).color(t.text_muted)).wrap(),
            );
        }
        (Ok(_), Some(sheet)) => {
            paint_sheet_contents(ui, d, t, area, canvas_h, sheet, pctx.sizes, thumb);
        }
        _ => {
            d.clipping_hazard = None;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_sheet_contents(
    ui: &egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    area: Rect,
    canvas_h: f32,
    sheet: &print::Sheet,
    sizes: &[(f64, f64)],
    thumb: &mut dyn FnMut(usize) -> Option<egui::TextureId>,
) {
    let box_w = (PREVIEW_W - 2.0 * PREVIEW_INSET).max(40.0);
    let box_h = (canvas_h - 2.0 * PREVIEW_INSET).max(40.0);
    let k = (box_w / sheet.size.0 as f32).min(box_h / sheet.size.1 as f32);
    let paper = Rect::from_center_size(area.center(), vec2(sheet.size.0 as f32 * k, sheet.size.1 as f32 * k));
    ui.painter().rect_filled(paper, 0.0, Color32::WHITE);

    if let Some(margins) = d.preview_hardware_margins {
        paint_hardware_margins(ui, paper, k, margins);
    }

    let has_clipping = check_sheet_clipping(d.preview_hardware_margins, sheet);
    if has_clipping {
        d.clipping_hazard = Some([1.0, 1.0, 1.0, 1.0]);
        ui.painter().rect_stroke(
            paper,
            0.0,
            Stroke::new(2.2, Color32::from_rgb(225, 45, 35)),
            egui::StrokeKind::Outside,
        );
    } else {
        d.clipping_hazard = None;
        ui.painter()
            .rect_stroke(paper, 0.0, Stroke::new(1.0, t.border), egui::StrokeKind::Outside);
    }

    paint_placed_pages(ui, d.grayscale, paper, k, sheet, sizes, thumb);
}

fn paint_hardware_margins(ui: &egui::Ui, paper: Rect, k: f32, [m_left, m_top, m_right, m_bottom]: [f64; 4]) {
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
    ui.painter().rect_stroke(
        printable,
        0.0,
        Stroke::new(1.0, Color32::from_rgb(70, 130, 230)),
        egui::StrokeKind::Inside,
    );
}

fn check_sheet_clipping(margins: Option<[f64; 4]>, sheet: &print::Sheet) -> bool {
    let Some([m_left, m_top, m_right, m_bottom]) = margins else {
        return false;
    };
    for pl in &sheet.placed {
        let [x0, y0, x1, y1] = pl.clip;
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| pl.matrix.apply(x, y));
        for (cx, cy) in corners {
            if cx < m_left - 0.5
                || cx > (sheet.size.0 - m_right) + 0.5
                || cy < m_bottom - 0.5
                || cy > (sheet.size.1 - m_top) + 0.5
            {
                return true;
            }
        }
    }
    false
}

fn paint_placed_pages(
    ui: &egui::Ui,
    grayscale: bool,
    paper: Rect,
    k: f32,
    sheet: &print::Sheet,
    sizes: &[(f64, f64)],
    thumb: &mut dyn FnMut(usize) -> Option<egui::TextureId>,
) {
    let to_screen = |x: f64, y: f64| -> Pos2 { pos2(paper.left() + x as f32 * k, paper.bottom() - y as f32 * k) };
    let painter = ui.painter().with_clip_rect(paper);
    for pl in &sheet.placed {
        let Some(&(dw, dh)) = sizes.get(pl.page) else { continue };
        let [x0, y0, x1, y1] = pl.clip;
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| {
            let (sx, sy) = pl.matrix.apply(x, y);
            to_screen(sx, sy)
        });
        let uv = |x: f64, y: f64| pos2((x / dw) as f32, (1.0 - y / dh) as f32);
        let uvs = [uv(x0, y0), uv(x1, y0), uv(x1, y1), uv(x0, y1)];
        match thumb(pl.page) {
            Some(tex) => {
                let mut mesh = egui::Mesh::with_texture(tex);
                let tint = if grayscale { Color32::from_gray(235) } else { Color32::WHITE };
                for (p, u) in corners.iter().zip(uvs) {
                    mesh.vertices.push(egui::epaint::Vertex { pos: *p, uv: u, color: tint });
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
        painter.line_segment(
            [to_screen(l[0], l[1]), to_screen(l[2], l[3])],
            Stroke::new(0.6, Color32::BLACK),
        );
    }
}

fn render_stepper_and_scale(
    ui: &mut egui::Ui,
    d: &mut PrintDraft,
    t: &Tokens,
    sheets: &[print::Sheet],
) {
    ui.horizontal(|ui| {
        let n = sheets.len();
        if ui
            .add_enabled(d.sheet > 0, egui::Button::new("‹"))
            .on_hover_text(tl!("Previous sheet"))
            .clicked()
        {
            d.sheet -= 1;
        }
        ui.label(if n == 0 {
            tl!("No sheets").to_string()
        } else {
            crate::i18n::fmt(
                tl!("Sheet {s} of {n}"),
                &[("s", &(d.sheet + 1).to_string()), ("n", &n.to_string())],
            )
        });
        if ui
            .add_enabled(d.sheet + 1 < n, egui::Button::new("›"))
            .on_hover_text(tl!("Next sheet"))
            .clicked()
        {
            d.sheet += 1;
        }
        if let Some(s) = sheets.get(d.sheet) {
            ui.label(
                egui::RichText::new(d.units.size(s.size.0, s.size.1))
                    .small()
                    .color(t.text_muted),
            );
        }
    });

    if let Some(s) = sheets.get(d.sheet)
        && d.handling == Handling::Size
        && let Some(p) = s.placed.first()
    {
        let scale = p.matrix.0[0].hypot(p.matrix.0[1]) * 100.0;
        ui.label(
            egui::RichText::new(crate::i18n::fmt(
                tl!("PDF scale: {scale}%"),
                &[("scale", &format!("{scale:.2}"))],
            ))
            .small(),
        );
        ui.label(
            egui::RichText::new(tl!("Printer paper and scaling may differ. Check driver settings."))
                .small()
                .color(t.text_muted),
        );
        if (scale - 100.0).abs() > 0.01 {
            ui.label(
                egui::RichText::new(tl!("Drawing scale changes. Use Actual size for full-scale plans."))
                    .small()
                    .color(t.text_muted),
            );
        }
    }
}

fn render_clipping_banner(ui: &mut egui::Ui, d: &mut PrintDraft) {
    ui.horizontal_wrapped(|ui| {
        ui.add(crate::icons::image("triangle-alert", 13.0, Color32::from_rgb(225, 45, 35)));
        ui.label(
            egui::RichText::new(tl!("Clipping hazard: content extends beyond printable area"))
                .color(Color32::from_rgb(225, 45, 35))
                .small(),
        );
        if ui.small_button(tl!("Auto-Fit to Printable Area")).clicked() {
            d.auto_fit_to_printable();
        }
    });
}

fn render_broadcast_preflight_card(ui: &mut egui::Ui, d: &PrintDraft, t: &Tokens) {
    let summaries = d.broadcast_summaries();
    egui::Frame::new()
        .fill(t.hover.gamma_multiply(0.5))
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width((PREVIEW_W - 18.0).max(200.0));
            ui.horizontal(|ui| {
                ui.add(crate::icons::image("check-circle-2", 13.0, t.accent));
                let heading = if summaries.len() > 1 {
                    format!("Broadcast Pre-Flight ({} queues)", summaries.len())
                } else if summaries.len() == 1 {
                    tl!("Dispatch Pre-Flight").to_string()
                } else {
                    tl!("Output Summary").to_string()
                };
                ui.label(egui::RichText::new(heading).font(theme::semibold(11.0)).color(t.text));
            });
            if summaries.is_empty() {
                let mode = if d.multi_printer_mode {
                    tl!("Select target printers on the left")
                } else {
                    tl!("Save print-ready PDF file")
                };
                ui.label(egui::RichText::new(mode).small().color(t.text_muted));
                return;
            }
            for item in summaries.iter().take(2) {
                ui.label(
                    egui::RichText::new(format_summary_line(item))
                        .font(theme::regular(10.5))
                        .color(t.text_muted),
                );
            }
            if summaries.len() > 2 {
                ui.label(
                    egui::RichText::new(format!(
                        "+{} more printers receiving identical unified specs",
                        summaries.len() - 2
                    ))
                    .font(theme::medium(10.5))
                    .color(t.accent_text),
                );
            }
        });
}

fn format_summary_line(s: &PrinterBroadcastSummary) -> String {
    let color = match s.color_intent {
        ColorIntent::Color => "Color",
        ColorIntent::Monochrome => "B&W",
    };
    let duplex = if s.duplex_fallback_to_simplex {
        "1-Sided fallback"
    } else {
        match s.effective_duplex {
            Some(spool::Duplex::LongEdge) => "2-Sided Long",
            Some(spool::Duplex::ShortEdge) => "2-Sided Short",
            Some(spool::Duplex::Off) => "1-Sided",
            None => "Auto Duplex",
        }
    };
    format!("• {}: {} · {} · {} · {}×", s.printer_name, s.paper_name, color, duplex, s.copies)
}
