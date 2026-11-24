//! Floating 3D viewport overlays: Orientation Gizmo, Preview Summary/Legend, Vertical Layer Scrubber, Bottom Pill, Drag-Drop Overlay.

use egui::{
    Align, Align2, Area, Color32, Context, CornerRadius, Id, Margin, Order, Popup, Rect, RichText,
    Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use layercraft_config::ModifierRole;
use layercraft_mesh::{ModifierPrimitive, SampleShape};
use layercraft_slice::ExtrusionRole;
use layercraft_viewport::{
    CameraPreset, ColorMode, MoveKind, color_ramp_sample, compute_color_mode_range, role_color,
};

use crate::icons::Icon;
use crate::theme::ThemePalette;
use crate::ui_kit::badge;
use crate::ui_kit::{floating_rail_frame, hud_frame, icon_button, rail_tool_button, ribbon_button};
use crate::{LayerCraftApp, PendingDiscardAction, ViewMode, ViewportToolMode};

/// Draw all floating HUD elements inside `viewport_rect` without permanently docking panels over the canvas.
pub fn draw_viewport_hud(
    app: &mut LayerCraftApp,
    ctx: &Context,
    viewport_rect: Rect,
    p: &ThemePalette,
) {
    if app.view_mode == ViewMode::Home
        || viewport_rect.width() < 80.0
        || viewport_rect.height() < 80.0
    {
        return;
    }

    draw_orientation_gizmo(app, ctx, viewport_rect, p);

    match app.view_mode {
        ViewMode::Home => {}
        ViewMode::Prepare => {
            draw_prepare_tool_rail(app, ctx, viewport_rect, p);
            draw_bottom_view_pill(app, ctx, viewport_rect, p);
        }
        ViewMode::Preview => {
            if app.show_legend_hud {
                draw_preview_summary_hud(app, ctx, viewport_rect, p);
            }
            if app.show_gcode_viewer {
                draw_gcode_viewer_hud(app, ctx, viewport_rect, p);
            }
            draw_vertical_layer_scrubber(app, ctx, viewport_rect, p);
            draw_preview_bottom_dock(app, ctx, viewport_rect, p);
        }
    }

    if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
        draw_drag_drop_overlay(ctx, viewport_rect, p);
    }
}

fn draw_prepare_tool_rail(
    app: &mut LayerCraftApp,
    ctx: &Context,
    viewport_rect: Rect,
    p: &ThemePalette,
) {
    Area::new(Id::unique("layercraft_hud_prepare_rail"))
        .order(Order::Foreground)
        .pivot(Align2::LEFT_TOP)
        .fixed_pos(viewport_rect.left_top() + vec2(12.0, 12.0))
        .constrain_to(viewport_rect)
        .show(ctx, |ui| {
            floating_rail_frame(p).show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 4.0);

                    if rail_tool_button(
                        ui,
                        p,
                        Icon::SelectCursor,
                        app.tool_mode == ViewportToolMode::Select,
                        "Select & inspect objects on build plate",
                    )
                    .clicked()
                    {
                        app.tool_mode = ViewportToolMode::Select;
                    }

                    if rail_tool_button(
                        ui,
                        p,
                        Icon::AddPlus,
                        false,
                        "Add STL or 3MF model to plate",
                    )
                    .clicked()
                    {
                        app.open_file_dialog(true);
                    }

                    let can_arrange = app.session.objects.len() > 1;
                    ui.add_enabled_ui(can_arrange, |ui| {
                        if rail_tool_button(
                            ui,
                            p,
                            Icon::ArrangeGrid,
                            false,
                            "Auto-arrange objects onto build plate",
                        )
                        .clicked()
                        {
                            app.auto_arrange_plate();
                        }
                    });

                    let mod_resp = rail_tool_button(
                        ui,
                        p,
                        Icon::BoxShape,
                        false,
                        "Add modifier volume, support blocker/enforcer, or cutout",
                    );
                    Popup::menu(&mod_resp).show(|ui| {
                        ui.set_min_width(185.0);
                        ui.label(
                            RichText::new("Add Volume")
                                .size(10.5)
                                .strong()
                                .color(p.text_muted),
                        );
                        ui.separator();
                        if ui.button("+ Box Modifier").clicked() {
                            app.add_modifier_primitive(
                                ModifierPrimitive::Box {
                                    width_mm: 12.0,
                                    depth_mm: 12.0,
                                    height_mm: 10.0,
                                },
                                ModifierRole::Modifier,
                            );
                            ui.close();
                        }
                        if ui.button("+ Cylinder Modifier").clicked() {
                            app.add_modifier_primitive(
                                ModifierPrimitive::Cylinder {
                                    radius_mm: 6.0,
                                    height_mm: 10.0,
                                    segments: 32,
                                },
                                ModifierRole::Modifier,
                            );
                            ui.close();
                        }
                        if ui.button("+ Support Blocker").clicked() {
                            app.add_modifier_primitive(
                                ModifierPrimitive::Box {
                                    width_mm: 12.0,
                                    depth_mm: 12.0,
                                    height_mm: 10.0,
                                },
                                ModifierRole::SupportBlocker,
                            );
                            ui.close();
                        }
                        if ui.button("+ Support Enforcer").clicked() {
                            app.add_modifier_primitive(
                                ModifierPrimitive::Box {
                                    width_mm: 12.0,
                                    depth_mm: 12.0,
                                    height_mm: 10.0,
                                },
                                ModifierRole::SupportEnforcer,
                            );
                            ui.close();
                        }
                        if ui.button("+ Negative Cutout").clicked() {
                            app.add_modifier_primitive(
                                ModifierPrimitive::Box {
                                    width_mm: 12.0,
                                    depth_mm: 12.0,
                                    height_mm: 10.0,
                                },
                                ModifierRole::NegativePart,
                            );
                            ui.close();
                        }
                    });

                    let sample_resp = rail_tool_button(
                        ui,
                        p,
                        Icon::SampleCube,
                        false,
                        "Load or add built-in calibration sample",
                    );
                    Popup::menu(&sample_resp).show(|ui| {
                        ui.set_min_width(195.0);
                        ui.label(
                            RichText::new("Calibration Shapes")
                                .size(10.5)
                                .strong()
                                .color(p.text_muted),
                        );
                        ui.separator();
                        for shape in SampleShape::ALL {
                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_label(app.selected_sample == shape, shape.label())
                                    .clicked()
                                {
                                    app.request_discard_or_run(PendingDiscardAction::LoadSample(
                                        shape,
                                    ));
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

                    let (div_r, _) = ui.allocate_exact_size(vec2(32.0, 5.0), Sense::hover());
                    ui.painter().line_segment(
                        [
                            pos2(div_r.left() + 4.0, div_r.center().y),
                            pos2(div_r.right() - 4.0, div_r.center().y),
                        ],
                        Stroke::new(1.0, p.border_subtle),
                    );

                    ui.add_enabled_ui(!app.session.objects.is_empty(), |ui| {
                        if rail_tool_button(
                            ui,
                            p,
                            Icon::Trash,
                            false,
                            "Clear objects and modifiers from active plate",
                        )
                        .clicked()
                        {
                            app.request_discard_or_run(PendingDiscardAction::ClearPlate);
                        }
                    });
                });
            });
        });
}

