use super::*;
use egui::{Align, Color32, RichText, Stroke};
use raze_rs::engine::scan::EntryKind;
use raze_rs::scrollbar::ScrollAreaExt;

impl RazeApp {
    pub(super) fn render(&mut self, ctx: &egui::Context) {
        self.title_bar(ctx);
        // Panels must be declared before CentralPanel, which takes the space left.
        egui::TopBottomPanel::bottom("footer")
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(egui::Margin::symmetric(26.0, 10.0)),
            )
            .show_separator_line(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.weak(RichText::new(format!("raze {VERSION} — by rercon prod.")).size(11.0));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.hyperlink_to(RichText::new("t.me/rercon").size(11.0), TELEGRAM_URL);
                    });
                });
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(26.0, 18.0)),
            )
            .show(ctx, |ui| {
                if matches!(self.phase, Phase::Confirming(_) | Phase::ConfirmingFallback) {
                    ui.disable();
                }
                let width = ui.available_width().min(style::CONTENT_WIDTH);
                ui.with_layout(egui::Layout::top_down(Align::Center), |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, ui.available_height()),
                        egui::Layout::top_down(Align::Min),
                        |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("main-scroll")
                                .auto_shrink([false, false])
                                .show_terminal(ui, |ui| {
                                    style::wordmark(ui);
                                    self.render_path(ui);
                                    ui.add_space(16.0);
                                    style::rule(ui);
                                    ui.add_space(14.0);
                                    self.render_targets(ui);
                                    ui.add_space(18.0);
                                    self.render_actions(ui);
                                    ui.add_space(14.0);
                                    self.render_log(ui);
                                });
                        },
                    );
                });
            });
        match self.phase {
            Phase::Confirming(confirm) => self.render_confirm(ctx, confirm),
            Phase::ConfirmingFallback => self.render_fallback(ctx),
            _ => {}
        }
        #[cfg(windows)]
        self.resize_edges(ctx);
    }

    fn title_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("titlebar")
            .exact_height(42.0)
            .show_separator_line(true)
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(egui::Margin::symmetric(14.0, 0.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let width = (ui.available_width() - 3.0 * 40.0 - style::THEME_BTN_WIDTH - 40.0)
                        .max(0.0);
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(width, 42.0),
                        egui::Sense::click_and_drag(),
                    );
                    ui.painter().text(
                        rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        "RAZE — by rercon prod.",
                        egui::FontId::new(13.0, egui::FontFamily::Name("title".into())),
                        ui.visuals().weak_text_color(),
                    );
                    if response.double_clicked() {
                        self.toggle_maximized(ctx);
                    } else if response.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    // Both buttons name what a click switches *to*, as in SNATCH.
                    if ui
                        .add_enabled(
                            !self.phase.working(),
                            egui::Button::new(tr!("RU", "EN"))
                                .frame(false)
                                .min_size(egui::vec2(40.0, 38.0)),
                        )
                        .on_hover_text(tr!("Switch to Russian", "Переключить на английский"))
                        .clicked()
                    {
                        let language = i18n::language().toggled();
                        i18n::set_language(language);
                        if let Err(error) = i18n::save_language(language) {
                            self.status_error = true;
                            self.status = Some(tr_format!(
                                "Could not save language: {error}",
                                "Не удалось сохранить язык: {error}"
                            ));
                        }
                    }
                    if ui
                        .add_sized(
                            [style::THEME_BTN_WIDTH, 38.0],
                            egui::Button::new(if self.dark {
                                tr!("light", "день")
                            } else {
                                tr!("dark", "ночь")
                            })
                            .frame(false),
                        )
                        .on_hover_text(if self.dark {
                            tr!("Light theme", "Светлая тема")
                        } else {
                            tr!("Dark theme", "Тёмная тема")
                        })
                        .clicked()
                    {
                        self.dark = !self.dark;
                        style::setup_theme(ctx, self.dark);
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("─").frame(false))
                        .on_hover_text(tr!("Minimize", "Свернуть"))
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("□").frame(false))
                        .on_hover_text(tr!("Maximize / restore", "Развернуть / восстановить"))
                        .clicked()
                    {
                        self.toggle_maximized(ctx);
                    }
                    if ui
                        .add_enabled(
                            !self.phase.working(),
                            egui::Button::new("×")
                                .frame(false)
                                .min_size(egui::vec2(40.0, 38.0)),
                        )
                        .on_hover_text(tr!(
                            "Close (after the current operation)",
                            "Закрыть (после текущей операции)"
                        ))
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
    }

    fn toggle_maximized(&mut self, ctx: &egui::Context) {
        self.maximized = !ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized));
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.maximized));
    }

    fn render_path(&mut self, ui: &mut egui::Ui) {
        style::tag(ui, tr!("target", "цель"));
        let idle = self.phase == Phase::Idle;
        let mut submit = false;
        ui.add_enabled_ui(idle, |ui| {
            ui.horizontal(|ui| {
                let button_width = 104.0;
                let field_width =
                    (ui.available_width() - button_width - ui.spacing().item_spacing.x).max(40.0);
                let response = ui.add_sized(
                    [field_width, style::CONTROL_HEIGHT],
                    // По умолчанию текст прижат к верху поля (LEFT_TOP), а поле
                    // растянуто до 40 pt: подпись и ввод стояли выше центра.
                    egui::TextEdit::singleline(&mut self.typed)
                        .font(style::field_font())
                        .vertical_align(Align::Center)
                        .margin(egui::vec2(12.0, 10.0))
                        .hint_text(tr!(
                            "C:\\path\\to\\stubborn-folder",
                            "C:\\путь\\к\\сложной-папке"
                        )),
                );
                submit = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let add = ui.add_enabled(
                    !self.typed.trim().is_empty(),
                    egui::Button::new(tr!("add", "добавить"))
                        .min_size(egui::vec2(button_width, style::CONTROL_HEIGHT)),
                );
                submit |= add.clicked();
            });
            ui.horizontal_wrapped(|ui| {
                if ui.button(tr!("folder…", "папка…")).clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.add_paths(ui.ctx(), vec![path]);
                    }
                }
                if ui.button(tr!("files…", "файлы…")).clicked() {
                    if let Some(paths) = rfd::FileDialog::new().pick_files() {
                        self.add_paths(ui.ctx(), paths);
                    }
                }
                ui.weak(
                    RichText::new(tr!(
                        "or drop them into this window",
                        "или перетащите их в окно"
                    ))
                    .size(11.5),
                );
            });
        });
        if submit {
            self.add_typed(ui.ctx());
        }
    }

    fn render_targets(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            style::tag(
                ui,
                &tr_format!(
                    "targets / {count}",
                    "цели / {count}",
                    count = self.plan.len()
                ),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(
                        self.phase == Phase::Idle && !self.plan.is_empty(),
                        egui::Button::new(tr!("clear", "очистить")).frame(false),
                    )
                    .clicked()
                {
                    self.plan.clear();
                    self.rows.clear();
                    self.status = None;
                }
                if ui
                    .add_enabled(
                        self.phase == Phase::Idle && !self.plan.is_empty(),
                        egui::Button::new(tr!("all", "все")).frame(false),
                    )
                    .clicked()
                {
                    let select = self
                        .plan
                        .iter()
                        .zip(&self.rows)
                        .any(|(t, r)| t.refused.is_none() && !r.selected);
                    for (t, r) in self.plan.iter().zip(&mut self.rows) {
                        r.selected = select && t.refused.is_none();
                    }
                }
            });
        });
        let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        if self.plan.is_empty() {
            egui::Frame::none()
                .stroke(Stroke::new(
                    1.0,
                    if hovering {
                        ui.visuals().hyperlink_color
                    } else {
                        ui.visuals().widgets.noninteractive.bg_stroke.color
                    },
                ))
                .inner_margin(egui::Margin::symmetric(18.0, 16.0))
                .show(ui, |ui| {
                    ui.set_min_width((ui.available_width()).max(0.0));
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(if self.phase == Phase::Planning {
                                "…"
                            } else {
                                "[ + ]"
                            })
                            .size(22.0)
                            .color(ui.visuals().hyperlink_color),
                        );
                        ui.label(tr!(
                            "Drop files or folders here",
                            "Перетащите сюда файлы или папки"
                        ));
                        ui.weak(
                            RichText::new(tr!(
                                "Choose exactly what to remove. Nothing is deleted on drop.",
                                "Выберите, что удалить. Перетаскивание ничего не удаляет."
                            ))
                            .size(11.5),
                        );
                    });
                });
        }
        for index in 0..self.plan.len() {
            let target = &self.plan[index];
            let mut row = self.rows[index];
            let blocked = target.refused.is_some();
            let color = if blocked {
                ui.visuals().error_fg_color
            } else if row.selected {
                ui.visuals().hyperlink_color
            } else {
                ui.visuals().widgets.inactive.bg_stroke.color
            };
            let card = egui::Frame::none()
                .fill(ui.visuals().window_fill)
                .stroke(Stroke::new(
                    1.0,
                    ui.visuals().widgets.inactive.bg_stroke.color,
                ))
                .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(self.phase == Phase::Idle && !blocked, |ui| {
                            ui.checkbox(&mut row.selected, tr!("select", "выбрать"));
                        });
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    !blocked,
                                    egui::Button::new(if row.expanded { "▾" } else { "▸" })
                                        .frame(false),
                                )
                                .on_hover_text(tr!(
                                    "Show contents and blockers",
                                    "Показать содержимое и проблемы"
                                ))
                                .clicked()
                            {
                                row.expanded = !row.expanded;
                            }
                            let name = target
                                .display
                                .trim_end_matches(['\\', '/'])
                                .rsplit(['\\', '/'])
                                .next()
                                .unwrap_or(&target.display);
                            ui.add(
                                egui::Label::new(RichText::new(name).color(if blocked {
                                    color
                                } else {
                                    ui.visuals().text_color()
                                }))
                                .truncate(),
                            )
                            .on_hover_text(&target.display);
                        });
                    });
                    ui.add(
                        egui::Label::new(RichText::new(&target.display).size(11.5).weak())
                            .truncate(),
                    )
                    .on_hover_text(&target.display);
                    if let Some(reason) = &target.refused {
                        ui.colored_label(ui.visuals().error_fg_color, reason);
                    } else {
                        ui.horizontal_wrapped(|ui| {
                            ui.weak(
                                RichText::new(tr_format!(
                                    "{files} files · {dirs} folders · {size}",
                                    "{files} файлов · {dirs} папок · {size}",
                                    files = target.summary.files,
                                    dirs = target.summary.directories,
                                    size = human_bytes(target.summary.bytes)
                                ))
                                .size(11.5),
                            );
                            let (badge, badge_color) = match target.mode {
                                Disposition::RecycleBin => {
                                    (tr!("recycle", "корзина"), ui.visuals().weak_text_color())
                                }
                                Disposition::Permanent => {
                                    (tr!("permanent", "навсегда"), ui.visuals().warn_fg_color)
                                }
                                Disposition::Reboot => (
                                    tr!("at next reboot", "при перезагрузке"),
                                    ui.visuals().warn_fg_color,
                                ),
                            };
                            style::badge(ui, badge, badge_color)
                                .on_hover_text(&target.verdict.reason);
                            if target.summary.troubled > 0 {
                                style::badge(
                                    ui,
                                    &tr_format!(
                                        "issues: {n}",
                                        "проблем: {n}",
                                        n = target.summary.troubled
                                    ),
                                    ui.visuals().warn_fg_color,
                                );
                            }
                        });
                        if !target.verdict.reusable {
                            ui.label(
                                RichText::new(&target.verdict.reason)
                                    .size(11.5)
                                    .color(ui.visuals().warn_fg_color),
                            );
                        }
                        if target.summary.truncated {
                            ui.colored_label(
                                ui.visuals().warn_fg_color,
                                tr!(
                                    "Scan is incomplete; counts are partial.",
                                    "Обход неполный; счётчики предварительные."
                                ),
                            );
                        }
                        if row.expanded {
                            ui.add_space(8.0);
                            style::rule(ui);
                            render_entries(ui, target, index);
                            if target.summary.unlisted > 0 {
                                ui.weak(
                                    RichText::new(tr_format!(
                                        "… and {n} more not listed",
                                        "… и ещё {n} не показано",
                                        n = target.summary.unlisted
                                    ))
                                    .size(11.5),
                                );
                            }
                        }
                    }
                });
            ui.painter().vline(
                card.response.rect.left() + 0.5,
                card.response.rect.y_range(),
                Stroke::new(2.0, color),
            );
            self.rows[index] = row;
            ui.add_space(4.0);
        }
    }

    fn render_actions(&mut self, ui: &mut egui::Ui) {
        let total = self.totals();
        let idle = self.phase == Phase::Idle;
        let enabled = idle && total.targets > 0;
        style::tag(ui, tr!("removal mode", "режим удаления"));
        ui.add_enabled_ui(idle, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (mode, label) in [
                    (
                        Confirm::Recycle,
                        tr!("recycle if possible", "корзина, если возможно"),
                    ),
                    (Confirm::Reboot, tr!("at next reboot", "при перезагрузке")),
                    (Confirm::Permanent, tr!("permanent", "безвозвратно")),
                ] {
                    // Необратимые и отложенные режимы читаются тревожно, когда выбраны.
                    let active = match mode {
                        Confirm::Recycle => ui.visuals().hyperlink_color,
                        Confirm::Reboot => ui.visuals().warn_fg_color,
                        Confirm::Permanent => ui.visuals().error_fg_color,
                    };
                    let response = style::choice(ui, label, self.mode == mode, active);
                    if response.clicked() {
                        self.mode = mode;
                    }
                }
            });
            ui.add_space(4.0);
            ui.checkbox(
                &mut self.close_locks,
                tr!(
                    "close programs holding files",
                    "закрывать программы, держащие файлы"
                ),
            )
            .on_hover_text(tr!(
                "Requests a normal close, not a forced kill. Save your work first.",
                "Запрашивает штатное закрытие, без принудительного завершения. Сохраните работу."
            ));
        });
        ui.add_space(8.0);
        let warning = if self.mode == Confirm::Permanent {
            tr!(
                "No Recycle Bin. This cannot be undone.",
                "Без Корзины. Отменить удаление нельзя."
            )
            .to_string()
        } else if self.mode == Confirm::Reboot {
            tr!(
                "Objects will be deleted at the next Windows restart. Nothing changes until then.",
                "Объекты будут удалены при следующей перезагрузке Windows. До неё ничего не меняется."
            )
            .to_string()
        } else if total.permanent > 0 {
            tr_format!(
                "Cannot be recycled: {n} target(s). These will be deleted permanently.",
                "Не попадут в Корзину: {n} целей. Они будут удалены навсегда.",
                n = total.permanent
            )
        } else {
            tr!(
                "The plan will be shown again before deletion.",
                "Перед удалением ещё раз покажу выбранные цели."
            )
            .to_string()
        };
        let warning_color = match self.mode {
            Confirm::Permanent => ui.visuals().error_fg_color,
            Confirm::Reboot => ui.visuals().warn_fg_color,
            Confirm::Recycle if total.permanent > 0 => ui.visuals().warn_fg_color,
            Confirm::Recycle => ui.visuals().weak_text_color(),
        };
        ui.label(RichText::new(warning).size(11.5).color(warning_color));
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(tr_format!(
                "Selected: {n} · {size}",
                "Выбрано: {n} · {size}",
                n = total.targets,
                size = human_bytes(total.bytes)
            ));
            ui.weak(
                RichText::new(tr_format!(
                    "issues: {n}",
                    "проблем: {n}",
                    n = total.troubled
                ))
                .size(11.5),
            );
        });
        ui.add_space(4.0);
        let preview = ui.add_enabled(
            enabled,
            egui::Button::new(tr!("preview — no deletion", "проверить — без удаления")),
        );
        if preview.clicked() {
            self.start_removal(
                ui.ctx(),
                Options {
                    dry_run: true,
                    ..self.base_options()
                },
            );
        }
        ui.add_space(6.0);
        // A disabled button keeps its accent and egui fades it: painting it in the
        // hairline colour made the main action all but invisible until a target was added.
        let color = match self.mode {
            Confirm::Permanent => ui.visuals().error_fg_color,
            Confirm::Reboot => ui.visuals().warn_fg_color,
            Confirm::Recycle => ui.visuals().hyperlink_color,
        };
        // add_sized centres the label, as SNATCH's "▶ DOWNLOAD"; min_size would leave it left-aligned.
        let primary = ui
            .add_enabled_ui(enabled, |ui| {
                ui.add_sized(
                    [ui.available_width(), 44.0],
                    style::accent_button(tr!("▶ DELETE SELECTED", "▶ УДАЛИТЬ ВЫБРАННОЕ"), color),
                )
            })
            .inner;
        if primary.clicked() {
            self.phase = Phase::Confirming(self.mode);
        }
        if self.phase.working() {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                style::slow_spinner(ui);
                ui.label(if self.phase == Phase::Planning {
                    tr!("Scanning…", "Сканирую…")
                } else {
                    tr!(
                        "Removing… Keep the window open.",
                        "Удаляю… Не закрывайте окно."
                    )
                });
            });
        }
        if let Some(status) = &self.status {
            ui.add_space(8.0);
            ui.colored_label(
                if self.status_error {
                    ui.visuals().error_fg_color
                } else {
                    ui.visuals().hyperlink_color
                },
                status,
            );
        }
    }

    fn render_log(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(format!(
                        "{} {}",
                        if self.show_log { "▾" } else { "▸" },
                        tr!("log", "журнал")
                    ))
                    .frame(false),
                )
                .clicked()
            {
                self.show_log = !self.show_log;
            }
            if !self.log.is_empty() {
                ui.weak(RichText::new(self.log.len().to_string()).size(11.0));
            }
        });
        if self.show_log {
            egui::Frame::none()
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .inner_margin(egui::Margin::same(10.0))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("log-scroll")
                        .max_height(160.0)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show_terminal(ui, |ui| {
                            if self.log.is_empty() {
                                ui.weak(tr!(
                                    "Operations will appear here.",
                                    "Здесь появится отчёт о действиях."
                                ));
                            }
                            for line in &self.log {
                                ui.label(RichText::new(line).monospace().size(11.5));
                            }
                        });
                });
        }
    }

    fn render_confirm(&mut self, ctx: &egui::Context, confirm: Confirm) {
        let screen = ctx.screen_rect();
        let layer = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("modal-dimmer"),
        ));
        layer.rect_filled(
            screen,
            0.0,
            Color32::from_black_alpha(if self.dark { 170 } else { 75 }),
        );
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.phase = Phase::Idle;
            return;
        }
        let total = self.totals();
        let width = (screen.width() - 80.0).min(440.0);
        // Foreground: the dimmer above is a bare layer and would otherwise be
        // drawn over the dialog itself, dimming the one thing the user must read.
        egui::Window::new(style::window_title(tr!(
            "Confirm deletion",
            "Подтвердите удаление"
        )))
        .id(egui::Id::new("delete-confirmation"))
        .order(egui::Order::Foreground)
        .collapsible(false)
        .resizable(false)
        .movable(false)
        .default_width(width)
        .max_width(width)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(
                RichText::new(tr_format!(
                    "Targets: {n} · {size}",
                    "Целей: {n} · {size}",
                    n = total.targets,
                    size = human_bytes(total.bytes)
                ))
                .size(15.0),
            );
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .id_salt("confirm-targets")
                .max_height((screen.height() * 0.25).min(180.0))
                .show_terminal(ui, |ui| {
                    for (target, row) in self.plan.iter().zip(&self.rows) {
                        if !row.selected || target.refused.is_some() {
                            continue;
                        }
                        ui.add(egui::Label::new(&target.display).truncate())
                            .on_hover_text(&target.display);
                        let (line, line_color) = match confirm {
                            Confirm::Reboot => (
                                tr!("→ at next reboot", "→ при следующей перезагрузке"),
                                ui.visuals().warn_fg_color,
                            ),
                            Confirm::Permanent => (
                                tr!("→ permanent", "→ навсегда"),
                                ui.visuals().error_fg_color,
                            ),
                            Confirm::Recycle if !target.verdict.reusable => (
                                tr!("→ permanent", "→ навсегда"),
                                ui.visuals().error_fg_color,
                            ),
                            Confirm::Recycle => (
                                tr!("→ Recycle Bin", "→ в Корзину"),
                                ui.visuals().hyperlink_color,
                            ),
                        };
                        ui.label(RichText::new(line).size(11.5).color(line_color));
                        ui.add_space(4.0);
                    }
                });
            ui.add_space(8.0);
            let irreversible = confirm == Confirm::Permanent || total.permanent > 0;
            let (warning, warning_color) = if confirm == Confirm::Reboot {
                (
                    tr!(
                        "Objects will be deleted at the next Windows restart. Until then nothing changes.",
                        "Объекты будут удалены при следующей перезагрузке Windows. До неё ничего не меняется."
                    ),
                    ui.visuals().warn_fg_color,
                )
            } else if irreversible {
                (
                    tr!(
                        "Permanently deleted objects cannot be restored from the Recycle Bin.",
                        "Удалённые навсегда объекты нельзя восстановить из Корзины."
                    ),
                    ui.visuals().error_fg_color,
                )
            } else {
                (
                    tr!(
                        "Selected targets will be sent to the Recycle Bin.",
                        "Выбранные цели будут отправлены в Корзину."
                    ),
                    ui.visuals().weak_text_color(),
                )
            };
            ui.label(RichText::new(warning).color(warning_color));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 8.0) / 2.0;
                if ui
                    .add_sized([width, 40.0], egui::Button::new(tr!("cancel", "отмена")))
                    .clicked()
                {
                    self.phase = Phase::Idle;
                }
                let color = match confirm {
                    Confirm::Permanent => ui.visuals().error_fg_color,
                    Confirm::Reboot => ui.visuals().warn_fg_color,
                    Confirm::Recycle if irreversible => ui.visuals().error_fg_color,
                    Confirm::Recycle => ui.visuals().hyperlink_color,
                };
                if ui
                    .add_sized(
                        [width, 40.0],
                        style::accent_button(tr!("delete", "удалить"), color),
                    )
                    .clicked()
                {
                    self.start_removal(
                        ctx,
                        Options {
                            permanent: confirm == Confirm::Permanent,
                            ..self.base_options()
                        },
                    );
                }
            });
        });
    }

    /// Корзина отказала: ничего не удалено, спросить о безвозвратном удалении.
    /// Те же цели, что не приняла оболочка, — остальные в плане не тронуты.
    fn render_fallback(&mut self, ctx: &egui::Context) {
        let screen = ctx.screen_rect();
        let layer = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("modal-dimmer"),
        ));
        layer.rect_filled(
            screen,
            0.0,
            Color32::from_black_alpha(if self.dark { 170 } else { 75 }),
        );
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.dismiss_fallback();
            return;
        }
        let width = (screen.width() - 80.0).min(440.0);
        egui::Window::new(style::window_title(tr!(
            "Recycle Bin refused",
            "Корзина отказала"
        )))
        .id(egui::Id::new("bin-refused"))
        .order(egui::Order::Foreground)
        .collapsible(false)
        .resizable(false)
        .movable(false)
        .default_width(width)
        .max_width(width)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(
                RichText::new(tr_format!(
                    "{n} target(s) could not be sent to the Recycle Bin. Nothing has been deleted.",
                    "{n} целей не удалось отправить в Корзину. Ничего не удалено.",
                    n = self.fallback.len()
                ))
                .size(15.0),
            );
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .id_salt("fallback-targets")
                .max_height((screen.height() * 0.25).min(180.0))
                .show_terminal(ui, |ui| {
                    for target in &self.plan {
                        let listed = target
                            .path
                            .as_ref()
                            .is_some_and(|p| self.fallback.contains(&p.to_path()));
                        if !listed {
                            continue;
                        }
                        ui.add(egui::Label::new(&target.display).truncate())
                            .on_hover_text(&target.display);
                        ui.add_space(4.0);
                    }
                });
            ui.add_space(8.0);
            ui.label(
                RichText::new(tr!(
                    "Deleting them permanently cannot be undone.",
                    "Безвозвратное удаление отменить нельзя."
                ))
                .color(ui.visuals().error_fg_color),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 8.0) / 2.0;
                if ui
                    .add_sized(
                        [width, 40.0],
                        egui::Button::new(tr!("leave as is", "оставить как есть")),
                    )
                    .clicked()
                {
                    self.dismiss_fallback();
                }
                if ui
                    .add_sized(
                        [width, 40.0],
                        style::accent_button(
                            tr!("delete permanently", "удалить безвозвратно"),
                            ui.visuals().error_fg_color,
                        ),
                    )
                    .clicked()
                {
                    self.confirm_fallback(ctx);
                }
            });
        });
    }

    #[cfg(windows)]
    fn resize_edges(&self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized)) {
            return;
        }
        let screen = ctx.screen_rect();
        let (w, h, e) = (screen.width(), screen.height(), 5.0);
        for (index, (pos, size, direction)) in [
            ([0.0, 0.0], [e, e], egui::ResizeDirection::NorthWest),
            ([e, 0.0], [w - 2.0 * e, e], egui::ResizeDirection::North),
            ([w - e, 0.0], [e, e], egui::ResizeDirection::NorthEast),
            ([0.0, e], [e, h - 2.0 * e], egui::ResizeDirection::West),
            ([w - e, e], [e, h - 2.0 * e], egui::ResizeDirection::East),
            ([0.0, h - e], [e, e], egui::ResizeDirection::SouthWest),
            ([e, h - e], [w - 2.0 * e, e], egui::ResizeDirection::South),
            ([w - e, h - e], [e, e], egui::ResizeDirection::SouthEast),
        ]
        .into_iter()
        .enumerate()
        {
            egui::Area::new(egui::Id::new(("resize", index)))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(pos[0], pos[1]))
                .show(ctx, |ui| {
                    let (_, r) =
                        ui.allocate_exact_size(egui::vec2(size[0], size[1]), egui::Sense::drag());
                    if r.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
                    }
                });
        }
    }
}

