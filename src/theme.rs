//! Visual design.
//!
//! Thesis: this is a bench instrument, not a dashboard. The numbers are the
//! product, so they get monospace and size; everything else recedes. Square
//! corners, hairline rules instead of cards, and exactly one signal colour
//! (amber) which is reserved for "live" and never used decoratively. If amber
//! is on screen, something is firing.

use egui::{Color32, CornerRadius, FontId, RichText, Stroke, TextStyle, Ui};

pub const BG: Color32 = Color32::from_rgb(0x13, 0x15, 0x17);
pub const PANEL: Color32 = Color32::from_rgb(0x18, 0x1B, 0x1E);
pub const SUNK: Color32 = Color32::from_rgb(0x0E, 0x10, 0x12);
pub const RULE: Color32 = Color32::from_rgb(0x2A, 0x2F, 0x35);
pub const RULE_SOFT: Color32 = Color32::from_rgb(0x21, 0x25, 0x2A);
pub const INK: Color32 = Color32::from_rgb(0xE7, 0xE4, 0xDE);
pub const DIM: Color32 = Color32::from_rgb(0x8B, 0x92, 0x99);
pub const FAINT: Color32 = Color32::from_rgb(0x5A, 0x61, 0x68);
/// The only accent. Means live / armed.
pub const LIVE: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);
pub const WARN: Color32 = Color32::from_rgb(0xD4, 0x5F, 0x4A);
pub const GOOD: Color32 = Color32::from_rgb(0x74, 0xA5, 0x88);

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();

    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = SUNK;
    v.faint_bg_color = PANEL;
    v.override_text_color = Some(INK);
    v.window_stroke = Stroke::new(1.0, RULE);
    v.selection.bg_fill = LIVE.gamma_multiply(0.30);
    v.selection.stroke = Stroke::new(1.0, LIVE);
    // No drop shadows anywhere -- they read as "web app", not instrument.
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow::NONE;

    let sq = CornerRadius::same(1);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = sq;
        w.bg_fill = PANEL;
        w.weak_bg_fill = PANEL;
        w.bg_stroke = Stroke::new(1.0, RULE_SOFT);
        w.fg_stroke = Stroke::new(1.0, INK);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, RULE_SOFT);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, DIM);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, RULE);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x1F, 0x23, 0x27);
    v.widgets.active.bg_stroke = Stroke::new(1.0, LIVE);
    v.widgets.active.bg_fill = Color32::from_rgb(0x24, 0x28, 0x2C);

    ctx.set_visuals(v);

    ctx.all_styles_mut(|s| {
        s.text_styles
            .insert(TextStyle::Heading, FontId::proportional(15.0));
        s.text_styles
            .insert(TextStyle::Body, FontId::proportional(13.0));
        s.text_styles
            .insert(TextStyle::Button, FontId::proportional(13.0));
        s.text_styles
            .insert(TextStyle::Small, FontId::proportional(11.0));
        s.text_styles
            .insert(TextStyle::Monospace, FontId::monospace(13.0));
        s.spacing.item_spacing = egui::vec2(8.0, 7.0);
        s.spacing.button_padding = egui::vec2(9.0, 5.0);
        s.spacing.slider_width = 150.0;
    });
}

/// Small uppercase letterspaced field label. Used above every readout.
pub fn caption(text: &str) -> RichText {
    // egui has no letter-spacing, so space the characters manually -- it is the
    // one typographic move that makes these read as instrument labels.
    let spaced: String = text
        .to_uppercase()
        .chars()
        .flat_map(|c| [c, ' '])
        .collect::<String>()
        .trim_end()
        .to_string();
    RichText::new(spaced).size(9.5).color(FAINT)
}

/// A big monospace measurement with a unit. The hero element.
pub fn readout(ui: &mut Ui, label: &str, value: &str, unit: &str, colour: Color32, size: f32) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(caption(label));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            ui.label(
                RichText::new(value)
                    .font(FontId::monospace(size))
                    .color(colour),
            );
            if !unit.is_empty() {
                ui.label(RichText::new(unit).size(10.0).color(FAINT));
            }
        });
    });
}

/// Hairline horizontal rule spanning the available width.
pub fn rule(ui: &mut Ui) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), egui::Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, RULE_SOFT));
}

pub fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(caption(title));
    ui.add_space(2.0);
    rule(ui);
    ui.add_space(4.0);
}

/// Monospace key/value line, value right-aligned against a fixed column.
pub fn stat(ui: &mut Ui, k: &str, v: String, colour: Color32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(k).size(11.5).color(DIM));
        let used = ui.min_rect().width();
        let pad = (ui.available_width() - used).max(0.0);
        let _ = pad;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(v).font(FontId::monospace(11.5)).color(colour));
        });
    });
}
