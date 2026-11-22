//! Sticky Preset Card + 7-tab Plate Outliner & Process/Machine Settings sidebar.

use egui::{Align, CornerRadius, Margin, RichText, Stroke, StrokeKind, pos2, vec2};
use layercraft_config::{
    GcodeFlavor, HeightRangeModifier, InfillPattern, IroningType, ModifierOverrides, ModifierRole,
    ProfileKind, SeamPosition, SupportStyle, WallGenerator,
};
use layercraft_mesh::{ModifierPrimitive, ModifierTransform, inspect_mesh};

use crate::LayerCraftApp;
use crate::icons::{Icon, paint_icon};
use crate::theme::ThemePalette;
use crate::ui_kit::{
    badge, card, setting_row, small_icon_button, toggle_switch, unit_drag, xy_row, xyz_row,
};

/// Settings and Plate Outliner sidebar tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarTab {
    #[default]
    Plate,
    Quality,
    Strength,
    Speed,
    Support,
    Finish,
    Machine,
}

impl SidebarTab {
    const ALL: [Self; 7] = [
        Self::Plate,
        Self::Quality,
        Self::Strength,
        Self::Speed,
        Self::Support,
        Self::Finish,
        Self::Machine,
    ];

    fn short_label(self) -> &'static str {
        match self {
            Self::Plate => "Plate",
            Self::Quality => "Qual",
            Self::Strength => "Str",
            Self::Speed => "Speed",
            Self::Support => "Supp",
            Self::Finish => "Finish",
            Self::Machine => "Mach",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Plate => "Plate — Objects, Modifiers, Height Ranges & Mesh Info",
            Self::Quality => "Quality — Layer Height, Adaptive Layers & Walls",
            Self::Strength => "Strength — Shells, Infill & Bed Adhesion",
            Self::Speed => "Speed — Print Speeds, Bridges, Cooling & Accelerations",
            Self::Support => "Support — Grid, Snug & Organic Tree Supports",
            Self::Finish => "Finish — Ironing, Scarf Seams & Arc Fitting",
            Self::Machine => "Machine — Printer, Temps, Retraction & G-code Macros",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Plate => Icon::TabPlate,
            Self::Quality => Icon::TabQuality,
            Self::Strength => Icon::TabStrength,
            Self::Speed => Icon::TabSpeed,
            Self::Support => Icon::TabSupport,
            Self::Finish => Icon::TabFinish,
            Self::Machine => Icon::TabMachine,
        }
    }
}

/// Draw the collapsible left dock containing the sticky preset card, tab strip, and tab content.
pub fn draw_sidebar(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    if !app.sidebar_open {
        return;
    }

    let resp = egui::Panel::left("layercraft_sidebar")
        .default_size(316.0)
        .size_range(280.0..=420.0)
        .frame(
            egui::Frame::NONE
                .fill(p.bg_panel)
                .inner_margin(Margin::symmetric(10, 10)),
        )
        .show(ui, |ui| {
            draw_preset_card(app, ui, p);
            ui.add_space(8.0);
            draw_tab_strip(app, ui, p);
            ui.add_space(6.0);

            egui::ScrollArea::vertical()
                .id_salt("layercraft_sidebar_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    match app.sidebar_tab {
                        SidebarTab::Plate => draw_plate_tab(app, ui, p),
                        SidebarTab::Quality => draw_quality_tab(app, ui, p),
                        SidebarTab::Strength => draw_strength_tab(app, ui, p),
                        SidebarTab::Speed => draw_speed_tab(app, ui, p),
                        SidebarTab::Support => draw_support_tab(app, ui, p),
                        SidebarTab::Finish => draw_finish_tab(app, ui, p),
                        SidebarTab::Machine => draw_machine_tab(app, ui, p),
                    }
                    ui.add_space(8.0);
                });
        })
        .response;

    let r = resp.rect;
    ui.painter().line_segment(
        [pos2(r.right(), r.top()), pos2(r.right(), r.bottom())],
        Stroke::new(1.0, p.border_subtle),
    );
}

fn draw_preset_card(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let mut apply_requested = false;
    let machines = app.session.profiles.list(Some(ProfileKind::Machine), false);
    let filaments = app
        .session
        .profiles
        .list(Some(ProfileKind::Filament), false);
    let processes = app.session.profiles.list(Some(ProfileKind::Process), false);

    card(ui, p, Some("Presets"), |ui| {
        setting_row(ui, p, "Printer", |ui| {
            let m_text = app
                .selected_machine_profile
                .as_deref()
                .unwrap_or("(Default)");
            egui::ComboBox::from_id_salt("prof_machine")
                .width(176.0)
                .selected_text(m_text)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(app.selected_machine_profile.is_none(), "(Default)")
                        .clicked()
                    {
                        app.selected_machine_profile = None;
                        apply_requested = true;
                    }
                    for item in &machines {
                        let sel =
                            app.selected_machine_profile.as_deref() == Some(item.name.as_str());
                        if ui.selectable_label(sel, &item.name).clicked() {
                            app.selected_machine_profile = Some(item.name.clone());
                            apply_requested = true;
                        }
                    }
                });
        });

        setting_row(ui, p, "Filament", |ui| {
            let f_text = app
                .selected_filament_profile
                .as_deref()
                .unwrap_or("(Default)");
            egui::ComboBox::from_id_salt("prof_filament")
                .width(176.0)
                .selected_text(f_text)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(app.selected_filament_profile.is_none(), "(Default)")
                        .clicked()
                    {
                        app.selected_filament_profile = None;
                        apply_requested = true;
                    }
                    for item in &filaments {
                        let sel =
                            app.selected_filament_profile.as_deref() == Some(item.name.as_str());
                        if ui.selectable_label(sel, &item.name).clicked() {
                            app.selected_filament_profile = Some(item.name.clone());
                            apply_requested = true;
                        }
                    }
                });
        });

        setting_row(ui, p, "Process", |ui| {
            let p_text = app
                .selected_process_profile
                .as_deref()
                .unwrap_or("(Default)");
            egui::ComboBox::from_id_salt("prof_process")
                .width(176.0)
                .selected_text(p_text)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(app.selected_process_profile.is_none(), "(Default)")
                        .clicked()
                    {
                        app.selected_process_profile = None;
                        apply_requested = true;
                    }
                    for item in &processes {
                        let sel =
                            app.selected_process_profile.as_deref() == Some(item.name.as_str());
                        if ui.selectable_label(sel, &item.name).clicked() {
                            app.selected_process_profile = Some(item.name.clone());
                            apply_requested = true;
                        }
                    }
                });
        });

        if app.profile_warning_count > 0 {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                badge(
                    ui,
                    p.accent_muted,
                    p.warning,
                    &format!("{} profile warning(s)", app.profile_warning_count),
                );
            });
        }
    });

    if apply_requested {
        app.apply_selected_profiles();
    }
}

