//! `yawn` command line tool.
//!
//! Layering, bottom to top:
//! * [`event`], [`state`]: data model and pure state logic, no I/O.
//! * [`pipeline`]: the queue connecting event sources to a presenter.
//! * [`output`], [`tui`]: presenters (plain/JSON lines, interactive UI).
//! * [`dev`]: the `yawn dev` supervisor (watcher, builds, server process) as an event source.
//! * [`session`], [`commands`], [`cli`]: wiring and argument handling.
//!
//! A future `yawn dev` or `yawn build` implements [`pipeline::EventSource`] for its compiler,
//! server or HMR and returns a [`session::Session`]; it gets the UI, plain output and JSON for free.

#![allow(clippy::type_complexity)]
pub mod build;
pub mod cli;
pub mod commands;
pub mod dev;
pub mod env;
pub mod error;
pub mod event;
pub mod finalize;
pub mod new;
pub mod optimize;
pub mod output;
pub mod pipeline;
pub mod prerender;
pub mod session;
pub mod split;
pub mod state;
pub mod tui;
pub mod upgrade;