fn draw_orientation_gizmo(
    app: &mut LayerCraftApp,
    ctx: &Context,
    viewport_rect: Rect,
    p: &ThemePalette,
) {
    Area::new(Id::unique("layercraft_hud_gizmo"))
        .order(Order::Foreground)
        .pivot(Align2::RIGHT_TOP)
        .fixed_pos(viewport_rect.right_top() + vec2(-12.0, 12.0))
        .constrain_to(viewport_rect)
        .show(ctx, |ui| {
            hud_frame(p).inner_margin(Margin::same(8)).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let dial_size = 76.0;
                    let (dial_rect, dial_resp) =
                        ui.allocate_exact_size(vec2(dial_size, dial_size), Sense::drag());
                    if dial_resp.dragged() {
                        let d = dial_resp.drag_delta();
                        app.camera.orbit(d.x, d.y);
                    }

                    let center = dial_rect.center();
                    let rad = dial_size * 0.44;
                    ui.painter()
                        .circle(center, rad, p.bg_panel, Stroke::new(1.0, p.border_subtle));

                    // Match OrbitCamera::view_matrix basis vectors cleanly in 2D screen space.
                    let cp = app.camera.pitch_rad.cos();
                    let sp = app.camera.pitch_rad.sin();
                    let cy = app.camera.yaw_rad.cos();
                    let sy = app.camera.yaw_rad.sin();
                    let f = [-cp * cy, -cp * sy, -sp];
                    let s = [-sy, cy, 0.0_f32];
                    let u = cross3(s, f);

                    let arm = rad * 0.72;
                    let axes: [(&str, [f32; 3], Color32, Option<CameraPreset>); 3] = [
                        ("X", [1.0, 0.0, 0.0], p.axis_x, Some(CameraPreset::Right)),
                        ("Y", [0.0, 1.0, 0.0], p.axis_y, Some(CameraPreset::Front)),
                        ("Z", [0.0, 0.0, 1.0], p.axis_z, Some(CameraPreset::Top)),
                    ];

                    let mut sorted_idx = [0usize, 1usize, 2usize];
                    sorted_idx.sort_by(|&a, &b| {
                        let da = axes.get(a).map(|it| -dot3(it.1, f)).unwrap_or(0.0);
                        let db = axes.get(b).map(|it| -dot3(it.1, f)).unwrap_or(0.0);
                        da.total_cmp(&db)
                    });

                    let mut snap_preset: Option<CameraPreset> = None;
                    for idx in sorted_idx {
                        let Some(&(label, dir, col, preset)) = axes.get(idx) else {
                            continue;
                        };
                        let sx = dot3(dir, s);
                        let sy_proj = dot3(dir, u);
                        let depth = -dot3(dir, f);

                        // Subtle negative stub
                        let neg_tip = center + vec2(-sx, sy_proj) * (arm * 0.45);
                        ui.painter().line_segment(
                            [center, neg_tip],
                            Stroke::new(1.2, col.gamma_multiply(0.35)),
                        );

                        // Positive axis arm + clickable label badge
                        let pos_tip = center + vec2(sx, -sy_proj) * arm;
                        let alpha_mul = if depth >= -0.05 { 1.0 } else { 0.55 };
                        let line_col = col.gamma_multiply(alpha_mul);
                        ui.painter()
                            .line_segment([center, pos_tip], Stroke::new(2.0, line_col));

                        let badge_r = 8.5;
                        let tip_rect = Rect::from_center_size(pos_tip, Vec2::splat(badge_r * 2.0));
                        let hit = ui.interact(
                            tip_rect,
                            ui.scope_id().with(("axis_tip", label)),
                            Sense::click(),
                        );
                        let fill = if hit.hovered() { col } else { line_col };
                        ui.painter().circle_filled(pos_tip, badge_r, fill);
                        ui.painter().text(
                            pos_tip,
                            Align2::CENTER_CENTER,
                            label,
                            egui::FontId::proportional(10.0),
                            Color32::WHITE,
                        );
                        if hit.clicked() {
                            snap_preset = preset;
                        }
                    }

                    if let Some(preset) = snap_preset {
                        app.frame_camera(preset);
                    }

                    ui.add_space(5.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (lbl, preset) in [
                            ("Iso", CameraPreset::Isometric),
                            ("Top", CameraPreset::Top),
                            ("Frt", CameraPreset::Front),
                            ("Rgt", CameraPreset::Right),
                        ] {
                            let (br, bresp) =
                                ui.allocate_exact_size(vec2(22.0, 18.0), Sense::click());
                            if ui.is_rect_visible(br) {
                                let bg = if bresp.hovered() {
                                    p.bg_hover
                                } else {
                                    p.bg_card
                                };
                                ui.painter().rect(
                                    br,
                                    CornerRadius::same(4),
                                    bg,
                                    Stroke::new(1.0, p.border_subtle),
                                    StrokeKind::Inside,
                                );
                                ui.painter().text(
                                    br.center(),
                                    Align2::CENTER_CENTER,
                                    lbl,
                                    egui::FontId::proportional(9.5),
                                    p.text_secondary,
                                );
                            }
                            if bresp.clicked() {
                                app.frame_camera(preset);
                            }
                        }
                    });
                });
            });
        });
}

