//! Two-tier top application chrome: slim header bar + collapsible contextual ribbon.

use egui::{Align, CornerRadius, Margin, Popup, RichText, Stroke, StrokeKind, pos2, vec2};
use layercraft_config::ModifierRole;
use layercraft_mesh::{ModifierPrimitive, SampleShape};
use layercraft_viewport::ColorMode;

use crate::icons::{Icon, paint_icon};
use crate::theme::{ThemeMode, ThemePalette};
use crate::ui_kit::{
    badge, icon_button, primary_button, ribbon_button, ribbon_dropdown_button, secondary_button,
    v_divider,
};
use crate::{LayerCraftApp, ViewMode};

/// Top header bar containing brand identity, Prepare/Preview mode switch, theme, and primary CTA.
pub fn draw_header_bar(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let resp = egui::Panel::top("layercraft_header")
        .frame(
            egui::Frame::NONE
                .fill(p.bg_chrome)
                .inner_margin(Margin::symmetric(12, 6)),
        )
        .show(ui, |ui| {
            let avail_w = ui.available_width();
            ui.horizontal(|ui| {
                ui.set_min_height(28.0);
                ui.spacing_mut().item_spacing.x = if avail_w < 540.0 { 5.0 } else { 8.0 };

                // Brand mark & title
                let (mark_rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), egui::Sense::hover());
                paint_icon(ui.painter(), mark_rect, Icon::BrandMark, p.accent);
                if avail_w >= 540.0 {
                    ui.label(
                        RichText::new("LayerCraft")
                            .size(14.5)
                            .strong()
                            .color(p.text_primary),
                    );
                    v_divider(ui, p);
                }

                if icon_button(
                    ui,
                    p,
                    Icon::SidebarToggle,
                    if app.sidebar_open {
                        "Hide settings sidebar"
                    } else {
                        "Show settings sidebar"
                    },
                    app.sidebar_open,
                )
                .clicked()
                {
                    app.sidebar_open = !app.sidebar_open;
                }

                // Prepare / Preview segmented pill switcher
                draw_view_mode_switcher(app, ui, p);

                // Compact active printer & layer height chip
                if avail_w >= 860.0 {
                    let machine_short = app
                        .selected_machine_profile
                        .as_deref()
                        .map(|s| s.strip_prefix("LayerCraft ").unwrap_or(s))
                        .unwrap_or("Default Printer");
                    let summary = format!(
                        "{machine_short} · {:.2} mm",
                        app.session.config.process.layer_height_mm
                    );
                    badge(ui, p.bg_card, p.text_secondary, &summary);
                }

                // Right-aligned primary actions & theme/ribbon controls
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let slicing = app.is_slicing();
                    let slicing_label = if slicing {
                        match app.slice_progress_snapshot() {
                            Some((done, total)) if total > 0 => format!("Slicing {done}/{total}..."),
                            _ => "Slicing...".to_owned(),
                        }
                    } else {
                        String::new()
                    };

                    match app.view_mode {
                        ViewMode::Prepare => {
                            let label = if slicing {
                                slicing_label.as_str()
                            } else if avail_w < 640.0 {
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
                            let export_label = if avail_w < 640.0 {
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
                            if has_gcode
                                && primary_button(ui, p, Icon::Export, export_label).clicked()
                            {
                                app.export_gcode_action();
                            }
                            #[cfg(not(target_arch = "wasm32"))]
                            if has_gcode {
                                let send_lbl = if avail_w < 720.0 {
                                    "Send"
                                } else {
                                    "Send to Printer"
                                };
                                ui.add_enabled_ui(!app.is_printer_busy(), |ui| {
                                    if secondary_button(ui, p, Some(Icon::SendPrinter), send_lbl)
                                        .on_hover_text(
                                            "Upload .bgcode or G-code directly to a printer over LAN",
                                        )
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
                                if secondary_button(ui, p, Some(Icon::SliceLayers), reslice_label)
                                    .clicked()
                                {
                                    app.start_slice_action(ui.ctx());
                                }
                            });
                            if slicing && secondary_button(ui, p, None, "Cancel").clicked() {
                                app.cancel_slice_action();
                            }
                        }
                    }

                    if avail_w >= 540.0 {
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

                    let ribbon_icon = if app.ribbon_collapsed {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronUp
                    };
                    if icon_button(
                        ui,
                        p,
                        ribbon_icon,
                        if app.ribbon_collapsed {
                            "Expand ribbon toolbar"
                        } else {
                            "Collapse ribbon toolbar"
                        },
                        !app.ribbon_collapsed,
                    )
                    .clicked()
                    {
                        app.ribbon_collapsed = !app.ribbon_collapsed;
                    }
                });
            });
        })
        .response;

    let r = resp.rect;
    ui.painter().line_segment(
        [pos2(r.left(), r.bottom()), pos2(r.right(), r.bottom())],
        Stroke::new(1.0, p.border_subtle),
    );
}

fn draw_view_mode_switcher(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    egui::Frame::NONE
        .fill(p.bg_input)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (mode, label) in [
                    (ViewMode::Prepare, "Prepare"),
                    (ViewMode::Preview, "Preview"),
                ] {
                    let active = app.view_mode == mode;
                    let text_col = if active { p.text_primary } else { p.text_muted };
                    let galley = ui.painter().layout_no_wrap(
                        label.to_owned(),
                        egui::TextStyle::Button.resolve(ui.style()),
                        text_col,
                    );
                    let (seg_rect, seg_resp) = ui.allocate_exact_size(
                        vec2(galley.size().x + 20.0, 22.0),
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
                        let pos = pos2(
                            seg_rect.center().x - galley.size().x * 0.5,
                            seg_rect.center().y - galley.size().y * 0.5,
                        );
                        ui.painter().galley(pos, galley, text_col);
                    }
                    if seg_resp.clicked() {
                        app.view_mode = mode;
                    }
                }
            });
        });
}

