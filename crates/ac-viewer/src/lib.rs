//! The terminal viewer without I/O (ADR-006): parsing tmux control mode, turning `capture-pane`
//! output into sanitised styled runs (SGR only; every other escape is dropped here, before
//! anything reaches a browser), diffing snapshots, building tmux input commands, the viewer
//! registry with its take-over lock, the window list, and the recording format.

pub mod control;
pub mod input;
pub mod json;
pub mod record;
pub mod registry;
pub mod screen;
pub mod sgr;
pub mod text;
pub mod windows;
