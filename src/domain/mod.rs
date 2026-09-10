//! Pure domain rules: what a token may do, what a tool needs, how a refresh
//! token is sealed, how a download link is named, and how a picture and an
//! audit argument are cut down to size. Nothing here opens a database or a
//! socket, and nothing reads the clock beyond what it is handed; `image` names
//! the client profile a token row stores, which is the one type this layer
//! borrows from another. `zone` is where an IANA time zone name is read, so
//! the portal and the terminal refuse the same name in the same words.

pub mod audit;
pub mod image;
pub mod limits;
pub mod link;
pub mod scope;
pub mod seal;
pub mod token;
pub mod zone;
