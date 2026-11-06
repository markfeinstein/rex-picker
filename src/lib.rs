#![forbid(unsafe_code)]

//! Reusable picker primitives for Rex terminal workflows.
//!
//! The crate separates fuzzy matching and selection state from terminal I/O and
//! adapter-specific behavior. Model, URL, file, password, and history pickers
//! can share the same engine while retaining explicit data and action adapters.

pub mod adapters;
pub mod engine;
pub mod protocol;
pub mod ui;

pub use engine::PickerEngine;
pub use protocol::{
    PickerAction, PickerDocument, PickerItem, PickerOptions, PickerResult, PickerSelection,
};
pub use ui::{run_picker, run_picker_with_handler};