/// Collapsible contextual action ribbon directly beneath the header bar.
pub fn draw_ribbon_bar(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    if app.ribbon_collapsed {
        return;
    }
    let resp = egui::Panel::top("layercraft_ribbon")
        .frame(
            egui::Frame::NONE
                .fill(p.bg_panel)
                .inner_margin(Margin::symmetric(12, 5)),
        )
        .show(ui, |ui| {
            egui::ScrollArea::horizontal()
                .id_salt("layercraft_ribbon_scroll")
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(26.0);
                        ui.spacing_mut().item_spacing.x = 4.0;
                        match app.view_mode {
                            ViewMode::Prepare => draw_prepare_ribbon(app, ui, p),
                            ViewMode::Preview => draw_preview_ribbon(app, ui, p),
                        }
                    });
                });
        })
        .response;

    let r = resp.rect;
    ui.painter().line_segment(
        [pos2(r.left(), r.bottom()), pos2(r.right(), r.bottom())],
        Stroke::new(1.0, p.border_subtle),
    );
}

/// Compact build plate switcher strip shown whenever the project has multiple plates.
pub fn draw_plates_bar(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    if app.session.plate_count() <= 1 {
        return;
    }
    let plates = app.session.list_plates();
    let mut select_idx: Option<usize> = None;
    let mut add_requested = false;

    let resp = egui::Panel::top("layercraft_plates_bar")
        .frame(
            egui::Frame::NONE
                .fill(p.bg_chrome)
                .inner_margin(Margin::symmetric(12, 3)),
        )
        .show(ui, |ui| {
            egui::ScrollArea::horizontal()
                .id_salt("layercraft_plates_scroll")
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(22.0);
                        ui.spacing_mut().item_spacing.x = 5.0;
                        ui.label(RichText::new("Plates:").size(11.0).color(p.text_muted));

                        for plate in &plates {
                            let label = format!(
                                "{}. {} ({})",
                                plate.index + 1,
                                plate.name,
                                plate.object_count
                            );
                            let fill = if plate.active {
                                p.accent_muted
                            } else {
                                p.bg_card
                            };
                            let stroke = if plate.active {
                                Stroke::new(1.0, p.accent)
                            } else {
                                Stroke::new(1.0, p.border_subtle)
                            };
                            let text_col = if plate.active {
                                p.text_primary
                            } else {
                                p.text_secondary
                            };
                            let btn_resp = egui::Frame::new()
                                .fill(fill)
                                .stroke(stroke)
                                .corner_radius(CornerRadius::same(5))
                                .inner_margin(Margin::symmetric(8, 2))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 4.0;
                                        if plate.sliced {
                                            let (dot_r, _) = ui.allocate_exact_size(
                                                vec2(6.0, 12.0),
                                                egui::Sense::hover(),
                                            );
                                            ui.painter().circle_filled(
                                                dot_r.center(),
                                                2.5,
                                                p.success,
                                            );
                                        }
                                        ui.label(
                                            RichText::new(&label)
                                                .size(11.5)
                                                .strong()
                                                .color(text_col),
                                        );
                                    });
                                })
                                .response
                                .interact(egui::Sense::click());
                            if btn_resp
                                .on_hover_text(format!(
                                    "Switch to `{}` ({} object(s){})",
                                    plate.name,
                                    plate.object_count,
                                    if plate.sliced { ", sliced" } else { "" }
                                ))
                                .clicked()
                                && !plate.active
                            {
                                select_idx = Some(plate.index);
                            }
                        }

                        if icon_button(ui, p, Icon::AddPlus, "Add new empty build plate", false)
                            .clicked()
                        {
                            add_requested = true;
                        }
                    });
                });
        })
        .response;

    let r = resp.rect;
    ui.painter().line_segment(
        [pos2(r.left(), r.bottom()), pos2(r.right(), r.bottom())],
        Stroke::new(1.0, p.border_subtle),
    );

    if let Some(idx) = select_idx {
        app.select_plate(idx);
    } else if add_requested {
        app.add_plate(None);
    }
}