fn draw_preview_summary_hud(
    app: &mut LayerCraftApp,
    ctx: &Context,
    viewport_rect: Rect,
    p: &ThemePalette,
) {
    let Some(stats) = app.session.last_gcode.as_ref().map(|g| g.stats.clone()) else {
        return;
    };

    Area::new(Id::unique("layercraft_hud_preview_summary"))
        .order(Order::Foreground)
        .pivot(Align2::LEFT_TOP)
        .fixed_pos(viewport_rect.left_top() + vec2(12.0, 12.0))
        .constrain_to(viewport_rect)
        .show(ctx, |ui| {
            hud_frame(p).show(ui, |ui| {
                ui.set_max_width(248.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("PRINT ESTIMATE")
                            .size(10.5)
                            .strong()
                            .color(p.text_muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if let Some(ms) = app.last_slice_ms {
                            badge(ui, p.bg_card, p.text_secondary, &format!("{ms:.0} ms"));
                        }
                    });
                });

                ui.add_space(4.0);
                let mins = (stats.estimated_time_s / 60.0).floor() as u64;
                let secs = (stats.estimated_time_s % 60.0).round() as u64;
                egui::Grid::new("hud_summary_grid")
                    .num_columns(2)
                    .spacing([14.0, 4.0])
                    .show(ui, |ui| {
                        metric_cell(ui, p, "Time", &format!("{mins}m {secs:02}s"));
                        metric_cell(
                            ui,
                            p,
                            "Filament",
                            &format!(
                                "{:.1} g · {:.1} m",
                                stats.filament_weight_g,
                                stats.filament_length_mm / 1000.0
                            ),
                        );
                        ui.end_row();
                        metric_cell(ui, p, "Layers", &format!("{}", stats.layer_count));
                        metric_cell(ui, p, "Height", &format!("{:.2} mm", stats.max_z_mm));
                        ui.end_row();
                    });

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);

                // Integrated Color Mode selector inside summary HUD
                let mut color_changed = false;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Color by").size(10.5).color(p.text_secondary));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        egui::ComboBox::from_id_salt("hud_color_mode")
                            .width(142.0)
                            .selected_text(app.color_mode.label())
                            .show_ui(ui, |ui| {
                                for mode in ColorMode::ALL {
                                    if ui
                                        .selectable_value(&mut app.color_mode, mode, mode.label())
                                        .changed()
                                    {
                                        color_changed = true;
                                    }
                                }
                            });
                    });
                });
                if color_changed {
                    app.rebuild_scene();
                }
                ui.add_space(5.0);

                match app.color_mode {
                    ColorMode::Feature => {
                        let all_roles = [
                            ExtrusionRole::OuterWall,
                            ExtrusionRole::InnerWall,
                            ExtrusionRole::OverhangWall,
                            ExtrusionRole::GapFill,
                            ExtrusionRole::TopSurface,
                            ExtrusionRole::Ironing,
                            ExtrusionRole::BottomSurface,
                            ExtrusionRole::InternalSolidInfill,
                            ExtrusionRole::Bridge,
                            ExtrusionRole::InternalBridge,
                            ExtrusionRole::SparseInfill,
                            ExtrusionRole::Support,
                            ExtrusionRole::SupportInterface,
                            ExtrusionRole::Skirt,
                            ExtrusionRole::Brim,
                        ];
                        let any_positive = all_roles.iter().any(|r| {
                            stats
                                .features
                                .get(r.label())
                                .is_some_and(|fs| fs.volume_mm3 > 0.0)
                        });
                        let show_all = app.show_all_legend_roles || !any_positive;

                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("FEATURE LEGEND")
                                    .size(10.0)
                                    .strong()
                                    .color(p.text_muted),
                            );
                            if any_positive {
                                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                    let toggle_txt = if app.show_all_legend_roles {
                                        "Active only"
                                    } else {
                                        "Show all"
                                    };
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(toggle_txt)
                                                    .size(10.0)
                                                    .color(p.accent),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                    {
                                        app.show_all_legend_roles = !app.show_all_legend_roles;
                                    }
                                });
                            }
                        });
                        ui.add_space(2.0);
                        for role in all_roles {
                            let vol_opt = stats.features.get(role.label()).map(|fs| fs.volume_mm3);
                            if !show_all && !vol_opt.is_some_and(|v| v > 0.0) {
                                continue;
                            }
                            let c = role_color(role);
                            let col = Color32::from_rgb(
                                (c[0] * 255.0) as u8,
                                (c[1] * 255.0) as u8,
                                (c[2] * 255.0) as u8,
                            );
                            ui.horizontal(|ui| {
                                let (sw, _) =
                                    ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                                ui.painter().rect_filled(sw, CornerRadius::same(2), col);
                                ui.label(
                                    RichText::new(role.label()).size(11.0).color(p.text_primary),
                                );
                                if let Some(vol) = vol_opt {
                                    ui.with_layout(
                                        egui::Layout::right_to_left(Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(format!("{vol:.1} mm³"))
                                                    .size(10.5)
                                                    .color(p.text_muted),
                                            );
                                        },
                                    );
                                }
                            });
                        }
                    }
                    ColorMode::Speed
                    | ColorMode::VolumetricFlow
                    | ColorMode::FanSpeed
                    | ColorMode::LineWidth
                    | ColorMode::LayerHeight
                    | ColorMode::Layer => {
                        let caption = match app.color_mode {
                            ColorMode::Speed => "Print speed (slow -> fast)",
                            ColorMode::VolumetricFlow => "Volumetric flow (low -> peak)",
                            ColorMode::FanSpeed => "Fan speed (0% -> 100%)",
                            ColorMode::LineWidth => "Line width (narrow -> wide)",
                            ColorMode::LayerHeight => "Layer height (thin -> thick)",
                            ColorMode::Layer => "Layer height Z (bottom -> top)",
                            ColorMode::Feature => "",
                        };
                        ui.label(RichText::new(caption).size(11.0).color(p.text_secondary));
                        ui.add_space(3.0);
                        let (bar_rect, _) =
                            ui.allocate_exact_size(vec2(190.0, 8.0), Sense::hover());
                        let segs = 20usize;
                        let sw = bar_rect.width() / segs as f32;
                        for i in 0..segs {
                            let t = i as f32 / (segs - 1) as f32;
                            let rgba = color_ramp_sample(app.color_mode, t);
                            let sr = Rect::from_min_max(
                                pos2(bar_rect.left() + i as f32 * sw, bar_rect.top()),
                                pos2(
                                    bar_rect.left() + (i + 1) as f32 * sw + 0.5,
                                    bar_rect.bottom(),
                                ),
                            );
                            ui.painter().rect_filled(
                                sr,
                                CornerRadius::ZERO,
                                Color32::from_rgb(
                                    (rgba[0].clamp(0.0, 1.0) * 255.0) as u8,
                                    (rgba[1].clamp(0.0, 1.0) * 255.0) as u8,
                                    (rgba[2].clamp(0.0, 1.0) * 255.0) as u8,
                                ),
                            );
                        }
                        let (lo, hi) = app
                            .session
                            .sliced
                            .as_ref()
                            .map(|s| {
                                compute_color_mode_range(
                                    s,
                                    app.color_mode,
                                    Some(&app.session.config),
                                )
                            })
                            .unwrap_or((0.0, 1.0));
                        let (lo_label, hi_label) = match app.color_mode {
                            ColorMode::Speed => (format!("{lo:.0}"), format!("{hi:.0} mm/s")),
                            ColorMode::VolumetricFlow => {
                                (format!("{lo:.1}"), format!("{hi:.1} mm³/s"))
                            }
                            ColorMode::FanSpeed => (format!("{lo:.0}%"), format!("{hi:.0}%")),
                            ColorMode::LineWidth => (format!("{lo:.2}"), format!("{hi:.2} mm")),
                            ColorMode::LayerHeight => (format!("{lo:.3}"), format!("{hi:.3} mm")),
                            ColorMode::Layer => ("0.0".to_owned(), format!("{hi:.1} mm")),
                            ColorMode::Feature => (String::new(), String::new()),
                        };
                        ui.horizontal(|ui| {
                            ui.set_width(190.0);
                            ui.label(RichText::new(lo_label).size(10.0).color(p.text_muted));
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(hi_label).size(10.0).color(p.text_muted));
                            });
                        });
                    }
                }

                if app.show_travels {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("TRAVEL MOVES")
                            .size(10.0)
                            .strong()
                            .color(p.text_muted),
                    );
                    ui.add_space(2.0);
                    for kind in [
                        MoveKind::Travel,
                        MoveKind::CombedTravel,
                        MoveKind::ZHopTravel,
                    ] {
                        let c = kind.color();
                        let col = Color32::from_rgb(
                            (c[0] * 255.0) as u8,
                            (c[1] * 255.0) as u8,
                            (c[2] * 255.0) as u8,
                        );
                        ui.horizontal(|ui| {
                            let (sw, _) = ui.allocate_exact_size(vec2(10.0, 3.0), Sense::hover());
                            ui.painter().rect_filled(sw, CornerRadius::same(1), col);
                            ui.label(RichText::new(kind.label()).size(11.0).color(p.text_primary));
                        });
                    }
                }
            });
        });
}

