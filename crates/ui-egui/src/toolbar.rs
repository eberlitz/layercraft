//! Sleek single-row 44px Studio top bar: workspace switcher, File menu, Project/Plate selector, and primary CTA.

use egui::{Align, CornerRadius, Margin, Popup, RichText, Stroke, StrokeKind, pos2, vec2};
use layercraft_viewport::ColorMode;

use crate::icons::{Icon, paint_icon};
use crate::recent_projects::{format_relative_time, now_unix_secs};
use crate::theme::{ThemeMode, ThemePalette};
use crate::ui_kit::{icon_button, primary_button, secondary_button, v_divider};
use crate::{LayerCraftApp, PendingDiscardAction, ViewMode};

/// Top 44px application header bar.
pub fn draw_header_bar(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let resp = egui::Panel::top("layercraft_header")
        .frame(
            egui::Frame::NONE
                .fill(p.bg_chrome)
                .inner_margin(Margin::symmetric(14, 7)),
        )
        .show(ui, |ui| {
            let avail_w = ui.available_width().max(120.0);
            let (row_rect, _) = ui.allocate_exact_size(vec2(avail_w, 30.0), egui::Sense::hover());

            let left_resp = ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(row_rect)
                    .layout(egui::Layout::left_to_right(Align::Center)),
                |ui| {
                    ui.spacing_mut().item_spacing.x = if avail_w < 680.0 { 6.0 } else { 8.0 };

                    // Clickable brand mark -> returns to Home hub
                    let (mark_rect, mark_resp) =
                        ui.allocate_exact_size(vec2(24.0, 24.0), egui::Sense::click());
                    if ui.is_rect_visible(mark_rect) {
                        if mark_resp.hovered() {
                            ui.painter().rect(
                                mark_rect,
                                CornerRadius::same(6),
                                p.bg_hover,
                                Stroke::new(1.0, p.border_subtle),
                                StrokeKind::Inside,
                            );
                        }
                        paint_icon(
                            ui.painter(),
                            mark_rect.shrink(3.0),
                            Icon::BrandMark,
                            p.accent,
                        );
                    }
                    if mark_resp
                        .on_hover_text("Return to LayerCraft Home")
                        .clicked()
                    {
                        app.view_mode = ViewMode::Home;
                    }

                    // Home | Prepare | Preview segmented pill switcher
                    draw_view_mode_switcher(app, ui, p);

                    // File menu popover
                    draw_file_menu(app, ui, p);
                },
            );

            let right_resp = ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(row_rect)
                    .layout(egui::Layout::right_to_left(Align::Center)),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    draw_right_actions(app, ui, p, avail_w);
                },
            );

            // Center Project + Plate selector pill (hidden on Home or ultra-narrow viewports)
            if app.view_mode != ViewMode::Home {
                let left_bound = left_resp.response.rect.right() + 12.0;
                let right_bound = right_resp.response.rect.left() - 12.0;
                let max_span = (right_bound - left_bound).max(0.0);
                if max_span >= 140.0 {
                    let target_w = max_span.min(340.0);
                    let ideal_left = (row_rect.center().x - target_w * 0.5)
                        .clamp(left_bound, (right_bound - target_w).max(left_bound));
                    let center_rect = egui::Rect::from_min_size(
                        pos2(ideal_left, row_rect.top()),
                        vec2(target_w.min(max_span), row_rect.height()),
                    );
                    ui.scope_builder(
                        egui::UiBuilder::new().max_rect(center_rect).layout(
                            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                        ),
                        |ui| {
                            draw_project_plate_pill(app, ui, p, center_rect.width());
                        },
                    );
                }
            }
        })
        .response;

    let r = resp.rect;
    ui.painter().line_segment(
        [pos2(r.left(), r.bottom()), pos2(r.right(), r.bottom())],
        Stroke::new(1.0, p.border_subtle),
    );
}

