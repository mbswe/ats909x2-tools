//! Transfers between MemoryManager and the radio through the ESP32 bridge.
//! No UI code: the TUI talks to [`worker::Worker`].

pub mod link;
pub mod log;
pub mod port;
pub mod protocol;
pub mod worker;

#[cfg(test)]
mod fake;
