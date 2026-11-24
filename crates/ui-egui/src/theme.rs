//! LayerCraft design tokens and `egui` theme installation (Dark Studio / Light Drafting).

use std::collections::BTreeMap;

use egui::style::{HandleShape, ScrollStyle, Selection, WidgetVisuals, Widgets};
use egui::{
    Color32, CornerRadius, CursorIcon, FontFamily, FontId, Margin, Stroke, Style, TextStyle,
};

/// User-selectable color theme preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
    System,
}

impl ThemeMode {
    /// Cycle Dark -> Light -> System -> Dark.
    pub fn cycle(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::System,
            Self::System => Self::Dark,
        }
    }

    /// Human-readable theme mode label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark Studio",
            Self::Light => "Light Drafting",
            Self::System => "System Theme",
        }
    }

    /// Convert to `egui::ThemePreference`.
    pub fn to_preference(self) -> egui::ThemePreference {
        match self {
            Self::Dark => egui::ThemePreference::Dark,
            Self::Light => egui::ThemePreference::Light,
            Self::System => egui::ThemePreference::System,
        }
    }
}

/// Semantic color palette shared across chrome, sidebar cards, and floating viewport HUDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    pub dark: bool,
    pub bg_chrome: Color32,
    pub bg_panel: Color32,
    pub bg_card: Color32,
    pub bg_input: Color32,
    pub bg_hover: Color32,
    pub bg_active: Color32,
    pub bg_hud: Color32,
    pub border_subtle: Color32,
    pub border_strong: Color32,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_muted: Color32,
    pub accent_text: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub axis_x: Color32,
    pub axis_y: Color32,
    pub axis_z: Color32,
}

impl ThemePalette {
    /// Neutral charcoal studio palette with Cool Precision Teal accent.
    pub fn dark_studio() -> Self {
        Self {
            dark: true,
            bg_chrome: Color32::from_rgb(17, 19, 24),
            bg_panel: Color32::from_rgb(23, 26, 33),
            bg_card: Color32::from_rgb(29, 33, 42),
            bg_input: Color32::from_rgb(18, 20, 26),
            bg_hover: Color32::from_rgb(41, 47, 60),
            bg_active: Color32::from_rgb(52, 59, 75),
            bg_hud: Color32::from_rgba_unmultiplied(20, 23, 30, 236),
            border_subtle: Color32::from_rgb(43, 49, 62),
            border_strong: Color32::from_rgb(64, 73, 92),
            text_primary: Color32::from_rgb(236, 240, 247),
            text_secondary: Color32::from_rgb(163, 173, 192),
            text_muted: Color32::from_rgb(112, 122, 142),
            accent: Color32::from_rgb(20, 184, 166),
            accent_hover: Color32::from_rgb(45, 212, 191),
            accent_muted: Color32::from_rgba_unmultiplied(20, 184, 166, 40),
            accent_text: Color32::from_rgb(10, 24, 24),
            success: Color32::from_rgb(52, 211, 153),
            warning: Color32::from_rgb(251, 191, 36),
            danger: Color32::from_rgb(248, 113, 113),
            axis_x: Color32::from_rgb(239, 83, 80),
            axis_y: Color32::from_rgb(76, 175, 80),
            axis_z: Color32::from_rgb(66, 165, 245),
        }
    }

    /// Clean bright drafting studio palette with deep precision teal accent.
    pub fn light_drafting() -> Self {
        Self {
            dark: false,
            bg_chrome: Color32::from_rgb(241, 244, 249),
            bg_panel: Color32::from_rgb(248, 250, 252),
            bg_card: Color32::from_rgb(255, 255, 255),
            bg_input: Color32::from_rgb(237, 240, 246),
            bg_hover: Color32::from_rgb(226, 232, 241),
            bg_active: Color32::from_rgb(212, 220, 233),
            bg_hud: Color32::from_rgba_unmultiplied(255, 255, 255, 242),
            border_subtle: Color32::from_rgb(218, 224, 235),
            border_strong: Color32::from_rgb(185, 195, 212),
            text_primary: Color32::from_rgb(24, 30, 41),
            text_secondary: Color32::from_rgb(76, 86, 106),
            text_muted: Color32::from_rgb(122, 133, 154),
            accent: Color32::from_rgb(13, 148, 136),
            accent_hover: Color32::from_rgb(15, 118, 110),
            accent_muted: Color32::from_rgba_unmultiplied(13, 148, 136, 30),
            accent_text: Color32::from_rgb(255, 255, 255),
            success: Color32::from_rgb(5, 150, 105),
            warning: Color32::from_rgb(217, 119, 6),
            danger: Color32::from_rgb(220, 38, 38),
            axis_x: Color32::from_rgb(211, 47, 47),
            axis_y: Color32::from_rgb(46, 125, 50),
            axis_z: Color32::from_rgb(25, 118, 210),
        }
    }

