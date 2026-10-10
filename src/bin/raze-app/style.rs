//! The same Terminal Native vocabulary as SNATCH, with RAZE's orange accent.
use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Stroke};

const MONO: &[u8] = include_bytes!("../../../fonts/CascadiaMono-Light.ttf");
pub const CONTENT_WIDTH: f32 = 720.0;
pub const CONTROL_HEIGHT: f32 = 40.0;

pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, offset) in [("body", 0.15), ("button", 0.0), ("field", 0.0), ("title", -0.08)] {
        let key = format!("cascadia-{name}");
        fonts.font_data.insert(key.clone(), egui::FontData::from_static(MONO).tweak(egui::FontTweak {
            y_offset_factor: offset, ..Default::default()
        }));
        fonts.families.insert(FontFamily::Name(name.into()), vec![key]);
    }
    for family in [FontFamily::Monospace, FontFamily::Proportional] {
        if let Some(list) = fonts.families.get_mut(&family) {
            list.insert(0, "cascadia-body".into());
        }
    }
    ctx.set_fonts(fonts);
}

pub fn setup_theme(ctx: &egui::Context, dark: bool) {
    // egui keeps two style buckets. Configure both so the OS theme cannot
    // replace our spacing or colors after eframe's first native frame.
    ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
    ctx.all_styles_mut(|style| {
        style.visuals = terminal_visuals(dark);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(12.0, 9.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.window_margin = egui::Margin::symmetric(18.0, 16.0);
        style.spacing.scroll = egui::style::ScrollStyle {
            foreground_color: true,
            ..egui::style::ScrollStyle::solid()
        };
        for (text, size, family) in [
            (egui::TextStyle::Body, 13.0, FontFamily::Proportional),
            (egui::TextStyle::Monospace, 13.0, FontFamily::Monospace),
            (egui::TextStyle::Button, 12.5, FontFamily::Name("button".into())),
            (egui::TextStyle::Small, 11.5, FontFamily::Proportional),
            (egui::TextStyle::Heading, 18.0, FontFamily::Name("title".into())),
        ] {
            style.text_styles.insert(text, FontId::new(size, family));
        }
    });
}

pub fn terminal_visuals(dark: bool) -> egui::Visuals {
    let rgb = Color32::from_rgb;
    let (bg, surface, field, line, bright, text, muted, accent, warn, error) = if dark {
        (rgb(11,11,14), rgb(14,14,17), rgb(17,17,20), rgb(37,37,44),
         rgb(65,65,76), rgb(220,220,226), rgb(151,151,161),
         rgb(255,138,60), rgb(224,179,77), rgb(240,112,104))
    } else {
        (rgb(243,243,241), rgb(255,255,255), rgb(251,251,250), rgb(218,218,214),
         rgb(181,181,176), rgb(28,28,30), rgb(92,92,98),
         rgb(163,70,12), rgb(138,89,0), rgb(185,32,45))
    };
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.window_rounding = egui::Rounding::ZERO;
    v.menu_rounding = egui::Rounding::ZERO;
    v.window_fill = surface;
    v.window_stroke = Stroke::new(1.0, line);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.panel_fill = bg;
    v.extreme_bg_color = field;
    v.faint_bg_color = field;
    v.code_bg_color = field;
    v.hyperlink_color = accent;
    v.warn_fg_color = warn;
    v.error_fg_color = error;
    v.selection.bg_fill = accent.gamma_multiply(0.16);
    v.selection.stroke = Stroke::new(1.0, accent);
    for widget in [&mut v.widgets.noninteractive, &mut v.widgets.inactive,
        &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        widget.rounding = egui::Rounding::ZERO;
        widget.bg_fill = field;
        widget.weak_bg_fill = Color32::TRANSPARENT;
        widget.bg_stroke = Stroke::new(1.0, line);
        widget.fg_stroke = Stroke::new(1.0, text);
        widget.expansion = 0.0;
    }
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, muted);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, bright);
    v.widgets.active.bg_stroke = Stroke::new(1.0, accent);
    v.widgets.active.weak_bg_fill = field;
    v.widgets.open.bg_stroke = Stroke::new(1.0, bright);
    v
}

pub fn field_font() -> FontId {
    FontId::new(13.0, FontFamily::Name("field".into()))
}

pub fn accent_button(text: &str, accent: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).color(accent))
        .stroke(Stroke::new(1.0, accent)).fill(Color32::TRANSPARENT)
}

pub fn tag(ui: &mut egui::Ui, text: &str) {
    ui.weak(RichText::new(format!("› {text}")).size(12.0));
}

pub fn rule(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, ui.visuals().widgets.noninteractive.bg_stroke);
}

pub fn badge(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    egui::Frame::none()
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.5)))
        .inner_margin(egui::Margin::symmetric(6.0, 2.0))
        .show(ui, |ui| ui.label(RichText::new(text).size(11.0).color(color)))
        .response
}

pub fn wordmark(ui: &mut egui::Ui) {
    const ART: &str = "    ____  ___  __________\n   / __ \\/   |/_  / ____/\n  / /_/ / /| | / / / __/  \n / _, _/ ___ |/ /_/ /___  \n/_/ |_/_/  |_/___/_____/  ";
    let font = FontId::monospace(15.0);
    let color = ui.visuals().weak_text_color().gamma_multiply(0.7);
    let galley = ui.painter().layout_no_wrap(ART.to_owned(), font, color);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), galley.size().y + 16.0), egui::Sense::hover());
    ui.painter().galley(egui::pos2(rect.center().x - galley.size().x / 2.0, rect.top() + 4.0), galley, color);
    ui.vertical_centered(|ui| {
        ui.weak(RichText::new(crate::tr!("stubborn files, clean folders", "сложные файлы. чистые папки.")).size(11.0));
    });
    ui.add_space(18.0);
}
