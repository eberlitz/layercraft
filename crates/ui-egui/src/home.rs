//! Dedicated Home workspace hub: Hero launcher, Recent Projects gallery, Quick Start shapes, and Active Setup bar.

use egui::{Align, CornerRadius, Margin, Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2};
use layercraft_mesh::SampleShape;

use crate::icons::{Icon, paint_icon};
use crate::recent_projects::{
    RecentProjectEntry, RecentProjectSource, decode_thumbnail_color_image, format_relative_time,
    now_unix_secs,
};
use crate::sidebar::SidebarTab;
use crate::theme::ThemePalette;
use crate::ui_kit::{badge, primary_button, secondary_button, small_icon_button};
use crate::{LayerCraftApp, PendingDiscardAction, ViewMode};

/// Render the full-canvas Home workspace page.
pub fn draw_home_page(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    egui::ScrollArea::vertical()
        .id_salt("layercraft_home_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let outer_w = ui.available_width().max(320.0);
            let max_content_w = 1040.0;
            let h_pad =
                ((outer_w - max_content_w) * 0.5).max(if outer_w < 680.0 { 14.0 } else { 28.0 });
            let content_w = (outer_w - h_pad * 2.0).max(260.0);

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(h_pad);
                ui.vertical(|ui| {
                    ui.set_width(content_w);
                    ui.add_space(22.0);
                    draw_hero_strip(app, ui, p, content_w);

                    ui.add_space(20.0);
                    draw_recent_section(app, ui, p, content_w);

                    ui.add_space(20.0);
                    draw_quick_start_section(app, ui, p);

                    ui.add_space(18.0);
                    draw_active_setup_bar(app, ui, p);

                    ui.add_space(24.0);
                });
            });
        });
}

fn draw_hero_strip(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette, content_w: f32) {
    egui::Frame::NONE
        .fill(p.bg_card)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(20, 16))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let has_active_plate = !app.session.objects.is_empty();
            let wide = content_w >= 680.0;

            let draw_brand = |ui: &mut egui::Ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    let (mark_box, _) =
                        ui.allocate_exact_size(vec2(38.0, 38.0), Sense::hover());
                    ui.painter().rect(
                        mark_box,
                        CornerRadius::same(9),
                        p.accent_muted,
                        Stroke::new(1.0, p.accent),
                        StrokeKind::Inside,
                    );
                    paint_icon(ui.painter(), mark_box.shrink(8.0), Icon::BrandMark, p.accent);

                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(
                            RichText::new("LayerCraft Studio")
                                .size(19.0)
                                .strong()
                                .color(p.text_primary),
                        );
                        ui.label(
                            RichText::new(
                                "Clean-room 3D slicer for desktop & browser · Drop a .3mf or .stl anytime",
                            )
                            .size(12.0)
                            .color(p.text_secondary),
                        );
                    });
                });
            };

            let draw_ctas = |ui: &mut egui::Ui, app: &mut LayerCraftApp| {
                if has_active_plate
                    && secondary_button(ui, p, Some(Icon::TabPlate), "Resume Plate").clicked()
                {
                    app.view_mode = ViewMode::Prepare;
                }
                if secondary_button(ui, p, Some(Icon::AddPlus), "New Empty").clicked() {
                    app.request_discard_or_run(PendingDiscardAction::NewEmptyProject);
                }
                if primary_button(ui, p, Icon::OpenFile, "Open File").clicked() {
                    app.request_discard_or_run(PendingDiscardAction::OpenFileDialog);
                }
            };

            if wide {
                ui.horizontal(|ui| {
                    draw_brand(ui);
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if primary_button(ui, p, Icon::OpenFile, "Open File").clicked() {
                            app.request_discard_or_run(PendingDiscardAction::OpenFileDialog);
                        }
                        if secondary_button(ui, p, Some(Icon::AddPlus), "New Empty").clicked() {
                            app.request_discard_or_run(PendingDiscardAction::NewEmptyProject);
                        }
                        if has_active_plate
                            && secondary_button(ui, p, Some(Icon::TabPlate), "Resume Plate")
                                .clicked()
                        {
                            app.view_mode = ViewMode::Prepare;
                        }
                    });
                });
            } else {
                draw_brand(ui);
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    draw_ctas(ui, app);
                });
            }
        });
}