fn draw_prepare_ribbon(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if ribbon_button(ui, p, Icon::OpenFile, "Open", false)
            .on_hover_text("Open STL or 3MF model/project (replaces current plate)")
            .clicked()
        {
            app.request_discard_or_run(crate::PendingDiscardAction::OpenFileDialog);
        }
        if ribbon_button(ui, p, Icon::AddPlus, "Add File", false)
            .on_hover_text("Add STL or 3MF model to plate")
            .clicked()
        {
            app.open_file_dialog(true);
        }
        let has_recent = !app.prefs.recent_files.is_empty();
        ui.add_enabled_ui(has_recent, |ui| {
            let recent_resp = ribbon_dropdown_button(ui, p, Icon::OpenFile, "Recent")
                .on_hover_text("Re-open a recent STL or 3MF file");
            Popup::menu(&recent_resp).show(|ui| {
                ui.set_min_width(240.0);
                let recent_snapshot = app.prefs.recent_files.clone();
                for path_str in &recent_snapshot {
                    let display = std::path::Path::new(path_str)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(path_str.as_str());
                    if ui.button(display).on_hover_text(path_str).clicked() {
                        app.request_discard_or_run(crate::PendingDiscardAction::OpenRecentPath(
                            path_str.clone(),
                        ));
                        ui.close();
                    }
                }
                if !recent_snapshot.is_empty() {
                    ui.separator();
                    if ui.small_button("Clear Recent").clicked() {
                        app.prefs.recent_files.clear();
                        app.sync_and_save_prefs();
                        ui.close();
                    }
                }
            });
        });
    }

    let save_label = if app.project_dirty {
        "Save .3mf*"
    } else {
        "Save .3mf"
    };
    if ribbon_button(ui, p, Icon::SaveProject, save_label, app.project_dirty)
        .on_hover_text(
            "Save full LayerCraft project (.3mf with objects, modifiers & settings) (Ctrl+S)",
        )
        .clicked()
    {
        app.save_project_action();
    }

    let hist = app.session.history_status();
    let undo_tip = match &hist.undo_label {
        Some(lbl) => format!("Undo `{lbl}` (Ctrl+Z)"),
        None => "Nothing to undo (Ctrl+Z)".to_owned(),
    };
    ui.add_enabled_ui(hist.can_undo, |ui| {
        if ribbon_button(ui, p, Icon::Undo, "Undo", false)
            .on_hover_text(undo_tip)
            .clicked()
        {
            app.undo_action();
        }
    });
    let redo_tip = match &hist.redo_label {
        Some(lbl) => format!("Redo `{lbl}` (Ctrl+Shift+Z)"),
        None => "Nothing to redo (Ctrl+Shift+Z)".to_owned(),
    };
    ui.add_enabled_ui(hist.can_redo, |ui| {
        if ribbon_button(ui, p, Icon::Redo, "Redo", false)
            .on_hover_text(redo_tip)
            .clicked()
        {
            app.redo_action();
        }
    });

    v_divider(ui, p);

    let sample_resp = ribbon_dropdown_button(ui, p, Icon::SampleCube, "Samples")
        .on_hover_text("Load or add built-in calibration meshes");
    Popup::menu(&sample_resp).show(|ui| {
        ui.set_min_width(185.0);
        for shape in SampleShape::ALL {
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(app.selected_sample == shape, shape.label())
                    .clicked()
                {
                    app.request_discard_or_run(crate::PendingDiscardAction::LoadSample(shape));
                    ui.close();
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .small_button("+")
                        .on_hover_text("Add to existing plate")
                        .clicked()
                    {
                        app.load_sample_with_mode(shape, true);
                        ui.close();
                    }
                });
            });
        }
    });

    let can_arrange = app.session.objects.len() > 1;
    ui.add_enabled_ui(can_arrange, |ui| {
        if ribbon_button(ui, p, Icon::ArrangeGrid, "Arrange", false)
            .on_hover_text("Auto-arrange objects onto build plate")
            .clicked()
        {
            app.auto_arrange_plate();
        }
    });

    ui.add_enabled_ui(!app.session.objects.is_empty(), |ui| {
        if ribbon_button(ui, p, Icon::Trash, "Clear", false)
            .on_hover_text("Clear objects and modifiers from active build plate")
            .clicked()
        {
            app.request_discard_or_run(crate::PendingDiscardAction::ClearPlate);
        }
    });

    v_divider(ui, p);

    ui.label(RichText::new("Modifiers:").size(11.0).color(p.text_muted));
    if ribbon_button(ui, p, Icon::BoxShape, "+ Box", false)
        .on_hover_text("Add box parameter modifier volume")
        .clicked()
    {
        app.add_modifier_primitive(
            ModifierPrimitive::Box {
                width_mm: 12.0,
                depth_mm: 12.0,
                height_mm: 10.0,
            },
            ModifierRole::Modifier,
        );
    }
    if ribbon_button(ui, p, Icon::CylinderShape, "+ Cyl", false)
        .on_hover_text("Add cylinder parameter modifier volume")
        .clicked()
    {
        app.add_modifier_primitive(
            ModifierPrimitive::Cylinder {
                radius_mm: 6.0,
                height_mm: 10.0,
                segments: 32,
            },
            ModifierRole::Modifier,
        );
    }
    if ribbon_button(ui, p, Icon::ShieldOff, "+ Blocker", false)
        .on_hover_text("Add support blocker box")
        .clicked()
    {
        app.add_modifier_primitive(
            ModifierPrimitive::Box {
                width_mm: 12.0,
                depth_mm: 12.0,
                height_mm: 10.0,
            },
            ModifierRole::SupportBlocker,
        );
    }
    if ribbon_button(ui, p, Icon::ShieldPlus, "+ Enforcer", false)
        .on_hover_text("Add support enforcer box")
        .clicked()
    {
        app.add_modifier_primitive(
            ModifierPrimitive::Box {
                width_mm: 12.0,
                depth_mm: 12.0,
                height_mm: 10.0,
            },
            ModifierRole::SupportEnforcer,
        );
    }
    if ribbon_button(ui, p, Icon::BoxShape, "+ Cutout", false)
        .on_hover_text("Add negative part cutout box")
        .clicked()
    {
        app.add_modifier_primitive(
            ModifierPrimitive::Box {
                width_mm: 12.0,
                depth_mm: 12.0,
                height_mm: 10.0,
            },
            ModifierRole::NegativePart,
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        v_divider(ui, p);
        if ribbon_button(ui, p, Icon::Import, "Import JSON", false)
            .on_hover_text("Import OrcaSlicer/Bambu JSON profile")
            .clicked()
        {
            app.import_profile_file_dialog();
        }
        if ribbon_button(ui, p, Icon::OpenFile, "Profile Dir", false)
            .on_hover_text("Load all JSON profiles in a folder")
            .clicked()
        {
            app.import_profile_dir_dialog();
        }
    }
}

