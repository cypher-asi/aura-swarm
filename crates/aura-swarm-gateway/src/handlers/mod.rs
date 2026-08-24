//! HTTP request handlers.
//!
//! This module contains all the endpoint handlers for the gateway API.

pub(crate) mod agents;
pub(crate) mod automaton;
pub(crate) mod files;
pub(crate) mod health;
pub(crate) mod internal;
pub(crate) mod preview_tcp;
pub(crate) mod process_triggers;
pub(crate) mod processes;
pub(crate) mod run;
pub(crate) mod secrets;
pub(crate) mod sessions;
pub(crate) mod terminal;
pub(crate) mod usage;
pub(crate) mod ws;
