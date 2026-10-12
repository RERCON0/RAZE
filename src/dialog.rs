//! Вопрос или сообщение из контекстного меню.
//!
//! У `raze.exe` нет графики, а процесс из меню отсоединён от консоли, поэтому
//! сказать что-то человеку он может только окном. Рисует его `raze-app.exe` в
//! режиме `--dialog`: тот же шрифт, палитра и рамка, что у окна приложения. Тут
//! — только описание окна и его передача через командную строку; обе стороны
//! пользуются одним кодом, и разойтись им нечему.

use std::ffi::{OsStr, OsString};

/// Человек нажал главную кнопку.
pub const EXIT_YES: i32 = 0;
/// Человек отказался или закрыл окно.
pub const EXIT_NO: i32 = 1;
/// Окно показать не вышло (нет OpenGL, повреждённые аргументы): ответа нет, и
/// вызывающий обязан спросить иначе, а не считать молчание отказом.
pub const EXIT_UNAVAILABLE: i32 = 2;

/// Цвет смысла, как в окне приложения: янтарный — отложенное, красный —
/// необратимое и неудача.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Warning,
    Error,
}

/// Всё, что окну нужно показать. Тексты уже на языке интерфейса: у окна своей
/// локализации нет.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// Главная фраза.
    pub head: String,
    /// Список под ней, по строке на элемент.
    pub items: Vec<String>,
    /// Замечание под списком, цветом тона; может быть пустым.
    pub foot: String,
    /// Подпись главной кнопки.
    pub yes: String,
    /// Подпись кнопки отказа. `None` — окно только сообщает, кнопка одна.
    pub no: Option<String>,
    pub tone: Tone,
}

const MODE: &str = "--dialog";

impl Spec {
    /// Аргументы для `raze-app.exe`. Список идёт после `--`: элемент может
    /// начинаться с дефиса.
    pub fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            MODE.into(),
            "--tone".into(),
            match self.tone {
                Tone::Warning => "warning",
                Tone::Error => "error",
            }
            .into(),
            "--head".into(),
            (&self.head).into(),
            "--foot".into(),
            (&self.foot).into(),
            "--yes".into(),
            (&self.yes).into(),
        ];
        if let Some(no) = &self.no {
            args.extend(["--no".into(), no.into()]);
        }
        args.push("--".into());
        args.extend(self.items.iter().map(OsString::from));
        args
    }

    /// Обратное к [`Spec::to_args`]; `args` — всё после имени программы.
    /// Незнакомое или неполное — `None`: окно с половиной вопроса хуже, чем
    /// никакого.
    pub fn parse(args: &[OsString]) -> Option<Self> {
        let mut args = args.iter();
        if args.next()? != OsStr::new(MODE) {
            return None;
        }
        let (mut tone, mut head, mut foot, mut yes, mut no) = (None, None, None, None, None);
        let mut items = Vec::new();
        while let Some(flag) = args.next() {
            if flag == OsStr::new("--") {
                items = args
                    .map(|item| item.to_string_lossy().into_owned())
                    .collect();
                break;
            }
            let value = args.next()?.to_string_lossy().into_owned();
            match flag.to_str()? {
                "--tone" => {
                    tone = Some(match value.as_str() {
                        "warning" => Tone::Warning,
                        "error" => Tone::Error,
                        _ => return None,
                    })
                }
                "--head" => head = Some(value),
                "--foot" => foot = Some(value),
                "--yes" => yes = Some(value),
                "--no" => no = Some(value),
                _ => return None,
            }
        }
        Some(Self {
            head: head?,
            items,
            foot: foot?,
            yes: yes?,
            no,
            tone: tone?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Spec {
        Spec {
            head: "3 target(s) could not be sent to the Recycle Bin.".into(),
            items: vec![
                r"C:\Users\me\Desktop\Seller".into(),
                "-starts-with-a-dash".into(),
                "quote \" and space".into(),
                "… и ещё 4".into(),
            ],
            foot: "Deleting them permanently cannot be undone.".into(),
            yes: "delete permanently".into(),
            no: Some("leave as is".into()),
            tone: Tone::Warning,
        }
    }

    #[test]
    fn a_dialog_survives_the_trip_through_the_command_line() {
        let spec = sample();
        assert_eq!(Spec::parse(&spec.to_args()), Some(spec));

        let notice = Spec {
            no: None,
            tone: Tone::Error,
            items: Vec::new(),
            foot: String::new(),
            ..sample()
        };
        assert_eq!(Spec::parse(&notice.to_args()), Some(notice));
    }

    #[test]
    fn half_a_question_is_not_shown() {
        let args = sample().to_args();
        // Нет главной кнопки.
        let without_yes: Vec<OsString> = args
            .iter()
            .enumerate()
            .filter(|(index, _)| !(7..=8).contains(index))
            .map(|(_, arg)| arg.clone())
            .collect();
        assert_eq!(Spec::parse(&without_yes), None);
        // Незнакомый флаг, неизвестный тон, не тот режим, пустота.
        let mut unknown = args.clone();
        unknown.insert(1, "--bogus".into());
        unknown.insert(2, "x".into());
        assert_eq!(Spec::parse(&unknown), None);
        let mut tone = args.clone();
        tone[2] = "loud".into();
        assert_eq!(Spec::parse(&tone), None);
        assert_eq!(Spec::parse(&args[1..]), None);
        assert_eq!(Spec::parse(&[]), None);
        // Флаг без значения.
        assert_eq!(Spec::parse(&args[..2]), None);
    }
}
