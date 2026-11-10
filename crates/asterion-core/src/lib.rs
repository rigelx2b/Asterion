//! Domain types shared by every Asterion crate.
//!
//! This crate does no I/O. Data collection, storage, and validation live in
//! `asterion-data`; this crate only defines what a market, a symbol, and a
//! daily bar are.

pub mod bar;
pub mod error;
pub mod market;
pub mod provenance;

pub use bar::{Bar, CorporateAction, CorporateActionKind};
pub use error::CoreError;
pub use market::{Market, Symbol};
pub use provenance::Provenance;
