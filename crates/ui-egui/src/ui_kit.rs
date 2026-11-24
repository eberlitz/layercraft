//! Reusable CAD-grade immediate-mode UI kit widgets layered over `theme` and `icons`.

use std::ops::RangeInclusive;

use egui::{
    Align, Color32, CornerRadius, Margin, Rect, Response, RichText, Sense, Shadow, Stroke,
    StrokeKind, Ui, emath, pos2, vec2,
};

use crate::icons::{Icon, paint_icon};
use crate::theme::ThemePalette;

/// Framed card container inside sidebars with an optional subtle header.
pub fn card(ui: &mut Ui, p: &ThemePalette, title: Option<&str>, body: impl FnOnce(&mut Ui)) {
    egui::Frame::NONE
        .fill(p.bg_card)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            if let Some(t) = title {
                ui.label(
                    RichText::new(t.to_ascii_uppercase())
                        .size(10.5)
                        .strong()
                        .color(p.text_muted),
                );
                ui.add_space(6.0);
            }
            body(ui);
        });
}

/// Translucent floating HUD panel frame for viewport overlays.
pub fn hud_frame(p: &ThemePalette) -> egui::Frame {
    egui::Frame::NONE
        .fill(p.bg_hud)
        .stroke(Stroke::new(1.0, p.border_strong))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(12, 9))
        .shadow(Shadow {
            offset: [0, 6],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(if p.dark { 105 } else { 36 }),
        })
}

/// Compact translucent pill/rail container used by floating viewport docks.
pub fn floating_rail_frame(p: &ThemePalette) -> egui::Frame {
    egui::Frame::NONE
        .fill(p.bg_hud)
        .stroke(Stroke::new(1.0, p.border_strong))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(5))
        .shadow(Shadow {
            offset: [0, 6],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(if p.dark { 110 } else { 34 }),
        })
}

/// High-contrast accent call-to-action button with leading icon.
pub fn primary_button(ui: &mut Ui, p: &ThemePalette, icon: Icon, label: &str) -> Response {
    let font_id = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font_id, p.accent_text);
    let icon_sz = 14.0;
    let gap = 6.0;
    let pad_x = 12.0;
    let width = pad_x * 2.0 + icon_sz + gap + galley.size().x;
    let height = 27.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());

    if ui.is_rect_visible(rect) {
        let fill = if resp.hovered() || resp.has_focus() {
            p.accent_hover
        } else {
            p.accent
        };
        ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
        let icon_rect = Rect::from_center_size(
            pos2(rect.left() + pad_x + icon_sz * 0.5, rect.center().y),
            vec2(icon_sz, icon_sz),
        );
        paint_icon(ui.painter(), icon_rect, icon, p.accent_text);
        let text_pos = pos2(
            icon_rect.right() + gap,
            rect.center().y - galley.size().y * 0.5,
        );
        ui.painter().galley(text_pos, galley, p.accent_text);
    }
    resp
}

/// Crisp secondary outlined button with optional icon.
pub fn secondary_button(
    ui: &mut Ui,
    p: &ThemePalette,
    icon: Option<Icon>,
    label: &str,
) -> Response {
    let font_id = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font_id, p.text_primary);
    let icon_sz = if icon.is_some() { 14.0 } else { 0.0 };
    let gap = if icon.is_some() { 5.0 } else { 0.0 };
    let pad_x = 10.0;
    let width = pad_x * 2.0 + icon_sz + gap + galley.size().x;
    let height = 26.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());

    if ui.is_rect_visible(rect) {
        let fill = if resp.is_pointer_button_down_on() {
            p.bg_active
        } else if resp.hovered() {
            p.bg_hover
        } else {
            p.bg_card
        };
        let stroke = Stroke::new(
            1.0,
            if resp.hovered() {
                p.border_strong
            } else {
                p.border_subtle
            },
        );
        ui.painter().rect(
            rect,
            CornerRadius::same(5),
            fill,
            stroke,
            StrokeKind::Inside,
        );
        let mut text_x = rect.left() + pad_x;
        if let Some(ic) = icon {
            let icon_rect = Rect::from_center_size(
                pos2(text_x + icon_sz * 0.5, rect.center().y),
                vec2(icon_sz, icon_sz),
            );
            paint_icon(ui.painter(), icon_rect, ic, p.text_secondary);
            text_x = icon_rect.right() + gap;
        }
        let text_pos = pos2(text_x, rect.center().y - galley.size().y * 0.5);
        ui.painter().galley(text_pos, galley, p.text_primary);
    }
    resp
}

