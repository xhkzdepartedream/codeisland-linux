//! Platform-neutral pieces shared by the hook bridge and the daemon.
//!
//! This crate deliberately holds no UI and no Linux-desktop specifics, so the
//! same event model can later back a Plasma widget or a tray icon without a
//! rewrite. It mirrors `Sources/CodeIslandCore` in the upstream macOS project.

pub mod event;
pub mod protocol;
pub mod socket_path;

pub use event::{EventNormalizer, RouteKind};
pub use socket_path::socket_path;