fn draw_view_mode_switcher(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let mut target_mode: Option<ViewMode> = None;

    egui::Frame::NONE
        .fill(p.bg_input)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (mode, label) in [
                    (ViewMode::Home, "Home"),
                    (ViewMode::Prepare, "Prepare"),
                    (ViewMode::Preview, "Preview"),
                ] {
                    let active = app.view_mode == mode;
                    let text_col = if active { p.text_primary } else { p.text_muted };
                    let galley = ui.painter().layout_no_wrap(
                        label.to_owned(),
                        egui::FontId::new(12.0, egui::FontFamily::Proportional),
                        text_col,
                    );
                    let icon_extra = if mode == ViewMode::Home { 16.0 } else { 0.0 };
                    let (seg_rect, seg_resp) = ui.allocate_exact_size(
                        vec2(galley.size().x + 18.0 + icon_extra, 22.0),
                        egui::Sense::click(),
                    );
                    if ui.is_rect_visible(seg_rect) {
                        if active {
                            ui.painter().rect(
                                seg_rect,
                                CornerRadius::same(5),
                                p.bg_card,
                                Stroke::new(1.0, p.border_strong),
                                StrokeKind::Inside,
                            );
                            let pip = egui::Rect::from_min_max(
                                pos2(seg_rect.left() + 8.0, seg_rect.bottom() - 2.0),
                                pos2(seg_rect.right() - 8.0, seg_rect.bottom()),
                            );
                            ui.painter()
                                .rect_filled(pip, CornerRadius::same(1), p.accent);
                        } else if seg_resp.hovered() {
                            ui.painter()
                                .rect_filled(seg_rect, CornerRadius::same(5), p.bg_hover);
                        }
                        if mode == ViewMode::Home {
                            let ir = egui::Rect::from_center_size(
                                pos2(seg_rect.left() + 14.0, seg_rect.center().y),
                                vec2(12.0, 12.0),
                            );
                            paint_icon(
                                ui.painter(),
                                ir,
                                Icon::Home,
                                if active { p.accent } else { p.text_muted },
                            );
                            let pos = pos2(
                                ir.right() + 4.0,
                                seg_rect.center().y - galley.size().y * 0.5,
                            );
                            ui.painter().galley(pos, galley, text_col);
                        } else {
                            let pos = pos2(
                                seg_rect.center().x - galley.size().x * 0.5,
                                seg_rect.center().y - galley.size().y * 0.5,
                            );
                            ui.painter().galley(pos, galley, text_col);
                        }
                    }
                    if seg_resp.clicked() && !active {
                        target_mode = Some(mode);
                    }
                }
            });
        });

    if let Some(mode) = target_mode {
        match mode {
            ViewMode::Preview => {
                if app.session.sliced.is_none() && !app.session.objects.is_empty() {
                    app.run_slice();
                } else {
                    app.view_mode = ViewMode::Preview;
                }
            }
            other => app.view_mode = other,
        }
    }
}