/// Compact ribbon / pill toggle action button with icon + label.
pub fn ribbon_button(
    ui: &mut Ui,
    p: &ThemePalette,
    icon: Icon,
    label: &str,
    active: bool,
) -> Response {
    let fg = if active { p.accent } else { p.text_primary };
    let font_id = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font_id, fg);
    let icon_sz = 14.0;
    let gap = 5.0;
    let pad_x = 8.0;
    let width = pad_x * 2.0 + icon_sz + gap + galley.size().x;
    let height = 25.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());

    if ui.is_rect_visible(rect) {
        let fill = if active {
            p.accent_muted
        } else if resp.is_pointer_button_down_on() {
            p.bg_active
        } else if resp.hovered() {
            p.bg_hover
        } else {
            Color32::TRANSPARENT
        };
        let stroke = if active {
            Stroke::new(1.0, p.accent)
        } else if resp.hovered() {
            Stroke::new(1.0, p.border_subtle)
        } else {
            Stroke::NONE
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(5),
            fill,
            stroke,
            StrokeKind::Inside,
        );
        let icon_rect = Rect::from_center_size(
            pos2(rect.left() + pad_x + icon_sz * 0.5, rect.center().y),
            vec2(icon_sz, icon_sz),
        );
        paint_icon(
            ui.painter(),
            icon_rect,
            icon,
            if active { p.accent } else { p.text_secondary },
        );
        let text_pos = pos2(
            icon_rect.right() + gap,
            rect.center().y - galley.size().y * 0.5,
        );
        ui.painter().galley(text_pos, galley, fg);
    }
    resp
}

/// Compact 26×26 icon-only button with hover tooltip.
pub fn icon_button(
    ui: &mut Ui,
    p: &ThemePalette,
    icon: Icon,
    tooltip: &str,
    active: bool,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if active {
            p.accent_muted
        } else if resp.is_pointer_button_down_on() {
            p.bg_active
        } else if resp.hovered() {
            p.bg_hover
        } else {
            Color32::TRANSPARENT
        };
        let stroke = if active {
            Stroke::new(1.0, p.accent)
        } else if resp.hovered() {
            Stroke::new(1.0, p.border_subtle)
        } else {
            Stroke::NONE
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(5),
            fill,
            stroke,
            StrokeKind::Inside,
        );
        let col = if active {
            p.accent
        } else if resp.hovered() {
            p.text_primary
        } else {
            p.text_secondary
        };
        paint_icon(ui.painter(), rect.shrink(4.0), icon, col);
    }
    resp.on_hover_text(tooltip)
}

/// Sleek pill toggle switch (`32×18` px).
pub fn toggle_switch(ui: &mut Ui, p: &ThemePalette, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(32.0, 18.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(resp.id, *on);
        let radius = rect.height() * 0.5;
        let track_fill = if *on { p.accent } else { p.bg_input };
        let track_stroke = if *on {
            Stroke::new(1.0, p.accent)
        } else {
            Stroke::new(1.0, p.border_strong)
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(radius.round() as u8),
            track_fill,
            track_stroke,
            StrokeKind::Inside,
        );
        let x0 = rect.left() + radius;
        let x1 = rect.right() - radius;
        let cx = emath::lerp(x0..=x1, how_on);
        let knob_col = if *on { p.accent_text } else { p.text_secondary };
        ui.painter()
            .circle_filled(pos2(cx, rect.center().y), radius - 3.0, knob_col);
    }
    resp
}

/// Consistent key/control row with left label and right-aligned control slot.
pub fn setting_row(ui: &mut Ui, p: &ThemePalette, label: &str, control: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(24.0);
        ui.label(RichText::new(label).color(p.text_secondary));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            control(ui);
        });
    });
}

/// Compact 20×20 icon-only button for dense list rows.
pub fn small_icon_button(ui: &mut Ui, p: &ThemePalette, icon: Icon, tooltip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter().rect(
                rect,
                CornerRadius::same(4),
                p.bg_hover,
                Stroke::new(1.0, p.border_subtle),
                StrokeKind::Inside,
            );
        }
        let col = if resp.hovered() {
            p.danger
        } else {
            p.text_muted
        };
        paint_icon(ui.painter(), rect.shrink(3.0), icon, col);
    }
    resp.on_hover_text(tooltip)
}

/// Compact numeric `DragValue` with right-aligned unit suffix.
pub fn unit_drag<Num: emath::Numeric>(
    ui: &mut Ui,
    value: &mut Num,
    range: RangeInclusive<Num>,
    speed: f64,
    suffix: &str,
) -> Response {
    let mut dv = egui::DragValue::new(value)
        .speed(speed)
        .range(range)
        .min_size(vec2(68.0, 22.0));
    if !suffix.is_empty() {
        dv = dv.suffix(format!(" {suffix}"));
    }
    ui.add(dv)
}

