//! Turning a picture into MCP content, in the shape the token's client
//! actually shows a model.
//!
//! The three clients differ in ways that matter, and the difference is not
//! cosmetic — get it wrong and the person sees the picture while the model
//! sees nothing:
//!
//! - **Claude Code** renders image content as a native image block, does not
//!   downscale, and counts the base64 against `MAX_MCP_OUTPUT_TOKENS`, so its
//!   pictures are the small profile. Versions before 2.1.128 drop the image
//!   when `structuredContent` is present, which is why neither image tool ever
//!   sets it.
//! - **OpenCode** accepts image content and resizes further itself.
//! - **Open WebUI** uploads image content to its file store and shows it under
//!   the message; its model never receives it. What the model does receive is
//!   an embedded resource with an `image/*` blob, so that profile gets both:
//!   the image content for the person, the resource for the model.
//!
//! Nothing here ever returns the original bytes: everything goes through
//! [`crate::domain::image::prepare`] first.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData, ResourceContents};

use super::{bad, refuse};
use crate::db::ClientProfile;
use crate::domain::image::{self, ImageError, Profile};

/// Where a picture came from, for the resource URI Open WebUI's model sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    GmailAttachment,
    DriveFile,
    DocsImage,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::GmailAttachment => "gmail",
            Kind::DriveFile => "drive",
            Kind::DocsImage => "docs",
        }
    }
}

/// What the caller knows about the picture before it is prepared.
pub struct Source<'a> {
    pub connection_id: i64,
    pub kind: Kind,
    /// The ids that name it inside its connection: one for a Drive file, two
    /// for a Gmail attachment.
    pub ids: &'a [&'a str],
    pub filename: &'a str,
    /// What Google said the type was, which is not what comes back: the
    /// picture is re-encoded.
    pub mime_type: &'a str,
}

impl Source<'_> {
    /// The URI the embedded resource carries. It names the picture inside this
    /// server rather than pointing anywhere fetchable: the blob travels with
    /// it, and a download link is what a fetchable URL would be.
    fn uri(&self) -> String {
        format!(
            "gmcp://{}/{}/{}",
            self.connection_id,
            self.kind.as_str(),
            self.ids.join("/")
        )
    }
}

/// Decode, downscale, re-encode, and wrap in the blocks this client needs.
/// Never sets `structured_content`.
pub fn content(
    client: ClientProfile,
    source: Source<'_>,
    bytes: &[u8],
) -> Result<CallToolResult, ErrorData> {
    let prepared = image::prepare(bytes, Profile::for_client(client)).map_err(|e| match e {
        ImageError::LinkOnly(_) => refuse(format!(
            "{}: {e}. Use the matching *_link tool and give the person the URL",
            source.filename
        )),
        ImageError::Decode(_) | ImageError::Encode(_) => bad(format!("{}: {e}", source.filename)),
    })?;
    let data = BASE64.encode(&prepared.bytes);
    let summary = format!(
        "{} ({}, sent as {}), {}x{}, {} KB. \
         This picture is visible only in this turn; call the tool again to look at it later.",
        source.filename,
        source.mime_type,
        prepared.mime,
        prepared.width,
        prepared.height,
        prepared.bytes.len().div_ceil(1024),
    );
    let mut blocks = vec![
        ContentBlock::text(summary),
        ContentBlock::image(data.clone(), prepared.mime),
    ];
    if client == ClientProfile::OpenWebUi {
        blocks.push(ContentBlock::resource(
            ResourceContents::blob(data, source.uri()).with_mime_type(prepared.mime),
        ));
    }
    Ok(CallToolResult::success(blocks))
}
