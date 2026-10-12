//! Пути Windows: расширенная форма `\\?\`, резервированные имена, длинные пути.
//!
//! Почему это отдельный модуль: почти каждая лишняя строка здесь — это
//! предотвращённая ошибка удаления. `\\?\` отключает нормализацию имён
//! (никакого `NUL`, никаких хвостовых точек, никакого лимита 260), поэтому
//! путь обязан быть абсолютным и собранным нами, а не «сгенерированным
//! системой».

use std::ffi::OsString;
use std::fmt;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::GetFullPathNameW;

/// Классический лимит пути Win32 без префикса `\\?\`.
pub const MAX_PATH: usize = 260;

/// С какого числа символов путь считается «длинным» для Корзины, Проводника
/// и прочих потребителей, которые лимит 260 всё ещё соблюдают. Ниже
/// официального порога — предупреждаем заранее.
pub const LONG_PATH_WARN: usize = 240;

/// Строка UTF-16 для Win32.
///
/// **Инвариант: последний `u16` всегда равен нулю.** Win32 читает
/// PCWSTR до первого NUL, поэтому буфер без терминатора — это
/// чтение за пределами выделенной памяти (UB). Инвариант держится в
/// конструкторах и в [`Wide::join`]; наружу отдаётся либо
/// [`Wide::as_slice`] (содержимое без терминатора), либо
/// [`Wide::pc`] (указатель, пригодный для передачи в API).
#[derive(Clone, PartialEq, Eq)]
pub struct Wide(Vec<u16>);

impl Default for Wide {
    fn default() -> Self {
        Self(vec![0])
    }
}

impl Wide {
    pub fn new(text: &str) -> Self {
        let mut buffer: Vec<u16> = text.encode_utf16().collect();
        buffer.push(0);
        Self(buffer)
    }

    pub fn from_path(path: &Path) -> Self {
        let mut buffer: Vec<u16> = path.as_os_str().encode_wide().collect();
        buffer.push(0);
        Self(buffer)
    }

    fn from_units(units: &[u16]) -> Self {
        Self::prefixed("", units)
    }

    fn prefixed(prefix: &str, units: &[u16]) -> Self {
        let mut buffer = Vec::with_capacity(prefix.len() + units.len() + 1);
        buffer.extend(prefix.encode_utf16());
        buffer.extend_from_slice(units);
        buffer.push(0);
        Self(buffer)
    }

    /// Содержимое строки **без** завершающего NUL.
    pub fn as_slice(&self) -> &[u16] {
        &self.0[..self.0.len().saturating_sub(1)]
    }

    /// Указатель, безопасный для передачи в Win32: NUL на месте.
    pub fn as_ptr(&self) -> *const u16 {
        self.0.as_ptr()
    }

    pub fn pc(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }

    pub fn len(&self) -> usize {
        self.0.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Длина корневой части пути в UTF-16 — компоненты начинаются
    /// после неё. `\\?\UNC\server\` — после сервера (шара уже
    /// компонент), `\\?\` — сам префикс, `\\server\share\` — для
    /// обычных UNC, `C:\` — для дисков.
    pub fn drive_prefix_len(&self) -> Option<usize> {
        const BSLASH: u16 = b'\\' as u16;
        let s = self.as_slice();
        if s.is_empty() {
            return None;
        }
        // \\?\UNC\server\
        if s.len() >= 8
            && s[..4] == [BSLASH, BSLASH, b'?' as u16, BSLASH]
            && (s[4] | 0x20) == b'u' as u16
            && (s[5] | 0x20) == b'n' as u16
            && (s[6] | 0x20) == b'c' as u16
            && s[7] == BSLASH
        {
            return Some(match s[8..].iter().position(|c| *c == BSLASH) {
                Some(pos) => 8 + pos + 1,
                None => s.len(),
            });
        }
        // \\?\
        if s.len() >= 4 && s[..4] == [BSLASH, BSLASH, b'?' as u16, BSLASH] {
            return Some(4);
        }
        // Обычный UNC: \\server\share\
        if s.len() >= 2 && s[..2] == [BSLASH, BSLASH] {
            let mut parts = 0usize;
            for (i, unit) in s.iter().enumerate().skip(2) {
                if *unit == BSLASH {
                    parts += 1;
                    if parts == 2 {
                        return Some(i + 1);
                    }
                }
            }
            return Some(s.len());
        }
        // Диск: C:\ или C:
        if s.len() >= 2
            && s[1] == b':' as u16
            && (s[0] | 0x20) >= b'a' as u16
            && (s[0] | 0x20) <= b'z' as u16
        {
            return Some(if s.len() >= 3 && s[2] == BSLASH { 3 } else { 2 });
        }
        None
    }

    /// Дописать путь относительно текущего: `self\` + `child`.
    pub fn join(&self, child: &[u16]) -> Self {
        let mut out = self.0.clone();
        out.pop(); // снимаем NUL — он вернётся в конце
        if !out.is_empty() && *out.last().unwrap() != b'\\' as u16 {
            out.push(b'\\' as u16);
        }
        out.extend_from_slice(child);
        out.push(0);
        Self(out)
    }

    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(self.as_slice())
    }