/// Single-row X / Y numeric pair with colored axis indicator pips.
pub fn xy_row(
    ui: &mut Ui,
    p: &ThemePalette,
    label: &str,
    values: &mut [f64; 2],
    ranges: [RangeInclusive<f64>; 2],
    speed: f64,
    suffix: &str,
) -> bool {
    let [mut vx, mut vy] = *values;
    let [rx, ry] = ranges;
    let mut changed = false;
    ui.label(RichText::new(label).size(11.5).color(p.text_secondary));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let cell_w = ((ui.available_width() - 4.0) * 0.5).max(56.0);
        changed |= axis_drag(ui, p.axis_x, &mut vx, rx, speed, suffix, cell_w);
        changed |= axis_drag(ui, p.axis_y, &mut vy, ry, speed, suffix, cell_w);
    });
    if changed {
        *values = [vx, vy];
    }
    changed
}

/// Single-row X / Y / Z numeric triad with R/G/B axis indicator pips.
pub fn xyz_row(
    ui: &mut Ui,
    p: &ThemePalette,
    label: &str,
    values: &mut [f64; 3],
    range: RangeInclusive<f64>,
    speed: f64,
    suffix: &str,
) -> bool {
    let [mut vx, mut vy, mut vz] = *values;
    let mut changed = false;
    ui.label(RichText::new(label).size(11.5).color(p.text_secondary));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let cell_w = ((ui.available_width() - 8.0) / 3.0).max(46.0);
        changed |= axis_drag(ui, p.axis_x, &mut vx, range.clone(), speed, suffix, cell_w);
        changed |= axis_drag(ui, p.axis_y, &mut vy, range.clone(), speed, suffix, cell_w);
        changed |= axis_drag(ui, p.axis_z, &mut vz, range, speed, suffix, cell_w);
    });
    if changed {
        *values = [vx, vy, vz];
    }
    changed
}

fn axis_drag(
    ui: &mut Ui,
    axis_color: Color32,
    value: &mut f64,
    range: RangeInclusive<f64>,
    speed: f64,
    suffix: &str,
    width: f32,
) -> bool {
    let mut resp_changed = false;
    ui.allocate_ui_with_layout(
        vec2(width, 22.0),
        egui::Layout::left_to_right(Align::Center),
        |ui| {
            let (pip_rect, _) = ui.allocate_exact_size(vec2(3.0, 14.0), Sense::hover());
            ui.painter()
                .rect_filled(pip_rect, CornerRadius::same(1), axis_color);
            let mut dv = egui::DragValue::new(value)
                .speed(speed)
                .range(range)
                .min_size(vec2((width - 7.0).max(34.0), 22.0));
            if !suffix.is_empty() {
                dv = dv.suffix(suffix);
            }
            resp_changed = ui.add(dv).changed();
        },
    );
    resp_changed
}

/// Compact rounded pill badge.
pub fn badge(ui: &mut Ui, bg: Color32, fg: Color32, text: &str) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(10.5, egui::FontFamily::Proportional),
        fg,
    );
    let size = galley.size() + vec2(14.0, 4.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, CornerRadius::same(10), bg);
        ui.painter().galley(
            pos2(rect.left() + 7.0, rect.center().y - galley.size().y * 0.5),
            galley,
            fg,
        );
    }
}

/// Thin vertical divider inside toolbars and ribbons.
pub fn v_divider(ui: &mut Ui, p: &ThemePalette) {
    let (rect, _) = ui.allocate_exact_size(vec2(6.0, 18.0), Sense::hover());
    ui.painter().line_segment(
        [
            pos2(rect.center().x, rect.top() + 2.0),
            pos2(rect.center().x, rect.bottom() - 2.0),
        ],
        Stroke::new(1.0, p.border_subtle),
    );
}

/// Square icon-only tool button inside floating viewport rails (32x32).
pub fn rail_tool_button(
    ui: &mut Ui,
    p: &ThemePalette,
    icon: Icon,
    active: bool,
    tooltip: &str,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if active {
            p.accent_muted
        } else if resp.is_pointer_button_down_on() {
            p.bg_active
        } else if resp.hovered() {
            p.bg_hover
        } else {
            Color32::TRANSPARENT
        };
        let stroke = if active {
            Stroke::new(1.0, p.accent)
        } else if resp.hovered() {
            Stroke::new(1.0, p.border_subtle)
        } else {
            Stroke::NONE
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(7),
            fill,
            stroke,
            StrokeKind::Inside,
        );
        let col = if active {
            p.accent
        } else if resp.hovered() {
            p.text_primary
        } else {
            p.text_secondary
        };
        paint_icon(ui.painter(), rect.shrink(7.0), icon, col);
    }
    resp.on_hover_text(tooltip)
}