fn draw_tab_strip(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let avail = ui.available_width();
    let count = SidebarTab::ALL.len() as f32;
    let gap = 3.0;
    let btn_w = ((avail - gap * (count - 1.0)) / count).max(34.0);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for tab in SidebarTab::ALL {
            let active = app.sidebar_tab == tab;
            let (rect, resp) = ui.allocate_exact_size(vec2(btn_w, 38.0), egui::Sense::click());
            if ui.is_rect_visible(rect) {
                let fill = if active {
                    p.accent_muted
                } else if resp.hovered() {
                    p.bg_hover
                } else {
                    p.bg_card
                };
                let stroke = Stroke::new(1.0, if active { p.accent } else { p.border_subtle });
                ui.painter().rect(
                    rect,
                    CornerRadius::same(6),
                    fill,
                    stroke,
                    StrokeKind::Inside,
                );

                let col = if active { p.accent } else { p.text_secondary };
                let icon_rect = egui::Rect::from_center_size(
                    pos2(rect.center().x, rect.top() + 13.0),
                    vec2(14.0, 14.0),
                );
                paint_icon(ui.painter(), icon_rect, tab.icon(), col);

                let galley = ui.painter().layout_no_wrap(
                    tab.short_label().to_owned(),
                    egui::FontId::proportional(10.0),
                    if active {
                        p.text_primary
                    } else {
                        p.text_secondary
                    },
                );
                let text_pos = pos2(
                    rect.center().x - galley.size().x * 0.5,
                    rect.bottom() - galley.size().y - 3.5,
                );
                ui.painter().galley(text_pos, galley, p.text_primary);
            }
            if resp.on_hover_text(tab.tooltip()).clicked() {
                app.sidebar_tab = tab;
            }
        }
    });
}

