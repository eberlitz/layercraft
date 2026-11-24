//! Custom HiDPI geometric vector icons painted via `egui::Painter`.

use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Stroke, StrokeKind, pos2};

/// Crisp monochrome geometric UI icons for LayerCraft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    BrandMark,
    #[cfg(not(target_arch = "wasm32"))]
    OpenFile,
    #[cfg(not(target_arch = "wasm32"))]
    AddPlus,
    SaveProject,
    Undo,
    Redo,
    SampleCube,
    ArrangeGrid,
    Trash,
    Export,
    SliceLayers,
    BoxShape,
    CylinderShape,
    ShieldOff,
    ShieldPlus,
    TabPlate,
    TabQuality,
    TabStrength,
    TabSpeed,
    TabSupport,
    TabFinish,
    TabMachine,
    ThemeDark,
    ThemeLight,
    ThemeSystem,
    ChevronDown,
    ChevronUp,
    SidebarToggle,
    FitFrame,
    Layers,
    Ghost,
    Legend,
    Travel,
    Play,
    Pause,
    #[cfg(not(target_arch = "wasm32"))]
    Import,
}

/// Paint `icon` centered cleanly inside `rect` with `color`.
pub fn paint_icon(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let side = rect.width().min(rect.height()).max(4.0);
    let r = Rect::from_center_size(rect.center(), egui::vec2(side, side)).shrink(side * 0.14);
    let w = (side * 0.095).clamp(1.1, 2.2);
    let stroke = Stroke::new(w, color);

    let pt =
        |nx: f32, ny: f32| -> Pos2 { pos2(r.left() + r.width() * nx, r.top() + r.height() * ny) };

    match icon {
        Icon::BrandMark => {
            // Stacked parallelogram print layers forming an isometric mark.
            for dy in [0.0, 0.25, 0.50] {
                let poly = vec![
                    pt(0.50, 0.08 + dy),
                    pt(0.92, 0.25 + dy),
                    pt(0.50, 0.42 + dy),
                    pt(0.08, 0.25 + dy),
                ];
                if dy < 0.05 {
                    painter.add(egui::Shape::convex_polygon(poly, color, Stroke::NONE));
                } else {
                    painter.add(egui::Shape::closed_line(poly, stroke));
                }
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        Icon::OpenFile => {
            painter.line_segment([pt(0.10, 0.22), pt(0.38, 0.22)], stroke);
            painter.line_segment([pt(0.38, 0.22), pt(0.48, 0.34)], stroke);
            painter.line_segment([pt(0.48, 0.34), pt(0.90, 0.34)], stroke);
            painter.line_segment([pt(0.90, 0.34), pt(0.90, 0.84)], stroke);
            painter.line_segment([pt(0.90, 0.84), pt(0.10, 0.84)], stroke);
            painter.line_segment([pt(0.10, 0.84), pt(0.10, 0.22)], stroke);
        }
        #[cfg(not(target_arch = "wasm32"))]
        Icon::AddPlus => {
            painter.line_segment([pt(0.50, 0.15), pt(0.50, 0.85)], stroke);
            painter.line_segment([pt(0.15, 0.50), pt(0.85, 0.50)], stroke);
        }
        Icon::SaveProject => {
            let poly = vec![
                pt(0.14, 0.12),
                pt(0.72, 0.12),
                pt(0.86, 0.26),
                pt(0.86, 0.88),
                pt(0.14, 0.88),
            ];
            painter.add(egui::Shape::closed_line(poly, stroke));
            let top_bay = Rect::from_min_max(pt(0.28, 0.12), pt(0.64, 0.36));
            painter.rect_stroke(top_bay, CornerRadius::same(1), stroke, StrokeKind::Inside);
            let bot_bay = Rect::from_min_max(pt(0.26, 0.54), pt(0.74, 0.88));
            painter.rect_stroke(bot_bay, CornerRadius::same(1), stroke, StrokeKind::Inside);
        }
        Icon::Undo => {
            let arc = vec![
                pt(0.22, 0.40),
                pt(0.42, 0.24),
                pt(0.68, 0.28),
                pt(0.82, 0.50),
                pt(0.76, 0.72),
                pt(0.52, 0.80),
            ];
            painter.add(egui::Shape::line(arc, stroke));
            painter.line_segment([pt(0.18, 0.22), pt(0.20, 0.44)], stroke);
            painter.line_segment([pt(0.20, 0.44), pt(0.42, 0.42)], stroke);
        }
        Icon::Redo => {
            let arc = vec![
                pt(0.78, 0.40),
                pt(0.58, 0.24),
                pt(0.32, 0.28),
                pt(0.18, 0.50),
                pt(0.24, 0.72),
                pt(0.48, 0.80),
            ];
            painter.add(egui::Shape::line(arc, stroke));
            painter.line_segment([pt(0.82, 0.22), pt(0.80, 0.44)], stroke);
            painter.line_segment([pt(0.80, 0.44), pt(0.58, 0.42)], stroke);
        }
        Icon::SampleCube | Icon::BoxShape => {
            // Isometric wireframe cube.
            let top = pt(0.50, 0.08);
            let tr = pt(0.88, 0.28);
            let tl = pt(0.12, 0.28);
            let mid = pt(0.50, 0.48);
            let br = pt(0.88, 0.72);
            let bl = pt(0.12, 0.72);
            let bot = pt(0.50, 0.92);
            for (a, b) in [
                (top, tr),
                (tr, mid),
                (mid, tl),
                (tl, top),
                (tl, bl),
                (bl, bot),
                (bot, br),
                (br, tr),
                (mid, bot),
            ] {
                painter.line_segment([a, b], stroke);
            }
        }
        Icon::CylinderShape => {
            let rx = r.width() * 0.34;
            let ry = r.height() * 0.14;
            let top_c = pt(0.50, 0.26);
            let bot_c = pt(0.50, 0.76);
            // Draw upper/lower ellipses approximated with clean polygons + side verticals.
            let mut top_pts = Vec::with_capacity(16);
            let mut bot_pts = Vec::with_capacity(9);
            for i in 0..16 {
                let a = (i as f32 / 16.0) * std::f32::consts::TAU;
                top_pts.push(pos2(top_c.x + rx * a.cos(), top_c.y + ry * a.sin()));
            }
            for i in 0..=8 {
                let a = (i as f32 / 8.0) * std::f32::consts::PI;
                bot_pts.push(pos2(bot_c.x + rx * a.cos(), bot_c.y + ry * a.sin()));
            }
            painter.add(egui::Shape::closed_line(top_pts, stroke));
            painter.add(egui::Shape::line(bot_pts, stroke));
            painter.line_segment(
                [pos2(top_c.x - rx, top_c.y), pos2(bot_c.x - rx, bot_c.y)],
                stroke,
            );
            painter.line_segment(
                [pos2(top_c.x + rx, top_c.y), pos2(bot_c.x + rx, bot_c.y)],
                stroke,
            );
        }
        Icon::ArrangeGrid | Icon::TabPlate => {
            let gap = 0.08;
            let s = 0.36;
            for (ox, oy) in [
                (0.10, 0.10),
                (0.10 + s + gap, 0.10),
                (0.10, 0.10 + s + gap),
                (0.10 + s + gap, 0.10 + s + gap),
            ] {
                let cell = Rect::from_min_max(pt(ox, oy), pt(ox + s, oy + s));
                painter.rect_stroke(cell, CornerRadius::same(1), stroke, StrokeKind::Inside);
            }
        }
        Icon::Trash => {
            painter.line_segment([pt(0.18, 0.26), pt(0.82, 0.26)], stroke);
            painter.line_segment([pt(0.36, 0.14), pt(0.64, 0.14)], stroke);
            painter.line_segment([pt(0.36, 0.14), pt(0.36, 0.26)], stroke);
            painter.line_segment([pt(0.64, 0.14), pt(0.64, 0.26)], stroke);
            let body = Rect::from_min_max(pt(0.24, 0.26), pt(0.76, 0.88));
            painter.rect_stroke(body, CornerRadius::same(2), stroke, StrokeKind::Inside);
            painter.line_segment([pt(0.41, 0.40), pt(0.41, 0.74)], stroke);
            painter.line_segment([pt(0.59, 0.40), pt(0.59, 0.74)], stroke);
        }
        Icon::Export => {
            painter.line_segment([pt(0.50, 0.62), pt(0.50, 0.14)], stroke);
            painter.line_segment([pt(0.30, 0.34), pt(0.50, 0.14)], stroke);
            painter.line_segment([pt(0.70, 0.34), pt(0.50, 0.14)], stroke);
            painter.line_segment([pt(0.14, 0.62), pt(0.14, 0.86)], stroke);
            painter.line_segment([pt(0.14, 0.86), pt(0.86, 0.86)], stroke);
            painter.line_segment([pt(0.86, 0.86), pt(0.86, 0.62)], stroke);
        }
        #[cfg(not(target_arch = "wasm32"))]
        Icon::Import => {
            painter.line_segment([pt(0.50, 0.14), pt(0.50, 0.62)], stroke);
            painter.line_segment([pt(0.30, 0.42), pt(0.50, 0.62)], stroke);
            painter.line_segment([pt(0.70, 0.42), pt(0.50, 0.62)], stroke);
            painter.line_segment([pt(0.14, 0.62), pt(0.14, 0.86)], stroke);
            painter.line_segment([pt(0.14, 0.86), pt(0.86, 0.86)], stroke);
            painter.line_segment([pt(0.86, 0.86), pt(0.86, 0.62)], stroke);
        }
        Icon::SliceLayers | Icon::Layers => {
            for y in [0.22, 0.50, 0.78] {
                painter.line_segment([pt(0.12, y), pt(0.88, y)], stroke);
            }
            painter.line_segment(
                [pt(0.24, 0.36), pt(0.76, 0.36)],
                Stroke::new(w * 0.8, color),
            );
            painter.line_segment(
                [pt(0.24, 0.64), pt(0.76, 0.64)],
                Stroke::new(w * 0.8, color),
            );
        }
        Icon::ShieldOff => {
            let poly = vec![
                pt(0.50, 0.10),
                pt(0.84, 0.24),
                pt(0.78, 0.62),
                pt(0.50, 0.90),
                pt(0.22, 0.62),
                pt(0.16, 0.24),
            ];
            painter.add(egui::Shape::closed_line(poly, stroke));
            painter.line_segment([pt(0.26, 0.74), pt(0.74, 0.26)], stroke);
        }
        Icon::ShieldPlus => {
            let poly = vec![
                pt(0.50, 0.10),
                pt(0.84, 0.24),
                pt(0.78, 0.62),
                pt(0.50, 0.90),
                pt(0.22, 0.62),
                pt(0.16, 0.24),
            ];
            painter.add(egui::Shape::closed_line(poly, stroke));
            painter.line_segment([pt(0.50, 0.32), pt(0.50, 0.66)], stroke);
            painter.line_segment([pt(0.33, 0.49), pt(0.67, 0.49)], stroke);
        }
        Icon::TabQuality => {
            for i in 0..4 {
                let y = 0.20 + i as f32 * 0.20;
                painter.line_segment([pt(0.12, y), pt(0.88, y)], stroke);
            }
        }
        Icon::TabStrength => {
            let outer = Rect::from_min_max(pt(0.12, 0.12), pt(0.88, 0.88));
            painter.rect_stroke(outer, CornerRadius::same(2), stroke, StrokeKind::Inside);
            painter.line_segment([pt(0.22, 0.78), pt(0.78, 0.22)], stroke);
            painter.line_segment([pt(0.22, 0.22), pt(0.78, 0.78)], stroke);
        }
        Icon::TabSpeed => {
            painter.line_segment([pt(0.16, 0.50), pt(0.68, 0.50)], stroke);
            painter.line_segment([pt(0.54, 0.26), pt(0.84, 0.50)], stroke);
            painter.line_segment([pt(0.54, 0.74), pt(0.84, 0.50)], stroke);
            painter.line_segment([pt(0.12, 0.28), pt(0.36, 0.28)], stroke);
            painter.line_segment([pt(0.12, 0.72), pt(0.36, 0.72)], stroke);
        }
        Icon::TabSupport => {
            painter.line_segment([pt(0.14, 0.20), pt(0.86, 0.20)], stroke);
            painter.line_segment([pt(0.14, 0.84), pt(0.86, 0.84)], stroke);
            painter.line_segment([pt(0.50, 0.84), pt(0.50, 0.20)], stroke);
            painter.line_segment([pt(0.50, 0.58), pt(0.26, 0.20)], stroke);
            painter.line_segment([pt(0.50, 0.58), pt(0.74, 0.20)], stroke);
        }
        Icon::TabFinish => {
            painter.line_segment([pt(0.50, 0.12), pt(0.50, 0.88)], stroke);
            painter.line_segment([pt(0.12, 0.50), pt(0.88, 0.50)], stroke);
            painter.line_segment(
                [pt(0.24, 0.24), pt(0.76, 0.76)],
                Stroke::new(w * 0.8, color),
            );
            painter.line_segment(
                [pt(0.24, 0.76), pt(0.76, 0.24)],
                Stroke::new(w * 0.8, color),
            );
        }
        Icon::TabMachine => {
            let box_r = Rect::from_min_max(pt(0.14, 0.14), pt(0.86, 0.86));
            painter.rect_stroke(box_r, CornerRadius::same(2), stroke, StrokeKind::Inside);
            painter.line_segment([pt(0.14, 0.34), pt(0.86, 0.34)], stroke);
            painter.line_segment([pt(0.50, 0.34), pt(0.50, 0.54)], stroke);
            painter.line_segment([pt(0.28, 0.74), pt(0.72, 0.74)], stroke);
        }
        Icon::ThemeDark => {
            let c = pt(0.52, 0.50);
            let rad = r.width() * 0.36;
            let mut arc_pts = Vec::with_capacity(14);
            for i in 0..=12 {
                let a = 0.65 + (i as f32 / 12.0) * (std::f32::consts::TAU - 1.55);
                arc_pts.push(pos2(c.x + rad * a.cos(), c.y + rad * a.sin()));
            }
            painter.add(egui::Shape::line(arc_pts, stroke));
            let c2 = pt(0.64, 0.40);
            let rad2 = r.width() * 0.24;
            let mut inner = Vec::with_capacity(10);
            for i in 0..=8 {
                let a = 1.0 + (i as f32 / 8.0) * (std::f32::consts::PI * 1.15);
                inner.push(pos2(c2.x - rad2 * a.cos(), c2.y + rad2 * a.sin()));
            }
            painter.add(egui::Shape::line(inner, stroke));
        }
        Icon::ThemeLight => {
            let c = r.center();
            let rad = r.width() * 0.22;
            painter.circle_stroke(c, rad, stroke);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let p0 = pos2(c.x + a.cos() * rad * 1.45, c.y + a.sin() * rad * 1.45);
                let p1 = pos2(c.x + a.cos() * rad * 2.05, c.y + a.sin() * rad * 2.05);
                painter.line_segment([p0, p1], stroke);
            }
        }
        Icon::ThemeSystem => {
            let scr = Rect::from_min_max(pt(0.10, 0.16), pt(0.90, 0.70));
            painter.rect_stroke(scr, CornerRadius::same(2), stroke, StrokeKind::Inside);
            painter.line_segment([pt(0.50, 0.70), pt(0.50, 0.86)], stroke);
            painter.line_segment([pt(0.28, 0.86), pt(0.72, 0.86)], stroke);
        }
        Icon::ChevronDown => {
            painter.line_segment([pt(0.20, 0.35), pt(0.50, 0.66)], stroke);
            painter.line_segment([pt(0.50, 0.66), pt(0.80, 0.35)], stroke);
        }
        Icon::ChevronUp => {
            painter.line_segment([pt(0.20, 0.65), pt(0.50, 0.34)], stroke);
            painter.line_segment([pt(0.50, 0.34), pt(0.80, 0.65)], stroke);
        }
        Icon::SidebarToggle => {
            let outer = Rect::from_min_max(pt(0.10, 0.16), pt(0.90, 0.84));
            painter.rect_stroke(outer, CornerRadius::same(2), stroke, StrokeKind::Inside);
            painter.line_segment([pt(0.38, 0.16), pt(0.38, 0.84)], stroke);
        }
        Icon::FitFrame => {
            let arms = [
                (pt(0.12, 0.34), pt(0.12, 0.12), pt(0.34, 0.12)),
                (pt(0.66, 0.12), pt(0.88, 0.12), pt(0.88, 0.34)),
                (pt(0.88, 0.66), pt(0.88, 0.88), pt(0.66, 0.88)),
                (pt(0.34, 0.88), pt(0.12, 0.88), pt(0.12, 0.66)),
            ];
            for (a, b, c) in arms {
                painter.line_segment([a, b], stroke);
                painter.line_segment([b, c], stroke);
            }
        }
        Icon::Ghost => {
            let body = Rect::from_min_max(pt(0.20, 0.14), pt(0.80, 0.84));
            painter.rect_stroke(body, CornerRadius::same(4), stroke, StrokeKind::Inside);
            painter.circle_filled(pt(0.38, 0.42), w, color);
            painter.circle_filled(pt(0.62, 0.42), w, color);
        }
        Icon::Legend => {
            for (y, dot) in [(0.25, 0.20), (0.50, 0.20), (0.75, 0.20)] {
                painter.circle_filled(pt(dot, y), w * 1.1, color);
                painter.line_segment([pt(0.36, y), pt(0.86, y)], stroke);
            }
        }
        Icon::Travel => {
            painter.line_segment([pt(0.14, 0.78), pt(0.42, 0.34)], stroke);
            painter.line_segment([pt(0.42, 0.34), pt(0.64, 0.62)], stroke);
            painter.line_segment([pt(0.64, 0.62), pt(0.86, 0.22)], stroke);
            painter.line_segment([pt(0.68, 0.22), pt(0.86, 0.22)], stroke);
            painter.line_segment([pt(0.86, 0.22), pt(0.86, 0.40)], stroke);
        }
        Icon::Play => {
            let tri = vec![pt(0.24, 0.16), pt(0.84, 0.50), pt(0.24, 0.84)];
            painter.add(egui::Shape::convex_polygon(tri, color, Stroke::NONE));
        }
        Icon::Pause => {
            let left = Rect::from_min_max(pt(0.22, 0.18), pt(0.40, 0.82));
            let right = Rect::from_min_max(pt(0.60, 0.18), pt(0.78, 0.82));
            painter.rect_filled(left, CornerRadius::same(1), color);
            painter.rect_filled(right, CornerRadius::same(1), color);
        }
    }
}
