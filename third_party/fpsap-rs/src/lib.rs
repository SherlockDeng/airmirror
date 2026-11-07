//! Minimal FairPlay SAP support vendored from doubletake-rs.
//!
//! Source: https://github.com/acquo/doubletake-rs (GPL-3.0-or-later).

pub mod error;
pub mod fairplay_md5;
pub mod fairplay_message;
pub mod fairplay_sap;
pub mod fp_tables_generated;
pub mod fpsap;

pub use error::{Error, Result};