fn draw_plate_tab(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let entries: Vec<(u64, String, layercraft_mesh::ObjectTransform)> = app
        .session
        .objects
        .iter()
        .map(|o| (o.id, o.name.clone(), o.transform))
        .collect();
    let mods_snapshot: Vec<(
        u64,
        String,
        ModifierRole,
        ModifierTransform,
        ModifierOverrides,
    )> = app
        .session
        .modifiers
        .iter()
        .map(|m| {
            (
                m.id,
                m.name.clone(),
                m.role,
                m.transform,
                m.overrides.clone(),
            )
        })
        .collect();

    enum PlateAction {
        RemoveObject(u64),
        TransformObject {
            id: u64,
            pos: [f64; 2],
            rot: [f64; 3],
            scale: [f64; 3],
        },
        AddModifier(ModifierPrimitive, ModifierRole),
        RemoveModifier(u64),
        UpdateModifier {
            id: u64,
            role: ModifierRole,
            pos: [f64; 3],
            rot: [f64; 3],
            scale: [f64; 3],
            overrides: ModifierOverrides,
        },
    }
    let mut action: Option<PlateAction> = None;

    // 1. Outliner Card
    card(ui, p, Some("Build Plate Outliner"), |ui| {
        ui.label(
            RichText::new(format!("Objects ({})", entries.len()))
                .size(11.0)
                .strong()
                .color(p.text_secondary),
        );
        if entries.is_empty() {
            ui.label(
                RichText::new("No objects on plate — open or drop an STL/3MF.")
                    .size(11.5)
                    .color(p.text_muted),
            );
        } else {
            for (id, name, _) in &entries {
                let selected = app.selected_object_id == Some(*id);
                ui.horizontal(|ui| {
                    let dot_r = ui
                        .allocate_exact_size(vec2(10.0, 16.0), egui::Sense::hover())
                        .0;
                    ui.painter().circle_filled(
                        dot_r.center(),
                        3.5,
                        if selected { p.accent } else { p.axis_z },
                    );
                    if ui
                        .selectable_label(selected, format!("#{id} {name}"))
                        .clicked()
                    {
                        app.selected_object_id = Some(*id);
                        app.selected_modifier_id = None;
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if small_icon_button(ui, p, Icon::Trash, "Remove object").clicked() {
                            action = Some(PlateAction::RemoveObject(*id));
                        }
                    });
                });
            }
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("Modifiers ({})", mods_snapshot.len()))
                    .size(11.0)
                    .strong()
                    .color(p.text_secondary),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("+ Cyl").clicked() {
                    action = Some(PlateAction::AddModifier(
                        ModifierPrimitive::Cylinder {
                            radius_mm: 6.0,
                            height_mm: 10.0,
                            segments: 32,
                        },
                        ModifierRole::Modifier,
                    ));
                }
                if ui.small_button("+ Box").clicked() {
                    action = Some(PlateAction::AddModifier(
                        ModifierPrimitive::Box {
                            width_mm: 12.0,
                            depth_mm: 12.0,
                            height_mm: 10.0,
                        },
                        ModifierRole::Modifier,
                    ));
                }
            });
        });

        if mods_snapshot.is_empty() {
            ui.label(
                RichText::new("No 3D modifier volumes.")
                    .size(11.5)
                    .color(p.text_muted),
            );
        } else {
            for (id, name, role, _, _) in &mods_snapshot {
                let selected = app.selected_modifier_id == Some(*id);
                let role_col = match role {
                    ModifierRole::Modifier => p.warning,
                    ModifierRole::SupportBlocker => p.danger,
                    ModifierRole::SupportEnforcer => p.success,
                };
                ui.horizontal(|ui| {
                    let dot_r = ui
                        .allocate_exact_size(vec2(10.0, 16.0), egui::Sense::hover())
                        .0;
                    ui.painter().circle_filled(dot_r.center(), 3.5, role_col);
                    if ui
                        .selectable_label(selected, format!("#{id} {name} ({})", role.label()))
                        .clicked()
                    {
                        app.selected_modifier_id = Some(*id);
                        app.selected_object_id = None;
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if small_icon_button(ui, p, Icon::Trash, "Remove modifier").clicked() {
                            action = Some(PlateAction::RemoveModifier(*id));
                        }
                    });
                });
            }
        }
    });

    // 2. Selected Object Transform Inspector
    if let Some(sel_id) = app.selected_object_id
        && let Some((_, name, xf)) = entries.iter().find(|(id, _, _)| *id == sel_id)
    {
        ui.add_space(6.0);
        let title = format!("Transform — #{sel_id} {name}");
        card(ui, p, Some(&title), |ui| {
            let mut pos = xf.position_xy_mm;
            let mut rot = xf.rotation_deg;
            let [mut uni_scale, _, _] = xf.scale;
            let mut changed = false;
            let bw = app.session.config.machine.bed_width_mm;
            let bd = app.session.config.machine.bed_depth_mm;

            changed |= xy_row(
                ui,
                p,
                "Position (mm)",
                &mut pos,
                [1.0..=bw, 1.0..=bd],
                0.5,
                "",
            );
            ui.add_space(4.0);
            changed |= xyz_row(ui, p, "Rotation (°)", &mut rot, -180.0..=180.0, 1.0, "°");
            ui.add_space(4.0);
            setting_row(ui, p, "Uniform scale", |ui| {
                changed |= unit_drag(ui, &mut uni_scale, 0.1..=5.0, 0.02, "×").changed();
            });

            if changed && action.is_none() {
                action = Some(PlateAction::TransformObject {
                    id: sel_id,
                    pos,
                    rot,
                    scale: [uni_scale, uni_scale, uni_scale],
                });
            }
        });
    }

    // 3. Selected Modifier Inspector
    if let Some(sel_id) = app.selected_modifier_id
        && let Some((_, name, role_cur, xf, ov_cur)) =
            mods_snapshot.iter().find(|(id, _, _, _, _)| *id == sel_id)
    {
        ui.add_space(6.0);
        let title = format!("Modifier — #{sel_id} {name}");
        card(ui, p, Some(&title), |ui| {
            let mut role = *role_cur;
            let mut pos = xf.position_xyz_mm;
            let mut rot = xf.rotation_deg;
            let mut scale = xf.scale;
            let mut ov = ov_cur.clone();
            let mut changed = false;
            let max_dim = app
                .session
                .config
                .machine
                .bed_width_mm
                .max(app.session.config.machine.bed_depth_mm)
                .max(app.session.config.machine.bed_height_mm);

            setting_row(ui, p, "Role", |ui| {
                egui::ComboBox::from_id_salt("mod_role_combo")
                    .selected_text(role.label())
                    .show_ui(ui, |ui| {
                        for r in ModifierRole::ALL {
                            changed |= ui.selectable_value(&mut role, r, r.label()).changed();
                        }
                    });
            });
            ui.add_space(4.0);
            changed |= xyz_row(ui, p, "Position (mm)", &mut pos, 0.0..=max_dim, 0.5, "");
            ui.add_space(4.0);
            let [_, _, mut rot_z] = rot;
            setting_row(ui, p, "Rotation Z", |ui| {
                if unit_drag(ui, &mut rot_z, -180.0..=180.0, 1.0, "°").changed() {
                    rot = [0.0, 0.0, rot_z];
                    changed = true;
                }
            });
            ui.add_space(4.0);
            changed |= xyz_row(ui, p, "Scale", &mut scale, 0.1..=10.0, 0.05, "×");

            if role == ModifierRole::Modifier {
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(
                    RichText::new("LOCAL OVERRIDES")
                        .size(10.0)
                        .strong()
                        .color(p.text_muted),
                );

                let mut has_dens = ov.sparse_infill_density.is_some();
                setting_row(ui, p, "Override infill %", |ui| {
                    if let Some(d) = &mut ov.sparse_infill_density {
                        let mut pct = *d * 100.0;
                        if unit_drag(ui, &mut pct, 0.0..=100.0, 1.0, "%").changed() {
                            *d = (pct / 100.0).clamp(0.0, 1.0);
                            changed = true;
                        }
                    }
                    if toggle_switch(ui, p, &mut has_dens).changed() {
                        ov.sparse_infill_density = if has_dens { Some(0.50) } else { None };
                        changed = true;
                    }
                });

                let mut has_pat = ov.sparse_infill_pattern.is_some();
                setting_row(ui, p, "Override pattern", |ui| {
                    if let Some(pat) = &mut ov.sparse_infill_pattern {
                        egui::ComboBox::from_id_salt("mod_pat_combo")
                            .width(100.0)
                            .selected_text(pat.label())
                            .show_ui(ui, |ui| {
                                for candidate in InfillPattern::ALL {
                                    changed |= ui
                                        .selectable_value(pat, candidate, candidate.label())
                                        .changed();
                                }
                            });
                    }
                    if toggle_switch(ui, p, &mut has_pat).changed() {
                        ov.sparse_infill_pattern = if has_pat {
                            Some(InfillPattern::Gyroid)
                        } else {
                            None
                        };
                        changed = true;
                    }
                });

                let mut has_walls = ov.wall_loops.is_some();
                setting_row(ui, p, "Override walls", |ui| {
                    if let Some(w) = &mut ov.wall_loops {
                        changed |= unit_drag(ui, w, 0..=20, 0.25, "").changed();
                    }
                    if toggle_switch(ui, p, &mut has_walls).changed() {
                        ov.wall_loops = if has_walls { Some(4) } else { None };
                        changed = true;
                    }
                });

                let mut has_iron = ov.ironing_mode.is_some();
                setting_row(ui, p, "Override ironing", |ui| {
                    if let Some(im) = &mut ov.ironing_mode {
                        egui::ComboBox::from_id_salt("mod_iron_combo")
                            .width(110.0)
                            .selected_text(im.label())
                            .show_ui(ui, |ui| {
                                for mode in IroningType::ALL {
                                    changed |=
                                        ui.selectable_value(im, mode, mode.label()).changed();
                                }
                            });
                    }
                    if toggle_switch(ui, p, &mut has_iron).changed() {
                        ov.ironing_mode = if has_iron {
                            Some(IroningType::TopmostSurface)
                        } else {
                            None
                        };
                        changed = true;
                    }
                });
            }

            if changed && action.is_none() {
                action = Some(PlateAction::UpdateModifier {
                    id: sel_id,
                    role,
                    pos,
                    rot,
                    scale,
                    overrides: ov,
                });
            }
        });
    }

    match action {
        Some(PlateAction::RemoveObject(id)) => app.remove_object(id),
        Some(PlateAction::TransformObject {
            id,
            pos,
            rot,
            scale,
        }) => app.transform_object(id, pos, rot, scale),
        Some(PlateAction::AddModifier(prim, role)) => app.add_modifier_primitive(prim, role),
        Some(PlateAction::RemoveModifier(id)) => app.remove_modifier(id),
        Some(PlateAction::UpdateModifier {
            id,
            role,
            pos,
            rot,
            scale,
            overrides,
        }) => app.update_modifier(id, role, pos, rot, scale, overrides),
        None => {}
    }

    // 4. Height Ranges Card
    ui.add_space(6.0);
    card(ui, p, Some("Height Range Bands"), |ui| {
        let max_nozzle_h = (app.session.config.machine.nozzle_diameter_mm * 1.25).max(0.04);
        let proc_cfg = &mut app.session.config.process;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Override layer height by Z range")
                    .size(11.5)
                    .color(p.text_muted),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("+ Add Z Band").clicked() {
                    let z0 = proc_cfg
                        .height_ranges
                        .last()
                        .map(|r| r.max_z_mm)
                        .unwrap_or(0.0);
                    proc_cfg.height_ranges.push(HeightRangeModifier {
                        min_z_mm: z0,
                        max_z_mm: z0 + 5.0,
                        layer_height_mm: Some(proc_cfg.layer_height_mm),
                        overrides: ModifierOverrides::default(),
                    });
                }
            });
        });

        let mut rm_hr_idx: Option<usize> = None;
        for (ri, hr) in proc_cfg.height_ranges.iter_mut().enumerate() {
            ui.push_id(ri, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Z:");
                    if ui
                        .add(
                            egui::DragValue::new(&mut hr.min_z_mm)
                                .speed(0.2)
                                .range(0.0..=500.0),
                        )
                        .changed()
                        && hr.max_z_mm <= hr.min_z_mm
                    {
                        hr.max_z_mm = hr.min_z_mm + 0.5;
                    }
                    ui.label("–");
                    let min_hi = hr.min_z_mm + 0.1;
                    ui.add(
                        egui::DragValue::new(&mut hr.max_z_mm)
                            .speed(0.2)
                            .range(min_hi..=500.0),
                    );
                    let mut h_val = hr.layer_height_mm.unwrap_or(0.0);
                    if ui
                        .add(
                            egui::DragValue::new(&mut h_val)
                                .speed(0.01)
                                .prefix("h:")
                                .range(0.0..=max_nozzle_h),
                        )
                        .changed()
                    {
                        hr.layer_height_mm = if h_val <= 1e-4 {
                            None
                        } else {
                            Some(h_val.clamp(0.02, max_nozzle_h))
                        };
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if small_icon_button(ui, p, Icon::Trash, "Remove Z band").clicked() {
                            rm_hr_idx = Some(ri);
                        }
                    });
                });
            });
        }
        if let Some(ri) = rm_hr_idx
            && ri < proc_cfg.height_ranges.len()
        {
            proc_cfg.height_ranges.remove(ri);
        }
    });

    // 5. Mesh Summary Card
    if let Some(mesh) = &app.session.prepared_mesh {
        ui.add_space(6.0);
        let insp = inspect_mesh(mesh);
        card(ui, p, Some("Mesh Summary"), |ui| {
            setting_row(ui, p, "Triangles", |ui| {
                ui.label(format!("{}", insp.triangle_count));
            });
            setting_row(ui, p, "Volume", |ui| {
                ui.label(format!("{:.1} mm³", insp.volume_mm3));
            });
            if let Some(bb) = insp.aabb {
                let s = bb.size();
                setting_row(ui, p, "Dimensions", |ui| {
                    ui.label(format!("{:.1} × {:.1} × {:.1} mm", s.x, s.y, s.z));
                });
            }
            setting_row(ui, p, "Topology", |ui| {
                if insp.non_manifold_edges == 0 {
                    badge(ui, p.bg_panel, p.success, "Watertight");
                } else {
                    badge(
                        ui,
                        p.bg_panel,
                        p.warning,
                        &format!("{} non-manifold", insp.non_manifold_edges),
                    );
                }
            });
        });
    }
}

