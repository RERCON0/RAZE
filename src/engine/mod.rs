//! Движок удаления: всё, что работает с Windows API напрямую.

pub mod dir;
pub mod elevation;
pub mod error;
pub mod handle;
pub mod locker;
pub mod path;
pub mod privs;
pub mod recycle;
pub mod remove;
pub mod scan;
pub mod security;

pub use error::NtError;
pub use path::{NtPath, PathError, Wide};
pub use remove::{Outcome, RemoveOptions};
pub use scan::{Blocker, Entry, EntryKind, ScanOptions, ScanSummary};