fn render_entries(ui: &mut egui::Ui, target: &Target, index: usize) {
    if target.entries.is_empty() {
        ui.weak(tr!("No child entries.", "Нет вложенных объектов."));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt(("entries", index))
        .max_height(220.0)
        .auto_shrink([false, false])
        .show_terminal_rows(ui, 46.0, target.entries.len(), |ui, range| {
            for entry in &target.entries[range] {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 46.0),
                    egui::Layout::top_down(Align::Min),
                    |ui| {
                        let name = match entry.kind {
                            EntryKind::File => tr!("file", "файл"),
                            EntryKind::Directory => tr!("folder", "папка"),
                            EntryKind::Link => tr!("link", "ссылка"),
                        };
                        ui.add(
                            egui::Label::new(RichText::new(format!("{name} / {}", entry.name)))
                                .truncate(),
                        )
                        .on_hover_text(entry.path.to_string_lossy());
                        let text = if entry.blockers.is_empty() {
                            human_bytes(entry.size)
                        } else {
                            entry
                                .blockers
                                .iter()
                                .map(|b| b.label())
                                .collect::<Vec<_>>()
                                .join(" · ")
                        };
                        ui.add(
                            egui::Label::new(RichText::new(&text).size(11.0).color(
                                if entry.has_trouble() {
                                    ui.visuals().warn_fg_color
                                } else {
                                    ui.visuals().weak_text_color()
                                },
                            ))
                            .truncate(),
                        )
                        .on_hover_text(text);
                    },
                );
            }
        });
}
