//! Independently selectable metrics and immutable, bounded guest metadata capture.
#![allow(missing_docs)]
pub mod host_pages;
mod api;
mod capture;
mod records;
mod manager;
pub use api::Resource;
pub use manager::Manager;
