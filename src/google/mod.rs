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

pub mod calendar;
pub mod client;
pub mod docs;
pub mod drive;
pub mod gmail;
pub mod multipart;
pub mod oauth;
pub mod sheets;
pub mod text;

#[cfg(test)]
mod tests;

pub use client::{BoxFuture, Client, ConnectionStore, Error, Result, http_client};
