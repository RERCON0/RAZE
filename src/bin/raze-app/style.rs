//! The same Terminal Native vocabulary as SNATCH, with RAZE's orange accent.
use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Stroke};

const MONO: &[u8] = include_bytes!("../../../fonts/CascadiaMono-Light.ttf");
/// Same cap as SNATCH: a wider window keeps the form centred, not stretched.
pub const CONTENT_WIDTH: f32 = 608.0;
pub const CONTROL_HEIGHT: f32 = 40.0;
/// Wider than the 40px window buttons: "день"/"ночь" need more room.
pub const THEME_BTN_WIDTH: f32 = 56.0;

/// Window size in egui points, as in SNATCH.
pub const WINDOW_SIZE: [f32; 2] = [460.0, 720.0];
pub const WINDOW_MIN_SIZE: [f32; 2] = [460.0, 600.0];

/// One design pixel is UI_SCALE physical pixels on any display, whatever the
/// OS scaling reports: fractional scales (125/150%) otherwise rasterise 13pt
/// text blurry and make RAZE measure differently from SNATCH. Viewport
/// commands take egui points; winit applies the effective scale itself.
const UI_SCALE: f32 = 1.18;

pub fn pin_pixel_scale(ctx: &egui::Context, resize_viewport: bool) {
    if ctx.pixels_per_point() == UI_SCALE && !resize_viewport {
        return;
    }
    ctx.set_pixels_per_point(UI_SCALE);
    if resize_viewport {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            WINDOW_SIZE[0],
            WINDOW_SIZE[1],
        )));
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
        WINDOW_MIN_SIZE[0],
        WINDOW_MIN_SIZE[1],
    )));
}

/// Масштаб без смены размера окна: окну-вопросу размер задаёт его содержимое.
pub fn pin_scale_only(ctx: &egui::Context) -> bool {
    if ctx.pixels_per_point() == UI_SCALE {
        return false;
    }
    ctx.set_pixels_per_point(UI_SCALE);
    true
}

pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, offset) in [
        ("body", 0.15),
        ("button", 0.0),
        ("field", 0.0),
        ("title", -0.08),
    ] {
        let key = format!("cascadia-{name}");
        fonts.font_data.insert(
            key.clone(),
            egui::FontData::from_static(MONO).tweak(egui::FontTweak {
                y_offset_factor: offset,
                ..Default::default()
            }),
        );
        fonts
            .families
            .insert(FontFamily::Name(name.into()), vec![key]);
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
    ctx.set_theme(if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    ctx.all_styles_mut(|style| {
        style.visuals = terminal_visuals(dark);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(12.0, 10.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.window_margin = egui::Margin::symmetric(10.0, 15.0);
        style.spacing.scroll = egui::style::ScrollStyle {
            foreground_color: true,
            ..egui::style::ScrollStyle::solid()
        };
        for (text, size, family) in [
            (egui::TextStyle::Body, 13.0, FontFamily::Proportional),
            (egui::TextStyle::Monospace, 13.0, FontFamily::Monospace),
            (
                egui::TextStyle::Button,
                12.5,
                FontFamily::Name("button".into()),
            ),
            (egui::TextStyle::Small, 11.5, FontFamily::Proportional),
            (
                egui::TextStyle::Heading,
                18.0,
                FontFamily::Name("title".into()),
            ),
        ] {
            style.text_styles.insert(text, FontId::new(size, family));
        }
    });
}

/// Same palette and widget chrome as SNATCH in both modes; only the accent is
/// RAZE's own. Square corners, hairline ghost buttons, one accent used sparingly.
pub fn terminal_visuals(dark: bool) -> egui::Visuals {
    let rgb = Color32::from_rgb;
    let (bg, bg_lift, field, line, line_bright, text, text_dim, accent, warn, err) = if dark {
        (
            rgb(0x0b, 0x0b, 0x0e),
            rgb(0x0e, 0x0e, 0x11),
            rgb(0x11, 0x11, 0x14),
            rgb(0x1e, 0x1e, 0x24),
            rgb(0x33, 0x33, 0x3c),
            rgb(0xd6, 0xd6, 0xda),
            rgb(0x83, 0x83, 0x8c),
            rgb(255, 138, 60),
            rgb(0xe0, 0xb3, 0x4d),
            rgb(0xe0, 0x65, 0x5c),
        )
    } else {
        (
            rgb(0xf3, 0xf3, 0xf1),
            rgb(0xff, 0xff, 0xff),
            rgb(0xfb, 0xfb, 0xfa),
            rgb(0xda, 0xda, 0xd6),
            rgb(0xb5, 0xb5, 0xb0),
            rgb(0x1c, 0x1c, 0x1e),
            rgb(0x5c, 0x5c, 0x62),
            rgb(163, 70, 12),
            rgb(0x9a, 0x67, 0x00),
            rgb(0xcf, 0x22, 0x2e),
        )
    };
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.window_rounding = egui::Rounding::ZERO;
    v.menu_rounding = egui::Rounding::ZERO;
    v.window_fill = bg_lift;
    v.window_stroke = Stroke::new(1.0, line);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.panel_fill = bg;
    v.extreme_bg_color = field; // TextEdit background: the one filled surface
    v.faint_bg_color = field;
    v.code_bg_color = field;
    v.hyperlink_color = accent;
    v.warn_fg_color = warn;
    v.error_fg_color = err;
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 45);
    v.selection.stroke = Stroke::new(1.0, accent);
    for widget in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        widget.rounding = egui::Rounding::ZERO;
        // bg_fill backs checkboxes and must stay visible; weak_bg_fill backs
        // plain buttons, which is what makes them "ghost" (border only).
        widget.bg_fill = field;
        widget.expansion = 0.0;
    }
    // `weak` text tints its colour towards noninteractive.weak_bg_fill. It has
    // to be the page background: a transparent fill tints towards black and
    // makes weak text *darker* than normal text in the light theme.
    v.widgets.noninteractive.weak_bg_fill = bg;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, line);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text_dim);

    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, line);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, text_dim);

    v.widgets.hovered.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, line_bright);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, text);

    v.widgets.active.weak_bg_fill = field;
    v.widgets.active.bg_stroke = Stroke::new(1.0, accent);
    v.widgets.active.fg_stroke = Stroke::new(1.0, text);

    v.widgets.open.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.open.bg_stroke = Stroke::new(1.0, line_bright);
    v.widgets.open.fg_stroke = Stroke::new(1.0, text);
    v
}

pub fn field_font() -> FontId {
    FontId::new(13.0, FontFamily::Name("field".into()))
}

/// Window captions use the un-nudged "title" face at 13pt; a plain `&str`
/// would fall back to the 18pt Heading style.
pub fn window_title(text: &str) -> RichText {
    RichText::new(text).font(FontId::new(13.0, FontFamily::Name("title".into())))
}

pub fn accent_button(text: &str, accent: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).color(accent))
        .stroke(Stroke::new(1.0, accent))
        .fill(Color32::TRANSPARENT)
}

/// Flat "● label" / "○ label" choice with no box, as SNATCH's engine row.
/// `active` is the colour of the picked choice.
pub fn choice(ui: &mut egui::Ui, label: &str, selected: bool, active: Color32) -> egui::Response {
    let color = if selected {
        active
    } else {
        ui.visuals().widgets.inactive.fg_stroke.color
    };
    let dot_color = if selected {
        active
    } else {
        ui.visuals().weak_text_color()
    };
    // The dot is 11pt and the label 13pt: two sizes in one label need a LayoutJob.
    let mut job = egui::text::LayoutJob::default();
    job.append(
        if selected { "● " } else { "○ " },
        0.0,
        egui::TextFormat {
            font_id: FontId::monospace(11.0),
            color: dot_color,
            ..Default::default()
        },
    );
    job.append(
        label,
        0.0,
        egui::TextFormat {
            font_id: FontId::monospace(13.0),
            color,
            ..Default::default()
        },
    );
    let text: egui::WidgetText = job.into();
    // Not .fill()/.stroke(): each of them silently turns the frame back on.
    ui.add(egui::Button::new(text).frame(false))
}