fn draw_preview_ribbon(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    ui.label(RichText::new("Color by:").size(11.0).color(p.text_muted));
    let mut mode_changed = false;
    for mode in ColorMode::ALL {
        let active = app.color_mode == mode;
        if ribbon_button(ui, p, Icon::Layers, mode.label(), active).clicked() && !active {
            app.color_mode = mode;
            mode_changed = true;
        }
    }
    if mode_changed {
        app.rebuild_scene();
    }

    v_divider(ui, p);

    if ribbon_button(ui, p, Icon::Layers, "Single Layer", app.single_layer_only).clicked() {
        app.single_layer_only = !app.single_layer_only;
    }
    if ribbon_button(ui, p, Icon::Ghost, "Ghost Mesh", app.show_ghost_mesh).clicked() {
        app.show_ghost_mesh = !app.show_ghost_mesh;
    }
    if ribbon_button(ui, p, Icon::ArrangeGrid, "Grid", app.show_bed_grid).clicked() {
        app.show_bed_grid = !app.show_bed_grid;
        app.rebuild_scene();
    }
    if ribbon_button(ui, p, Icon::Legend, "Summary & Legend", app.show_legend_hud).clicked() {
        app.show_legend_hud = !app.show_legend_hud;
    }
    if ribbon_button(ui, p, Icon::Legend, "G-code", app.show_gcode_viewer).clicked() {
        app.show_gcode_viewer = !app.show_gcode_viewer;
    }

    if app.session.last_gcode.is_some() {
        v_divider(ui, p);
        if ribbon_button(ui, p, Icon::Export, ".bgcode", false)
            .on_hover_text("Export or download Prusa Binary G-code (.bgcode)")
            .clicked()
        {
            app.export_gcode_with_format(true);
        }
        if ribbon_button(ui, p, Icon::Export, ".gcode", false)
            .on_hover_text("Export or download plain-text G-code (.gcode)")
            .clicked()
        {
            app.export_gcode_with_format(false);
        }
    }
}
