//! Right-docked Studio Inspector: Presets card, 3-way Objects/Settings/Printer switcher, and searchable settings.

use egui::{Align, CornerRadius, Margin, Popup, RichText, Stroke, StrokeKind, pos2, vec2};
use layercraft_config::{
    GcodeFlavor, HeightRangeModifier, InfillPattern, IroningType, LayerZEvent, ModifierOverrides,
    ModifierRole, PrintSequence, ProfileKind, SeamPosition, SupportStyle, SurfacePattern,
    ThumbnailSize, WallGenerator, WallOrder, ZEventAction,
};
use layercraft_engine::{PrinterAuth, PrinterProtocol};
use layercraft_mesh::{ModifierPrimitive, ModifierTransform};
use layercraft_slice::{IssueCategory, IssueSeverity, PrintabilityRisk};

use crate::LayerCraftApp;
use crate::icons::{Icon, paint_icon};
use crate::theme::ThemePalette;
use crate::ui_kit::{
    badge, card, icon_button, inspector_section, secondary_button, segmented_control, setting_row,
    small_icon_button, toggle_switch, unit_drag, xy_row, xyz_row,
};

/// Settings and Plate Outliner inspector tabs.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectorGroup {
    Objects,
    Settings,
    Printer,
}

impl SidebarTab {
    const SETTINGS_TABS: [(Self, &str); 5] = [
        (Self::Quality, "Quality"),
        (Self::Strength, "Strength"),
        (Self::Speed, "Speed"),
        (Self::Support, "Support"),
        (Self::Finish, "Finish"),
    ];

    fn group(self) -> InspectorGroup {
        match self {
            Self::Plate => InspectorGroup::Objects,
            Self::Quality | Self::Strength | Self::Speed | Self::Support | Self::Finish => {
                InspectorGroup::Settings
            }
            Self::Machine => InspectorGroup::Printer,
        }
    }

    pub fn is_settings_tab(self) -> bool {
        self.group() == InspectorGroup::Settings
    }
}

/// Draw the collapsible right inspector dock containing presets, group switcher, and active view.
pub fn draw_sidebar(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    if !app.sidebar_open {
        return;
    }

    if app.sidebar_tab.is_settings_tab() {
        app.last_settings_tab = app.sidebar_tab;
    }

    let resp = egui::Panel::right("layercraft_inspector")
        .default_size(332.0)
        .size_range(296.0..=440.0)
        .frame(
            egui::Frame::NONE
                .fill(p.bg_panel)
                .inner_margin(Margin::symmetric(12, 10)),
        )
        .show(ui, |ui| {
            draw_preset_card(app, ui, p);
            ui.add_space(8.0);

            let mut group = app.sidebar_tab.group();
            if segmented_control(
                ui,
                p,
                "inspector_group_switch",
                &mut group,
                &[
                    (InspectorGroup::Objects, "Objects"),
                    (InspectorGroup::Settings, "Settings"),
                    (InspectorGroup::Printer, "Printer"),
                ],
            ) {
                app.sidebar_tab = match group {
                    InspectorGroup::Objects => SidebarTab::Plate,
                    InspectorGroup::Settings => {
                        if app.last_settings_tab.is_settings_tab() {
                            app.last_settings_tab
                        } else {
                            SidebarTab::Quality
                        }
                    }
                    InspectorGroup::Printer => SidebarTab::Machine,
                };
            }
            ui.add_space(8.0);

            egui::ScrollArea::vertical()
                .id_salt("layercraft_inspector_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    match app.sidebar_tab {
                        SidebarTab::Plate => draw_plate_tab(app, ui, p),
                        SidebarTab::Quality
                        | SidebarTab::Strength
                        | SidebarTab::Speed
                        | SidebarTab::Support
                        | SidebarTab::Finish => draw_settings_area(app, ui, p),
                        SidebarTab::Machine => draw_machine_tab(app, ui, p),
                    }
                    ui.add_space(10.0);
                });
        })
        .response;

    let r = resp.rect;
    ui.painter().line_segment(
        [pos2(r.left(), r.top()), pos2(r.left(), r.bottom())],
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

    let deletables = [
        (
            ProfileKind::Machine,
            "Printer",
            app.selected_machine_profile.clone(),
        ),
        (
            ProfileKind::Filament,
            "Filament",
            app.selected_filament_profile.clone(),
        ),
        (
            ProfileKind::Process,
            "Process",
            app.selected_process_profile.clone(),
        ),
    ];
    let custom_selected: Vec<(ProfileKind, &str, String)> = deletables
        .into_iter()
        .filter_map(|(kind, kind_lbl, opt)| {
            let name = opt?;
            if app.session.profiles.get(kind, &name).is_some()
                && !app.session.profiles.is_builtin(kind, &name)
            {
                Some((kind, kind_lbl, name))
            } else {
                None
            }
        })
        .collect();

    card(ui, p, None, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("PRESETS")
                    .size(10.5)
                    .strong()
                    .color(p.text_muted),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let more_resp = icon_button(
                    ui,
                    p,
                    Icon::MoreDots,
                    "Save, delete, or import presets",
                    false,
                );
                Popup::menu(&more_resp).show(|ui| {
                    ui.set_min_width(210.0);
                    ui.label(
                        RichText::new("Save Custom Preset")
                            .size(10.5)
                            .strong()
                            .color(p.text_muted),
                    );
                    if ui.button("Save Printer Preset...").clicked() {
                        app.open_save_preset_dialog(ProfileKind::Machine);
                        ui.close();
                    }
                    if ui.button("Save Filament Preset...").clicked() {
                        app.open_save_preset_dialog(ProfileKind::Filament);
                        ui.close();
                    }
                    if ui.button("Save Process Preset...").clicked() {
                        app.open_save_preset_dialog(ProfileKind::Process);
                        ui.close();
                    }
                    if !custom_selected.is_empty() {
                        ui.separator();
                        ui.label(
                            RichText::new("Delete Custom Preset")
                                .size(10.5)
                                .strong()
                                .color(p.text_muted),
                        );
                        for (kind, kind_lbl, name) in custom_selected {
                            if ui.button(format!("Delete {kind_lbl}: {name}")).clicked() {
                                app.delete_user_preset_action(kind, &name);
                                ui.close();
                            }
                        }
                    }
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
                });

                if app.profile_warning_count > 0 {
                    badge(
                        ui,
                        p.accent_muted,
                        p.warning,
                        &format!("{} warn", app.profile_warning_count),
                    );
                }
            });
        });
        ui.add_space(4.0);

        let combo_w = (ui.available_width() - 80.0).clamp(120.0, 210.0);

        setting_row(ui, p, "Printer", |ui| {
            let m_text = app
                .selected_machine_profile
                .as_deref()
                .unwrap_or("(Default)");
            egui::ComboBox::from_id_salt("prof_machine")
                .width(combo_w)
                .truncate()
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
                        let label = if app
                            .session
                            .profiles
                            .is_builtin(ProfileKind::Machine, &item.name)
                        {
                            item.name.clone()
                        } else {
                            format!("{} [user]", item.name)
                        };
                        if ui.selectable_label(sel, label).clicked() {
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
                .width(combo_w)
                .truncate()
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
                        let label = if app
                            .session
                            .profiles
                            .is_builtin(ProfileKind::Filament, &item.name)
                        {
                            item.name.clone()
                        } else {
                            format!("{} [user]", item.name)
                        };
                        if ui.selectable_label(sel, label).clicked() {
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
                .width(combo_w)
                .truncate()
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
                        let label = if app
                            .session
                            .profiles
                            .is_builtin(ProfileKind::Process, &item.name)
                        {
                            item.name.clone()
                        } else {
                            format!("{} [user]", item.name)
                        };
                        if ui.selectable_label(sel, label).clicked() {
                            app.selected_process_profile = Some(item.name.clone());
                            apply_requested = true;
                        }
                    }
                });
        });
    });

    if apply_requested {
        app.apply_selected_profiles();
    }
}

