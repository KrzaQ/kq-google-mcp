//! Pure domain rules: what a token may do, what a tool needs, how a refresh
//! token is sealed, how a download link is named, and how a picture and an
//! audit argument are cut down to size. Nothing here touches the database,
//! the network, or the clock beyond what it is handed.

// The layers that consume these land later: `scope` in the MCP server
// (step 5) and the token API (step 4), `token` in the token flows (step 4)
// and the CLI (step 6).
#[allow(dead_code)]
pub mod limits;
#[allow(dead_code)]
pub mod scope;
#[allow(dead_code)]
pub mod token;
