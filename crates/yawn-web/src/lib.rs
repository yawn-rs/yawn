//! Yawn framework core.
//!
//! One crate, three layers, selected by target and feature so an application depends on it once:
//!
//! * [`config`] (feature `config`): `Yawn.toml`, port resolution. Pure and shared with the CLI.
//! * [`server`] (native, feature `server`): the HTTP server an application starts from its own
//!   `main`. Serves the HTML shell, the compiled client and assets; in development it also
//!   exposes the reload channel the CLI drives. Built on axum; routes can be added with
//!   [`server::App::merge`], which is where SSR, routing and server functions will attach.
//! * [`ui`]: the fine-grained renderer `.rsx` components compile to, independent of the target.
//! * `web` (wasm32 only): the browser backend of [`ui`] and the `client!` entry point.
//!
//! The application is a normal package: `src/main.rs` runs the server, `src/lib.rs` (compiled to
//! WASM) is the client. `yawn dev` builds and supervises both.

#![allow(clippy::type_complexity)]
pub use yawn_reactivity as reactivity;

#[cfg(all(feature = "config", not(target_arch = "wasm32")))]
pub mod config;

/// Hashed names of the files a production build serves
pub mod assets;

/// CLI <-> dev server messages
pub mod protocol;

/// HTTP client, the same on the server and in the browser (feature `http`)
#[cfg(feature = "http")]
pub mod http;

/// Values handed down to what a component builds: [`provide`](context::provide) and
/// [`use_context`](context::use_context)
pub mod context;

/// Data a page needs: [`resource`](data::resource) (it comes with the server's page),
/// [`prefetch`](data::prefetch) and the live [`stream`](data::stream) of what the server pushes
pub mod data;

/// Work that belongs to a component: [`spawn`](task::spawn) a future, run something
/// [`after`](task::after) a while, [`sleep`](task::sleep)
pub mod task;

/// What only a browser has: [`global`](browser::global) listeners, [`script`](browser::script)s and
/// [`platform`](browser::platform) calls
pub mod browser;

/// Fine-grained UI runtime for `.rsx` components (target independent)
pub mod ui;

#[doc(hidden)]
pub mod dynamic;

#[cfg(all(feature = "server", not(target_arch = "wasm32")))]
pub mod dev;
#[cfg(all(feature = "server", not(target_arch = "wasm32")))]
pub mod server;

#[cfg(target_arch = "wasm32")]
pub mod dom;
#[cfg(target_arch = "wasm32")]
pub mod web;

/// Pulls in the modules generated from the project's `.rsx` files (`yawn dev` and `yawn rsx` write
/// them to `.yawn/rsx/`)
#[macro_export]
macro_rules! rsx_modules {
    () => {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/.yawn/rsx/modules.rs"));
    };
}