    /// Select dark or light palette directly.
    pub fn for_dark(dark: bool) -> Self {
        if dark {
            Self::dark_studio()
        } else {
            Self::light_drafting()
        }
    }
}

/// Build an `egui::Style` driven by `palette`.
pub fn build_egui_style(palette: &ThemePalette, dark: bool) -> Style {
    let mut style = Style {
        visuals: if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        },
        ..Style::default()
    };

    let text_styles: BTreeMap<TextStyle, FontId> = [
        (
            TextStyle::Heading,
            FontId::new(14.5, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(12.5, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(12.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(11.5, FontFamily::Monospace),
        ),
        (
            TextStyle::Small,
            FontId::new(10.5, FontFamily::Proportional),
        ),
    ]
    .into();
    style.text_styles = text_styles;

    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.window_margin = Margin::same(12);
    style.spacing.menu_margin = Margin::same(8);
    style.spacing.interact_size = egui::vec2(36.0, 25.0);
    style.spacing.combo_width = 140.0;
    style.spacing.scroll = ScrollStyle::thin();

    let cr = CornerRadius::same(6);
    let v = &mut style.visuals;
    v.dark_mode = dark;
    v.panel_fill = palette.bg_panel;
    v.window_fill = palette.bg_card;
    v.window_stroke = Stroke::new(1.0, palette.border_strong);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.faint_bg_color = palette.bg_hover;
    v.extreme_bg_color = palette.bg_input;
    v.text_edit_bg_color = Some(palette.bg_input);
    v.code_bg_color = palette.bg_input;
    v.warn_fg_color = palette.warning;
    v.error_fg_color = palette.danger;
    v.override_text_color = Some(palette.text_primary);
    v.weak_text_color = Some(palette.text_secondary);
    v.hyperlink_color = palette.accent;
    v.selection = Selection {
        bg_fill: palette.accent_muted,
        stroke: Stroke::new(1.0, palette.accent),
    };
    v.interact_cursor = Some(CursorIcon::PointingHand);
    v.slider_trailing_fill = true;
    v.handle_shape = HandleShape::Circle;
    v.button_frame = true;
    v.collapsing_header_frame = false;

    v.widgets = Widgets {
        noninteractive: WidgetVisuals {
            weak_bg_fill: palette.bg_panel,
            bg_fill: palette.bg_card,
            bg_stroke: Stroke::new(1.0, palette.border_subtle),
            fg_stroke: Stroke::new(1.0, palette.text_primary),
            corner_radius: cr,
            expansion: 0.0,
        },
        inactive: WidgetVisuals {
            weak_bg_fill: palette.bg_input,
            bg_fill: palette.bg_card,
            bg_stroke: Stroke::new(1.0, palette.border_subtle),
            fg_stroke: Stroke::new(1.0, palette.text_primary),
            corner_radius: cr,
            expansion: 0.0,
        },
        hovered: WidgetVisuals {
            weak_bg_fill: palette.bg_hover,
            bg_fill: palette.bg_hover,
            bg_stroke: Stroke::new(1.0, palette.border_strong),
            fg_stroke: Stroke::new(1.25, palette.text_primary),
            corner_radius: cr,
            expansion: 0.0,
        },
        active: WidgetVisuals {
            weak_bg_fill: palette.bg_active,
            bg_fill: palette.bg_active,
            bg_stroke: Stroke::new(1.0, palette.accent),
            fg_stroke: Stroke::new(1.5, palette.text_primary),
            corner_radius: cr,
            expansion: 0.0,
        },
        open: WidgetVisuals {
            weak_bg_fill: palette.bg_active,
            bg_fill: palette.bg_card,
            bg_stroke: Stroke::new(1.0, palette.accent),
            fg_stroke: Stroke::new(1.0, palette.text_primary),
            corner_radius: cr,
            expansion: 0.0,
        },
    };

    style
}

/// Install Dark Studio and Light Drafting `egui` styles onto `ctx`.
pub fn install_styles(ctx: &egui::Context) {
    let dark = ThemePalette::dark_studio();
    let light = ThemePalette::light_drafting();
    ctx.set_style_of(egui::Theme::Dark, build_egui_style(&dark, true));
    ctx.set_style_of(egui::Theme::Light, build_egui_style(&light, false));
}

/// Read the currently resolved palette from `ctx.theme()`.
pub fn active_palette(ctx: &egui::Context) -> ThemePalette {
    ThemePalette::for_dark(ctx.theme() == egui::Theme::Dark)
}