fn draw_quality_tab(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let max_nozzle_h = (app.session.config.machine.nozzle_diameter_mm * 1.25).max(0.04);
    let proc_cfg = &mut app.session.config.process;

    card(ui, p, Some("Layer Heights & Shells"), |ui| {
        setting_row(ui, p, "Layer height", |ui| {
            unit_drag(ui, &mut proc_cfg.layer_height_mm, 0.04..=0.80, 0.01, "mm");
        });
        setting_row(ui, p, "First layer height", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.first_layer_height_mm,
                0.05..=0.80,
                0.01,
                "mm",
            );
        });

        let mut top_thick = proc_cfg.top_shell_thickness_mm.unwrap_or(0.0);
        setting_row(ui, p, "Top min thick (0=off)", |ui| {
            if unit_drag(ui, &mut top_thick, 0.0..=10.0, 0.05, "mm").changed() {
                proc_cfg.top_shell_thickness_mm = if top_thick <= 1e-4 {
                    None
                } else {
                    Some(top_thick)
                };
            }
        });

        let mut bot_thick = proc_cfg.bottom_shell_thickness_mm.unwrap_or(0.0);
        setting_row(ui, p, "Bottom min thick (0=off)", |ui| {
            if unit_drag(ui, &mut bot_thick, 0.0..=10.0, 0.05, "mm").changed() {
                proc_cfg.bottom_shell_thickness_mm = if bot_thick <= 1e-4 {
                    None
                } else {
                    Some(bot_thick)
                };
            }
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Adaptive Layer Height"), |ui| {
        let al = &mut proc_cfg.adaptive_layer_height;
        setting_row(ui, p, "Enable adaptive", |ui| {
            toggle_switch(ui, p, &mut al.enabled);
        });

        let mut min_v = al.min_layer_height_mm.unwrap_or(0.0);
        setting_row(ui, p, "Min height (0=auto)", |ui| {
            if unit_drag(ui, &mut min_v, 0.0..=max_nozzle_h, 0.01, "mm").changed() {
                al.min_layer_height_mm = if min_v <= 1e-4 {
                    None
                } else {
                    Some(min_v.clamp(0.02, max_nozzle_h))
                };
            }
        });

        let mut max_v = al.max_layer_height_mm.unwrap_or(0.0);
        setting_row(ui, p, "Max height (0=auto)", |ui| {
            if unit_drag(ui, &mut max_v, 0.0..=max_nozzle_h, 0.01, "mm").changed() {
                al.max_layer_height_mm = if max_v <= 1e-4 {
                    None
                } else {
                    Some(max_v.clamp(0.02, max_nozzle_h))
                };
            }
        });

        setting_row(ui, p, "Quality bias", |ui| {
            unit_drag(ui, &mut al.quality, 0.0..=1.0, 0.02, "");
        });
        setting_row(ui, p, "Smooth radius", |ui| {
            unit_drag(ui, &mut al.smooth_radius_mm, 0.0..=20.0, 0.1, "mm");
        });
        setting_row(ui, p, "Max step ratio", |ui| {
            unit_drag(ui, &mut al.max_step_ratio, 1.05..=3.0, 0.05, "×");
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Perimeter Walls & Seam"), |ui| {
        setting_row(ui, p, "Wall generator", |ui| {
            egui::ComboBox::from_id_salt("wall_gen")
                .selected_text(proc_cfg.wall_generator.label())
                .show_ui(ui, |ui| {
                    for wg in WallGenerator::ALL {
                        ui.selectable_value(&mut proc_cfg.wall_generator, wg, wg.label());
                    }
                });
        });

        if proc_cfg.wall_generator == WallGenerator::Arachne {
            let mut min_pct = proc_cfg.arachne.min_bead_width_ratio * 100.0;
            setting_row(ui, p, "Min bead width", |ui| {
                if unit_drag(ui, &mut min_pct, 20.0..=100.0, 1.0, "%").changed() {
                    proc_cfg.arachne.min_bead_width_ratio = (min_pct / 100.0).clamp(0.20, 1.00);
                }
            });

            let mut max_pct = proc_cfg.arachne.max_bead_width_ratio * 100.0;
            setting_row(ui, p, "Max bead width", |ui| {
                if unit_drag(ui, &mut max_pct, 100.0..=300.0, 1.0, "%").changed() {
                    proc_cfg.arachne.max_bead_width_ratio = (max_pct / 100.0).clamp(1.00, 3.00);
                }
            });

            let mut feat_pct = proc_cfg.arachne.min_feature_size_ratio * 100.0;
            setting_row(ui, p, "Min feature size", |ui| {
                if unit_drag(ui, &mut feat_pct, 5.0..=100.0, 1.0, "%").changed() {
                    proc_cfg.arachne.min_feature_size_ratio = (feat_pct / 100.0).clamp(0.05, 1.00);
                }
            });

            setting_row(ui, p, "Wall dist count", |ui| {
                unit_drag(
                    ui,
                    &mut proc_cfg.arachne.wall_distribution_count,
                    1..=8,
                    0.25,
                    "",
                );
            });
        }

        setting_row(ui, p, "Seam position", |ui| {
            egui::ComboBox::from_id_salt("seam_pos")
                .selected_text(proc_cfg.seam_position.label())
                .show_ui(ui, |ui| {
                    for pos in SeamPosition::ALL {
                        ui.selectable_value(&mut proc_cfg.seam_position, pos, pos.label());
                    }
                });
        });

        setting_row(ui, p, "Outer wall width", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.outer_wall_width_mm,
                0.10..=1.20,
                0.01,
                "mm",
            );
        });
        setting_row(ui, p, "Inner wall width", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.inner_wall_width_mm,
                0.10..=1.20,
                0.01,
                "mm",
            );
        });
    });
}