fn metric_cell(ui: &mut Ui, p: &ThemePalette, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(label).size(10.0).color(p.text_muted));
        ui.add(
            egui::Label::new(
                RichText::new(value)
                    .size(12.0)
                    .strong()
                    .color(p.text_primary),
            )
            .extend(),
        );
    });
}

fn scrubber_step_button(
    ui: &mut Ui,
    p: &ThemePalette,
    label: &str,
    tooltip: &str,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 20.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if resp.is_pointer_button_down_on() {
            p.bg_active
        } else if resp.hovered() {
            p.bg_hover
        } else {
            p.bg_card
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(4),
            fill,
            Stroke::new(1.0, p.border_subtle),
            StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(13.0),
            p.text_primary,
        );
    }
    resp.on_hover_text(tooltip)
}

fn draw_vertical_layer_scrubber(
    app: &mut LayerCraftApp,
    ctx: &Context,
    viewport_rect: Rect,
    p: &ThemePalette,
) {
    let total = app
        .session
        .sliced
        .as_ref()
        .map(|s| s.layers.len())
        .unwrap_or(0);
    if total == 0 {
        return;
    }
    if app.max_layer >= total {
        app.max_layer = total - 1;
    }

    let track_h = (viewport_rect.height() * 0.50).clamp(150.0, 380.0);
    let anchor_pos = pos2(
        viewport_rect.right() - 12.0,
        viewport_rect.center().y + 38.0,
    );

    let area_resp = Area::new(Id::unique("layercraft_hud_scrubber"))
        .order(Order::Foreground)
        .pivot(Align2::RIGHT_CENTER)
        .fixed_pos(anchor_pos)
        .constrain_to(viewport_rect)
        .show(ctx, |ui| {
            hud_frame(p)
                .inner_margin(Margin::symmetric(6, 8))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        if scrubber_step_button(ui, p, "+", "Next layer (Up Arrow)").clicked()
                            && app.max_layer + 1 < total
                        {
                            let next = app.max_layer + 1;
                            app.set_active_layer(next);
                        }

                        let (track_rect, track_resp) =
                            ui.allocate_exact_size(vec2(24.0, track_h), Sense::click_and_drag());

                        if track_resp.hovered() {
                            let wheel = ui.input(|i| i.smooth_scroll_delta.y);
                            if wheel > 4.0 && app.max_layer + 1 < total {
                                let next = app.max_layer + 1;
                                app.set_active_layer(next);
                            } else if wheel < -4.0 && app.max_layer > 0 {
                                let prev = app.max_layer - 1;
                                app.set_active_layer(prev);
                            }
                        }

                        if (track_resp.clicked() || track_resp.dragged())
                            && let Some(ptr) = track_resp.interact_pointer_pos()
                        {
                            let frac = ((track_rect.bottom() - ptr.y) / track_rect.height())
                                .clamp(0.0, 1.0);
                            let idx = (frac * (total.saturating_sub(1) as f32)).round() as usize;
                            let clamped = idx.min(total - 1);
                            if clamped != app.max_layer {
                                app.set_active_layer(clamped);
                            }
                        }

                        let frac = if total <= 1 {
                            1.0
                        } else {
                            app.max_layer as f32 / (total - 1) as f32
                        };
                        let rail = Rect::from_center_size(
                            track_rect.center(),
                            vec2(5.0, track_rect.height()),
                        );
                        ui.painter()
                            .rect_filled(rail, CornerRadius::same(3), p.bg_input);

                        let thumb_y = track_rect.bottom() - frac * track_rect.height();
                        let active_rail = Rect::from_min_max(
                            pos2(rail.left(), thumb_y),
                            pos2(rail.right(), rail.bottom()),
                        );
                        ui.painter()
                            .rect_filled(active_rail, CornerRadius::same(3), p.accent);

                        let knob = pos2(track_rect.center().x, thumb_y);
                        ui.painter()
                            .circle(knob, 7.0, p.text_primary, Stroke::new(2.0, p.accent));

                        if scrubber_step_button(ui, p, "–", "Previous layer (Down Arrow)").clicked()
                            && app.max_layer > 0
                        {
                            let prev = app.max_layer - 1;
                            app.set_active_layer(prev);
                        }
                    });
                });
        })
        .response;

    if let Some(sliced) = &app.session.sliced
        && let Some(layer) = sliced.layers.get(app.max_layer)
    {
        let scrubber_rect = area_resp.rect;
        let frac = if total <= 1 {
            1.0
        } else {
            app.max_layer as f32 / (total - 1) as f32
        };
        let callout_y = (scrubber_rect.bottom() - 28.0 - frac * (track_h - 10.0))
            .clamp(scrubber_rect.top() + 26.0, scrubber_rect.bottom() - 26.0);
        let callout_pos = pos2(scrubber_rect.left() - 8.0, callout_y);

        Area::new(Id::unique("layercraft_hud_layer_callout"))
            .order(Order::Foreground)
            .pivot(Align2::RIGHT_CENTER)
            .fixed_pos(callout_pos)
            .constrain_to(viewport_rect)
            .interactable(false)
            .show(ctx, |ui| {
                hud_frame(p)
                    .inner_margin(Margin::symmetric(9, 6))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("Layer {} / {total}", app.max_layer + 1))
                                .size(12.0)
                                .strong()
                                .color(p.accent),
                        );
                        ui.label(
                            RichText::new(format!(
                                "Z {:.2} mm · h {:.3} mm",
                                layer.z_top_mm, layer.height_mm
                            ))
                            .size(11.0)
                            .color(p.text_primary),
                        );
                        ui.label(
                            RichText::new(format!(
                                "{} paths · Fan {}% · {:.1}s",
                                layer.paths.len(),
                                layer.fan_percent,
                                layer.estimated_time_s
                            ))
                            .size(10.5)
                            .color(p.text_secondary),
                        );
                    });
            });
    }
}

