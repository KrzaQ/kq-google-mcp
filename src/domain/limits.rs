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

/// Serialised tool arguments are cut to this before they reach the log.
pub const AUDIT_ARGS_MAX_BYTES: usize = 4096;
/// How long a browser session lasts, and how recently a delegate token's
/// acting user must have logged in.
pub const SESSION_DAYS: i64 = 30;
