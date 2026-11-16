//! Offline memory management for the Sangean ATS-909X2.
//!
//! The [`image`] module parses and edits DATA OUT images, and [`radio`] moves
//! them to and from the radio through the ESP32 bridge. Neither has terminal
//! or UI dependencies; the `memory-manager` binary builds its TUI on top.
//! [`sample`] builds a made-up image for tests and for trying the editor.
//! [`settings`] decodes the menu settings and timers.

pub mod image;
pub mod radio;
pub mod sample;
pub mod settings;
