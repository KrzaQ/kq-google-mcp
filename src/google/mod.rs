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

// The HTTP layer uses the connect flow, the download route and health; the
// rest of this surface — most of the per-service wrappers, the image and text
// helpers — is what the MCP tools call in step 5. It is written whole and
// tested whole, so until then dead-code warnings would drown out real ones.
#![allow(dead_code)]

pub mod calendar;
pub mod client;
pub mod docs;
pub mod drive;
pub mod gmail;
pub mod oauth;
pub mod sheets;
pub mod text;

#[cfg(test)]
mod tests;

// `Download`, `GoogleError` and `TokenSource` are named by the MCP tools of
// step 5; the re-export is the whole surface even while part of it waits.
#[allow(unused_imports)]
pub use client::{
    BoxFuture, Client, ConnectionStore, Download, Error, GoogleError, Result, TokenSource,
    http_client,
};
