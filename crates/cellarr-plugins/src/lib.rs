//! cellarr-plugins — third-party indexers and download clients as sandboxed WASM.
//!
//! A plugin is a WASM component implementing `wit/plugins.wit`: it says who it is
//! and which settings it takes, and implements an indexer or a download client.
//! It can do nothing but compute and make HTTP requests to the hosts its own
//! settings name; CPU, wall-clock and memory are bounded per call, and every call
//! runs in a fresh instance. Loaded plugins appear to *arr clients as ordinary
//! implementations, configured through the same API as the built-in ones.
//!
//! Authors can ship a component, or simply the core module `cargo build --target
//! wasm32-unknown-unknown` produces from a `wit-bindgen` crate; the host turns
//! the latter into a component at load time. `examples/plugins/` has a working
//! indexer and download client.
//!
//! The wasmtime host is behind the **`wasm`** feature; the capability and policy
//! layer is always compiled.

#![forbid(unsafe_code)]

pub mod capability;
pub mod catalog;
pub mod config;
pub mod error;

pub use capability::{AllowListHttp, DenyAllHttp, HttpCapability, HttpRequest, HttpResponse};
pub use config::HostConfig;
pub use error::{PluginError, Result};

#[cfg(feature = "wasm")]
pub mod adapters;
#[cfg(feature = "wasm")]
pub mod host;
#[cfg(feature = "wasm")]
pub mod registry;

#[cfg(feature = "wasm")]
pub use adapters::{PluginDownloadClient, PluginIndexer};
#[cfg(feature = "wasm")]
pub use host::{Plugin, PluginField, PluginInfo, PluginKind, PluginProtocol};