fn draw_strength_tab(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;

    card(ui, p, Some("Walls & Solid Shells"), |ui| {
        setting_row(ui, p, "Wall loops", |ui| {
            unit_drag(ui, &mut proc_cfg.wall_loops, 0..=12, 0.25, "");
        });
        setting_row(ui, p, "Top shell layers", |ui| {
            unit_drag(ui, &mut proc_cfg.top_shell_layers, 0..=20, 0.25, "");
        });
        setting_row(ui, p, "Bottom shell layers", |ui| {
            unit_drag(ui, &mut proc_cfg.bottom_shell_layers, 0..=20, 0.25, "");
        });
        setting_row(ui, p, "Monotonic top/bottom", |ui| {
            toggle_switch(ui, p, &mut proc_cfg.monotonic_solid_infill);
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Sparse Infill"), |ui| {
        let mut pct = proc_cfg.sparse_infill_density * 100.0;
        setting_row(ui, p, "Infill density", |ui| {
            if unit_drag(ui, &mut pct, 0.0..=100.0, 1.0, "%").changed() {
                proc_cfg.sparse_infill_density = (pct / 100.0).clamp(0.0, 1.0);
            }
        });
        setting_row(ui, p, "Infill pattern", |ui| {
            egui::ComboBox::from_id_salt("infill_pat")
                .selected_text(proc_cfg.sparse_infill_pattern.label())
                .show_ui(ui, |ui| {
                    for pat in InfillPattern::ALL {
                        ui.selectable_value(&mut proc_cfg.sparse_infill_pattern, pat, pat.label());
                    }
                });
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Bed Adhesion"), |ui| {
        setting_row(ui, p, "Skirt loops", |ui| {
            unit_drag(ui, &mut proc_cfg.skirt_loops, 0..=10, 0.25, "");
        });
        setting_row(ui, p, "Brim width", |ui| {
            unit_drag(ui, &mut proc_cfg.brim_width_mm, 0.0..=20.0, 0.2, "mm");
        });
    });
}

fn draw_speed_tab(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
    let f = &mut app.session.config.filament;

    card(ui, p, Some("Print Speeds"), |ui| {
        setting_row(ui, p, "Outer wall", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.outer_wall_speed_mm_s,
                5.0..=500.0,
                2.0,
                "mm/s",
            );
        });
        setting_row(ui, p, "Inner wall", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.inner_wall_speed_mm_s,
                5.0..=500.0,
                2.0,
                "mm/s",
            );
        });
        setting_row(ui, p, "Sparse infill", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.infill_speed_mm_s,
                5.0..=600.0,
                2.0,
                "mm/s",
            );
        });
        setting_row(ui, p, "First layer", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.first_layer_speed_mm_s,
                5.0..=200.0,
                1.0,
                "mm/s",
            );
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Bridges & Overhangs"), |ui| {
        setting_row(ui, p, "Detect bridges", |ui| {
            toggle_switch(ui, p, &mut proc_cfg.detect_bridges);
        });
        setting_row(ui, p, "Bridge speed", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.bridge_speed_mm_s,
                5.0..=150.0,
                1.0,
                "mm/s",
            );
        });
        setting_row(ui, p, "Bridge flow ratio", |ui| {
            unit_drag(ui, &mut proc_cfg.bridge_flow_ratio, 0.5..=1.5, 0.02, "×");
        });
        setting_row(ui, p, "Bridge fan", |ui| {
            unit_drag(ui, &mut f.bridge_fan_percent, 0..=100, 1.0, "%");
        });
        setting_row(ui, p, "Detect overhangs", |ui| {
            toggle_switch(ui, p, &mut proc_cfg.detect_overhangs);
        });
        setting_row(ui, p, "Overhang angle", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.overhang_threshold_deg,
                10.0..=85.0,
                1.0,
                "°",
            );
        });
        setting_row(ui, p, "Overhang speed", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.overhang_speed_mm_s,
                5.0..=150.0,
                1.0,
                "mm/s",
            );
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Cooling & Volumetric Flow"), |ui| {
        setting_row(ui, p, "Max volumetric flow", |ui| {
            unit_drag(
                ui,
                &mut f.max_volumetric_speed_mm3_s,
                0.5..=60.0,
                0.2,
                "mm³/s",
            );
        });
        setting_row(ui, p, "Fan min", |ui| {
            if unit_drag(ui, &mut f.fan_min_percent, 0..=100, 1.0, "%").changed()
                && f.fan_min_percent > f.fan_max_percent
            {
                f.fan_max_percent = f.fan_min_percent;
            }
        });
        setting_row(ui, p, "Fan max", |ui| {
            if unit_drag(ui, &mut f.fan_max_percent, 0..=100, 1.0, "%").changed()
                && f.fan_max_percent < f.fan_min_percent
            {
                f.fan_min_percent = f.fan_max_percent;
            }
        });
        setting_row(ui, p, "Disable fan first layers", |ui| {
            unit_drag(ui, &mut f.disable_fan_first_layers, 0..=20, 0.25, "");
        });
        setting_row(ui, p, "Slowdown layer time", |ui| {
            unit_drag(ui, &mut f.slow_down_layer_time_s, 0.0..=60.0, 0.2, "s");
        });
        setting_row(ui, p, "Min slowdown speed", |ui| {
            unit_drag(
                ui,
                &mut f.slow_down_min_speed_mm_s,
                1.0..=100.0,
                0.5,
                "mm/s",
            );
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Acceleration (0 = Firmware)"), |ui| {
        for (label, opt_val) in [
            ("Default", &mut proc_cfg.default_acceleration_mm_s2),
            ("Outer wall", &mut proc_cfg.outer_wall_acceleration_mm_s2),
            ("First layer", &mut proc_cfg.first_layer_acceleration_mm_s2),
            ("Travel", &mut proc_cfg.travel_acceleration_mm_s2),
        ] {
            let mut v = opt_val.unwrap_or(0.0);
            setting_row(ui, p, label, |ui| {
                if unit_drag(ui, &mut v, 0.0..=50_000.0, 50.0, "mm/s²").changed() {
                    *opt_val = if v <= 0.0 { None } else { Some(v.max(10.0)) };
                }
            });
        }
    });
}