fn draw_file_menu(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let font_id = egui::FontId::new(12.0, egui::FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap("File".to_owned(), font_id, p.text_secondary);
    let width = galley.size().x + 26.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 26.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if resp.hovered() || resp.is_pointer_button_down_on() {
            p.bg_hover
        } else {
            p.bg_chrome
        };
        let stroke = if resp.hovered() {
            Stroke::new(1.0, p.border_subtle)
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
        let col = if resp.hovered() {
            p.text_primary
        } else {
            p.text_secondary
        };
        ui.painter().galley(
            pos2(rect.left() + 8.0, rect.center().y - galley.size().y * 0.5),
            galley,
            col,
        );
        let chev = egui::Rect::from_center_size(
            pos2(rect.right() - 9.0, rect.center().y),
            vec2(10.0, 10.0),
        );
        paint_icon(ui.painter(), chev, Icon::ChevronDown, p.text_muted);
    }

    let menu_resp = resp.on_hover_text("Project files, exports, profiles, and recent items");
    Popup::menu(&menu_resp).show(|ui| {
        ui.set_min_width(230.0);

        if ui.button("Open File...  (STL / 3MF)").clicked() {
            app.request_discard_or_run(PendingDiscardAction::OpenFileDialog);
            ui.close();
        }
        if ui.button("Add Model to Plate...").clicked() {
            app.open_file_dialog(true);
            ui.close();
        }
        if ui.button("New Empty Plate").clicked() {
            app.request_discard_or_run(PendingDiscardAction::NewEmptyProject);
            ui.close();
        }

        ui.separator();

        let save_lbl = if app.project_dirty {
            "Save Project (.3mf)*   Ctrl+S"
        } else {
            "Save Project (.3mf)   Ctrl+S"
        };
        if ui.button(save_lbl).clicked() {
            app.save_project_action();
            ui.close();
        }

        let has_gcode = app.session.last_gcode.is_some();
        ui.add_enabled_ui(has_gcode, |ui| {
            if ui.button("Export G-code (.gcode)").clicked() {
                app.export_gcode_with_format(false);
                ui.close();
            }
            if ui.button("Export Binary G-code (.bgcode)").clicked() {
                app.export_gcode_with_format(true);
                ui.close();
            }
        });

        ui.separator();

        if ui.button("Import Profile JSON...").clicked() {
            app.import_profile_file_dialog();
            ui.close();
        }
        #[cfg(not(target_arch = "wasm32"))]
        if ui.button("Load Profile Folder...").clicked() {
            app.import_profile_dir_dialog();
            ui.close();
        }

        ui.separator();
        ui.label(
            RichText::new("Recent Projects")
                .size(10.5)
                .color(p.text_muted),
        );
        if app.recent_projects.entries.is_empty() {
            ui.label(
                RichText::new("No recent projects yet")
                    .size(11.0)
                    .color(p.text_muted),
            );
        } else {
            let now = now_unix_secs();
            let recent_snapshot: Vec<(String, String, String)> = app
                .recent_projects
                .entries
                .iter()
                .take(5)
                .map(|e| {
                    (
                        e.id.clone(),
                        format!(
                            "{}  [{} · {}]",
                            e.name,
                            e.format.to_ascii_uppercase(),
                            format_relative_time(e.last_opened_unix_secs, now)
                        ),
                        e.path.clone().unwrap_or_else(|| e.name.clone()),
                    )
                })
                .collect();
            for (id, label, tip) in recent_snapshot {
                if ui.button(&label).on_hover_text(tip).clicked() {
                    app.request_discard_or_run(PendingDiscardAction::OpenRecentId(id));
                    ui.close();
                }
            }
        }
        ui.separator();
        if ui.button("View All Recent on Home").clicked() {
            app.view_mode = ViewMode::Home;
            ui.close();
        }
    });
}

fn draw_project_plate_pill(
    app: &mut LayerCraftApp,
    ui: &mut egui::Ui,
    p: &ThemePalette,
    max_width: f32,
) {
    let plates = app.session.list_plates();
    let total_plates = plates.len().max(1);
    let active_plate = plates.iter().find(|pl| pl.active);
    let active_idx = active_plate.map(|pl| pl.index).unwrap_or(0);
    let active_name = active_plate.map(|pl| pl.name.as_str()).unwrap_or("Plate 1");
    let is_sliced = active_plate.is_some_and(|pl| pl.sliced);

    let title = app.active_project_title();
    let dirty_mark = if app.project_dirty { "*" } else { "" };
    let full_label = format!(
        "{title}{dirty_mark}  ·  {active_name} ({}/{total_plates})",
        active_idx + 1
    );

    let font_id = egui::FontId::new(12.0, egui::FontFamily::Proportional);
    let mut display_label = full_label.clone();
    let mut galley =
        ui.painter()
            .layout_no_wrap(display_label.clone(), font_id.clone(), p.text_primary);
    let extra_w = 34.0;
    if galley.size().x + extra_w > max_width {
        display_label = format!("{title}{dirty_mark} · P{}/{total_plates}", active_idx + 1);
        galley = ui
            .painter()
            .layout_no_wrap(display_label, font_id, p.text_primary);
    }

    let pill_w = (galley.size().x + extra_w).min(max_width).max(110.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(pill_w, 26.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if resp.hovered() || resp.is_pointer_button_down_on() {
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
            CornerRadius::same(7),
            fill,
            stroke,
            StrokeKind::Inside,
        );

        let dot_col = if is_sliced { p.success } else { p.text_muted };
        ui.painter()
            .circle_filled(pos2(rect.left() + 11.0, rect.center().y), 3.0, dot_col);

        let avail_text_w = (rect.width() - 34.0).max(20.0);
        let text_x = rect.left() + 19.0;
        let clipped_rect = egui::Rect::from_min_max(
            pos2(text_x, rect.top()),
            pos2(text_x + avail_text_w, rect.bottom()),
        );
        ui.painter().with_clip_rect(clipped_rect).galley(
            pos2(text_x, rect.center().y - galley.size().y * 0.5),
            galley,
            p.text_primary,
        );

        let chev = egui::Rect::from_center_size(
            pos2(rect.right() - 10.0, rect.center().y),
            vec2(10.0, 10.0),
        );
        paint_icon(ui.painter(), chev, Icon::ChevronDown, p.text_muted);
    }

    let pill_menu = resp.on_hover_text(format!(
        "{full_label} — click to switch, add, or remove build plates"
    ));
    let mut select_idx: Option<usize> = None;
    let mut add_requested = false;
    let mut remove_idx: Option<usize> = None;

    Popup::menu(&pill_menu).show(|ui| {
        ui.set_min_width(240.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Build Plates")
                    .size(11.0)
                    .strong()
                    .color(p.text_muted),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("+ Add Plate").clicked() {
                    add_requested = true;
                    ui.close();
                }
            });
        });
        ui.separator();
        for plate in &plates {
            ui.horizontal(|ui| {
                let status_dot = if plate.sliced { "● " } else { "○ " };
                let row_lbl = format!(
                    "{status_dot}{}. {}  ({} obj)",
                    plate.index + 1,
                    plate.name,
                    plate.object_count
                );
                if ui.selectable_label(plate.active, row_lbl).clicked() && !plate.active {
                    select_idx = Some(plate.index);
                    ui.close();
                }
                if total_plates > 1 {
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("×").on_hover_text("Remove plate").clicked() {
                            remove_idx = Some(plate.index);
                            ui.close();
                        }
                    });
                }
            });
        }
    });

    if let Some(idx) = select_idx {
        app.select_plate(idx);
    } else if add_requested {
        app.add_plate(None);
    } else if let Some(idx) = remove_idx {
        app.remove_plate(idx);
    }
}

