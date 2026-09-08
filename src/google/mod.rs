//! Everything that talks to Google. One [`Client`] with configurable base
//! URLs carries every call, so a test points the three bases at wiremock and
//! nothing else in the process knows the difference.
//!
//! The layout is one module per service plus the two that cut across them:
//! [`oauth`] for the connect flow and [`text`] for turning an attachment or a
//! Drive file into something a chat model can read. The service modules are
//! thin typed wrappers over the REST endpoints the plan lists and nothing
//! more; the curation is the point, and what is missing from them is missing
//! on purpose. Nothing here sends, trashes or deletes a message.
//!
//! This module has no database dependency: the access-token cache reaches the
//! `connections` table through the [`ConnectionStore`] trait, which the HTTP
//! layer implements over `Db`.

// The callers land in step 4 (the connect flow, the download route and
// health) and step 5 (the MCP tools). Until then the surface below is written
// whole and used only by its own tests, and dead-code warnings would drown out
// real ones.
#![allow(dead_code)]

pub mod client;
pub mod docs;
pub mod drive;
pub mod gmail;
pub mod oauth;
pub mod sheets;

#[cfg(test)]
mod tests;

// Re-exported for the layers that come later; nothing in the binary reaches
// for them yet.
#[allow(unused_imports)]
pub use client::{
    Client, ConnectionStore, Download, Error, GoogleError, Result, TokenSource, http_client,
};