fn draw_recent_section(
    app: &mut LayerCraftApp,
    ui: &mut egui::Ui,
    p: &ThemePalette,
    content_w: f32,
) {
    let total_count = app.recent_projects.entries.len();

    // Header bar with title badge, live search box, and Clear All button
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(
            RichText::new("Recent Projects")
                .size(15.0)
                .strong()
                .color(p.text_primary),
        );
        badge(ui, p.bg_card, p.text_secondary, &total_count.to_string());

        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if total_count > 0
                && secondary_button(ui, p, None, "Clear All")
                    .on_hover_text("Clear recent projects history")
                    .clicked()
            {
                app.clear_recent_projects();
            }

            // Live search pill
            egui::Frame::NONE
                .fill(p.bg_input)
                .stroke(Stroke::new(1.0, p.border_subtle))
                .corner_radius(CornerRadius::same(7))
                .inner_margin(Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    let w = (content_w * 0.22).clamp(140.0, 210.0);
                    ui.set_max_width(w + 46.0);
                    ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        let (s_rect, _) = ui.allocate_exact_size(vec2(13.0, 13.0), Sense::hover());
                        paint_icon(ui.painter(), s_rect, Icon::Search, p.text_muted);

                        ui.add(
                            egui::TextEdit::singleline(&mut app.home_search)
                                .desired_width(w)
                                .frame(egui::Frame::NONE)
                                .hint_text("Search recent..."),
                        );
                        if !app.home_search.is_empty() && ui.small_button("×").clicked() {
                            app.home_search.clear();
                        }
                    });
                });
        });
    });

    ui.add_space(10.0);

    let needle = app.home_search.trim().to_ascii_lowercase();
    let filtered: Vec<RecentProjectEntry> = app
        .recent_projects
        .entries
        .iter()
        .filter(|e| {
            if needle.is_empty() {
                return true;
            }
            e.name.to_ascii_lowercase().contains(&needle)
                || e.format.to_ascii_lowercase().contains(&needle)
                || e.printer_name
                    .as_deref()
                    .is_some_and(|pr| pr.to_ascii_lowercase().contains(&needle))
                || e.path
                    .as_deref()
                    .is_some_and(|pa| pa.to_ascii_lowercase().contains(&needle))
        })
        .cloned()
        .collect();

    if filtered.is_empty() {
        draw_recent_empty_state(app, ui, p, !needle.is_empty());
        return;
    }

    let gap = 12.0;
    let cols = ((content_w + gap) / (230.0 + gap)).floor().clamp(1.0, 4.0) as usize;
    let card_w = ((content_w - gap * (cols.saturating_sub(1) as f32)) / cols as f32).max(190.0);

    let now = now_unix_secs();
    let mut open_id: Option<String> = None;
    let mut remove_id: Option<String> = None;

    for row in filtered.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for entry in row {
                let outcome = draw_recent_card(app, ui, p, entry, card_w, now);
                if outcome.remove_clicked {
                    remove_id = Some(entry.id.clone());
                } else if outcome.open_clicked {
                    open_id = Some(entry.id.clone());
                }
            }
        });
        ui.add_space(gap);
    }

    if let Some(id) = remove_id {
        app.remove_recent_project(&id);
    } else if let Some(id) = open_id {
        app.request_discard_or_run(PendingDiscardAction::OpenRecentId(id));
    }
}

struct RecentCardOutcome {
    open_clicked: bool,
    remove_clicked: bool,
}

