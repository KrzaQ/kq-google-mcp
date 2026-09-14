//! The numbers the design fixed once. They are constants and not
//! configuration on purpose: a deployment that could raise the image cap or
//! the link lifetime would be a deployment whose behaviour has to be
//! explained per host. The modules beside this one refer to these; nothing
//! else spells the numbers out.

/// How long a download link lives after it is minted.
pub const LINK_TTL_MINUTES: i64 = 15;
/// How many times one link may be fetched before it is spent.
pub const LINK_USES: i64 = 3;
/// The largest file the download route streams from Google.
pub const DOWNLOAD_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// The most extracted text a tool returns, before the truncation notice.
pub const TEXT_MAX_CHARS: usize = 200_000;

/// Longest side, in pixels, of an image returned as MCP content.
pub const IMAGE_LONG_SIDE: u32 = 1568;
/// Claude Code counts the base64 against `MAX_MCP_OUTPUT_TOKENS`, so its
/// pictures are smaller.
pub const IMAGE_LONG_SIDE_CLAUDE_CODE: u32 = 1024;
/// Byte cap of the encoded image, matching the long side above.
pub const IMAGE_MAX_BYTES: usize = 200 * 1024;
pub const IMAGE_MAX_BYTES_CLAUDE_CODE: usize = 100 * 1024;

/// How long an upload ticket lives after it is minted. It is the download
/// link's life, on purpose and through the same constant: both are a URL a
/// person or an agent is handed and told to use now, and two numbers would be
/// two things to explain.
pub const UPLOAD_TICKET_TTL_MINUTES: i64 = LINK_TTL_MINUTES;
/// How long a staged file waits to be attached to a draft. Longer than the
/// ticket, because writing the draft is the slow part: a model uploads three
/// files, reads a thread, asks the person a question and only then drafts.
pub const UPLOAD_TTL_MINUTES: i64 = 60;
/// The largest file that may be staged, and the largest total one draft may
/// carry. Gmail's own limit for a whole message.
pub const ATTACHMENT_MAX_BYTES: usize = 25 * 1024 * 1024;

/// The largest picture Docs will fetch for `insertInlineImage`, and the most
/// pixels it will take. Google's own two numbers, checked here so a picture
/// past either is refused before a call is spent on it. The staging route's
/// [`ATTACHMENT_MAX_BYTES`] is the smaller of the two byte caps and so is
/// what a file actually meets first.
pub const INSERT_IMAGE_MAX_BYTES: usize = 50 * 1024 * 1024;
pub const INSERT_IMAGE_MAX_PIXELS: u64 = 25_000_000;

/// The largest table a tool writes. A magazine table is small, and a model
/// that has miscounted its data is stopped by these two numbers rather than
/// by Google, before any call is made.
pub const TABLE_MAX_ROWS: usize = 100;
pub const TABLE_MAX_COLUMNS: usize = 20;

/// Serialised tool arguments are cut to this before they reach the log.
pub const AUDIT_ARGS_MAX_BYTES: usize = 4096;
/// How long a browser session lasts, and how recently a delegate token's
/// acting user must have logged in.
pub const SESSION_DAYS: i64 = 30;