/// `egui::Spinner` с ограниченной частотой перерисовки. Штатный вызывает
/// `request_repaint()` на каждом кадре, и пока идёт сканирование или удаление,
/// окно крутится на полной частоте и отбирает процессор у потока, который
/// эту работу делает. Здесь тот же рисунок (дуга 240°, 20 точек), но следующий
/// кадр запрашивается через ~66 мс; события ввода перерисовывают сразу.
pub fn slow_spinner(ui: &mut egui::Ui) {
    let size = ui.style().spacing.interact_size.y;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::new(egui::WidgetType::ProgressIndicator));
    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(66));
    let radius = (rect.height() / 2.0) - 2.0;
    let time = ui.input(|i| i.time);
    let start = time * std::f64::consts::TAU;
    let end = start + 240f64.to_radians() * time.sin();
    let points: Vec<egui::Pos2> = (0..20)
        .map(|i| {
            let angle = egui::lerp(start..=end, f64::from(i) / 20.0);
            let (sin, cos) = angle.sin_cos();
            rect.center() + radius * egui::vec2(cos as f32, sin as f32)
        })
        .collect();
    ui.painter().add(egui::Shape::line(
        points,
        Stroke::new(3.0, ui.visuals().strong_text_color()),
    ));
}

pub fn tag(ui: &mut egui::Ui, text: &str) {
    ui.weak(RichText::new(format!("› {text}")).size(12.0));
}

/// Dashed 1px rule across the available width, as SNATCH's separator.
pub fn rule(ui: &mut egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    let stroke = Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
    let (dash, gap) = (4.0, 3.0);
    let mut x = rect.left();
    while x < rect.right() {
        let end = (x + dash).min(rect.right());
        ui.painter().line_segment(
            [
                egui::pos2(x, rect.center().y),
                egui::pos2(end, rect.center().y),
            ],
            stroke,
        );
        x += dash + gap;
    }
}

pub fn badge(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    egui::Frame::none()
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.5)))
        .inner_margin(egui::Margin::symmetric(6.0, 2.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).color(color))
        })
        .response
}

/// The wordmark, drawn like SNATCH's banner: the font shrinks from 11pt until
/// the widest row fits, then the rows are pinned to one left edge and the
/// whole block is centred. Only line breaks are trimmed: `trim()` would eat the
/// first row's leading space and shift it one column left.
pub fn wordmark(ui: &mut egui::Ui) {
    let lines: Vec<&str> = raze_rs::WORDMARK
        .trim_matches(['\r', '\n'])
        .lines()
        .collect();
    let measure = |ui: &egui::Ui, text: &str, size: f32| -> egui::Vec2 {
        ui.fonts(|fonts| {
            fonts
                .layout_job(egui::text::LayoutJob::single_section(
                    text.to_string(),
                    egui::TextFormat {
                        font_id: FontId::monospace(size),
                        ..Default::default()
                    },
                ))
                .size()
        })
    };
    ui.add_space(2.0);
    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
        let available = ui.available_width();
        let width_at = |size: f32| {
            lines
                .iter()
                .map(|line| measure(ui, line, size).x)
                .fold(0.0_f32, f32::max)
        };
        // Подбор кегля — до двенадцати замеров всех строк; он зависит только от
        // ширины окна, поэтому считается один раз на ширину, а не на кадр.
        let cache_id = egui::Id::new("wordmark-size");
        let cached: Option<(u32, f32)> = ui.ctx().data(|data| data.get_temp(cache_id));
        let size = match cached {
            Some((width_bits, size)) if width_bits == available.to_bits() => size,
            _ => {
                let mut size = 11.0_f32;
                while size > 5.0 && width_at(size) > available {
                    size -= 0.5;
                }
                ui.ctx()
                    .data_mut(|data| data.insert_temp(cache_id, (available.to_bits(), size)));
                size
            }
        };
        let block = egui::vec2(
            width_at(size) + 2.0,
            measure(ui, "█", size).y * lines.len() as f32,
        );
        // Align::Min: the art is not a padded rectangle, so rows that centred
        // themselves would drift sideways from their neighbours.
        ui.allocate_ui_with_layout(block, egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            for line in &lines {
                ui.label(RichText::new(*line).monospace().size(size).weak());
            }
        });
        ui.add_space(4.0);
        ui.weak(RichText::new(raze_rs::tagline()).size(11.0));
    });
    ui.add_space(14.0);
}