fn draw_preview_bottom_dock(
    app: &mut LayerCraftApp,
    ctx: &Context,
    viewport_rect: Rect,
    p: &ThemePalette,
) {
    let total_layers = app.scene.layer_ranges.len();
    let step_info = if total_layers > 0 {
        let layer_idx = app.max_layer.min(total_layers - 1);
        let step_count = app
            .scene
            .layer_steps
            .get(layer_idx)
            .map(Vec::len)
            .unwrap_or(0);
        if step_count > 0 {
            let cur_step = app.layer_move_step.min(step_count);
            let active_step_info = if cur_step == 0 {
                app.scene
                    .layer_steps
                    .get(layer_idx)
                    .and_then(|s| s.first())
                    .copied()
            } else {
                app.scene
                    .layer_steps
                    .get(layer_idx)
                    .and_then(|s| s.get(cur_step - 1))
                    .copied()
            };
            Some((step_count, cur_step, active_step_info))
        } else {
            None
        }
    } else {
        None
    };

    let track_w = (viewport_rect.width() * 0.28).clamp(120.0, 290.0);
    Area::new(Id::unique("layercraft_hud_preview_dock"))
        .order(Order::Foreground)
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_pos(pos2(
            viewport_rect.center().x,
            viewport_rect.bottom() - 12.0,
        ))
        .constrain_to(viewport_rect)
        .show(ctx, |ui| {
            floating_rail_frame(p)
                .inner_margin(Margin::symmetric(12, 7))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 5.0;
                        if let Some((step_count, cur_step, active_step_info)) = step_info {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                let play_icon = if app.playback_playing {
                                    Icon::Pause
                                } else {
                                    Icon::Play
                                };
                                let play_tip = if app.playback_playing {
                                    "Pause toolpath playback (Space)"
                                } else {
                                    "Play layer toolpath moves (Space)"
                                };
                                if icon_button(ui, p, play_icon, play_tip, app.playback_playing)
                                    .clicked()
                                {
                                    if app.playback_playing {
                                        app.playback_playing = false;
                                        app.last_playback_time_s = None;
                                    } else {
                                        if app.layer_move_step >= step_count {
                                            app.layer_move_step = 0;
                                        }
                                        app.playback_playing = true;
                                        app.playback_accum = 0.0;
                                        app.last_playback_time_s = None;
                                    }
                                }

                                let speed_label = format!("{}×", app.playback_speed as u32);
                                let (sp_rect, sp_resp) =
                                    ui.allocate_exact_size(vec2(28.0, 20.0), Sense::click());
                                if ui.is_rect_visible(sp_rect) {
                                    let bg = if sp_resp.hovered() {
                                        p.bg_hover
                                    } else {
                                        p.bg_card
                                    };
                                    ui.painter().rect(
                                        sp_rect,
                                        CornerRadius::same(4),
                                        bg,
                                        Stroke::new(1.0, p.border_subtle),
                                        StrokeKind::Inside,
                                    );
                                    ui.painter().text(
                                        sp_rect.center(),
                                        Align2::CENTER_CENTER,
                                        &speed_label,
                                        egui::FontId::proportional(10.5),
                                        p.text_secondary,
                                    );
                                }
                                if sp_resp
                                    .on_hover_text("Cycle playback speed (1× / 2× / 5× / 10×)")
                                    .clicked()
                                {
                                    app.playback_speed = if app.playback_speed < 1.5 {
                                        2.0
                                    } else if app.playback_speed < 3.5 {
                                        5.0
                                    } else if app.playback_speed < 8.0 {
                                        10.0
                                    } else {
                                        1.0
                                    };
                                }

                                if scrubber_step_button(ui, p, "<", "Previous move (Left Arrow)")
                                    .clicked()
                                {
                                    app.playback_playing = false;
                                    app.last_playback_time_s = None;
                                    if app.layer_move_step > 0 {
                                        app.layer_move_step -= 1;
                                    }
                                }

                                let (track_rect, track_resp) = ui.allocate_exact_size(
                                    vec2(track_w, 20.0),
                                    Sense::click_and_drag(),
                                );

                                if track_resp.hovered() {
                                    let wheel = ui.input(|i| i.smooth_scroll_delta.y);
                                    if wheel > 4.0 && app.layer_move_step < step_count {
                                        app.layer_move_step += 1;
                                    } else if wheel < -4.0 && app.layer_move_step > 0 {
                                        app.layer_move_step -= 1;
                                    }
                                }

                                if (track_resp.clicked() || track_resp.dragged())
                                    && let Some(ptr) = track_resp.interact_pointer_pos()
                                {
                                    app.playback_playing = false;
                                    app.last_playback_time_s = None;
                                    let frac = ((ptr.x - track_rect.left()) / track_rect.width())
                                        .clamp(0.0, 1.0);
                                    let next = (frac * (step_count as f32)).round() as usize;
                                    app.layer_move_step = next.min(step_count);
                                }

                                let frac = if step_count == 0 {
                                    1.0
                                } else {
                                    (cur_step as f32) / (step_count as f32)
                                };
                                let rail = Rect::from_center_size(
                                    track_rect.center(),
                                    vec2(track_rect.width(), 5.0),
                                );
                                ui.painter()
                                    .rect_filled(rail, CornerRadius::same(3), p.bg_input);

                                let thumb_x = track_rect.left() + frac * track_rect.width();
                                let active_rail = Rect::from_min_max(
                                    pos2(rail.left(), rail.top()),
                                    pos2(thumb_x, rail.bottom()),
                                );
                                ui.painter().rect_filled(
                                    active_rail,
                                    CornerRadius::same(3),
                                    p.accent,
                                );

                                let knob = pos2(thumb_x, track_rect.center().y);
                                ui.painter().circle(
                                    knob,
                                    6.0,
                                    p.text_primary,
                                    Stroke::new(2.0, p.accent),
                                );

                                if scrubber_step_button(ui, p, ">", "Next move (Right Arrow)")
                                    .clicked()
                                {
                                    app.playback_playing = false;
                                    app.last_playback_time_s = None;
                                    if app.layer_move_step < step_count {
                                        app.layer_move_step += 1;
                                    }
                                }

                                ui.add_space(4.0);
                                if let Some(step) = active_step_info {
                                    let c = step.kind.color();
                                    let dot_col = Color32::from_rgb(
                                        (c[0] * 255.0) as u8,
                                        (c[1] * 255.0) as u8,
                                        (c[2] * 255.0) as u8,
                                    );
                                    let (dot_r, _) =
                                        ui.allocate_exact_size(vec2(8.0, 12.0), Sense::hover());
                                    ui.painter().circle_filled(dot_r.center(), 3.5, dot_col);

                                    let desc = if cur_step == 0 {
                                        format!("0/{step_count} · Start")
                                    } else if matches!(step.kind, MoveKind::Extrusion(_)) {
                                        format!(
                                            "{cur_step}/{step_count} · {} · {:.1} mm³/s · Fan {}%",
                                            step.kind.label(),
                                            step.flow_mm3_s,
                                            step.fan_percent
                                        )
                                    } else {
                                        format!(
                                            "{cur_step}/{step_count} · {} · Fan {}%",
                                            step.kind.label(),
                                            step.fan_percent
                                        )
                                    };
                                    ui.label(RichText::new(desc).size(11.0).color(p.text_primary));
                                }
                            });
                            ui.separator();
                        }

                        // Canonical Preview view toggles row
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if icon_button(
                                ui,
                                p,
                                Icon::FitFrame,
                                "Frame build plate & part (F)",
                                false,
                            )
                            .clicked()
                            {
                                app.frame_camera(CameraPreset::Isometric);
                            }
                            if ribbon_button(ui, p, Icon::Layers, "Single", app.single_layer_only)
                                .on_hover_text("Show only the currently selected layer")
                                .clicked()
                            {
                                app.single_layer_only = !app.single_layer_only;
                            }
                            if ribbon_button(ui, p, Icon::Travel, "Travels", app.show_travels)
                                .on_hover_text("Toggle travel move lines (T)")
                                .clicked()
                            {
                                app.show_travels = !app.show_travels;
                            }
                            if ribbon_button(ui, p, Icon::Ghost, "Ghost", app.show_ghost_mesh)
                                .on_hover_text("Show translucent source mesh overlay")
                                .clicked()
                            {
                                app.show_ghost_mesh = !app.show_ghost_mesh;
                            }
                            if ribbon_button(ui, p, Icon::ArrangeGrid, "Grid", app.show_bed_grid)
                                .on_hover_text("Toggle build plate grid")
                                .clicked()
                            {
                                app.show_bed_grid = !app.show_bed_grid;
                                app.rebuild_scene();
                            }
                            if ribbon_button(ui, p, Icon::Legend, "Legend", app.show_legend_hud)
                                .on_hover_text("Show summary & feature legend card")
                                .clicked()
                            {
                                app.show_legend_hud = !app.show_legend_hud;
                            }
                            if ribbon_button(ui, p, Icon::Code, "G-code", app.show_gcode_viewer)
                                .on_hover_text("Toggle live synchronized G-code viewer")
                                .clicked()
                            {
                                app.show_gcode_viewer = !app.show_gcode_viewer;
                            }
                        });
                    });
                });
        });
}

