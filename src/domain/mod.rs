//! Pure domain rules: what a token may do, what a tool needs, how a refresh
//! token is sealed, how a download link is named, and how a picture and an
//! audit argument are cut down to size. Nothing here touches the database,
//! the network, or the clock beyond what it is handed.

// The layers that consume these land later: `scope` and `image` in the MCP
// server (step 5) and the token API (step 4), `token` and `seal` in the
// token and connection flows (step 4) and the CLI (step 6), `link` in the
// download route (step 4), `audit` wherever a call is logged.
#[allow(dead_code)]
pub mod audit;
#[allow(dead_code)]
pub mod image;
#[allow(dead_code)]
pub mod limits;
#[allow(dead_code)]
pub mod link;
#[allow(dead_code)]
pub mod scope;
#[allow(dead_code)]
pub mod seal;
#[allow(dead_code)]
pub mod token;