fn draw_support_tab(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let sup = &mut app.session.config.process.support;

    card(ui, p, Some("Automatic Supports"), |ui| {
        setting_row(ui, p, "Enable supports", |ui| {
            toggle_switch(ui, p, &mut sup.enabled);
        });
        setting_row(ui, p, "Support style", |ui| {
            egui::ComboBox::from_id_salt("support_style")
                .selected_text(sup.style.label())
                .show_ui(ui, |ui| {
                    for st in SupportStyle::ALL {
                        ui.selectable_value(&mut sup.style, st, st.label());
                    }
                });
        });
        setting_row(ui, p, "On build plate only", |ui| {
            toggle_switch(ui, p, &mut sup.on_build_plate_only);
        });
        setting_row(ui, p, "Threshold angle", |ui| {
            unit_drag(ui, &mut sup.threshold_angle_deg, 10.0..=85.0, 1.0, "°");
        });
        setting_row(ui, p, "Top Z gap", |ui| {
            unit_drag(ui, &mut sup.top_z_distance_mm, 0.0..=2.0, 0.05, "mm");
        });
        setting_row(ui, p, "XY clearance", |ui| {
            unit_drag(ui, &mut sup.xy_distance_mm, 0.0..=5.0, 0.05, "mm");
        });
        setting_row(ui, p, "Pattern spacing", |ui| {
            unit_drag(ui, &mut sup.base_pattern_spacing_mm, 0.5..=20.0, 0.1, "mm");
        });
        setting_row(ui, p, "Interface layers", |ui| {
            unit_drag(ui, &mut sup.interface_layers, 0..=8, 0.25, "");
        });
    });

    if sup.style == SupportStyle::Tree {
        ui.add_space(6.0);
        let tr = &mut sup.tree;
        card(ui, p, Some("Tree Branching"), |ui| {
            setting_row(ui, p, "Max branch angle", |ui| {
                unit_drag(ui, &mut tr.max_branch_angle_deg, 10.0..=70.0, 1.0, "°");
            });
            setting_row(ui, p, "Tip diameter", |ui| {
                unit_drag(
                    ui,
                    &mut tr.tip_diameter_mm,
                    0.4..=tr.max_trunk_diameter_mm.min(10.0),
                    0.1,
                    "mm",
                );
            });
            setting_row(ui, p, "Branch diameter", |ui| {
                unit_drag(
                    ui,
                    &mut tr.branch_diameter_mm,
                    0.6..=tr.max_trunk_diameter_mm.min(20.0),
                    0.1,
                    "mm",
                );
            });
            setting_row(ui, p, "Max trunk diameter", |ui| {
                if unit_drag(ui, &mut tr.max_trunk_diameter_mm, 1.0..=50.0, 0.2, "mm").changed() {
                    tr.tip_diameter_mm = tr.tip_diameter_mm.min(tr.max_trunk_diameter_mm);
                    tr.branch_diameter_mm = tr.branch_diameter_mm.min(tr.max_trunk_diameter_mm);
                }
            });
            setting_row(ui, p, "Branch distance", |ui| {
                unit_drag(ui, &mut tr.branch_distance_mm, 0.8..=25.0, 0.2, "mm");
            });
            setting_row(ui, p, "Branch wall loops", |ui| {
                unit_drag(ui, &mut tr.branch_wall_loops, 1..=6, 0.25, "");
            });
            setting_row(ui, p, "Trunk base flare", |ui| {
                unit_drag(ui, &mut tr.trunk_base_flare_mm, 0.0..=10.0, 0.1, "mm");
            });
        });
    }
}