fn draw_settings_area(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    // Live filter search input
    egui::Frame::NONE
        .fill(p.bg_input)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (s_rect, _) = ui.allocate_exact_size(vec2(13.0, 13.0), egui::Sense::hover());
                paint_icon(ui.painter(), s_rect, Icon::Search, p.text_muted);

                let text_w = (ui.available_width() - 26.0).max(80.0);
                ui.add(
                    egui::TextEdit::singleline(&mut app.settings_search)
                        .desired_width(text_w)
                        .frame(egui::Frame::NONE)
                        .hint_text("Filter settings (layer, infill, support, speed...)"),
                );
                if !app.settings_search.is_empty() && ui.small_button("×").clicked() {
                    app.settings_search.clear();
                }
            });
        });

    ui.add_space(7.0);

    let needle = app.settings_search.trim().to_ascii_lowercase();
    if needle.is_empty() {
        // Full-word pill navigation (no cryptic abbreviations)
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(5.0, 5.0);
            for (tab, label) in SidebarTab::SETTINGS_TABS {
                let active = app.sidebar_tab == tab;
                let fill = if active { p.accent_muted } else { p.bg_card };
                let stroke = Stroke::new(1.0, if active { p.accent } else { p.border_subtle });
                let fg = if active { p.accent } else { p.text_secondary };
                let galley = ui.painter().layout_no_wrap(
                    label.to_owned(),
                    egui::FontId::new(11.5, egui::FontFamily::Proportional),
                    fg,
                );
                let (pill_rect, pill_resp) = ui
                    .allocate_exact_size(vec2(galley.size().x + 16.0, 23.0), egui::Sense::click());
                if ui.is_rect_visible(pill_rect) {
                    ui.painter().rect(
                        pill_rect,
                        CornerRadius::same(11),
                        if !active && pill_resp.hovered() {
                            p.bg_hover
                        } else {
                            fill
                        },
                        stroke,
                        StrokeKind::Inside,
                    );
                    ui.painter().galley(
                        pos2(
                            pill_rect.center().x - galley.size().x * 0.5,
                            pill_rect.center().y - galley.size().y * 0.5,
                        ),
                        galley,
                        fg,
                    );
                }
                if pill_resp.clicked() {
                    app.sidebar_tab = tab;
                    app.last_settings_tab = tab;
                }
            }
        });

        ui.add_space(7.0);
        let active_tab = app.sidebar_tab;
        render_settings_cards_for(app, ui, p, |meta| meta.tab == active_tab);
    } else {
        ui.label(
            RichText::new(format!("Matching sections for \"{needle}\""))
                .size(11.0)
                .color(p.text_muted),
        );
        ui.add_space(4.0);
        let support_style = app.session.config.process.support.style;
        let matched =
            render_settings_cards_for(app, ui, p, |meta| meta.matches(&needle, support_style));
        if matched == 0 {
            card(ui, p, None, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("No matching settings found")
                            .size(12.5)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.label(
                        RichText::new(
                            "Try terms like layer, wall, infill, support, speed, ironing, or brim.",
                        )
                        .size(11.5)
                        .color(p.text_muted),
                    );
                    ui.add_space(4.0);
                    if secondary_button(ui, p, None, "Clear Filter").clicked() {
                        app.settings_search.clear();
                    }
                });
            });
        }
    }
}

