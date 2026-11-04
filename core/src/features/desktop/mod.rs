//! Desktop and App projection, input, audio and file drops.
pub mod audio;
mod drop;
mod input;
mod screen;
mod types;

pub use drop::{AppDropOptions, DropReceipt};
pub use screen::{AppOptions, Screen, ScreenConfiguration, ScreenEvent};
pub use screen::{LEFT_HELD, MOUSE_CTRL, MOUSE_META, MOUSE_SHIFT, RIGHT_HELD};
pub use screen::{MouseAction, android_key};