/// Equal-width sleek segmented control strip returning `true` when changed.
pub fn segmented_control<T: Copy + PartialEq>(
    ui: &mut Ui,
    p: &ThemePalette,
    id_source: impl std::hash::Hash + std::fmt::Debug,
    current: &mut T,
    options: &[(T, &str)],
) -> bool {
    let mut changed = false;
    let count = options.len().max(1);
    let total_w = ui.available_width();
    egui::Frame::NONE
        .fill(p.bg_input)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            let inner_w = (total_w - 8.0).max(72.0);
            let gap = 3.0;
            let seg_w =
                ((inner_w - gap * (count.saturating_sub(1) as f32)) / count as f32).max(28.0);
            let seg_h = 26.0;
            ui.push_id(id_source, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for &(val, label) in options {
                        let active = *current == val;
                        let (rect, resp) =
                            ui.allocate_exact_size(vec2(seg_w, seg_h), Sense::click());
                        if ui.is_rect_visible(rect) {
                            let fill = if active {
                                p.bg_card
                            } else if resp.hovered() {
                                p.bg_hover.gamma_multiply(0.6)
                            } else {
                                Color32::TRANSPARENT
                            };
                            let stroke = if active {
                                Stroke::new(1.0, p.accent.gamma_multiply(0.85))
                            } else {
                                Stroke::NONE
                            };
                            ui.painter().rect(
                                rect,
                                CornerRadius::same(6),
                                fill,
                                stroke,
                                StrokeKind::Inside,
                            );
                            let col = if active {
                                p.accent
                            } else if resp.hovered() {
                                p.text_primary
                            } else {
                                p.text_secondary
                            };
                            let galley = ui.painter().layout_no_wrap(
                                label.to_owned(),
                                egui::FontId::new(12.0, egui::FontFamily::Proportional),
                                col,
                            );
                            ui.painter().galley(
                                pos2(
                                    rect.center().x - galley.size().x * 0.5,
                                    rect.center().y - galley.size().y * 0.5,
                                ),
                                galley,
                                col,
                            );
                        }
                        if resp.clicked() && !active {
                            *current = val;
                            changed = true;
                        }
                    }
                });
            });
        });
    changed
}

/// Collapsible framed inspector section with header chevron and optional count pill.
pub fn inspector_section(
    ui: &mut Ui,
    p: &ThemePalette,
    id_source: impl std::hash::Hash + std::fmt::Debug,
    title: &str,
    badge_text: Option<&str>,
    default_open: bool,
    body: impl FnOnce(&mut Ui),
) {
    let id = ui.make_persistent_id(id_source);
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        id,
        default_open,
    );
    let is_open = state.is_open();
    egui::Frame::NONE
        .fill(p.bg_card)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let (rect, resp) =
                ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click());
            if ui.is_rect_visible(rect) {
                let icon_rect = Rect::from_center_size(
                    pos2(rect.left() + 6.0, rect.center().y),
                    vec2(12.0, 12.0),
                );
                paint_icon(
                    ui.painter(),
                    icon_rect,
                    if is_open {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronUp
                    },
                    if resp.hovered() {
                        p.text_primary
                    } else {
                        p.text_muted
                    },
                );
                let title_col = if resp.hovered() {
                    p.text_primary
                } else {
                    p.text_secondary
                };
                let title_galley = ui.painter().layout_no_wrap(
                    title.to_ascii_uppercase(),
                    egui::FontId::new(10.5, egui::FontFamily::Proportional),
                    title_col,
                );
                ui.painter().galley(
                    pos2(
                        icon_rect.right() + 6.0,
                        rect.center().y - title_galley.size().y * 0.5,
                    ),
                    title_galley,
                    title_col,
                );
                if let Some(bt) = badge_text {
                    let b_galley = ui.painter().layout_no_wrap(
                        bt.to_owned(),
                        egui::FontId::new(10.5, egui::FontFamily::Proportional),
                        p.text_muted,
                    );
                    let bw = b_galley.size().x + 12.0;
                    let brect = Rect::from_min_size(
                        pos2(rect.right() - bw, rect.center().y - 8.5),
                        vec2(bw, 17.0),
                    );
                    ui.painter().rect(
                        brect,
                        CornerRadius::same(8),
                        p.bg_input,
                        Stroke::new(1.0, p.border_subtle),
                        StrokeKind::Inside,
                    );
                    ui.painter().galley(
                        pos2(
                            brect.center().x - b_galley.size().x * 0.5,
                            brect.center().y - b_galley.size().y * 0.5,
                        ),
                        b_galley,
                        p.text_muted,
                    );
                }
            }
            if resp.clicked() {
                state.toggle(ui);
            }
            if is_open {
                ui.add_space(6.0);
                body(ui);
            }
        });
    state.store(ui.ctx());
}