fn draw_plate_tab(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    // 0. Build Plates Card
    let plates = app.session.list_plates();
    let active_plate_idx = app.session.active_plate_index();
    let active_plate_id = app.session.active_plate_id();
    let active_plate_name = app.session.active_plate_name().to_owned();
    if app.plate_rename_buf.0 != active_plate_id {
        app.plate_rename_buf = (active_plate_id, active_plate_name.clone());
    }

    let mut select_plate_idx: Option<usize> = None;
    let mut remove_plate_idx: Option<usize> = None;
    let mut add_plate_clicked = false;
    let mut commit_rename: Option<String> = None;

    let plate_count_badge = format!("{}", plates.len());
    inspector_section(
        ui,
        p,
        "sec_build_plates",
        "Build Plates",
        Some(plate_count_badge.as_str()),
        plates.len() > 1,
        |ui| {
            for plate in &plates {
                ui.horizontal(|ui| {
                    let row_bg = if plate.active {
                        p.accent_muted
                    } else {
                        egui::Color32::TRANSPARENT
                    };
                    let row_stroke = if plate.active {
                        Stroke::new(1.0, p.accent)
                    } else {
                        Stroke::new(1.0, p.border_subtle)
                    };
                    let row_resp = egui::Frame::new()
                        .fill(row_bg)
                        .stroke(row_stroke)
                        .corner_radius(CornerRadius::same(5))
                        .inner_margin(Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.set_width((ui.available_width() - 28.0).max(80.0));
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("{}. {}", plate.index + 1, plate.name))
                                        .size(12.0)
                                        .color(if plate.active {
                                            p.text_primary
                                        } else {
                                            p.text_secondary
                                        }),
                                );
                                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                    badge(
                                        ui,
                                        p.bg_input,
                                        p.text_muted,
                                        &format!("{} obj", plate.object_count),
                                    );
                                    if plate.sliced {
                                        badge(ui, p.accent_muted, p.success, "sliced");
                                    }
                                });
                            });
                        })
                        .response
                        .interact(egui::Sense::click());
                    if row_resp.clicked() && !plate.active {
                        select_plate_idx = Some(plate.index);
                    }

                    ui.add_enabled_ui(plates.len() > 1, |ui| {
                        if small_icon_button(ui, p, Icon::Trash, "Remove build plate").clicked() {
                            remove_plate_idx = Some(plate.index);
                        }
                    });
                });
            }

            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Name:").size(11.5).color(p.text_secondary));
                let edit_resp = ui.add(
                    egui::TextEdit::singleline(&mut app.plate_rename_buf.1)
                        .desired_width((ui.available_width() - 102.0).max(60.0)),
                );
                if !edit_resp.has_focus()
                    && app.plate_rename_buf.0 == active_plate_id
                    && app.plate_rename_buf.1 != active_plate_name
                    && !edit_resp.lost_focus()
                {
                    app.plate_rename_buf.1 = active_plate_name.clone();
                }
                let enter = edit_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter || (edit_resp.lost_focus() && app.plate_rename_buf.1 != active_plate_name)
                {
                    commit_rename = Some(app.plate_rename_buf.1.clone());
                }
                if secondary_button(ui, p, Some(Icon::AddPlus), "Plate")
                    .on_hover_text("Add a new empty build plate")
                    .clicked()
                {
                    add_plate_clicked = true;
                }
            });
        },
    );

    if let Some(new_name) = commit_rename {
        let trimmed = new_name.trim();
        if !trimmed.is_empty() && trimmed != active_plate_name {
            app.rename_plate(active_plate_idx, trimmed);
        }
        app.plate_rename_buf = (
            app.session.active_plate_id(),
            app.session.active_plate_name().to_owned(),
        );
    }
    if add_plate_clicked {
        app.add_plate(None);
    } else if let Some(idx) = remove_plate_idx {
        app.remove_plate(idx);
    } else if let Some(idx) = select_plate_idx {
        app.select_plate(idx);
    }

    struct ObjectOutlinerItem {
        id: u64,
        name: String,
        transform: layercraft_mesh::ObjectTransform,
        overrides: ModifierOverrides,
    }
    let entries: Vec<ObjectOutlinerItem> = app
        .session
        .objects
        .iter()
        .map(|o| ObjectOutlinerItem {
            id: o.id,
            name: o.name.clone(),
            transform: o.transform,
            overrides: o.overrides.clone(),
        })
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
        SetObjectOverrides {
            id: u64,
            overrides: ModifierOverrides,
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

    // 1. Outliner Section
    ui.add_space(6.0);
    let outliner_badge = format!("{}", entries.len() + mods_snapshot.len());
    inspector_section(
        ui,
        p,
        "sec_plate_outliner",
        "Build Plate Outliner",
        Some(outliner_badge.as_str()),
        true,
        |ui| {
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
                for item in &entries {
                    let id = item.id;
                    let selected = app.selected_object_id == Some(id);
                    let is_oob = app
                        .session
                        .plate_layout
                        .out_of_bounds_object_ids
                        .contains(&id);
                    let is_overlap = app.session.plate_layout.colliding_object_ids.contains(&id);
                    ui.horizontal(|ui| {
                        let dot_r = ui
                            .allocate_exact_size(vec2(10.0, 16.0), egui::Sense::hover())
                            .0;
                        let dot_col = if is_oob {
                            p.danger
                        } else if is_overlap {
                            p.warning
                        } else if selected {
                            p.accent
                        } else {
                            p.axis_z
                        };
                        ui.painter().circle_filled(dot_r.center(), 3.5, dot_col);
                        if ui
                            .selectable_label(selected, format!("#{id} {}", item.name))
                            .clicked()
                        {
                            app.selected_object_id = Some(id);
                            app.selected_modifier_id = None;
                            app.rebuild_scene();
                        }
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if small_icon_button(ui, p, Icon::Trash, "Remove object").clicked() {
                                action = Some(PlateAction::RemoveObject(id));
                            }
                            if is_overlap {
                                badge(ui, p.bg_panel, p.warning, "Overlap");
                            }
                            if is_oob {
                                badge(ui, p.bg_panel, p.danger, "OOB");
                            }
                            if !item.overrides.is_empty() {
                                badge(ui, p.bg_panel, p.accent, "Custom");
                            }
                        });
                    });
                }
                if app.session.plate_layout.has_issues {
                    let oob_n = app.session.plate_layout.out_of_bounds_object_ids.len();
                    let ov_n = app.session.plate_layout.colliding_pairs.len();
                    let mut parts = Vec::new();
                    if oob_n > 0 {
                        parts.push(format!("{oob_n} out of bounds"));
                    }
                    if ov_n > 0 {
                        parts.push(format!("{ov_n} overlap(s)"));
                    }
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        badge(ui, p.bg_panel, p.danger, "Plate Check");
                        ui.label(
                            RichText::new(parts.join(", "))
                                .size(11.0)
                                .color(if oob_n > 0 { p.danger } else { p.warning }),
                        );
                        if secondary_button(ui, p, Some(Icon::ArrangeGrid), "Auto-Arrange")
                            .on_hover_text("Pack all objects cleanly inside bed bounds")
                            .clicked()
                        {
                            app.auto_arrange_plate();
                        }
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
                    if ui.small_button("+ Cutout").clicked() {
                        action = Some(PlateAction::AddModifier(
                            ModifierPrimitive::Box {
                                width_mm: 12.0,
                                depth_mm: 12.0,
                                height_mm: 10.0,
                            },
                            ModifierRole::NegativePart,
                        ));
                    }
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
                        ModifierRole::NegativePart => egui::Color32::from_rgb(217, 56, 184),
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
                            app.rebuild_scene();
                        }
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if small_icon_button(ui, p, Icon::Trash, "Remove modifier").clicked() {
                                action = Some(PlateAction::RemoveModifier(*id));
                            }
                        });
                    });
                }
            }
        },
    );

    // 2. Selected Object Transform & Overrides Inspector
    if let Some(sel_id) = app.selected_object_id
        && let Some(item) = entries.iter().find(|item| item.id == sel_id)
    {
        ui.add_space(6.0);
        let title = format!("Transform — #{sel_id} {}", item.name);
        inspector_section(ui, p, "sec_selected_transform", &title, None, true, |ui| {
            let mut pos = item.transform.position_xy_mm;
            let mut rot = item.transform.rotation_deg;
            let [mut uni_scale, _, _] = item.transform.scale;
            let mut xf_changed = false;
            let bw = app.session.config.machine.bed_width_mm;
            let bd = app.session.config.machine.bed_depth_mm;

            xf_changed |= xy_row(
                ui,
                p,
                "Position (mm)",
                &mut pos,
                [1.0..=bw, 1.0..=bd],
                0.5,
                "",
            );
            ui.add_space(4.0);
            xf_changed |= xyz_row(ui, p, "Rotation (°)", &mut rot, -180.0..=180.0, 1.0, "°");
            ui.add_space(4.0);
            setting_row(ui, p, "Uniform scale", |ui| {
                xf_changed |= unit_drag(ui, &mut uni_scale, 0.1..=5.0, 0.02, "×").changed();
            });

            if xf_changed && action.is_none() {
                action = Some(PlateAction::TransformObject {
                    id: sel_id,
                    pos,
                    rot,
                    scale: [uni_scale, uni_scale, uni_scale],
                });
            }

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
            ui.label(
                RichText::new("PER-OBJECT OVERRIDES")
                    .size(10.0)
                    .strong()
                    .color(p.text_muted),
            );
            let mut ov = item.overrides.clone();
            let mut ov_changed = false;
            draw_overrides_editor(ui, p, &mut ov, "obj", &mut ov_changed);
            if ov_changed && action.is_none() {
                action = Some(PlateAction::SetObjectOverrides {
                    id: sel_id,
                    overrides: ov,
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
        inspector_section(ui, p, "sec_selected_modifier", &title, None, true, |ui| {
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
                draw_overrides_editor(ui, p, &mut ov, "mod", &mut changed);
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
        Some(PlateAction::SetObjectOverrides { id, overrides }) => {
            app.set_object_overrides(id, overrides);
        }
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

    // 4. Print Sequence Section
    ui.add_space(6.0);
    inspector_section(
        ui,
        p,
        "sec_print_sequence",
        "Print Sequence",
        None,
        app.session.config.process.print_sequence == PrintSequence::ByObject,
        |ui| {
            let proc_cfg = &mut app.session.config.process;
            setting_row(ui, p, "Print sequence", |ui| {
                egui::ComboBox::from_id_salt("print_sequence_combo")
                    .selected_text(proc_cfg.print_sequence.label())
                    .show_ui(ui, |ui| {
                        for seq in PrintSequence::ALL {
                            ui.selectable_value(&mut proc_cfg.print_sequence, seq, seq.label());
                        }
                    });
            });
            if proc_cfg.print_sequence == PrintSequence::ByObject {
                setting_row(ui, p, "Clearance radius", |ui| {
                    unit_drag(
                        ui,
                        &mut proc_cfg.extruder_clearance_radius_mm,
                        5.0..=150.0,
                        0.5,
                        "mm",
                    );
                });
                setting_row(ui, p, "Clearance height", |ui| {
                    unit_drag(
                        ui,
                        &mut proc_cfg.extruder_clearance_height_mm,
                        1.0..=250.0,
                        0.5,
                        "mm",
                    );
                });
            }
        },
    );

    // 5. Height Ranges Section
    ui.add_space(6.0);
    let hr_count = app.session.config.process.height_ranges.len();
    let hr_badge = (hr_count > 0).then(|| hr_count.to_string());
    inspector_section(
        ui,
        p,
        "sec_height_ranges",
        "Height Range Bands",
        hr_badge.as_deref(),
        hr_count > 0,
        |ui| {
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
        },
    );

    // 6. Pause / Color Change / Custom Z Events Section
    ui.add_space(6.0);
    let ze_count = app.session.config.process.z_events.len();
    let ze_badge = (ze_count > 0).then(|| ze_count.to_string());
    inspector_section(
        ui,
        p,
        "sec_z_events",
        "Pause / Color Change (Z Events)",
        ze_badge.as_deref(),
        ze_count > 0,
        |ui| {
            let max_bed_z = app.session.config.machine.bed_height_mm;
            let proc_cfg = &mut app.session.config.process;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Insert M601/PAUSE, M600, or custom G-code at Z")
                        .size(11.0)
                        .color(p.text_muted),
                );
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("+ Add Z Event").clicked() {
                        let next_z = proc_cfg
                            .z_events
                            .last()
                            .map(|e| e.z_mm + 5.0)
                            .unwrap_or(5.0)
                            .min(max_bed_z);
                        proc_cfg.z_events.push(LayerZEvent {
                            z_mm: next_z,
                            action: ZEventAction::Pause,
                            message: None,
                            custom_gcode: None,
                        });
                    }
                });
            });

            let mut rm_ev_idx: Option<usize> = None;
            for (ei, ev) in proc_cfg.z_events.iter_mut().enumerate() {
                ui.push_id(("z_ev", ei), |ui| {
                    ui.add_space(3.0);
                    ui.horizontal(|ui| {
                        ui.label("Z:");
                        ui.add(
                            egui::DragValue::new(&mut ev.z_mm)
                                .speed(0.2)
                                .range(0.05..=max_bed_z)
                                .suffix(" mm"),
                        );
                        egui::ComboBox::from_id_salt(("z_ev_action", ei))
                            .width(120.0)
                            .selected_text(ev.action.label())
                            .show_ui(ui, |ui| {
                                for act in ZEventAction::ALL {
                                    ui.selectable_value(&mut ev.action, act, act.label());
                                }
                            });
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if small_icon_button(ui, p, Icon::Trash, "Remove Z event").clicked() {
                                rm_ev_idx = Some(ei);
                            }
                        });
                    });
                    if ev.action == ZEventAction::CustomGcode {
                        let mut buf = ev.custom_gcode.clone().unwrap_or_default();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut buf)
                                .hint_text("e.g. M118 Z={layer_z}")
                                .desired_width(f32::INFINITY),
                        );
                        if resp.changed() {
                            ev.custom_gcode = if buf.trim().is_empty() {
                                None
                            } else {
                                Some(buf)
                            };
                        }
                    } else {
                        let mut msg_buf = ev.message.clone().unwrap_or_default();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut msg_buf)
                                .hint_text("Optional M117 display message")
                                .desired_width(f32::INFINITY),
                        );
                        if resp.changed() {
                            ev.message = if msg_buf.trim().is_empty() {
                                None
                            } else {
                                Some(msg_buf)
                            };
                        }
                    }
                });
            }
            if let Some(ei) = rm_ev_idx
                && ei < proc_cfg.z_events.len()
            {
                proc_cfg.z_events.remove(ei);
            }
        },
    );

    // 5. Mesh Summary Card
    if let Some(insp) = app.session.plate_inspection.clone() {
        ui.add_space(6.0);
        let mut repair_clicked = false;
        inspector_section(
            ui,
            p,
            "sec_mesh_summary",
            "Mesh Summary",
            if insp.needs_repair {
                Some("Repair")
            } else {
                None
            },
            insp.needs_repair,
            |ui| {
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
                    if !insp.needs_repair {
                        badge(ui, p.bg_panel, p.success, "Watertight");
                    } else if insp.inverted_shells > 0 {
                        badge(
                            ui,
                            p.bg_panel,
                            p.warning,
                            &format!("{} inverted shell(s)", insp.inverted_shells),
                        );
                    } else if insp.inconsistent_winding_edges > 0 {
                        badge(
                            ui,
                            p.bg_panel,
                            p.warning,
                            &format!("{} flipped edge(s)", insp.inconsistent_winding_edges),
                        );
                    } else if insp.boundary_edges > 0 {
                        badge(
                            ui,
                            p.bg_panel,
                            p.warning,
                            &format!("{} open edge(s)", insp.boundary_edges),
                        );
                    } else {
                        badge(
                            ui,
                            p.bg_panel,
                            p.warning,
                            &format!("{} non-manifold", insp.non_manifold_edges),
                        );
                    }
                });
                if insp.needs_repair {
                    let mut details = Vec::new();
                    if insp.boundary_edges > 0 {
                        details.push(format!("{} open", insp.boundary_edges));
                    }
                    if insp.inconsistent_winding_edges > 0 {
                        details.push(format!("{} flipped", insp.inconsistent_winding_edges));
                    }
                    if insp.inverted_shells > 0 {
                        details.push(format!("{} inverted", insp.inverted_shells));
                    }
                    if insp.duplicate_triangles > 0 {
                        details.push(format!("{} dup", insp.duplicate_triangles));
                    }
                    if insp.degenerate_triangles > 0 {
                        details.push(format!("{} deg", insp.degenerate_triangles));
                    }
                    if insp.non_manifold_edges > insp.boundary_edges {
                        details.push(format!(
                            "{} over-shared",
                            insp.non_manifold_edges - insp.boundary_edges
                        ));
                    }
                    if !details.is_empty() {
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(format!("Defects: {}", details.join(", ")))
                                .size(11.0)
                                .color(p.text_muted),
                        );
                    }
                    ui.add_space(3.0);
                    if secondary_button(ui, p, None, "Repair Mesh")
                    .on_hover_text(
                        "Weld close vertices, remove duplicates, orient shells outward, and fill holes",
                    )
                    .clicked()
                {
                    repair_clicked = true;
                }
                }
            },
        );
        if repair_clicked {
            app.repair_plate_mesh(None);
        }

        ui.add_space(6.0);
        let mut analyze_clicked = false;
        let mut fix_tree_supports = false;
        let mut fix_brim = false;
        let mut fix_repair = false;
        let mut fix_arrange = false;
        let mesh_needs_repair = insp.needs_repair;
        let layout_has_issues = app.session.plate_layout.has_issues;
        let can_arrange = app.session.objects.len() > 1;
        let support_enabled = app.session.config.process.support.enabled;
        let brim_width = app.session.config.process.brim_width_mm;

        let show_dfm_open = app.printability_report.is_some();
        inspector_section(
            ui,
            p,
            "sec_printability_dfm",
            "Printability / DFM",
            None,
            show_dfm_open,
            |ui| {
                if let Some(report) = &app.printability_report {
                    let (risk_col, risk_lbl) = match report.overall_risk {
                        PrintabilityRisk::Low => (p.success, "Low Risk"),
                        PrintabilityRisk::Medium => (p.warning, "Moderate Risk"),
                        PrintabilityRisk::High => (p.danger, "High Risk"),
                    };
                    ui.horizontal(|ui| {
                        badge(ui, p.bg_panel, risk_col, risk_lbl);
                        let scope = if report.sliced {
                            format!("{} layers", report.layer_count)
                        } else {
                            "pre-slice".to_owned()
                        };
                        ui.label(
                            RichText::new(format!("{} issue(s) ({scope})", report.issues.len()))
                                .size(11.0)
                                .color(p.text_muted),
                        );
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("Re-analyze").clicked() {
                                analyze_clicked = true;
                            }
                        });
                    });

                    if report.issues.is_empty() {
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new("No printability warnings detected.")
                                .size(11.0)
                                .color(p.text_muted),
                        );
                    } else {
                        ui.add_space(3.0);
                        for issue in report.issues.iter().take(4) {
                            let (sev_col, sev_lbl) = match issue.severity {
                                IssueSeverity::Critical => (p.danger, "Crit"),
                                IssueSeverity::Warning => (p.warning, "Warn"),
                                IssueSeverity::Info => (p.accent, "Info"),
                            };
                            ui.horizontal_wrapped(|ui| {
                                badge(ui, p.bg_panel, sev_col, sev_lbl);
                                ui.label(
                                    RichText::new(&issue.summary)
                                        .size(11.0)
                                        .color(p.text_secondary),
                                )
                                .on_hover_text(&issue.suggestion);
                            });
                        }
                        if report.issues.len() > 4 {
                            ui.label(
                                RichText::new(format!(
                                    "+{} more issue(s)",
                                    report.issues.len() - 4
                                ))
                                .size(10.5)
                                .color(p.text_muted),
                            );
                        }
                    }

                    let needs_supports_fix = !support_enabled
                        && report.issues.iter().any(|i| {
                            matches!(
                                i.category,
                                IssueCategory::FloatingIsland | IssueCategory::Overhang
                            )
                        });
                    let needs_brim_fix = brim_width < 5.0
                        && report
                            .issues
                            .iter()
                            .any(|i| i.category == IssueCategory::BedAdhesion);

                    if needs_supports_fix
                        || needs_brim_fix
                        || mesh_needs_repair
                        || (layout_has_issues && can_arrange)
                    {
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            if needs_supports_fix
                                && secondary_button(ui, p, None, "Enable Tree Supports")
                                    .on_hover_text("Turn on organic/tree supports (undoable)")
                                    .clicked()
                            {
                                fix_tree_supports = true;
                            }
                            if needs_brim_fix
                                && secondary_button(ui, p, None, "Add 5mm Brim")
                                    .on_hover_text("Set brim width to 5 mm for bed adhesion")
                                    .clicked()
                            {
                                fix_brim = true;
                            }
                            if mesh_needs_repair
                                && secondary_button(ui, p, None, "Repair Mesh").clicked()
                            {
                                fix_repair = true;
                            }
                            if layout_has_issues
                                && can_arrange
                                && secondary_button(ui, p, Some(Icon::ArrangeGrid), "Auto-Arrange")
                                    .clicked()
                            {
                                fix_arrange = true;
                            }
                        });
                    }
                } else {
                    ui.label(
                        RichText::new(
                            "Inspect overhangs, floating islands, thin walls, and bed adhesion.",
                        )
                        .size(11.0)
                        .color(p.text_muted),
                    );
                    ui.add_space(3.0);
                    if secondary_button(ui, p, None, "Analyze Printability").clicked() {
                        analyze_clicked = true;
                    }
                }
            },
        );

        if analyze_clicked {
            app.analyze_printability_action();
        } else if fix_tree_supports {
            app.apply_quick_fix_tree_supports();
        } else if fix_brim {
            app.apply_quick_fix_brim();
        } else if fix_repair {
            app.repair_plate_mesh(None);
        } else if fix_arrange {
            app.auto_arrange_plate();
        }
    }
}