fn draw_recent_card(
    app: &mut LayerCraftApp,
    ui: &mut egui::Ui,
    p: &ThemePalette,
    entry: &RecentProjectEntry,
    card_w: f32,
    now_secs: u64,
) -> RecentCardOutcome {
    let mut open_clicked = false;
    let mut remove_clicked = false;

    let inner_resp = egui::Frame::NONE
        .fill(p.bg_card)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_width((card_w - 20.0).max(160.0));
                let thumb_w = ui.available_width();
                let thumb_h = 124.0;
                let (thumb_rect, thumb_resp) =
                    ui.allocate_exact_size(vec2(thumb_w, thumb_h), Sense::click());

                ui.painter().rect(
                    thumb_rect,
                    CornerRadius::same(7),
                    p.bg_input,
                    Stroke::new(1.0, p.border_subtle),
                    StrokeKind::Inside,
                );

                // Lazy-decode 240x136 base64 PNG thumbnail and cache GPU texture
                let tex_id_opt = resolve_thumbnail_texture(app, ui.ctx(), entry);
                if let Some(tex) = tex_id_opt {
                    ui.put(
                        thumb_rect.shrink(1.0),
                        egui::Image::new((tex.id(), (thumb_rect.shrink(1.0)).size()))
                            .corner_radius(CornerRadius::same(6)),
                    );
                } else {
                    draw_fallback_thumb_placeholder(ui, thumb_rect, p, &entry.format);
                }

                // Format & source pills pinned cleanly inside top-left of thumbnail
                let fmt_text = entry.format.to_ascii_uppercase();
                let src_text = match entry.source {
                    RecentProjectSource::NativePath => "Local",
                    RecentProjectSource::BrowserIndexedDb => "Browser",
                };
                paint_thumb_pill(
                    ui,
                    pos2(thumb_rect.left() + 7.0, thumb_rect.top() + 7.0),
                    p.bg_hud,
                    p.border_strong,
                    p.text_primary,
                    &fmt_text,
                );
                paint_thumb_pill(
                    ui,
                    pos2(
                        thumb_rect.left() + 7.0 + fmt_text.len() as f32 * 6.8 + 16.0,
                        thumb_rect.top() + 7.0,
                    ),
                    p.bg_hud,
                    p.border_subtle,
                    p.text_secondary,
                    src_text,
                );

                // Top-right remove button inside thumbnail corner (use new_child so parent cursor stays at thumb bottom)
                let rm_rect = Rect::from_min_size(
                    pos2(thumb_rect.right() - 25.0, thumb_rect.top() + 5.0),
                    vec2(20.0, 20.0),
                );
                ui.painter().rect(
                    rm_rect,
                    CornerRadius::same(5),
                    p.bg_hud,
                    Stroke::new(1.0, p.border_subtle),
                    StrokeKind::Inside,
                );
                let mut rm_ui = ui.new_child(egui::UiBuilder::new().max_rect(rm_rect));
                if small_icon_button(&mut rm_ui, p, Icon::Trash, "Remove from recent projects")
                    .clicked()
                {
                    remove_clicked = true;
                }

                ui.add_space(8.0);

                // Card footer
                let footer_resp = ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.add(
                        egui::Label::new(
                            RichText::new(&entry.name)
                                .size(13.0)
                                .strong()
                                .color(p.text_primary),
                        )
                        .truncate(),
                    );

                    let plate_word = if entry.plate_count == 1 {
                        "plate"
                    } else {
                        "plates"
                    };
                    let left_meta = format!(
                        "{} obj · {} {plate_word}",
                        entry.object_count,
                        entry.plate_count.max(1)
                    );
                    let right_meta = format_relative_time(entry.last_opened_unix_secs, now_secs);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(left_meta).size(11.0).color(p.text_secondary));
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(right_meta).size(10.5).color(p.text_muted));
                        });
                    });

                    let setup_meta = match (&entry.printer_name, &entry.estimated_time) {
                        (Some(pr), Some(tm)) => format!("{pr} · {tm}"),
                        (Some(pr), None) => pr.clone(),
                        (None, Some(tm)) => format!("Est. {tm}"),
                        (None, None) => "Ready to slice".to_owned(),
                    };
                    ui.add(
                        egui::Label::new(RichText::new(setup_meta).size(10.5).color(p.text_muted))
                            .truncate(),
                    );
                });

                let footer_click = ui.interact(
                    footer_resp.response.rect,
                    ui.scope_id().with(&entry.id),
                    Sense::click(),
                );
                if !remove_clicked && (thumb_resp.clicked() || footer_click.clicked()) {
                    open_clicked = true;
                }
                thumb_resp.hovered() || footer_click.hovered()
            })
            .inner
        });

    let card_rect = inner_resp.response.rect;
    let hovered = inner_resp.inner || ui.rect_contains_pointer(card_rect);
    if hovered && !remove_clicked {
        ui.painter().rect(
            card_rect,
            CornerRadius::same(10),
            egui::Color32::TRANSPARENT,
            Stroke::new(1.5, p.accent),
            StrokeKind::Inside,
        );
    }

    let tip = entry.path.as_deref().unwrap_or(entry.name.as_str());
    inner_resp.response.on_hover_text(format!("Open `{tip}`"));

    RecentCardOutcome {
        open_clicked,
        remove_clicked,
    }
}

fn resolve_thumbnail_texture(
    app: &mut LayerCraftApp,
    ctx: &egui::Context,
    entry: &RecentProjectEntry,
) -> Option<egui::TextureHandle> {
    let b64 = entry.thumbnail_png_b64.as_deref()?;
    if let Some(cached) = app.recent_thumb_textures.get(&entry.id) {
        return cached.clone();
    }
    let decoded = match decode_thumbnail_color_image(b64) {
        Ok(img) => Some(ctx.load_texture(
            format!("recent_thumb_{}", entry.id),
            img,
            egui::TextureOptions::LINEAR,
        )),
        Err(_) => None,
    };
    app.recent_thumb_textures
        .insert(entry.id.clone(), decoded.clone());
    decoded
}

fn paint_thumb_pill(
    ui: &egui::Ui,
    top_left: egui::Pos2,
    bg: egui::Color32,
    border: egui::Color32,
    fg: egui::Color32,
    text: &str,
) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(9.5, egui::FontFamily::Proportional),
        fg,
    );
    let rect = Rect::from_min_size(top_left, vec2(galley.size().x + 10.0, 16.0));
    ui.painter().rect(
        rect,
        CornerRadius::same(5),
        bg,
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );
    ui.painter().galley(
        pos2(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        fg,
    );
}