fn draw_finish_tab(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;

    card(ui, p, Some("Top-Surface Ironing"), |ui| {
        setting_row(ui, p, "Ironing mode", |ui| {
            egui::ComboBox::from_id_salt("ironing_mode")
                .selected_text(proc_cfg.ironing.mode.label())
                .show_ui(ui, |ui| {
                    for mode in IroningType::ALL {
                        ui.selectable_value(&mut proc_cfg.ironing.mode, mode, mode.label());
                    }
                });
        });
        let mut iron_flow_pct = proc_cfg.ironing.flow_ratio * 100.0;
        setting_row(ui, p, "Ironing flow", |ui| {
            if unit_drag(ui, &mut iron_flow_pct, 1.0..=50.0, 0.5, "%").changed() {
                proc_cfg.ironing.flow_ratio = (iron_flow_pct / 100.0).clamp(0.01, 0.5);
            }
        });
        setting_row(ui, p, "Line spacing", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.ironing.line_spacing_mm,
                0.04..=1.0,
                0.01,
                "mm",
            );
        });
        setting_row(ui, p, "Perimeter inset", |ui| {
            unit_drag(ui, &mut proc_cfg.ironing.inset_mm, 0.0..=5.0, 0.02, "mm");
        });
        setting_row(ui, p, "Ironing speed", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.ironing.speed_mm_s,
                5.0..=200.0,
                1.0,
                "mm/s",
            );
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Scarf Seams"), |ui| {
        setting_row(ui, p, "Enable scarf seams", |ui| {
            toggle_switch(ui, p, &mut proc_cfg.scarf_seam.enabled);
        });
        setting_row(ui, p, "Scarf length", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.scarf_seam.length_mm,
                1.0..=200.0,
                0.5,
                "mm",
            );
        });
        setting_row(ui, p, "Scarf steps", |ui| {
            unit_drag(ui, &mut proc_cfg.scarf_seam.steps, 2..=64, 0.5, "");
        });
        let mut start_pct = proc_cfg.scarf_seam.start_height_ratio * 100.0;
        setting_row(ui, p, "Start height", |ui| {
            if unit_drag(ui, &mut start_pct, 2.0..=50.0, 1.0, "%").changed() {
                proc_cfg.scarf_seam.start_height_ratio = (start_pct / 100.0).clamp(0.02, 0.50);
            }
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Arc Fitting (G2 / G3)"), |ui| {
        setting_row(ui, p, "Enable G2/G3 arcs", |ui| {
            toggle_switch(ui, p, &mut proc_cfg.arc_fitting);
        });
        setting_row(ui, p, "Arc tolerance", |ui| {
            unit_drag(ui, &mut proc_cfg.arc_tolerance_mm, 0.002..=0.5, 0.005, "mm");
        });
    });
}