struct SettingsCardMeta {
    tab: SidebarTab,
    id: &'static str,
    title: &'static str,
    keywords: &'static str,
    requires_tree_support: bool,
    body: fn(&mut LayerCraftApp, &mut egui::Ui, &ThemePalette),
}

impl SettingsCardMeta {
    fn matches(&self, needle: &str, support_style: SupportStyle) -> bool {
        if self.requires_tree_support && support_style != SupportStyle::Tree {
            return false;
        }
        self.title.to_ascii_lowercase().contains(needle) || self.keywords.contains(needle)
    }
}

const SETTINGS_CARDS: &[SettingsCardMeta] = &[
    SettingsCardMeta {
        tab: SidebarTab::Quality,
        id: "q_layers_shells",
        title: "Layer Heights & Shells",
        keywords: "layer height first top bottom min thick shell quality",
        requires_tree_support: false,
        body: sec_quality_layer_shells,
    },
    SettingsCardMeta {
        tab: SidebarTab::Quality,
        id: "q_adaptive",
        title: "Adaptive Layer Height",
        keywords: "adaptive variable layer height min max quality bias smooth radius step ratio",
        requires_tree_support: false,
        body: sec_quality_adaptive,
    },
    SettingsCardMeta {
        tab: SidebarTab::Quality,
        id: "q_accuracy",
        title: "Dimensional Accuracy",
        keywords: "elephant foot xy contour hole compensation dimensional accuracy tolerance",
        requires_tree_support: false,
        body: sec_quality_accuracy,
    },
    SettingsCardMeta {
        tab: SidebarTab::Quality,
        id: "q_walls_seam",
        title: "Perimeter Walls & Seam",
        keywords: "wall order generator classic arachne bead width feature seam position aligned nearest random outer inner perimeter",
        requires_tree_support: false,
        body: sec_quality_walls_seam,
    },
    SettingsCardMeta {
        tab: SidebarTab::Strength,
        id: "s_walls_solid",
        title: "Walls & Solid Shells",
        keywords: "wall loops top bottom shell layers gap fill monotonic top bottom surface pattern strength",
        requires_tree_support: false,
        body: sec_strength_walls_solid,
    },
    SettingsCardMeta {
        tab: SidebarTab::Strength,
        id: "s_sparse_infill",
        title: "Sparse Infill",
        keywords: "sparse infill density pattern gyroid grid honeycomb cubic overlap strength",
        requires_tree_support: false,
        body: sec_strength_sparse_infill,
    },
    SettingsCardMeta {
        tab: SidebarTab::Strength,
        id: "s_bed_adhesion",
        title: "Bed Adhesion",
        keywords: "bed adhesion skirt loops brim width",
        requires_tree_support: false,
        body: sec_strength_bed_adhesion,
    },
    SettingsCardMeta {
        tab: SidebarTab::Speed,
        id: "sp_print",
        title: "Print Speeds",
        keywords: "print speed outer inner wall sparse infill first layer",
        requires_tree_support: false,
        body: sec_speed_print,
    },
    SettingsCardMeta {
        tab: SidebarTab::Speed,
        id: "sp_bridges_overhangs",
        title: "Bridges & Overhangs",
        keywords: "detect bridge flow ratio fan overhang angle threshold speed",
        requires_tree_support: false,
        body: sec_speed_bridges_overhangs,
    },
    SettingsCardMeta {
        tab: SidebarTab::Speed,
        id: "sp_cooling_flow",
        title: "Cooling & Volumetric Flow",
        keywords: "cooling max volumetric flow fan min max disable first layers slowdown layer time speed",
        requires_tree_support: false,
        body: sec_speed_cooling_flow,
    },
    SettingsCardMeta {
        tab: SidebarTab::Speed,
        id: "sp_acceleration",
        title: "Acceleration (0 = Firmware)",
        keywords: "acceleration default outer wall first layer travel firmware",
        requires_tree_support: false,
        body: sec_speed_acceleration,
    },
    SettingsCardMeta {
        tab: SidebarTab::Support,
        id: "sup_auto",
        title: "Automatic Supports",
        keywords: "support enable style grid snug tree build plate threshold angle top z gap xy clearance spacing interface layers",
        requires_tree_support: false,
        body: sec_support_auto,
    },
    SettingsCardMeta {
        tab: SidebarTab::Support,
        id: "sup_tree",
        title: "Tree Branching",
        keywords: "tree support branch angle tip trunk diameter distance wall loops flare organic",
        requires_tree_support: true,
        body: sec_support_tree,
    },
    SettingsCardMeta {
        tab: SidebarTab::Finish,
        id: "fin_ironing",
        title: "Top-Surface Ironing",
        keywords: "ironing mode flow line spacing perimeter inset speed finish",
        requires_tree_support: false,
        body: sec_finish_ironing,
    },
    SettingsCardMeta {
        tab: SidebarTab::Finish,
        id: "fin_scarf",
        title: "Scarf Seams",
        keywords: "scarf seam enable length steps start height finish",
        requires_tree_support: false,
        body: sec_finish_scarf,
    },
    SettingsCardMeta {
        tab: SidebarTab::Finish,
        id: "fin_arc",
        title: "Arc Fitting (G2 / G3)",
        keywords: "arc fitting g2 g3 tolerance gcode finish",
        requires_tree_support: false,
        body: sec_finish_arc,
    },
];

