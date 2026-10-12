//! Окно-вопрос для процесса из контекстного меню (`raze-app.exe --dialog`).
//!
//! Оформление то же, что у окна приложения: шрифт, палитра, рамка и кнопки
//! берутся из `style`, чтобы системное окно сообщений не выбивалось из вида.
//! Размер окна задаёт его содержимое: оно измеряется заранее, без окна.
use super::*;
use eframe::egui::{Color32, FontFamily, FontId, Key, RichText, ViewportCommand};
use raze_rs::dialog::{Spec, Tone, EXIT_NO, EXIT_UNAVAILABLE, EXIT_YES};
use std::cell::Cell;
use std::rc::Rc;

const WIDTH: f32 = style::WINDOW_SIZE[0];
const TITLE_BAR: f32 = 42.0;
const MARGIN: egui::Vec2 = egui::vec2(26.0, 18.0);
/// Запас на округление строк: лучше лишние пиксели, чем обрезанная кнопка.
const SLACK: f32 = 6.0;
const BUTTON_HEIGHT: f32 = 40.0;

/// Что нарисовал кадр.
struct Frame {
    /// Нажатая кнопка: `true` — главная.
    answer: Option<bool>,
    /// Высота содержимого под заголовком, без полей.
    content_height: f32,
}

fn tone_color(ui: &egui::Ui, tone: Tone) -> Color32 {
    match tone {
        Tone::Warning => ui.visuals().warn_fg_color,
        Tone::Error => ui.visuals().error_fg_color,
    }
}

/// Строка списка с переносом в любом месте: путь без пробелов иначе вылез бы
/// за край окна.
fn wrapped(text: &str, size: f32, color: Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: FontId::monospace(size),
            color,
            ..Default::default()
        },
    );
    job.wrap.break_anywhere = true;
    job
}

fn render(ui: &mut egui::Ui, spec: &Spec) -> Frame {
    let ctx = ui.ctx().clone();
    let ctx = &ctx;
    let mut answer = None;
    egui::Panel::top("titlebar")
        .exact_size(TITLE_BAR)
        .show_separator_line(true)
        .frame(
            egui::Frame::NONE
                .fill(ctx.global_style().visuals.window_fill)
                .inner_margin(egui::Margin::symmetric(14, 0)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let width = (ui.available_width() - 40.0).max(0.0);
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(width, TITLE_BAR),
                    egui::Sense::click_and_drag(),
                );
                ui.painter().text(
                    rect.left_center(),
                    egui::Align2::LEFT_CENTER,
                    "RAZE — by rercon prod.",
                    FontId::new(13.0, FontFamily::Name("title".into())),
                    ui.visuals().weak_text_color(),
                );
                if response.drag_started() {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }
                // Закрыть окно — то же, что отказаться.
                if ui
                    .add_sized([40.0, 38.0], egui::Button::new("×").frame(false))
                    .clicked()
                {
                    answer = Some(false);
                }
            });
        });

    let mut content_height = 0.0;
    egui::CentralPanel::default()
        .frame(
            egui::Frame::NONE
                .fill(ctx.global_style().visuals.panel_fill)
                .inner_margin(egui::Margin::symmetric(MARGIN.x as i8, MARGIN.y as i8)),
        )
        .show(ui, |ui| {
            let top = ui.cursor().top();
            let tone = tone_color(ui, spec.tone);
            ui.label(RichText::new(&spec.head).size(14.0));
            if !spec.items.is_empty() {
                ui.add_space(6.0);
                style::rule(ui);
                ui.add_space(6.0);
                for item in &spec.items {
                    ui.label(wrapped(item, 11.5, ui.visuals().text_color()));
                }
            }
            if !spec.foot.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new(&spec.foot).size(11.5).color(tone));
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| match &spec.no {
                Some(no) => {
                    let width = (ui.available_width() - 8.0) / 2.0;
                    if ui
                        .add_sized([width, BUTTON_HEIGHT], egui::Button::new(no))
                        .clicked()
                    {
                        answer = Some(false);
                    }
                    if ui
                        .add_sized(
                            [width, BUTTON_HEIGHT],
                            style::accent_button(&spec.yes, tone),
                        )
                        .clicked()
                    {
                        answer = Some(true);
                    }
                }
                None => {
                    if ui
                        .add_sized(
                            [ui.available_width(), BUTTON_HEIGHT],
                            style::accent_button(&spec.yes, tone),
                        )
                        .clicked()
                    {
                        answer = Some(true);
                    }
                }
            });
            // Не `min_rect`: у центральной панели он занимает всё свободное место.
            content_height = ui.cursor().top() - top;
        });
    Frame {
        answer,
        content_height,
    }
}