fn draw_machine_tab(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let mut bed_changed = false;

    card(ui, p, Some("Printer Hardware"), |ui| {
        let m = &mut app.session.config.machine;
        setting_row(ui, p, "Firmware flavor", |ui| {
            egui::ComboBox::from_id_salt("flavor_sel")
                .selected_text(m.flavor.label())
                .show_ui(ui, |ui| {
                    for fl in GcodeFlavor::ALL {
                        ui.selectable_value(&mut m.flavor, fl, fl.label());
                    }
                });
        });
        setting_row(ui, p, "Nozzle diameter", |ui| {
            unit_drag(ui, &mut m.nozzle_diameter_mm, 0.15..=1.20, 0.05, "mm");
        });
        setting_row(ui, p, "High-flow nozzle", |ui| {
            toggle_switch(ui, p, &mut m.nozzle_high_flow);
        });
        setting_row(ui, p, "Bed width (X)", |ui| {
            bed_changed |= unit_drag(ui, &mut m.bed_width_mm, 50.0..=600.0, 1.0, "mm").changed();
        });
        setting_row(ui, p, "Bed depth (Y)", |ui| {
            bed_changed |= unit_drag(ui, &mut m.bed_depth_mm, 50.0..=600.0, 1.0, "mm").changed();
        });
        setting_row(ui, p, "Bed height (Z)", |ui| {
            unit_drag(ui, &mut m.bed_height_mm, 50.0..=600.0, 1.0, "mm");
        });
        setting_row(ui, p, "Relative extrusion", |ui| {
            toggle_switch(ui, p, &mut m.relative_extrusion);
        });
        setting_row(ui, p, "Emit M73 progress", |ui| {
            toggle_switch(ui, p, &mut m.emit_progress_m73);
        });
    });

    if bed_changed {
        app.rebuild_scene();
    }

    ui.add_space(6.0);
    card(ui, p, Some("Filament & Temperatures"), |ui| {
        let f = &mut app.session.config.filament;
        setting_row(ui, p, "Nozzle temp", |ui| {
            unit_drag(ui, &mut f.nozzle_temp_c, 150..=340, 1.0, "°C");
        });
        setting_row(ui, p, "Bed temp", |ui| {
            unit_drag(ui, &mut f.bed_temp_c, 0..=130, 1.0, "°C");
        });
        let mut ch_val = f.chamber_temp_c.unwrap_or(0);
        setting_row(ui, p, "Chamber (0=off)", |ui| {
            if unit_drag(ui, &mut ch_val, 0..=100, 1.0, "°C").changed() {
                f.chamber_temp_c = if ch_val == 0 { None } else { Some(ch_val) };
            }
        });
        let mut pa_val = f.pressure_advance.unwrap_or(0.0);
        setting_row(ui, p, "Pressure advance", |ui| {
            if unit_drag(ui, &mut pa_val, 0.0..=1.0, 0.002, "").changed() {
                f.pressure_advance = if pa_val <= 0.0 { None } else { Some(pa_val) };
            }
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Retraction & Wipe"), |ui| {
        let m = &mut app.session.config.machine;
        setting_row(ui, p, "Retract length", |ui| {
            unit_drag(ui, &mut m.retraction_length_mm, 0.0..=15.0, 0.05, "mm");
        });
        setting_row(ui, p, "Z-hop", |ui| {
            unit_drag(ui, &mut m.z_hop_mm, 0.0..=5.0, 0.05, "mm");
        });
        setting_row(ui, p, "Wipe on retract", |ui| {
            toggle_switch(ui, p, &mut m.wipe_on_retract);
        });
        setting_row(ui, p, "Wipe distance", |ui| {
            unit_drag(ui, &mut m.wipe_distance_mm, 0.0..=10.0, 0.1, "mm");
        });
    });

    ui.add_space(6.0);
    card(ui, p, Some("Custom Start / End G-code"), |ui| {
        let m = &mut app.session.config.machine;
        ui.label(
            RichText::new("Start G-code (empty = built-in):")
                .size(11.0)
                .color(p.text_secondary),
        );
        let mut start_buf = m.start_gcode.clone().unwrap_or_default();
        if ui
            .add(
                egui::TextEdit::multiline(&mut start_buf)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY)
                    .code_editor(),
            )
            .changed()
        {
            m.start_gcode = if start_buf.trim().is_empty() {
                None
            } else {
                Some(start_buf)
            };
        }

        ui.add_space(6.0);
        ui.label(
            RichText::new("End G-code (empty = built-in):")
                .size(11.0)
                .color(p.text_secondary),
        );
        let mut end_buf = m.end_gcode.clone().unwrap_or_default();
        if ui
            .add(
                egui::TextEdit::multiline(&mut end_buf)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY)
                    .code_editor(),
            )
            .changed()
        {
            m.end_gcode = if end_buf.trim().is_empty() {
                None
            } else {
                Some(end_buf)
            };
        }
    });
}