fn draw_gcode_viewer_hud(
    app: &mut LayerCraftApp,
    ctx: &Context,
    viewport_rect: Rect,
    p: &ThemePalette,
) {
    app.sync_gcode_index();
    let Some(gcode_text) = app.session.last_gcode.as_ref().map(|g| g.gcode.as_str()) else {
        return;
    };
    let total_lines = app.gcode_index.total_lines();
    if total_lines == 0 {
        return;
    }
    let total_layers = app
        .session
        .sliced
        .as_ref()
        .map(|s| s.layers.len())
        .unwrap_or(0);
    let layer_idx = app.max_layer.min(total_layers.saturating_sub(1));
    let (start_line, end_line) = if app.gcode_viewer_all_layers {
        (0, total_lines)
    } else {
        app.gcode_index
            .layer_line_range(layer_idx)
            .unwrap_or((0, total_lines))
    };
    let line_count = end_line.saturating_sub(start_line);
    let step_count = app
        .scene
        .layer_steps
        .get(layer_idx)
        .map(Vec::len)
        .unwrap_or(0);
    let cur_step = app.layer_move_step.min(step_count);
    let active_line = app
        .gcode_index
        .active_move_line(layer_idx, cur_step, step_count);

    let panel_w = (viewport_rect.width() * 0.34).clamp(240.0, 400.0);
    let max_h = (viewport_rect.height() * 0.36).clamp(130.0, 270.0);
    let mut close_clicked = false;
    let mut next_all_layers = app.gcode_viewer_all_layers;

    Area::new(Id::unique("layercraft_hud_gcode_viewer"))
        .order(Order::Foreground)
        .pivot(Align2::LEFT_BOTTOM)
        .fixed_pos(viewport_rect.left_bottom() + vec2(12.0, -86.0))
        .constrain_to(viewport_rect)
        .show(ctx, |ui| {
            hud_frame(p)
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(panel_w);
                    ui.horizontal(|ui| {
                        let title = if next_all_layers {
                            format!("G-CODE ({line_count} lines)")
                        } else {
                            format!("G-CODE · LAYER {} ({line_count})", layer_idx + 1)
                        };
                        ui.label(RichText::new(title).size(10.5).strong().color(p.text_muted));
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if ui
                                .selectable_label(false, "×")
                                .on_hover_text("Hide G-code viewer")
                                .clicked()
                            {
                                close_clicked = true;
                            }
                            if ui
                                .selectable_label(false, "Copy")
                                .on_hover_text("Copy displayed G-code to clipboard")
                                .clicked()
                            {
                                let mut copied = String::new();
                                for idx in start_line..end_line {
                                    if !copied.is_empty() {
                                        copied.push('\n');
                                    }
                                    copied.push_str(app.gcode_index.line_str(gcode_text, idx));
                                }
                                ctx.copy_text(copied);
                            }
                            if ui
                                .selectable_label(next_all_layers, "All")
                                .on_hover_text("Show full program")
                                .clicked()
                            {
                                next_all_layers = true;
                            }
                            if ui
                                .selectable_label(!next_all_layers, "Layer")
                                .on_hover_text("Scope to selected layer")
                                .clicked()
                            {
                                next_all_layers = false;
                            }
                        });
                    });

                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);

                    let row_h = 15.0_f32;
                    egui::ScrollArea::vertical()
                        .id_salt("layercraft_gcode_viewer_scroll")
                        .max_height(max_h)
                        .auto_shrink([false, false])
                        .show_rows(ui, row_h, line_count, |ui, row_range| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for rel in row_range {
                                let abs_idx = start_line.saturating_add(rel);
                                let line = app.gcode_index.line_str(gcode_text, abs_idx);
                                let is_active = active_line == Some(abs_idx);
                                let trimmed = line.trim_start();
                                let text_col = if is_active {
                                    p.text_primary
                                } else if trimmed.starts_with(';') {
                                    p.text_muted
                                } else if trimmed.starts_with('M') {
                                    p.warning
                                } else if trimmed.starts_with('G') {
                                    p.accent
                                } else {
                                    p.text_secondary
                                };
                                let mut row_frame =
                                    egui::Frame::NONE.inner_margin(Margin::symmetric(4, 1));
                                if is_active {
                                    row_frame = row_frame
                                        .fill(p.accent_muted)
                                        .corner_radius(CornerRadius::same(3));
                                }
                                row_frame.show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.set_height(row_h - 2.0);
                                        ui.label(
                                            RichText::new(format!("{:>5}", abs_idx + 1))
                                                .monospace()
                                                .size(10.5)
                                                .color(p.text_muted),
                                        );
                                        ui.label(
                                            RichText::new(line)
                                                .monospace()
                                                .size(10.5)
                                                .color(text_col),
                                        );
                                    });
                                });
                            }
                        });
                });
        });

    app.gcode_viewer_all_layers = next_all_layers;
    if close_clicked {
        app.show_gcode_viewer = false;
    }
}