fn render_settings_cards_for(
    app: &mut LayerCraftApp,
    ui: &mut egui::Ui,
    p: &ThemePalette,
    predicate: impl Fn(&SettingsCardMeta) -> bool,
) -> usize {
    let mut rendered = 0usize;
    for meta in SETTINGS_CARDS {
        if meta.requires_tree_support
            && app.session.config.process.support.style != SupportStyle::Tree
        {
            continue;
        }
        if !predicate(meta) {
            continue;
        }
        if rendered > 0 {
            ui.add_space(6.0);
        }
        ui.push_id(meta.id, |ui| {
            card(ui, p, Some(meta.title), |ui| (meta.body)(app, ui, p));
        });
        rendered += 1;
    }
    rendered
}

fn sec_quality_layer_shells(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
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
}

fn sec_quality_adaptive(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let max_nozzle_h = (app.session.config.machine.nozzle_diameter_mm * 1.25).max(0.04);
    let al = &mut app.session.config.process.adaptive_layer_height;
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
}

fn sec_quality_accuracy(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
    setting_row(ui, p, "Elephant foot comp", |ui| {
        unit_drag(
            ui,
            &mut proc_cfg.elephant_foot_compensation_mm,
            0.0..=2.0,
            0.02,
            "mm",
        );
    });
    setting_row(ui, p, "XY contour comp", |ui| {
        unit_drag(
            ui,
            &mut proc_cfg.xy_contour_compensation_mm,
            -2.0..=2.0,
            0.02,
            "mm",
        );
    });
    setting_row(ui, p, "XY hole comp", |ui| {
        unit_drag(
            ui,
            &mut proc_cfg.xy_hole_compensation_mm,
            -2.0..=2.0,
            0.02,
            "mm",
        );
    });
}

