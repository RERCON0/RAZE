//! Чтение содержимого каталога через `FindFirstFileExW`.
//!
//! Отдельный маленький модуль, потому что его используют и обход дерева, и
//! удаление, и им обоим нужно ровно одно: список **сырых** имён без
//! нормализации. Имя из `cFileName` приходит уже «как есть» — с хвостовыми
//! пробелами и точками, которые обычный API срезал бы.

use windows::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
};

use super::error::{nt, NtError};
use super::path::NtPath;

#[derive(Clone, Debug)]
pub struct Child {
    /// Сырое NT-имя в UTF-16, без NUL.
    pub name: Vec<u16>,
    pub attributes: u32,
    pub size: u64,
}

/// Спец-имена каталога, которые `FindFirstFileExW` возвращает вместе с
/// реальным содержимым.
const CURRENT: &[u16] = &[b'.' as u16];
const PARENT: &[u16] = &[b'.' as u16, b'.' as u16];

/// Содержимое каталога. `Err` — каталога нет или его не дают прочитать;
/// вызывающий решает сам, что с этим делать (в обходе это «пропустить»,
/// в удалении — «записать отказ»).
pub fn children(dir: &NtPath) -> Result<Vec<Child>, NtError> {
    children_limited(dir, usize::MAX).map(|(children, _)| children)
}

/// Ограниченный список; `true` означает, что найден реальный объект сверх
/// лимита. Не выделяет память для остальных имён большого каталога.
pub fn children_limited(dir: &NtPath, limit: usize) -> Result<(Vec<Child>, bool), NtError> {
    let pattern = dir.ext().join(&[b'*' as u16]);
    let mut data = WIN32_FIND_DATAW::default();
    let handle = nt(unsafe {
        FindFirstFileExW(
            pattern.pc(),
            FindExInfoBasic,
            &mut data as *mut _ as *mut _,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    })?;

    let mut out = Vec::new();
    loop {
        let name = name_of(&data);
        if name != CURRENT && name != PARENT {
            if out.len() == limit {
                unsafe {
                    let _ = FindClose(handle);
                }
                return Ok((out, true));
            }
            out.push(Child {
                name: name.to_vec(),
                attributes: data.dwFileAttributes,
                size: ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64,
            });
        }
        data = WIN32_FIND_DATAW::default();
        if let Err(error) = nt(unsafe { FindNextFileW(handle, &mut data) }) {
            unsafe {
                let _ = FindClose(handle);
            }
            // Нормальный конец списка — ERROR_NO_MORE_FILES. Всё остальное
            // (отказ посреди каталога, обрыв сети) раньше тоже означало «конец»,
            // и недочитанный каталог выдавался за полностью прочитанный.
            return if error.code() == ERROR_NO_MORE_FILES {
                Ok((out, false))
            } else {
                Err(error)
            };
        }
    }
}

const ERROR_NO_MORE_FILES: u32 = 18;

fn name_of(data: &WIN32_FIND_DATAW) -> &[u16] {
    let end = data
        .cFileName
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(data.cFileName.len());
    &data.cFileName[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;

    #[test]
    fn name_stops_at_first_nul() {
        let mut data = WIN32_FIND_DATAW::default();
        data.cFileName[0] = b'a' as u16;
        data.cFileName[1] = 0;
        data.cFileName[2] = b'b' as u16;
        assert_eq!(name_of(&data), &[b'a' as u16]);
    }
}
