//! Pure domain rules: what a token may do, what a tool needs, how a refresh
//! token is sealed, how a download link is named, and how a picture and an
//! audit argument are cut down to size. Nothing here touches the database,
//! the network, or the clock beyond what it is handed.

// The HTTP layer consumes `scope`, `token`, `seal`, `link` and `audit`; what
// is left of them, and all of `image`, belongs to the MCP tools (step 5) and
// the CLI (step 6), which is why the allows stay for now.
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