/// Высота окна под это содержимое. Тот же код рисования прогоняется на
/// контексте без окна: окно сразу открывается нужного размера и не прыгает.
fn measure(spec: &Spec) -> f32 {
    let ctx = egui::Context::default();
    style::setup_fonts(&ctx);
    style::setup_theme(&ctx, true);
    style::pin_scale_only(&ctx);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(WIDTH, 4000.0),
        )),
        ..Default::default()
    };
    // Шрифты и стиль вступают в силу с началом прохода, поэтому проходов два.
    let mut height = 0.0;
    for _ in 0..2 {
        let mut output = ctx.run_ui(input.clone(), |ui| {
            height = render(ui, spec).content_height;
        });
        // Measuring headlessly has no GPU to apply the generated font textures.
        output.textures_delta.clear();
    }
    TITLE_BAR + 1.0 + 2.0 * MARGIN.y + height + SLACK
}

struct DialogApp {
    spec: Spec,
    height: f32,
    frames: u32,
    answer: Rc<Cell<Option<bool>>>,
}

impl eframe::App for DialogApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // ОС сообщает свой масштаб на каждом кадре; размер окна — в точках egui.
        if style::pin_scale_only(ctx) {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(WIDTH, self.height)));
        }
        // Центр считается по уже применённому размеру, поэтому кадром позже.
        self.frames += 1;
        if self.frames == 2 {
            if let Some(center) = ViewportCommand::center_on_screen(ctx) {
                ctx.send_viewport_cmd(center);
            }
        } else if self.frames < 2 {
            ctx.request_repaint();
        }

        let mut answer = render(ui, &self.spec).answer;
        if ctx.input(|input| input.key_pressed(Key::Escape)) {
            answer = Some(false);
        } else if self.spec.no.is_none() && ctx.input(|input| input.key_pressed(Key::Enter)) {
            answer = Some(true);
        }
        if let Some(answer) = answer {
            self.answer.set(Some(answer));
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

/// Показывает окно и возвращает код выхода процесса: ответ или
/// [`EXIT_UNAVAILABLE`], если окно открыть не вышло.
pub fn run(spec: Spec) -> i32 {
    let height = measure(&spec);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([WIDTH, height])
        .with_resizable(false)
        .with_always_on_top()
        .with_title("RAZE");
    #[cfg(windows)]
    {
        viewport = viewport.with_decorations(false);
    }
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    let answer = Rc::new(Cell::new(None));
    let shared = answer.clone();
    let result = eframe::run_native(
        "RAZE",
        eframe::NativeOptions {
            viewport,
            centered: true,
            ..Default::default()
        },
        Box::new(move |cc| {
            style::setup_fonts(&cc.egui_ctx);
            style::setup_theme(&cc.egui_ctx, true);
            Ok(Box::new(DialogApp {
                spec,
                height,
                frames: 0,
                answer: shared,
            }))
        }),
    );
    match result {
        Ok(()) if answer.get() == Some(true) => EXIT_YES,
        Ok(()) => EXIT_NO,
        Err(_) => EXIT_UNAVAILABLE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(items: usize, foot: &str) -> Spec {
        Spec {
            head: "1 target(s) could not be sent to the Recycle Bin. Nothing has been deleted."
                .into(),
            items: (0..items)
                .map(|n| format!(r"C:\Users\me\Desktop\a-rather-long-folder-name-{n}\and\more"))
                .collect(),
            foot: foot.into(),
            yes: "delete permanently".into(),
            no: Some("leave as is".into()),
            tone: Tone::Error,
        }
    }

    #[test]
    fn the_window_is_as_tall_as_its_content() {
        let one = measure(&spec(1, "Deleting them permanently cannot be undone."));
        let six = measure(&spec(6, "Deleting them permanently cannot be undone."));
        let bare = measure(&spec(1, ""));
        assert!(six > one + 40.0, "{one} -> {six}");
        assert!(one > bare, "the footer takes room: {bare} -> {one}");
        // Влезает в обычный экран, но не меньше заголовка с кнопками.
        assert!((TITLE_BAR + BUTTON_HEIGHT..900.0).contains(&six), "{six}");
    }

    #[test]
    fn a_long_unbroken_path_wraps_instead_of_widening_the_window() {
        let narrow = measure(&spec(1, ""));
        let mut long = spec(1, "");
        long.items = vec!["x".repeat(400)];
        assert!(measure(&long) > narrow + 30.0);
    }
}