fn draw_fallback_thumb_placeholder(
    ui: &egui::Ui,
    thumb_rect: Rect,
    p: &ThemePalette,
    format: &str,
) {
    let c = thumb_rect.center();
    let dx = 34.0;
    let dy = 14.0;
    let plate = vec![
        pos2(c.x, c.y - dy + 12.0),
        pos2(c.x + dx, c.y + 12.0),
        pos2(c.x, c.y + dy + 12.0),
        pos2(c.x - dx, c.y + 12.0),
    ];
    ui.painter().add(egui::Shape::closed_line(
        plate,
        Stroke::new(1.0, p.border_strong),
    ));
    let icon_rect = Rect::from_center_size(pos2(c.x, c.y - 4.0), vec2(26.0, 26.0));
    paint_icon(
        ui.painter(),
        icon_rect,
        Icon::SampleCube,
        p.text_muted.gamma_multiply(0.9),
    );
    ui.painter().text(
        pos2(c.x, thumb_rect.bottom() - 16.0),
        egui::Align2::CENTER_CENTER,
        format!("{} project", format.to_ascii_uppercase()),
        egui::FontId::new(10.0, egui::FontFamily::Proportional),
        p.text_muted,
    );
}

fn draw_recent_empty_state(
    app: &mut LayerCraftApp,
    ui: &mut egui::Ui,
    p: &ThemePalette,
    is_filtered: bool,
) {
    egui::Frame::NONE
        .fill(p.bg_card)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(24, 24))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                let (icon_r, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
                paint_icon(
                    ui.painter(),
                    icon_r,
                    if is_filtered {
                        Icon::Search
                    } else {
                        Icon::OpenFile
                    },
                    p.text_muted,
                );
                ui.label(
                    RichText::new(if is_filtered {
                        "No recent projects match your filter"
                    } else {
                        "No recent projects yet"
                    })
                    .size(13.5)
                    .strong()
                    .color(p.text_primary),
                );
                ui.label(
                    RichText::new(if is_filtered {
                        "Clear the search query to see all recent projects."
                    } else {
                        "Open a .3mf or .stl file, drag and drop a mesh anywhere, or launch a Quick Start shape below."
                    })
                    .size(12.0)
                    .color(p.text_secondary),
                );
                ui.add_space(4.0);
                if is_filtered {
                    if secondary_button(ui, p, None, "Clear Search").clicked() {
                        app.home_search.clear();
                    }
                } else if primary_button(ui, p, Icon::OpenFile, "Open File").clicked() {
                    app.request_discard_or_run(PendingDiscardAction::OpenFileDialog);
                }
            });
        });
}

fn draw_quick_start_section(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(
            RichText::new("Quick Start Calibration Shapes")
                .size(14.5)
                .strong()
                .color(p.text_primary),
        );
        ui.label(
            RichText::new("Open a built-in parametric benchmark on a fresh plate")
                .size(11.5)
                .color(p.text_muted),
        );
    });
    ui.add_space(8.0);

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        for shape in SampleShape::ALL {
            if secondary_button(ui, p, Some(Icon::SampleCube), shape.label())
                .on_hover_text(format!("Load `{}` onto a fresh plate", shape.label()))
                .clicked()
            {
                app.request_discard_or_run(PendingDiscardAction::LoadSample(shape));
            }
        }
    });
}

fn draw_active_setup_bar(app: &mut LayerCraftApp, ui: &mut egui::Ui, p: &ThemePalette) {
    let machine = app
        .selected_machine_profile
        .as_deref()
        .unwrap_or("Default Printer");
    let filament = app
        .selected_filament_profile
        .as_deref()
        .unwrap_or("Default Filament");
    let process = app
        .selected_process_profile
        .as_deref()
        .unwrap_or("Default Process");

    egui::Frame::NONE
        .fill(p.bg_card)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
                ui.label(
                    RichText::new("ACTIVE SETUP")
                        .size(10.5)
                        .strong()
                        .color(p.text_muted),
                );
                badge(ui, p.bg_input, p.text_primary, machine);
                badge(ui, p.bg_input, p.text_secondary, filament);
                badge(ui, p.bg_input, p.text_secondary, process);
                badge(
                    ui,
                    p.bg_input,
                    p.text_muted,
                    &format!(
                        "{:.2} mm layer · {:.2} mm nozzle",
                        app.session.config.process.layer_height_mm,
                        app.session.config.machine.nozzle_diameter_mm
                    ),
                );

                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if secondary_button(ui, p, Some(Icon::TabMachine), "Edit Setup")
                        .on_hover_text("Open workspace with Printer & Preset inspector")
                        .clicked()
                    {
                        app.sidebar_open = true;
                        app.sidebar_tab = SidebarTab::Machine;
                        app.view_mode = ViewMode::Prepare;
                    }
                });
            });
        });
}