fn sec_quality_walls_seam(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
    setting_row(ui, p, "Wall order", |ui| {
        egui::ComboBox::from_id_salt("wall_order")
            .selected_text(proc_cfg.wall_order.label())
            .show_ui(ui, |ui| {
                for wo in WallOrder::ALL {
                    ui.selectable_value(&mut proc_cfg.wall_order, wo, wo.label());
                }
            });
    });
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
}

fn draw_overrides_editor(
    ui: &mut egui::Ui,
    p: &ThemePalette,
    ov: &mut ModifierOverrides,
    id_prefix: &str,
    changed: &mut bool,
) {
    let mut has_dens = ov.sparse_infill_density.is_some();
    setting_row(ui, p, "Override infill %", |ui| {
        if let Some(d) = &mut ov.sparse_infill_density {
            let mut pct = *d * 100.0;
            if unit_drag(ui, &mut pct, 0.0..=100.0, 1.0, "%").changed() {
                *d = (pct / 100.0).clamp(0.0, 1.0);
                *changed = true;
            }
        }
        if toggle_switch(ui, p, &mut has_dens).changed() {
            ov.sparse_infill_density = if has_dens { Some(0.50) } else { None };
            *changed = true;
        }
    });

    let mut has_pat = ov.sparse_infill_pattern.is_some();
    setting_row(ui, p, "Override pattern", |ui| {
        if let Some(pat) = &mut ov.sparse_infill_pattern {
            egui::ComboBox::from_id_salt((id_prefix, "pat_combo"))
                .width(100.0)
                .selected_text(pat.label())
                .show_ui(ui, |ui| {
                    for candidate in InfillPattern::ALL {
                        *changed |= ui
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
            *changed = true;
        }
    });

    let mut has_walls = ov.wall_loops.is_some();
    setting_row(ui, p, "Override walls", |ui| {
        if let Some(w) = &mut ov.wall_loops {
            *changed |= unit_drag(ui, w, 0..=20, 0.25, "").changed();
        }
        if toggle_switch(ui, p, &mut has_walls).changed() {
            ov.wall_loops = if has_walls { Some(4) } else { None };
            *changed = true;
        }
    });

    let mut has_top = ov.top_shell_layers.is_some();
    setting_row(ui, p, "Override top shells", |ui| {
        if let Some(t) = &mut ov.top_shell_layers {
            *changed |= unit_drag(ui, t, 0..=20, 0.25, "").changed();
        }
        if toggle_switch(ui, p, &mut has_top).changed() {
            ov.top_shell_layers = if has_top { Some(5) } else { None };
            *changed = true;
        }
    });

    let mut has_bot = ov.bottom_shell_layers.is_some();
    setting_row(ui, p, "Override bottom shells", |ui| {
        if let Some(b) = &mut ov.bottom_shell_layers {
            *changed |= unit_drag(ui, b, 0..=20, 0.25, "").changed();
        }
        if toggle_switch(ui, p, &mut has_bot).changed() {
            ov.bottom_shell_layers = if has_bot { Some(4) } else { None };
            *changed = true;
        }
    });

    let mut has_iron = ov.ironing_mode.is_some();
    setting_row(ui, p, "Override ironing", |ui| {
        if let Some(im) = &mut ov.ironing_mode {
            egui::ComboBox::from_id_salt((id_prefix, "iron_combo"))
                .width(110.0)
                .selected_text(im.label())
                .show_ui(ui, |ui| {
                    for mode in IroningType::ALL {
                        *changed |= ui.selectable_value(im, mode, mode.label()).changed();
                    }
                });
        }
        if toggle_switch(ui, p, &mut has_iron).changed() {
            ov.ironing_mode = if has_iron {
                Some(IroningType::TopmostSurface)
            } else {
                None
            };
            *changed = true;
        }
    });
}

fn sec_strength_walls_solid(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
    setting_row(ui, p, "Wall loops", |ui| {
        unit_drag(ui, &mut proc_cfg.wall_loops, 0..=12, 0.25, "");
    });
    setting_row(ui, p, "Top shell layers", |ui| {
        unit_drag(ui, &mut proc_cfg.top_shell_layers, 0..=20, 0.25, "");
    });
    setting_row(ui, p, "Bottom shell layers", |ui| {
        unit_drag(ui, &mut proc_cfg.bottom_shell_layers, 0..=20, 0.25, "");
    });
    setting_row(ui, p, "Gap fill (Classic)", |ui| {
        toggle_switch(ui, p, &mut proc_cfg.gap_fill_enabled);
    });
    if proc_cfg.gap_fill_enabled {
        let mut gf_speed = proc_cfg.gap_fill_speed_mm_s.unwrap_or(0.0);
        setting_row(ui, p, "Gap fill speed (0=auto)", |ui| {
            if unit_drag(ui, &mut gf_speed, 0.0..=300.0, 2.0, "mm/s").changed() {
                proc_cfg.gap_fill_speed_mm_s = if gf_speed <= 0.0 {
                    None
                } else {
                    Some(gf_speed.max(5.0))
                };
            }
        });
        setting_row(ui, p, "Min gap width", |ui| {
            unit_drag(
                ui,
                &mut proc_cfg.gap_fill_min_width_mm,
                0.05..=0.80,
                0.01,
                "mm",
            );
        });
    }
    setting_row(ui, p, "Monotonic top/bottom", |ui| {
        toggle_switch(ui, p, &mut proc_cfg.monotonic_solid_infill);
    });
    setting_row(ui, p, "Top surface pattern", |ui| {
        egui::ComboBox::from_id_salt("top_surface_pat")
            .selected_text(proc_cfg.top_surface_pattern.label())
            .show_ui(ui, |ui| {
                for pat in SurfacePattern::ALL {
                    if ui
                        .selectable_value(&mut proc_cfg.top_surface_pattern, pat, pat.label())
                        .changed()
                    {
                        proc_cfg.monotonic_solid_infill = proc_cfg.top_surface_pattern
                            == SurfacePattern::Monotonic
                            || proc_cfg.bottom_surface_pattern == SurfacePattern::Monotonic;
                    }
                }
            });
    });
    setting_row(ui, p, "Bottom surface pattern", |ui| {
        egui::ComboBox::from_id_salt("bot_surface_pat")
            .selected_text(proc_cfg.bottom_surface_pattern.label())
            .show_ui(ui, |ui| {
                for pat in SurfacePattern::ALL {
                    if ui
                        .selectable_value(&mut proc_cfg.bottom_surface_pattern, pat, pat.label())
                        .changed()
                    {
                        proc_cfg.monotonic_solid_infill = proc_cfg.top_surface_pattern
                            == SurfacePattern::Monotonic
                            || proc_cfg.bottom_surface_pattern == SurfacePattern::Monotonic;
                    }
                }
            });
    });
}

fn sec_strength_sparse_infill(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
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
    let mut overlap_pct = proc_cfg.infill_wall_overlap_ratio * 100.0;
    setting_row(ui, p, "Infill–wall overlap", |ui| {
        if unit_drag(ui, &mut overlap_pct, 0.0..=50.0, 1.0, "%").changed() {
            proc_cfg.infill_wall_overlap_ratio = (overlap_pct / 100.0).clamp(0.0, 0.5);
        }
    });
}

fn sec_strength_bed_adhesion(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
    setting_row(ui, p, "Skirt loops", |ui| {
        unit_drag(ui, &mut proc_cfg.skirt_loops, 0..=10, 0.25, "");
    });
    setting_row(ui, p, "Brim width", |ui| {
        unit_drag(ui, &mut proc_cfg.brim_width_mm, 0.0..=20.0, 0.2, "mm");
    });
}

fn sec_speed_print(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
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
}

fn sec_speed_bridges_overhangs(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
    let f = &mut app.session.config.filament;
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
    for (label, slot) in [
        (
            "Overhang 10–25% (0=off)",
            &mut proc_cfg.overhang_1_4_speed_mm_s,
        ),
        (
            "Overhang 25–50% (0=off)",
            &mut proc_cfg.overhang_2_4_speed_mm_s,
        ),
        (
            "Overhang 75–100% (0=off)",
            &mut proc_cfg.overhang_4_4_speed_mm_s,
        ),
    ] {
        let mut spd = slot.unwrap_or(0.0);
        setting_row(ui, p, label, |ui| {
            if unit_drag(ui, &mut spd, 0.0..=300.0, 2.0, "mm/s").changed() {
                *slot = if spd <= 0.0 { None } else { Some(spd.max(5.0)) };
            }
        });
    }
}

fn sec_speed_cooling_flow(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let f = &mut app.session.config.filament;
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
}

fn sec_speed_acceleration(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
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
}

fn sec_support_auto(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let sup = &mut app.session.config.process.support;
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
}

fn sec_support_tree(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let tr = &mut app.session.config.process.support.tree;
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
}

fn sec_finish_ironing(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
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
}

fn sec_finish_scarf(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
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
}

fn sec_finish_arc(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let proc_cfg = &mut app.session.config.process;
    setting_row(ui, p, "Enable G2/G3 arcs", |ui| {
        toggle_switch(ui, p, &mut proc_cfg.arc_fitting);
    });
    setting_row(ui, p, "Arc tolerance", |ui| {
        unit_drag(ui, &mut proc_cfg.arc_tolerance_mm, 0.002..=0.5, 0.005, "mm");
    });
}

fn draw_machine_tab(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let mut bed_changed = false;

    inspector_section(
        ui,
        p,
        "sec_printer_hw",
        "Printer Hardware",
        None,
        true,
        |ui| {
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
                bed_changed |=
                    unit_drag(ui, &mut m.bed_width_mm, 50.0..=600.0, 1.0, "mm").changed();
            });
            setting_row(ui, p, "Bed depth (Y)", |ui| {
                bed_changed |=
                    unit_drag(ui, &mut m.bed_depth_mm, 50.0..=600.0, 1.0, "mm").changed();
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
            setting_row(ui, p, "Binary G-code (.bgcode)", |ui| {
                toggle_switch(ui, p, &mut m.binary_gcode);
            });
            setting_row(ui, p, "Label objects (Cancel-Object)", |ui| {
                toggle_switch(ui, p, &mut m.label_objects);
            });
            let mut thumb_enabled = !m.thumbnails.is_empty();
            setting_row(ui, p, "Embed G-code thumbnails", |ui| {
                if toggle_switch(ui, p, &mut thumb_enabled).changed() {
                    if thumb_enabled {
                        if m.flavor == GcodeFlavor::MarlinBuddy {
                            m.thumbnails = vec![
                                ThumbnailSize::qoi(16, 16),
                                ThumbnailSize::qoi(313, 173),
                                ThumbnailSize::qoi(440, 240),
                                ThumbnailSize::png(160, 120),
                            ];
                        } else {
                            m.thumbnails = vec![ThumbnailSize::png(160, 120)];
                        }
                    } else {
                        m.thumbnails.clear();
                    }
                }
            });
        },
    );

    if bed_changed {
        app.rebuild_scene();
    }

    ui.add_space(6.0);
    inspector_section(
        ui,
        p,
        "sec_filament_temps",
        "Filament & Temperatures",
        None,
        true,
        |ui| {
            let f = &mut app.session.config.filament;
            setting_row(ui, p, "Nozzle temp", |ui| {
                unit_drag(ui, &mut f.nozzle_temp_c, 150..=340, 1.0, "°C");
            });
            setting_row(ui, p, "Bed temp", |ui| {
                unit_drag(ui, &mut f.bed_temp_c, 0..=130, 1.0, "°C");
            });
            setting_row(ui, p, "Flow ratio", |ui| {
                unit_drag(ui, &mut f.flow_ratio, 0.5..=1.5, 0.01, "×");
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
        },
    );

    ui.add_space(6.0);
    inspector_section(
        ui,
        p,
        "sec_retraction_travel",
        "Travel, Retraction & Combing",
        None,
        false,
        |ui| {
            let cfg = &mut app.session.config;
            setting_row(ui, p, "Retract length", |ui| {
                unit_drag(
                    ui,
                    &mut cfg.machine.retraction_length_mm,
                    0.0..=15.0,
                    0.05,
                    "mm",
                );
            });
            setting_row(ui, p, "Z-hop", |ui| {
                unit_drag(ui, &mut cfg.machine.z_hop_mm, 0.0..=5.0, 0.05, "mm");
            });
            setting_row(ui, p, "Wipe on retract", |ui| {
                toggle_switch(ui, p, &mut cfg.machine.wipe_on_retract);
            });
            setting_row(ui, p, "Wipe distance", |ui| {
                unit_drag(ui, &mut cfg.machine.wipe_distance_mm, 0.0..=10.0, 0.1, "mm");
            });
            setting_row(ui, p, "Reduce infill retractions", |ui| {
                toggle_switch(ui, p, &mut cfg.process.reduce_infill_retraction);
            });
            setting_row(ui, p, "Avoid crossing walls", |ui| {
                toggle_switch(ui, p, &mut cfg.process.avoid_crossing_perimeters);
            });
            setting_row(ui, p, "Max detour (0=∞)", |ui| {
                unit_drag(
                    ui,
                    &mut cfg.process.avoid_crossing_perimeters_max_detour_mm,
                    0.0..=500.0,
                    1.0,
                    "mm",
                );
            });
        },
    );

    ui.add_space(6.0);
    let has_custom_macros = app.session.config.machine.start_gcode.is_some()
        || app.session.config.machine.end_gcode.is_some();
    inspector_section(
        ui,
        p,
        "sec_custom_gcode",
        "Custom Start / End G-code",
        if has_custom_macros {
            Some("Custom")
        } else {
            None
        },
        has_custom_macros,
        |ui| {
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
        },
    );

    ui.add_space(6.0);
    draw_direct_printer_card(app, ui, p);
}

fn draw_direct_printer_card(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let mut picked_saved: Option<String> = None;
    let mut new_clicked = false;
    let mut save_clicked = false;
    let mut delete_clicked = false;
    #[cfg(not(target_arch = "wasm32"))]
    let mut test_clicked = false;

    card(ui, p, Some("Direct Printer Upload"), |ui| {
        ui.horizontal(|ui| {
            let sel_label = app
                .session
                .printers
                .iter()
                .find(|pr| pr.id == app.printer_draft.id)
                .map(|pr| pr.name.as_str())
                .unwrap_or("(Unsaved printer)");
            egui::ComboBox::from_id_salt("saved_printer_sel")
                .width(136.0)
                .selected_text(sel_label)
                .show_ui(ui, |ui| {
                    for pr in &app.session.printers {
                        if ui
                            .selectable_label(pr.id == app.printer_draft.id, &pr.name)
                            .clicked()
                        {
                            picked_saved = Some(pr.id.clone());
                        }
                    }
                });
            if secondary_button(ui, p, Some(Icon::AddPlus), "New").clicked() {
                new_clicked = true;
            }
            let can_delete = app
                .session
                .printers
                .iter()
                .any(|pr| pr.id == app.printer_draft.id);
            ui.add_enabled_ui(can_delete, |ui| {
                if small_icon_button(ui, p, Icon::Trash, "Delete saved printer").clicked() {
                    delete_clicked = true;
                }
            });
        });

        setting_row(ui, p, "Name", |ui| {
            ui.add(egui::TextEdit::singleline(&mut app.printer_draft.name).desired_width(140.0));
        });

        setting_row(ui, p, "Protocol", |ui| {
            egui::ComboBox::from_id_salt("printer_proto_sel")
                .width(140.0)
                .selected_text(app.printer_draft.protocol.label())
                .show_ui(ui, |ui| {
                    for proto in PrinterProtocol::ALL {
                        ui.selectable_value(&mut app.printer_draft.protocol, proto, proto.label());
                    }
                });
        });

        setting_row(ui, p, "Host URL", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.printer_draft.base_url)
                    .hint_text("http://192.168.1.50")
                    .desired_width(140.0),
            );
        });

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum AuthKindChoice {
            Digest,
            ApiKey,
            Bearer,
            None,
        }
        let mut choice = match &app.printer_draft.auth {
            PrinterAuth::Digest { .. } => AuthKindChoice::Digest,
            PrinterAuth::ApiKey { .. } => AuthKindChoice::ApiKey,
            PrinterAuth::Bearer { .. } => AuthKindChoice::Bearer,
            PrinterAuth::None => AuthKindChoice::None,
        };
        let choice_label = match choice {
            AuthKindChoice::Digest => "HTTP Digest",
            AuthKindChoice::ApiKey => "API Key",
            AuthKindChoice::Bearer => "Bearer Token",
            AuthKindChoice::None => "None",
        };
        let prev_choice = choice;
        setting_row(ui, p, "Auth", |ui| {
            egui::ComboBox::from_id_salt("printer_auth_sel")
                .width(140.0)
                .selected_text(choice_label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut choice, AuthKindChoice::Digest, "HTTP Digest");
                    ui.selectable_value(&mut choice, AuthKindChoice::ApiKey, "API Key");
                    ui.selectable_value(&mut choice, AuthKindChoice::Bearer, "Bearer Token");
                    ui.selectable_value(&mut choice, AuthKindChoice::None, "None");
                });
        });
        if choice != prev_choice {
            app.printer_draft.auth = match choice {
                AuthKindChoice::Digest => PrinterAuth::Digest {
                    username: "maker".to_owned(),
                    password: String::new(),
                },
                AuthKindChoice::ApiKey => PrinterAuth::ApiKey {
                    api_key: String::new(),
                },
                AuthKindChoice::Bearer => PrinterAuth::Bearer {
                    token: String::new(),
                },
                AuthKindChoice::None => PrinterAuth::None,
            };
        }

        match &mut app.printer_draft.auth {
            PrinterAuth::Digest { username, password } => {
                setting_row(ui, p, "Username", |ui| {
                    ui.add(egui::TextEdit::singleline(username).desired_width(140.0));
                });
                setting_row(ui, p, "Password", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(password)
                            .password(true)
                            .desired_width(140.0),
                    );
                });
            }
            PrinterAuth::ApiKey { api_key } => {
                setting_row(ui, p, "API key", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(api_key)
                            .password(true)
                            .desired_width(140.0),
                    );
                });
            }
            PrinterAuth::Bearer { token } => {
                setting_row(ui, p, "Bearer token", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(token)
                            .password(true)
                            .desired_width(140.0),
                    );
                });
            }
            PrinterAuth::None => {}
        }

        let mut storage_buf = app.printer_draft.storage.clone().unwrap_or_default();
        let default_storage = app.printer_draft.protocol.default_storage();
        setting_row(ui, p, "Storage", |ui| {
            if ui
                .add(
                    egui::TextEdit::singleline(&mut storage_buf)
                        .hint_text(default_storage)
                        .desired_width(140.0),
                )
                .changed()
            {
                let trimmed = storage_buf.trim();
                app.printer_draft.storage = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                };
            }
        });

        if app.printer_draft.protocol == PrinterProtocol::PrusaConnectLocal {
            let mut dev_buf = app.printer_draft.device_id.clone().unwrap_or_default();
            setting_row(ui, p, "Device ID", |ui| {
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut dev_buf)
                            .hint_text("required for auto-print")
                            .desired_width(140.0),
                    )
                    .changed()
                {
                    let trimmed = dev_buf.trim();
                    app.printer_draft.device_id = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_owned())
                    };
                }
            });
        }

        setting_row(ui, p, "Prefer .bgcode", |ui| {
            toggle_switch(ui, p, &mut app.printer_draft.prefer_bgcode);
        });
        setting_row(ui, p, "Allow self-signed", |ui| {
            toggle_switch(ui, p, &mut app.printer_draft.accept_invalid_certs);
        });

        ui.horizontal(|ui| {
            if secondary_button(ui, p, Some(Icon::SaveProject), "Save Printer").clicked() {
                save_clicked = true;
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                let busy = app.is_printer_busy();
                let ready = !busy && !app.printer_draft.base_url.trim().is_empty();
                ui.add_enabled_ui(ready, |ui| {
                    let lbl = if busy {
                        "Testing..."
                    } else {
                        "Test Connection"
                    };
                    if secondary_button(ui, p, Some(Icon::SendPrinter), lbl).clicked() {
                        test_clicked = true;
                    }
                });
            }
        });

        #[cfg(target_arch = "wasm32")]
        ui.label(
            RichText::new(
                "Direct LAN HTTP upload requires desktop Studio due to browser CORS; `.bgcode` download works in-browser.",
            )
            .size(10.5)
            .color(p.text_muted),
        );

        #[cfg(not(target_arch = "wasm32"))]
        ui.label(
            RichText::new(format!(
                "Credentials: {}",
                layercraft_engine::printer_credential_storage_mode()
            ))
            .size(10.0)
            .color(p.text_muted),
        );

        #[cfg(not(target_arch = "wasm32"))]
        if let Some(rep) = &app.printer_status_report {
            ui.separator();
            let state = rep.state.as_deref().unwrap_or("Reachable");
            let fw = rep.firmware.as_deref().unwrap_or("-");
            let model = rep.model.as_deref().unwrap_or("-");
            ui.label(
                RichText::new(format!("State: {state} · Model: {model} · FW: {fw}"))
                    .size(11.0)
                    .color(p.success),
            );
            if rep.nozzle_temp_c.is_some() || rep.bed_temp_c.is_some() {
                let noz = rep
                    .nozzle_temp_c
                    .map(|t| format!("{t:.1}°C"))
                    .unwrap_or_else(|| "-".to_owned());
                let bed = rep
                    .bed_temp_c
                    .map(|t| format!("{t:.1}°C"))
                    .unwrap_or_else(|| "-".to_owned());
                ui.label(
                    RichText::new(format!("Temps: Nozzle {noz} · Bed {bed}"))
                        .size(11.0)
                        .color(p.text_secondary),
                );
            }
        }
    });

    if let Some(id) = picked_saved {
        app.select_saved_printer(&id);
    } else if new_clicked {
        app.start_new_printer_draft();
    } else if save_clicked {
        app.save_printer_draft();
    } else if delete_clicked {
        app.delete_selected_printer();
    }
    #[cfg(not(target_arch = "wasm32"))]
    if test_clicked {
        app.start_printer_probe_action(ui.ctx());
    }
}