fn draw_right_actions(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette, avail_w: f32) {
    let slicing = app.is_slicing();
    let slicing_label = if slicing {
        match app.slice_progress_snapshot() {
            Some((done, total)) if total > 0 => format!("Slicing {done}/{total}..."),
            _ => "Slicing...".to_owned(),
        }
    } else {
        String::new()
    };

    // Primary CTA rendered first in right_to_left so it sits at far right
    match app.view_mode {
        ViewMode::Home => {
            if primary_button(ui, p, Icon::OpenFile, "Open File").clicked() {
                app.request_discard_or_run(PendingDiscardAction::OpenFileDialog);
            }
        }
        ViewMode::Prepare => {
            let label = if slicing {
                slicing_label.as_str()
            } else if avail_w < 720.0 {
                "Slice"
            } else {
                "Slice Plate"
            };
            ui.add_enabled_ui(!slicing, |ui| {
                if primary_button(ui, p, Icon::SliceLayers, label).clicked() {
                    app.start_slice_action(ui.ctx());
                }
            });
            if slicing && secondary_button(ui, p, None, "Cancel").clicked() {
                app.cancel_slice_action();
            }
        }
        ViewMode::Preview => {
            let binary_mode = app.session.config.machine.binary_gcode;
            let export_label = if avail_w < 720.0 {
                if binary_mode { ".bgcode" } else { "G-code" }
            } else if cfg!(target_arch = "wasm32") {
                if binary_mode {
                    "Download .bgcode"
                } else {
                    "Download G-code"
                }
            } else if binary_mode {
                "Export .bgcode"
            } else {
                "Export G-code"
            };
            let has_gcode = app.session.last_gcode.is_some();
            if has_gcode && primary_button(ui, p, Icon::Export, export_label).clicked() {
                app.export_gcode_action();
            }
            #[cfg(not(target_arch = "wasm32"))]
            if has_gcode {
                let send_lbl = if avail_w < 840.0 {
                    "Send"
                } else {
                    "Send to Printer"
                };
                ui.add_enabled_ui(!app.is_printer_busy(), |ui| {
                    if secondary_button(ui, p, Some(Icon::SendPrinter), send_lbl)
                        .on_hover_text("Upload .bgcode or G-code directly to a printer over LAN")
                        .clicked()
                    {
                        app.open_upload_dialog();
                    }
                });
            }
            let reslice_label = if slicing {
                slicing_label.as_str()
            } else {
                "Re-slice"
            };
            ui.add_enabled_ui(!slicing, |ui| {
                if secondary_button(ui, p, Some(Icon::SliceLayers), reslice_label).clicked() {
                    app.start_slice_action(ui.ctx());
                }
            });
            if slicing && secondary_button(ui, p, None, "Cancel").clicked() {
                app.cancel_slice_action();
            }
        }
    }

    if avail_w >= 560.0 {
        v_divider(ui, p);
    }

    let theme_icon = match app.theme_mode {
        ThemeMode::Dark => Icon::ThemeDark,
        ThemeMode::Light => Icon::ThemeLight,
        ThemeMode::System => Icon::ThemeSystem,
    };
    if icon_button(
        ui,
        p,
        theme_icon,
        &format!("Theme: {} (click to cycle)", app.theme_mode.label()),
        false,
    )
    .clicked()
    {
        app.cycle_theme_mode();
    }

    if app.view_mode != ViewMode::Home {
        if icon_button(
            ui,
            p,
            Icon::SidebarToggle,
            if app.sidebar_open {
                "Hide right inspector"
            } else {
                "Show right inspector"
            },
            app.sidebar_open,
        )
        .clicked()
        {
            app.sidebar_open = !app.sidebar_open;
        }

        let hist = app.session.history_status();
        let redo_tip = match &hist.redo_label {
            Some(lbl) => format!("Redo `{lbl}` (Ctrl+Shift+Z)"),
            None => "Nothing to redo (Ctrl+Shift+Z)".to_owned(),
        };
        ui.add_enabled_ui(hist.can_redo, |ui| {
            if icon_button(ui, p, Icon::Redo, &redo_tip, false).clicked() {
                app.redo_action();
            }
        });

        let undo_tip = match &hist.undo_label {
            Some(lbl) => format!("Undo `{lbl}` (Ctrl+Z)"),
            None => "Nothing to undo (Ctrl+Z)".to_owned(),
        };
        ui.add_enabled_ui(hist.can_undo, |ui| {
            if icon_button(ui, p, Icon::Undo, &undo_tip, false).clicked() {
                app.undo_action();
            }
        });

        // Subtle color-mode quick indicator when in Preview on wide screens
        if app.view_mode == ViewMode::Preview
            && app.color_mode != ColorMode::Feature
            && avail_w >= 960.0
        {
            ui.label(
                RichText::new(app.color_mode.label())
                    .size(11.0)
                    .color(p.text_muted),
            );
        }
    }
}