    pub fn to_os_string(&self) -> OsString {
        OsString::from_wide(self.as_slice())
    }

    pub fn to_path(&self) -> PathBuf {
        PathBuf::from(OsString::from_wide(self.as_slice()))
    }

    /// Список путей для `SHFileOperationW`: каждый путь,
    /// заканчивающийся дополнительным NUL, и общий NUL в конце.
    pub fn nul_terminated_list(paths: &[Wide]) -> Vec<u16> {
        let mut out = Vec::new();
        for path in paths {
            out.extend_from_slice(path.as_slice());
            out.push(0);
        }
        out.push(0);
        out
    }
}

impl fmt::Debug for Wide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

impl From<&str> for Wide {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Расширенный путь: `ext` уходит в Win32, `plain` показывается пользователю.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NtPath {
    ext: Wide,
    plain: Wide,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    Empty,
    /// `\\.\PhysicalDrive0`, `\\.\NUL` и прочие device-пути: их нельзя
    /// отдавать программе удаления — это выход за пределы файловой системы.
    DevicePath,
    /// Что-то вроде `\\server` без общей папки — неполный UNC.
    IncompleteUnc,
    /// Путь не удалось превратить в абсолютный.
    NotAbsolute,
    /// Байты не декодируются как корректный путь Windows.
    NotUtf16,
    /// Символы, которых в именах быть не может: `* ? " < > |`, управляющие и
    /// двоеточие (поток NTFS). Они опасны не сами по себе, а тем, что оболочка
    /// трактует их иначе: `C:\Users\me\*` в Корзину — это маска «все файлы», а
    /// `C:\Windows::$INDEX_ALLOCATION` — та же папка под другим именем.
    InvalidCharacters,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::tr;
        match self {
            Self::Empty => f.write_str(tr!("empty path", "путь пустой")),
            Self::DevicePath => f.write_str(tr!(
                "device paths (\\\\.\\...) are not allowed",
                "device-пути (\\\\.\\...) запрещены"
            )),
            Self::IncompleteUnc => f.write_str(tr!(
                "incomplete UNC path: a share name is required",
                "неполный UNC-путь: нужно указать имя общей папки"
            )),
            Self::NotAbsolute => f.write_str(tr!(
                "path cannot be resolved to absolute",
                "путь не удалось привести к абсолютному"
            )),
            Self::NotUtf16 => f.write_str(tr!(
                "path is not valid UTF-16",
                "путь не является корректным UTF-16"
            )),
            Self::InvalidCharacters => f.write_str(tr!(
                "path contains characters that names cannot have (* ? \" < > | :)",
                "в пути есть символы, которых не бывает в именах (* ? \" < > | :)"
            )),
        }
    }
}

impl std::error::Error for PathError {}

impl NtPath {
    /// Нормализует пользовательский путь и строит его расширенную форму.
    pub fn new(input: &Path) -> Result<Self, PathError> {
        Self::from_wide(&Wide::from_path(input))
    }

