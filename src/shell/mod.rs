//! Интеграция с оболочкой Windows: контекстное меню Проводника.

pub mod install;

pub use install::{install, InstallReport, Step};