fn draw_bottom_view_pill(
    app: &mut LayerCraftApp,
    ctx: &Context,
    viewport_rect: Rect,
    p: &ThemePalette,
) {
    Area::new(Id::unique("layercraft_hud_bottom_pill"))
        .order(Order::Foreground)
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_pos(pos2(
            viewport_rect.center().x,
            viewport_rect.bottom() - 12.0,
        ))
        .constrain_to(viewport_rect)
        .show(ctx, |ui| {
            floating_rail_frame(p)
                .inner_margin(Margin::symmetric(10, 5))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if icon_button(ui, p, Icon::FitFrame, "Frame build plate & part (F)", false)
                            .clicked()
                        {
                            app.frame_camera(CameraPreset::Isometric);
                        }
                        if ribbon_button(ui, p, Icon::ArrangeGrid, "Grid", app.show_bed_grid)
                            .on_hover_text("Toggle build plate grid")
                            .clicked()
                        {
                            app.show_bed_grid = !app.show_bed_grid;
                            app.rebuild_scene();
                        }
                        ui.label(
                            RichText::new(format!(
                                "{} obj · {} mod",
                                app.session.objects.len(),
                                app.session.modifiers.len()
                            ))
                            .size(11.0)
                            .color(p.text_secondary),
                        );
                    });
                });
        });
}

fn draw_drag_drop_overlay(ctx: &Context, viewport_rect: Rect, p: &ThemePalette) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        Order::Foreground,
        Id::unique("layercraft_drop_overlay_painter"),
    ));
    let inner = viewport_rect.shrink(14.0);
    painter.rect(
        inner,
        CornerRadius::same(12),
        p.accent_muted,
        Stroke::new(2.0, p.accent),
        StrokeKind::Inside,
    );

    Area::new(Id::unique("layercraft_drop_overlay_card"))
        .order(Order::Foreground)
        .pivot(Align2::CENTER_CENTER)
        .fixed_pos(viewport_rect.center())
        .interactable(false)
        .show(ctx, |ui| {
            hud_frame(p).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("Drop .stl, .3mf, or .json profile to load")
                            .size(14.0)
                            .strong()
                            .color(p.text_primary),
                    );
                });
            });
        });
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