    pub fn from_wide(raw: &Wide) -> Result<Self, PathError> {
        let slice = raw.as_slice();
        if slice.is_empty() {
            return Err(PathError::Empty);
        }
        if slice.contains(&0) {
            return Err(PathError::NotUtf16);
        }

        let text = raw.to_string_lossy();
        let first = text.as_bytes()[0];

        // Уже расширенный: принимаем как есть, но проверяем префикс.
        if starts_with_ignore_case(&text, "\\\\?\\") {
            if starts_with_ignore_case(&text, "\\\\?\\UNC\\") {
                let rest = &text[8..];
                if rest.is_empty() || !rest.contains('\\') {
                    return Err(PathError::IncompleteUnc);
                }
                reject_invalid_characters(rest)?;
                // Для показа и сравнения это обычный UNC-путь: `\\server\share`.
                return Ok(Self {
                    plain: normalize_absolute(&Wide::prefixed(r"\\", &slice[8..]), false),
                    ext: raw.clone(),
                });
            }
            // Дальше законна только форма с буквой диска `\\?\C:\...`. Всё
            // остальное (`GLOBALROOT`, `Volume{..}`, `PhysicalDrive0`, `pipe`,
            // голый `\\?\C:` — сам том) — пространство устройств, а не файлы.
            let body = &text[4..];
            if !is_drive_spec(body) {
                return Err(PathError::DevicePath);
            }
            reject_invalid_characters(&body[2..])?;
            return Ok(Self {
                plain: normalize_absolute(&Wide::from_units(&slice[4..]), false),
                ext: raw.clone(),
            });
        }

        // Device-пространство — закрываем на входе.
        if starts_with_ignore_case(&text, "\\\\.\\") {
            return Err(PathError::DevicePath);
        }

        // UNC
        if first == b'\\' && text.as_bytes().get(1) == Some(&b'\\') {
            let rest = &text[2..];
            reject_invalid_characters(rest)?;
            let body = rest.trim_start_matches('\\');
            let has_share = body.split('\\').filter(|s| !s.is_empty()).count() >= 2;
            if !has_share {
                return Err(PathError::IncompleteUnc);
            }
            let plain = normalize_absolute(raw, true);
            // `plain` уже начинается с `\\`, а расширенная форма
            // UNC — `\\?\UNC\server\share`: разделители не дублируем.
            return Ok(Self {
                ext: Wide::prefixed(r"\\?\UNC\", &plain.as_slice()[2..]),
                plain,
            });
        }

        // Диск: `C:\...` или `C:...`
        if text.len() >= 2 && text.as_bytes()[1] == b':' && text.as_bytes()[0].is_ascii_alphabetic()
        {
            reject_invalid_characters(&text[2..])?;
            if text.len() >= 3 && text.as_bytes()[2] == b'\\' {
                let plain = normalize_absolute(raw, true);
                return Ok(Self {
                    ext: Wide::prefixed(r"\\?\", plain.as_slice()),
                    plain,
                });
            }
            if text.len() == 2 {
                // Голый `C:` в Win32 — «текущая папка диска C», а после `\\?\`
                // — сам том как устройство. Ни то ни другое не имеют в виду,
                // поэтому читаем его как корень тома `C:\`: `protect` такую цель
                // отклонит, а не отдаст дальше то, что случайно откроется.
                let plain = normalize_absolute(&Wide::new(&format!("{text}\\")), true);
                return Ok(Self {
                    ext: Wide::prefixed(r"\\?\", plain.as_slice()),
                    plain,
                });
            }
            // `C:foo` — относительно текущей папки диска C. Тут нужен
            // GetFullPathNameW: он знает про per-drive current directory.
            let full = full_path_name(raw)?;
            let plain = normalize_absolute(&Wide::from_path(&full), true);
            return Ok(Self {
                ext: Wide::prefixed(r"\\?\", plain.as_slice()),
                plain,
            });
        }

        // Всё остальное — относительный путь.
        if first == b'\\' {
            return Err(PathError::NotAbsolute);
        }
        reject_invalid_characters(&text)?;
        let full = full_path_name(raw)?;
        let text = full.to_string_lossy();
        if !text.as_bytes().get(1).map(|b| *b == b':').unwrap_or(false) {
            return Err(PathError::NotAbsolute);
        }
        let plain = normalize_absolute(&Wide::from_path(&full), true);
        Ok(Self {
            ext: Wide::prefixed(r"\\?\", plain.as_slice()),
            plain,
        })
    }

    /// Расширенный путь для Win32-вызовов.
    pub fn ext(&self) -> &Wide {
        &self.ext
    }

    /// Обычный путь для показа, сравнения и открытия в Проводнике.
    pub fn plain(&self) -> &Wide {
        &self.plain
    }

    pub fn to_path(&self) -> PathBuf {
        self.plain.to_path()
    }

    pub fn to_string_lossy(&self) -> String {
        self.plain.to_string_lossy()
    }

    /// Родитель на `depth` уровней выше. `depth = 1` — непосредственный
    /// родитель каталога (сам каталог не входит). Подниматься до корня
    /// тома нельзя: у компонентов заканчиваются «родители» раньше.
    pub fn parent(&self, depth: usize) -> Option<Self> {
        const BSLASH: u16 = b'\\' as u16;
        let skip = self.ext.drive_prefix_len()?;
        let s = self.ext.as_slice();
        // Разделитель, завершающий корень тома: `C:\` или `\\srv\shr\`.
        let boundary = if s.len() >= 8
            && s[..4] == [BSLASH, BSLASH, b'?' as u16, BSLASH]
            && (s[4] | 0x20) == b'u' as u16
            && (s[5] | 0x20) == b'n' as u16
            && (s[6] | 0x20) == b'c' as u16
            && s[7] == BSLASH
        {
            // UNC: после имени шары.
            skip + s[skip..].iter().position(|c| *c == BSLASH)?
        } else {
            // Диск: `\\?\` + `X:`.
            skip + 2
        };
        let mut separators = 0usize;
        let mut cut = s.len();
        for i in (skip..s.len()).rev() {
            if s[i] == BSLASH {
                separators += 1;
                if separators == depth {
                    cut = i;
                    break;
                }
            }
        }
        // `cut == s.len()` — уровней меньше, чем запрошено.
        // `cut <= boundary` — упёрлись в корень тома (`C:`, `\\srv`).
        if cut == s.len() || cut <= boundary {
            return None;
        }
        let mut parent = s[..cut].to_vec();
        parent.push(0); // инвариант Wide: NUL на месте
        Self::from_wide(&Wide(parent)).ok()
    }

    /// Присоединить дочерний элемент по его **сырому** NT-имени.
    /// Сырое имя обязательно: с `\\?\` система не обрезает хвостовые точки
    /// и пробелы, значит `foo ` и `foo` — разные файлы.
    pub fn child_raw(&self, name: &[u16]) -> Self {
        let child = self.ext.join(name);
        let plain = self.plain.join(name);
        Self { ext: child, plain }
    }

    /// Первый компонент с зарезервированным именем — тот, из-за которого
    /// обычный `DeleteFileW` не работает.
    pub fn reserved_component(&self) -> Option<String> {
        let skip = self.ext.drive_prefix_len().unwrap_or(0);
        self.ext.as_slice()[skip..]
            .split(|c| *c == b'\\' as u16)
            .filter(|c| !c.is_empty())
            .find(|c| is_reserved_component_wide(c))
            .map(String::from_utf16_lossy)
    }

    /// Длина обычного (без `\\?\`) пути в UTF-16 — именно её сравнивают с
    /// лимитом 260.
    pub fn plain_len(&self) -> usize {
        self.plain.len()
    }

    /// Корневой компонент: `C:` для локального диска, `\\srv\shr` для UNC.
    pub fn root(&self) -> String {
        let text = self.plain.to_string_lossy();
        if let Some(rest) = text.strip_prefix("\\\\") {
            let body = rest.trim_start_matches('\\');
            let mut parts = body.splitn(3, '\\');
            let server = parts.next().unwrap_or("");
            let share = parts.next().unwrap_or("");
            if share.is_empty() {
                server.to_string()
            } else {
                format!("\\\\{server}\\{share}")
            }
        } else {
            text.chars().take(2).collect()
        }
    }

    /// Является ли путь корнем тома (`C:\`) или корнем сетевой шары.
    pub fn is_volume_root(&self) -> bool {
        let text = self.plain.to_string_lossy();
        if text.len() == 3 && text.ends_with('\\') && text.as_bytes()[1] == b':' {
            return true;
        }
        if let Some(rest) = text.strip_prefix("\\\\") {
            let parts: Vec<&str> = rest.split('\\').filter(|s| !s.is_empty()).collect();
            return parts.len() == 2;
        }
        false
    }
}

impl fmt::Display for NtPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.plain.to_string_lossy())
    }
}

/// Имена, которые Win32 превращает в устройства. Наличие такого компонента
/// делает путь непригодным для обычных API вызова.
pub fn is_reserved_name(name: &str) -> bool {
    // Имя зарезервировано, если его «база» (до первой точки) совпадает с
    // устройством. Хвостовые точки и пробелы система тоже отбрасывает, их
    // нужно учесть.
    let trimmed = name.trim_end_matches([' ', '.']);
    let base = match trimmed.find('.') {
        Some(pos) => &trimmed[..pos],
        None => trimmed,
    };
    let base = base.trim_end_matches([' ', '.']);
    if base.eq_ignore_ascii_case("CON")
        || base.eq_ignore_ascii_case("PRN")
        || base.eq_ignore_ascii_case("AUX")
        || base.eq_ignore_ascii_case("NUL")
        || base.eq_ignore_ascii_case("CLOCK$")
        || base.eq_ignore_ascii_case("CONIN$")
        || base.eq_ignore_ascii_case("CONOUT$")
    {
        return true;
    }
    // По байтам, а не срезом `str`: имя из двух кириллических букв занимает
    // четыре байта, и `base[..3]` резал бы вторую букву посреди (паника).
    let bytes = base.as_bytes();
    for prefix in ["COM", "LPT"] {
        if bytes.len() > prefix.len()
            && bytes[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
            && (bytes[prefix.len()..].len() == 1 && bytes[prefix.len()].is_ascii_digit()
                || matches!(&base[prefix.len()..], "¹" | "²" | "³"))
        {
            return true;
        }
    }
    false
}

/// Проверка с учётом надстрочных цифр (они многобайтовые в UTF-16).
pub fn is_reserved_component_wide(name: &[u16]) -> bool {
    let end = name
        .iter()
        .position(|unit| *unit == b'.' as u16)
        .unwrap_or(name.len());
    let base = &name[..end];
    let end = base
        .iter()
        .rposition(|unit| *unit != b' ' as u16)
        .map_or(0, |pos| pos + 1);
    let base = &base[..end];
    for device in [
        b"CON".as_slice(),
        b"PRN",
        b"AUX",
        b"NUL",
        b"CLOCK$",
        b"CONIN$",
        b"CONOUT$",
    ] {
        if wide_eq_ascii(base, device) {
            return true;
        }
    }
    base.len() == 4
        && (wide_eq_ascii(&base[..3], b"COM") || wide_eq_ascii(&base[..3], b"LPT"))
        && (matches!(base[3], 0x00B9 | 0x00B2 | 0x00B3)
            || (b'0' as u16..=b'9' as u16).contains(&base[3]))
}

fn wide_eq_ascii(wide: &[u16], ascii: &[u8]) -> bool {
    wide.len() == ascii.len()
        && wide
            .iter()
            .zip(ascii)
            .all(|(unit, byte)| *unit <= 0x7F && (*unit as u8).eq_ignore_ascii_case(byte))
}

/// `X:\` — буква диска и разделитель. Голый `X:` сюда не входит: после
/// префикса `\\?\` он означает сам том, а не файлы на нём.
fn is_drive_spec(body: &str) -> bool {
    let bytes = body.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
}

/// Символы, которых нет в именах NTFS: `* ? " < > |`, управляющие и `:`
/// (двоеточие вне буквы диска — это поток данных). Проверяется только часть
/// пути после префикса и буквы диска.
fn reject_invalid_characters(text: &str) -> Result<(), PathError> {
    if text
        .chars()
        .any(|c| c < ' ' || matches!(c, '*' | '?' | '"' | '<' | '>' | '|' | ':'))
    {
        Err(PathError::InvalidCharacters)
    } else {
        Ok(())
    }
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Лексическая нормализация: `/` приводится к `\`, повторные разделители
/// схлопываются, компоненты `.` отбрасываются.
///
/// `resolve_dots` включает разбор `..` так, как это делает `GetFullPathNameW`:
/// компонент снимает предыдущий, но не корень (`C:\..` — это `C:\`). Это нужно
/// для обычных путей: после `\\?\` система `..` не разбирает вовсе и отвечает
/// «неверное имя», так что без разбора `C:\a\..\b` не открывался бы, а
/// проверка запретных мест видела бы не тот путь, что уходит в Win32. Для
/// явно расширенного пути (`\\?\...`) флаг выключен: показываемый путь обязан
/// совпадать с тем, который уйдёт в Win32 дословно.
///
/// Хвостовые точки и пробелы внутри компонента сохраняются: с `\\?\`
/// `foo ` и `foo` — это разные файлы.
fn normalize_absolute(raw: &Wide, resolve_dots: bool) -> Wide {
    const BSLASH: u16 = b'\\' as u16;
    const DOT: u16 = b'.' as u16;

    let source: Vec<u16> = raw
        .as_slice()
        .iter()
        .map(|c| if *c == b'/' as u16 { BSLASH } else { *c })
        .collect();

    // UNC сохраняет ведущие `\\`: без них `\\server\share` превратился бы в
    // относительный путь.
    let unc = source.first() == Some(&BSLASH) && source.get(1) == Some(&BSLASH);
    let drive = !unc
        && source.len() >= 2
        && source[1] == b':' as u16
        && (source[0] | 0x20).wrapping_sub(b'a' as u16) < 26;
    // Корень, выше которого `..` не поднимается: `C:` либо `server` и `share`.
    let root_segments = if unc { 2 } else { usize::from(drive) };

    let mut segments: Vec<&[u16]> = Vec::new();
    for segment in source[if unc { 2 } else { 0 }..].split(|c| *c == BSLASH) {
        // Пустой сегмент — повторный разделитель: иначе `C:\\dir` схлопнулся
        // бы не в `C:\dir`.
        if segment.is_empty() || segment == [DOT] {
            continue;
        }
        if resolve_dots && segment == [DOT, DOT] {
            if segments.len() > root_segments {
                segments.pop();
            }
            continue;
        }
        segments.push(segment);
    }

    let mut out: Vec<u16> = Vec::with_capacity(source.len() + 1);
    if unc {
        out.extend([BSLASH, BSLASH]);
    }
    for (index, segment) in segments.iter().enumerate() {
        if index > 0 {
            out.push(BSLASH);
        }
        out.extend_from_slice(segment);
    }
    // Хвостовой разделитель у каталога — часть смысла пути, сохраняем. У
    // одного лишь диска он обязателен: `C:` без него — «текущая папка диска C»,
    // а не корень тома, и `C:\.` с `C:\a\..` проскакивали бы мимо запрета корня.
    let bare_drive = drive && segments.len() == 1;
    if (source.last() == Some(&BSLASH) || bare_drive)
        && out.last().is_some_and(|last| *last != BSLASH)
    {
        out.push(BSLASH);
    }
    out.push(0); // инвариант Wide: NUL на месте
    Wide(out)
}

/// Порядковое сравнение без учёта регистра — так сравнивает имена сама
/// Windows. `to_lowercase` и ASCII-вариант путают кириллические пути:
/// `C:\Данные` и `c:\данные` — один и тот же каталог.
pub fn eq_ignore_case(a: &[u16], b: &[u16]) -> bool {
    use windows::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
    a == b || unsafe { CompareStringOrdinal(a, b, true) } == CSTR_EQUAL
}

/// Ключ для сравнения целей: разобранный путь без хвостового разделителя
/// (`C:\a\` и `c:/a` — одна цель), а если разобрать нельзя — набранный как
/// есть. Ключи сравниваются через [`eq_ignore_case`].
pub fn comparison_key(path: &Path) -> Vec<u16> {
    let mut key = match NtPath::new(path) {
        Ok(parsed) => parsed.plain().as_slice().to_vec(),
        Err(_) => path.as_os_str().encode_wide().collect(),
    };
    while key.len() > 3 && key.last() == Some(&(b'\\' as u16)) {
        key.pop();
    }
    key
}

fn full_path_name(raw: &Wide) -> Result<PathBuf, PathError> {
    unsafe {
        // Первый вызов с пустым буфером возвращает нужную длину; система
        // при этом не трогает ни один компонент пути на диске.
        let needed = GetFullPathNameW(raw.pc(), None, None);
        if needed == 0 {
            return Err(PathError::NotAbsolute);
        }
        let mut buffer = vec![0u16; needed as usize + 1];
        let written = GetFullPathNameW(raw.pc(), Some(&mut buffer), None);
        if written == 0 || written as usize > buffer.len() {
            return Err(PathError::NotAbsolute);
        }
        buffer.truncate(written as usize);
        Ok(PathBuf::from(OsString::from_wide(&buffer)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nt(text: &str) -> NtPath {
        NtPath::new(Path::new(text)).expect("path parses")
    }

    #[test]
    fn empty_wide_values_are_safe_win32_strings() {
        for empty in [
            Wide::default(),
            Wide::new(""),
            Wide::from_path(Path::new("")),
        ] {
            assert!(unsafe { empty.pc().as_wide() }.is_empty());
            assert_eq!(NtPath::from_wide(&empty).unwrap_err(), PathError::Empty);
        }
    }

    #[test]
    fn drive_paths_get_extended_prefix() {
        let path = nt(r"C:\Users\name\Desktop\Seller");
        assert_eq!(
            path.ext().to_string_lossy(),
            r"\\?\C:\Users\name\Desktop\Seller"
        );
        assert_eq!(path.to_string_lossy(), r"C:\Users\name\Desktop\Seller");
    }

    #[test]
    fn unc_paths_get_unc_extended_prefix() {
        let path = nt(r"\\server\share\deep\item");
        assert_eq!(
            path.ext().to_string_lossy(),
            r"\\?\UNC\server\share\deep\item"
        );
        assert_eq!(path.root(), r"\\server\share");
    }

    #[test]
    fn already_extended_paths_are_preserved() {
        let path = nt(r"\\?\C:\dir\NUL");
        assert_eq!(path.ext().to_string_lossy(), r"\\?\C:\dir\NUL");
        assert_eq!(path.to_string_lossy(), r"C:\dir\NUL");
    }

    #[test]
    fn device_paths_are_refused() {
        assert_eq!(
            NtPath::new(Path::new(r"\\.\PhysicalDrive0")).unwrap_err(),
            PathError::DevicePath
        );
        assert_eq!(
            NtPath::new(Path::new(
                r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1"
            ))
            .unwrap_err(),
            PathError::DevicePath
        );
    }

    #[test]
    fn characters_that_names_cannot_have_are_refused() {
        // `*` и `?` оболочка раскрывает как маску (Корзина удалила бы «всё»),
        // а `::$INDEX_ALLOCATION` — это та же папка под другим именем, мимо
        // списка запретных.
        for bad in [
            r"C:\Users\me\*",
            r"C:\a\b?",
            r"C:\a\b|c",
            r"C:\Windows::$INDEX_ALLOCATION",
            r"C:\a\b:stream",
            "C:\\a\\\u{1}x",
            r"foo:bar",
            r#"C:\a"b"#,
            r"\\srv\shr\a*",
            r"\\?\C:\a\b*",
        ] {
            assert_eq!(
                NtPath::new(Path::new(bad)).unwrap_err(),
                PathError::InvalidCharacters,
                "{bad}"
            );
        }
        for fine in [
            r"C:\a\file (1).txt",
            r"C:\a\dir.",
            "C:\\a\\x ",
            r"C:\a\[abc]",
            r"C:\a\#1&2,3'4!5@6%7^8;9=0+~$",
            r"C:",
            r"C:\ИП\отчёт",
        ] {
            assert!(NtPath::new(Path::new(fine)).is_ok(), "{fine}");
        }
    }

    #[test]
    fn extended_paths_must_name_a_drive_or_a_share() {
        for device in [
            r"\\?\PhysicalDrive0",
            r"\\?\pipe\x",
            r"\\?\C:",
            r"\\?\GLOBALROOT\Device\X",
            r"\\?\Volume{1234}\x",
            r"\\?\HarddiskVolume1\x",
        ] {
            assert_eq!(
                NtPath::new(Path::new(device)).unwrap_err(),
                PathError::DevicePath,
                "{device}"
            );
        }
        // Для показа и сравнения расширенный UNC — обычный `\\server\share`.
        assert_eq!(
            nt(r"\\?\UNC\srv\shr\dir").to_string_lossy(),
            r"\\srv\shr\dir"
        );
        assert!(nt(r"\\?\UNC\srv\shr").is_volume_root());
        assert!(nt(r"\\?\C:\").is_volume_root());
    }

    #[test]
    fn incomplete_unc_is_refused() {
        assert_eq!(
            NtPath::new(Path::new(r"\\server")).unwrap_err(),
            PathError::IncompleteUnc
        );
    }

    #[test]
    fn reserved_names_are_detected_in_any_component() {
        assert!(is_reserved_name("NUL"));
        assert!(is_reserved_name("nul"));
        assert!(is_reserved_name("NUL.txt"));
        assert!(is_reserved_name("CON "));
        assert!(is_reserved_name("COM1"));
        assert!(is_reserved_name("LPT9"));
        assert!(!is_reserved_name("COM"));
        assert!(!is_reserved_name("NULL"));
        assert!(!is_reserved_name("NUL1.txt"));
        for name in ["COM¹", "com².txt", "Lpt³.log ", "nul.txt"] {
            assert!(is_reserved_name(name), "{name}");
            assert!(
                is_reserved_component_wide(Wide::new(name).as_slice()),
                "{name}"
            );
        }

        assert_eq!(
            nt(r"C:\dir\NUL").reserved_component().as_deref(),
            Some("NUL")
        );
        assert_eq!(
            nt(r"C:\dir\sub\CON.log").reserved_component().as_deref(),
            Some("CON.log")
        );
        assert_eq!(nt(r"C:\dir\report.txt").reserved_component(), None);
    }

    #[test]
    fn multibyte_names_never_panic_the_reserved_name_check() {
        // Регрессия: «Да» занимает четыре байта, а проверка COMn/LPTn резала
        // строку по третьему байту, то есть посреди второй буквы, и
        // `str`-срез паниковал. Любая папка вроде `D:\ИП\отчёт` валила
        // и сканер, и окно (в релизе panic = abort).
        for name in ["Да", "ИП", "Мы", "abé", "é€", "a€", "СОМ1", "COM€"] {
            assert!(!is_reserved_name(name), "{name}");
        }
        assert_eq!(nt(r"D:\ИП\отчёт").reserved_component(), None);
        assert!(is_reserved_name("com1"));
        assert!(is_reserved_name("LPT7.txt"));
    }

    #[test]
    fn trailing_dots_survive_extended_parsing() {
        // С \\?\ файл `foo ` и `foo` — разные объекты; наша нормализация
        // обязана сохранить пробел, иначе удалится не то.
        let path = nt(r"C:\dir\foo ");
        assert_eq!(path.to_string_lossy(), "C:\\dir\\foo ");
        assert!(!path.is_volume_root());
    }

    #[test]
    fn dot_segments_are_resolved_like_win32_before_anything_is_checked() {
        // После `\\?\` система `..` не разбирает (ERROR_INVALID_NAME), а
        // проверка запретных мест читает именно этот путь: разбирать `..`
        // приходится нам, до проверки и до открытия.
        assert_eq!(nt(r"C:\a\.\b\..\c\").to_string_lossy(), r"C:\a\c\");
        assert_eq!(nt(r"C:\a\b\..\..\..\x").to_string_lossy(), r"C:\x");
        assert_eq!(nt(r"\\srv\shr\a\..\b").to_string_lossy(), r"\\srv\shr\b");
        assert_eq!(
            nt(r"C:\a\b\..").ext().to_string_lossy(),
            r"\\?\C:\a",
            "в Win32 уходит тот же путь, что показан"
        );
    }

    #[test]
    fn every_spelling_of_a_volume_root_is_the_volume_root() {
        // Раньше `D:\.` превращалось в голый `D:`: запрет корня тома его не
        // узнавал, а обход перечислял корень диска.
        for spelling in [
            r"C:",
            r"C:\",
            r"C:/",
            r"C:\.",
            r"C:\\.\.",
            r"C:\a\..",
            r"C:\..",
            r"C:\a\..\..\b\..",
            r"\\srv\shr\.",
            r"\\srv\shr\x\..\",
        ] {
            assert!(nt(spelling).is_volume_root(), "{spelling}");
        }
        assert!(!nt(r"C:\a").is_volume_root());
        assert!(!nt(r"C:\a\..\b").is_volume_root());
    }

    #[test]
    fn explicitly_extended_paths_stay_verbatim_dots_included() {
        // `\\?\` отключает разбор имён, и показываемый путь обязан совпадать с
        // тем, что уйдёт в Win32: открыть его система всё равно не даст.
        assert_eq!(nt(r"\\?\C:\a\..\b").to_string_lossy(), r"C:\a\..\b");
    }

    #[test]
    fn names_are_compared_the_way_windows_compares_them() {
        let wide = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
        assert!(eq_ignore_case(
            &wide(r"C:\Данные\Файл"),
            &wide(r"c:\данные\файл")
        ));
        assert!(eq_ignore_case(&wide("A"), &wide("a")));
        assert!(!eq_ignore_case(&wide(r"C:\a"), &wide(r"C:\ab")));
    }

    #[test]
    fn spellings_of_one_target_share_a_comparison_key() {
        let key = |text: &str| comparison_key(Path::new(text));
        let same = |a: &str, b: &str| eq_ignore_case(&key(a), &key(b));
        assert!(same(r"C:\a\", r"C:\a"));
        assert!(same(r"C:/a", r"c:\A"));
        assert!(same(r"C:\Данные\.", r"c:\данные"));
        assert!(!same(r"C:\a", r"C:\b"));
        // Корень тома не теряет разделитель и не сливается с соседями.
        assert_eq!(key(r"C:\").len(), 3);
    }

    #[test]
    fn duplicate_separators_collapse() {
        assert_eq!(nt(r"C:\\dir\\\\file").to_string_lossy(), r"C:\dir\file");
        assert_eq!(nt(r"C:/dir/file").to_string_lossy(), r"C:\dir\file");
        assert_eq!(nt(r"C:\\").to_string_lossy(), r"C:\");
    }

    #[test]
    fn volume_roots_are_recognised() {
        assert!(nt(r"C:\").is_volume_root());
        assert!(nt(r"\\server\share").is_volume_root());
        assert!(!nt(r"C:\dir").is_volume_root());
    }

    #[test]
    fn parent_walks_up_components() {
        let path = nt(r"C:\a\b\c");
        assert_eq!(path.parent(1).unwrap().to_string_lossy(), r"C:\a\b");
        assert_eq!(path.parent(2).unwrap().to_string_lossy(), r"C:\a");
        assert!(path.parent(3).is_none());
    }

    #[test]
    fn child_uses_raw_name_verbatim() {
        let path = nt(r"C:\dir");
        let child = path.child_raw(Wide::new("NUL").as_slice());
        assert_eq!(child.ext().to_string_lossy(), r"\\?\C:\dir\NUL");
    }

    #[test]
    fn nul_terminated_list_is_double_nul_ended() {
        let list = Wide::nul_terminated_list(&[Wide::new("a"), Wide::new("b")]);
        assert_eq!(list, vec![b'a' as u16, 0, b'b' as u16, 0, 0]);
    }
}
