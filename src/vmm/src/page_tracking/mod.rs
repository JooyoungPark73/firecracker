//! Independently selectable metrics and immutable, bounded guest metadata capture.
#![allow(missing_docs)]
mod api;
mod capture;
pub mod host_pages;
mod manager;
mod records;
pub use api::Resource;
pub use manager::Manager;
