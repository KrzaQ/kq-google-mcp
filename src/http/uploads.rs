//! Staged uploads: the ticket a tool mints, the unauthenticated `/up/{id}`
//! route that spends it, and the files waiting to be attached to a draft.
//!
//! A file gets into a draft the only way it can here: over the wire. The
//! server runs in a container on a host the calling agent reaches over the
//! network, so there is no such thing as a local file — nothing reads a path
//! a model supplies. A tool mints a ticket, the agent POSTs the bytes to the
//! URL it answers, and the `upload_id` it reads back goes to a draft tool,
//! which attaches the file and forgets it.
//!
//! The ticket is the whole capability, exactly as a download link's id is, so
//! the route needs no session and no bearer token and the id is unguessable.
//! It is spent before the body is read, so two POSTs with one ticket cannot
//! both win; the three ways it can be refused — unknown, expired, already
//! spent — answer the same 404 with the same body, so a ticket cannot be
//! probed for its state.
//!
//! Nothing here is in the database. Tickets and staged entries live in this
//! process and the bytes live in [`crate::config::Config::upload_dir`], which
//! is emptied when the process starts. A restart therefore loses every ticket
//! and every staged file, and that is the point: nothing survives that the
//! database does not know about.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Mutex;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiError, ApiResult};
use crate::domain::limits::{ATTACHMENT_MAX_BYTES, UPLOAD_TICKET_TTL_MINUTES, UPLOAD_TTL_MINUTES};
use crate::domain::link;

/// The name a file is stored under when nothing of the one the caller gave
/// survives being made safe.
const FALLBACK_NAME: &str = "upload";
/// How much of a filename is kept, in bytes of UTF-8. Long enough for any
/// name a person types, short enough that the id in front of it still leaves
/// room under the 255 bytes a file name may have.
const NAME_MAX_BYTES: usize = 120;

/// What a tool knows when it mints an upload ticket.
#[derive(Debug, Clone)]
pub struct NewUpload {
    /// Whose upload this is. Only this person's draft may attach it.
    pub user_id: i64,
    /// The token whose call minted it.
    pub token_id: i64,
    pub filename: String,
    /// What the caller says the file is. Absent, the filename decides.
    pub mime_type: Option<String>,
}

/// A minted ticket, as a tool result reports it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MintedUpload {
    pub url: String,
    pub id: String,
    pub filename: String,
    pub expires_at: DateTime<Utc>,
}

/// A ticket waiting to be spent.
#[derive(Debug, Clone)]
struct Ticket {
    id: String,
    user_id: i64,
    token_id: i64,
    filename: String,
    mime_type: Option<String>,
    expires_at: DateTime<Utc>,
}

/// A file on disk waiting to be attached.
#[derive(Debug, Clone)]
struct Staged {
    user_id: i64,
    filename: String,
    mime_type: String,
    size: usize,
    path: PathBuf,
    expires_at: DateTime<Utc>,
}

/// One staged file, read out for the draft that attaches it.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedFile {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// Why a draft could not have the files it asked for. Nothing is taken when
/// one of these is answered: a draft carries all of its files or none.
#[derive(Debug)]
pub enum TakeError {
    /// Unknown, expired, or somebody else's. One answer for all three: a
    /// model has the same thing to do about each of them, and the third is
    /// nobody's business to be able to tell apart from the first.
    Unknown(String),
    /// The files come to more than one message may carry.
    TooLarge {
        files: Vec<(String, usize)>,
    },
    Io(String),
}

impl fmt::Display for TakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(id) => write!(
                f,
                "there is no staged upload `{id}`; it was never uploaded, it has expired, or it \
                 belongs to somebody else. Mint a fresh link with gmail_upload_link, post the \
                 file to it again and use the upload_id it answers"
            ),
            Self::TooLarge { files } => {
                let total: usize = files.iter().map(|(_, size)| size).sum();
                let named: Vec<String> = files
                    .iter()
                    .map(|(name, size)| format!("{name} {}", megabytes(*size)))
                    .collect();
                write!(
                    f,
                    "the attachments come to {} ({}), over the {} Gmail allows one message; \
                     attach fewer files, or send the rest as download links",
                    megabytes(total),
                    named.join(", "),
                    megabytes(ATTACHMENT_MAX_BYTES)
                )
            }
            Self::Io(e) => write!(f, "a staged upload could not be read: {e}"),
        }
    }
}

/// A size as a person reads it. One decimal is enough to tell 24 MB from 26.
pub fn megabytes(bytes: usize) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// The tickets and the staged files of one process, and the directory their
/// bytes live in.
///
/// The directory is emptied when this is built — `serve` builds one at
/// startup — and created when the first file is written, so a process that
/// stages nothing leaves nothing behind at all.
#[derive(Debug)]
pub struct Staging {
    dir: PathBuf,
    tickets: Mutex<HashMap<String, Ticket>>,
    staged: Mutex<HashMap<String, Staged>>,
}

impl Staging {
    pub fn new(dir: PathBuf) -> Self {
        // Whatever a previous process left is not ours: its tickets are gone
        // with its memory, so its bytes are unreachable and go too.
        //
        // The contents go and the directory stays, because in the container
        // it is a bind mount: removing a mount point fails, and what has to
        // be gone is the files.
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let removed = if path.is_dir() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    if let Err(e) = removed {
                        tracing::warn!("removing {} at startup: {e}", path.display());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("emptying the upload directory {}: {e}", dir.display()),
        }
        Self {
            dir,
            tickets: Mutex::new(HashMap::new()),
            staged: Mutex::new(HashMap::new()),
        }
    }

    pub fn dir(&self) -> &FsPath {
        &self.dir
    }

    /// Mint a ticket. Expired tickets and staged files are swept on the way
    /// past, which is what link minting does: this is the one moment there is
    /// certainly something happening, and it keeps the sweep from being a
    /// background job's problem.
    fn mint(&self, new: NewUpload, now: DateTime<Utc>) -> Ticket {
        self.sweep(now);
        let ticket = Ticket {
            id: link::new_id(),
            user_id: new.user_id,
            token_id: new.token_id,
            filename: new.filename.trim().to_string(),
            mime_type: new.mime_type,
            expires_at: now + TimeDelta::minutes(UPLOAD_TICKET_TTL_MINUTES),
        };
        self.tickets
            .lock()
            .expect("the ticket lock")
            .insert(ticket.id.clone(), ticket.clone());
        ticket
    }

    /// Spend a ticket. It is gone whether or not the upload that follows
    /// works, so a second POST with the same id has nothing to race.
    fn take_ticket(&self, id: &str, now: DateTime<Utc>) -> Option<Ticket> {
        let ticket = self.tickets.lock().expect("the ticket lock").remove(id)?;
        (ticket.expires_at > now).then_some(ticket)
    }

    /// Write the bytes and record what they are. The name on disk is the
    /// upload's own id in front of the filename made safe, so two uploads of
    /// one name are two files and no name can name a path.
    fn store(
        &self,
        ticket: Ticket,
        bytes: Vec<u8>,
        now: DateTime<Utc>,
    ) -> std::io::Result<(String, Staged)> {
        let id = link::new_id();
        let path = self
            .dir
            .join(format!("{id}-{}", safe_filename(&ticket.filename)));
        self.create_dir()?;
        std::fs::write(&path, &bytes)?;
        let mime_type = match &ticket.mime_type {
            Some(given) => link::safe_mime(given),
            None => guess_mime(&ticket.filename),
        };
        let staged = Staged {
            user_id: ticket.user_id,
            filename: ticket.filename,
            mime_type,
            size: bytes.len(),
            path,
            expires_at: now + TimeDelta::minutes(UPLOAD_TTL_MINUTES),
        };
        tracing::info!(
            "staged {} ({} bytes) for user {} from token {}",
            staged.filename,
            staged.size,
            staged.user_id,
            ticket.token_id
        );
        self.staged
            .lock()
            .expect("the staged lock")
            .insert(id.clone(), staged.clone());
        Ok((id, staged))
    }

    /// Read the files a draft is about to carry, and forget them. Every id is
    /// checked before any of them is taken — one unknown id leaves the others
    /// where they are, because a draft is written once with all of its files
    /// or not at all.
    ///
    /// `user_id` is checked against every entry: one person's staged file is
    /// not another's to attach, whatever id they guessed.
    pub fn take(&self, user_id: i64, ids: &[String]) -> Result<Vec<StagedFile>, TakeError> {
        let now = Utc::now();
        let taken = {
            let mut staged = self.staged.lock().expect("the staged lock");
            let found = find(&staged, user_id, ids, now)?;
            let total: usize = found.iter().map(|(_, entry)| entry.size).sum();
            if total > ATTACHMENT_MAX_BYTES {
                return Err(TakeError::TooLarge {
                    files: found
                        .iter()
                        .map(|(_, entry)| (entry.filename.clone(), entry.size))
                        .collect(),
                });
            }
            for (id, _) in &found {
                staged.remove(id);
            }
            found
        };
        let mut files = Vec::with_capacity(taken.len());
        for (_, entry) in taken {
            let bytes = std::fs::read(&entry.path).map_err(|e| TakeError::Io(e.to_string()))?;
            remove(&entry.path);
            files.push(StagedFile {
                filename: entry.filename,
                mime_type: entry.mime_type,
                bytes,
            });
        }
        Ok(files)
    }

    /// What the named uploads weigh, and what they are called, without taking
    /// them. A draft that is rebuilt around the files it already carries has
    /// to know what the new ones add up to before it writes anything, and
    /// finding that out must not spend them: the ids are still good for the
    /// draft the person tries next.
    pub fn sizes(&self, user_id: i64, ids: &[String]) -> Result<Vec<(String, usize)>, TakeError> {
        let staged = self.staged.lock().expect("the staged lock");
        Ok(find(&staged, user_id, ids, Utc::now())?
            .into_iter()
            .map(|(_, entry)| (entry.filename, entry.size))
            .collect())
    }

    /// Everything past its time: tickets nobody used, and files nobody
    /// attached.
    fn sweep(&self, now: DateTime<Utc>) {
        self.tickets
            .lock()
            .expect("the ticket lock")
            .retain(|_, ticket| ticket.expires_at > now);
        let gone: Vec<Staged> = {
            let mut staged = self.staged.lock().expect("the staged lock");
            let expired: Vec<String> = staged
                .iter()
                .filter(|(_, entry)| entry.expires_at <= now)
                .map(|(id, _)| id.clone())
                .collect();
            expired
                .iter()
                .filter_map(|id| staged.remove(id))
                .collect::<Vec<_>>()
        };
        for entry in gone {
            remove(&entry.path);
        }
    }

    /// The directory, made when it is first needed and readable by nobody
    /// else: the files in it are somebody's mail.
    fn create_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

/// The entries the named ids stand for, or the first id that stands for
/// nothing this person may attach. Nothing is removed here: the two callers
/// differ in whether they spend what they find.
fn find(
    staged: &HashMap<String, Staged>,
    user_id: i64,
    ids: &[String],
    now: DateTime<Utc>,
) -> Result<Vec<(String, Staged)>, TakeError> {
    let mut found: Vec<(String, Staged)> = Vec::with_capacity(ids.len());
    for id in ids {
        let id = id.trim();
        match staged.get(id) {
            Some(entry)
                if entry.user_id == user_id
                    && entry.expires_at > now
                    && !found.iter().any(|(seen, _)| seen == id) =>
            {
                found.push((id.to_string(), entry.clone()));
            }
            _ => return Err(TakeError::Unknown(id.to_string())),
        }
    }
    Ok(found)
}

fn remove(path: &FsPath) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("removing the staged file {}: {e}", path.display());
    }
}

/// A filename a model supplied, made into something that can only be a name
/// in one directory. This is load-bearing: the name is interpolated into a
/// path, so `../../etc/passwd` must come out as `passwd` and nothing may keep
/// a separator. Only the basename survives, control characters and everything
/// that is not plainly a filename become `_`, and a name with nothing left of
/// it becomes [`FALLBACK_NAME`]. Letters outside ASCII are kept, because a
/// Polish invoice is called what it is called.
fn safe_filename(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches('.');
    let mut out = String::with_capacity(base.len().min(NAME_MAX_BYTES));
    for c in base.chars() {
        let keep = match c {
            '.' | '-' | '_' | ' ' => c,
            c if c.is_alphanumeric() && !c.is_control() => c,
            _ => '_',
        };
        if out.len() + keep.len_utf8() > NAME_MAX_BYTES {
            break;
        }
        out.push(keep);
    }
    let out = out.trim().to_string();
    if out.is_empty() || out.chars().all(|c| c == '.' || c == '_') {
        return FALLBACK_NAME.to_string();
    }
    out
}

/// What a file is, when the uploader did not say: the filename's own
/// extension, and `application/octet-stream` for bytes nobody can vouch for.
fn guess_mime(filename: &str) -> String {
    mime_guess::from_path(filename)
        .first()
        .map(|m| m.to_string())
        .unwrap_or_else(|| link::OCTET_STREAM.to_string())
}

/// Mint a ticket and answer the URL the bytes go to. The caller has already
/// checked the token's scope; this only writes the capability down.
pub fn mint(state: &AppState, new: NewUpload) -> MintedUpload {
    let ticket = state.staging.mint(new, Utc::now());
    MintedUpload {
        url: url_of(state, &ticket.id),
        id: ticket.id,
        filename: ticket.filename,
        expires_at: ticket.expires_at,
    }
}

/// The public URL of a ticket. Built from `GMCP_PUBLIC_URL` and never from a
/// request header, so a forwarded `Host` cannot move where the bytes are sent.
fn url_of(state: &AppState, id: &str) -> String {
    match state.config.public_url.join(&format!("/up/{id}")) {
        Ok(url) => url.to_string(),
        Err(e) => {
            tracing::error!("GMCP_PUBLIC_URL cannot carry an upload path: {e}");
            format!("/up/{id}")
        }
    }
}

/// What the uploader reads back: the id a draft tool takes, and what the
/// server now holds under it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Uploaded {
    /// Pass this to a draft tool's `attachments`.
    pub upload_id: String,
    pub filename: String,
    pub size: usize,
    pub mime_type: String,
    pub expires_at: DateTime<Utc>,
    pub note: String,
}

/// Take the bytes of one upload. Unauthenticated by design: the ticket is the
/// permission, and it is spent before a byte of the body is read.
#[utoipa::path(post, path = "/up/{id}", tag = "links",
    params(("id" = String, Path, description = "the upload ticket id")),
    request_body(content = Vec<u8>, description = "the file, as the whole request body"),
    responses(
        (status = 200, body = Uploaded, description = "the file is staged"),
        (status = 404, body = super::error::ErrorBody, description = "unknown, expired or spent"),
        (status = 413, body = super::error::ErrorBody, description = "over the attachment cap"),
    ))]
pub async fn upload(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> ApiResult<axum::Json<Uploaded>> {
    let now = Utc::now();
    // Spent first, and only then is anything read: two POSTs with one ticket
    // must not both be able to win.
    let ticket = state
        .staging
        .take_ticket(&id, now)
        .ok_or_else(ApiError::not_found)?;
    // A length the sender declared is refused before the bytes are on the
    // wire at all; a sender that declares nothing is cut off at the cap below.
    if let Some(length) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        && length > ATTACHMENT_MAX_BYTES
    {
        return Err(too_large());
    }
    let bytes = axum::body::to_bytes(body, ATTACHMENT_MAX_BYTES)
        .await
        .map_err(|_| too_large())?;
    if bytes.is_empty() {
        return Err(ApiError::bad_request(
            "the request body is empty; post the file itself as the body",
        ));
    }
    let (upload_id, staged) = state
        .staging
        .store(ticket, bytes.to_vec(), now)
        .map_err(ApiError::internal)?;
    Ok(axum::Json(Uploaded {
        upload_id,
        filename: staged.filename,
        size: staged.size,
        mime_type: staged.mime_type,
        expires_at: staged.expires_at,
        note: format!(
            "pass upload_id as one of `attachments` within {UPLOAD_TTL_MINUTES} minutes: to \
             gmail_create_draft, gmail_reply_draft or gmail_update_draft for a draft you are \
             writing, or to gmail_attach_to_draft for one that already exists. The file is \
             attached once and then forgotten"
        ),
    }))
}

/// The one answer for a file over the cap, wherever it is noticed.
fn too_large() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!(
            "the file is over the {} one mail may carry",
            megabytes(ATTACHMENT_MAX_BYTES)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A staging directory of this test's own, removed when the test ends.
    /// The only thing in this suite that touches the disk at all.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("gmcp-staging-test-{}", link::new_id())))
        }

        fn staging(&self) -> Staging {
            Staging::new(self.0.clone())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn new_upload(user_id: i64, filename: &str) -> NewUpload {
        NewUpload {
            user_id,
            token_id: 1,
            filename: filename.to_string(),
            mime_type: None,
        }
    }

    fn files_in(dir: &FsPath) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_ticket_is_good_once_and_for_a_quarter_of_an_hour() {
        let dir = TempDir::new();
        let staging = dir.staging();
        let now = Utc::now();
        let ticket = staging.mint(new_upload(1, "report.pdf"), now);
        assert!(staging.take_ticket(&ticket.id, now).is_some());
        // The second POST has nothing left to spend, which is what keeps two
        // uploads from racing for one ticket.
        assert!(staging.take_ticket(&ticket.id, now).is_none());
        // Nor does an id nobody minted.
        assert!(staging.take_ticket("neverminted0000000000", now).is_none());

        let later = staging.mint(new_upload(1, "report.pdf"), now);
        assert!(
            staging
                .take_ticket(&later.id, now + TimeDelta::minutes(16))
                .is_none(),
            "a ticket does not outlive its fifteen minutes"
        );
    }

    #[test]
    fn a_staged_file_is_one_persons_and_is_read_once() {
        let dir = TempDir::new();
        let staging = dir.staging();
        let now = Utc::now();
        let ticket = staging.mint(new_upload(7, "../../etc/passwd"), now);
        let ticket = staging.take_ticket(&ticket.id, now).unwrap();
        let (id, staged) = staging.store(ticket, b"the bytes".to_vec(), now).unwrap();

        // The name a model supplied names a file in the staging directory and
        // nothing else, whatever it looked like.
        assert_eq!(staged.path.parent(), Some(staging.dir()));
        assert_eq!(files_in(staging.dir()).len(), 1);
        assert!(files_in(staging.dir())[0].ends_with("-passwd"));
        assert!(!dir.0.join("etc").exists());
        // The filename the mail carries is still the one that was uploaded.
        assert_eq!(staged.filename, "../../etc/passwd");

        // Somebody else's id is nobody else's to attach, and asking leaves the
        // file where it was.
        let refused = staging.take(8, std::slice::from_ref(&id)).unwrap_err();
        assert!(matches!(refused, TakeError::Unknown(_)), "{refused}");
        assert_eq!(files_in(staging.dir()).len(), 1);

        let files = staging.take(7, std::slice::from_ref(&id)).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].bytes, b"the bytes");
        assert_eq!(files[0].filename, "../../etc/passwd");
        // Reading it is taking it: the entry and the bytes are both gone.
        assert!(staging.take(7, &[id]).is_err());
        assert!(files_in(staging.dir()).is_empty());
    }

    #[test]
    fn one_unknown_id_leaves_every_other_file_where_it_is() {
        let dir = TempDir::new();
        let staging = dir.staging();
        let now = Utc::now();
        let mut ids = Vec::new();
        for name in ["one.pdf", "two.pdf"] {
            let ticket = staging.mint(new_upload(7, name), now);
            let ticket = staging.take_ticket(&ticket.id, now).unwrap();
            ids.push(staging.store(ticket, b"x".to_vec(), now).unwrap().0);
        }
        let mut asked = ids.clone();
        asked.push("notathing000000000000".into());
        // A draft carries all of its files or none, so a call that names one
        // id nobody knows takes nothing at all.
        assert!(staging.take(7, &asked).is_err());
        assert_eq!(files_in(staging.dir()).len(), 2);

        let files = staging.take(7, &ids).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files_in(staging.dir()).is_empty());
    }

    #[test]
    fn a_restart_loses_everything_the_database_does_not_know_about() {
        let dir = TempDir::new();
        let now = Utc::now();
        let id = {
            let staging = dir.staging();
            let ticket = staging.mint(new_upload(7, "half-finished.pdf"), now);
            let ticket = staging.take_ticket(&ticket.id, now).unwrap();
            let (id, _) = staging.store(ticket, b"x".to_vec(), now).unwrap();
            assert_eq!(files_in(staging.dir()).len(), 1);
            id
        };
        // A second process over the same directory. Its tickets and its
        // staged entries went with the first one's memory, so the bytes are
        // unreachable and go as well.
        let restarted = dir.staging();
        assert!(files_in(restarted.dir()).is_empty());
        assert!(restarted.take(7, &[id]).is_err());
    }

    #[test]
    fn a_file_nobody_attached_is_swept_with_the_next_ticket() {
        let dir = TempDir::new();
        let staging = dir.staging();
        let now = Utc::now();
        let ticket = staging.mint(new_upload(7, "forgotten.pdf"), now);
        let ticket = staging.take_ticket(&ticket.id, now).unwrap();
        let (id, _) = staging.store(ticket, b"x".to_vec(), now).unwrap();
        assert_eq!(files_in(staging.dir()).len(), 1);

        // An hour on, minting anything at all clears it: the sweep rides along
        // with the one moment there is certainly something happening.
        staging.mint(
            new_upload(7, "next.pdf"),
            now + TimeDelta::minutes(UPLOAD_TTL_MINUTES + 1),
        );
        assert!(files_in(staging.dir()).is_empty());
        assert!(staging.take(7, &[id]).is_err());
    }

    #[test]
    fn what_a_draft_may_not_carry_is_said_with_the_sizes() {
        let error = TakeError::TooLarge {
            files: vec![
                ("plan.pdf".into(), 20 * 1024 * 1024),
                ("zdjęcia.zip".into(), 10 * 1024 * 1024),
            ],
        }
        .to_string();
        assert!(error.contains("30.0 MB"), "{error}");
        assert!(error.contains("plan.pdf 20.0 MB"), "{error}");
        assert!(error.contains("zdjęcia.zip 10.0 MB"), "{error}");
        assert!(error.contains("25.0 MB"), "{error}");
    }

    #[test]
    fn a_filename_from_a_model_cannot_name_a_path() {
        // The whole point: a name is a name in one directory, never a way out
        // of it.
        assert_eq!(safe_filename("../../etc/passwd"), "passwd");
        assert_eq!(safe_filename("/etc/shadow"), "shadow");
        assert_eq!(safe_filename(r"..\..\windows\system32"), "system32");
        assert_eq!(safe_filename(".."), FALLBACK_NAME);
        assert_eq!(safe_filename("."), FALLBACK_NAME);
        assert_eq!(safe_filename(""), FALLBACK_NAME);
        assert_eq!(safe_filename("   "), FALLBACK_NAME);
        assert_eq!(safe_filename(".hidden"), "hidden");
        // A name a person would type survives, diacritics included.
        assert_eq!(safe_filename("zażółć gęślą.pdf"), "zażółć gęślą.pdf");
        assert_eq!(safe_filename("Faktura 04-2026.pdf"), "Faktura 04-2026.pdf");
        // Everything else becomes an underscore rather than being dropped, so
        // two names cannot collapse into one by accident.
        assert_eq!(safe_filename("re\"port\".csv"), "re_port_.csv");
        assert_eq!(safe_filename("a\nb\0c.txt"), "a_b_c.txt");
        assert_eq!(safe_filename("$(rm -rf ~).sh"), "__rm -rf __.sh");
        // And a name long enough to break the file system is cut.
        let long = format!("{}.pdf", "ą".repeat(200));
        assert!(safe_filename(&long).len() <= NAME_MAX_BYTES);
    }

    #[test]
    fn a_file_is_named_by_what_it_is_when_nobody_says() {
        assert_eq!(guess_mime("report.pdf"), "application/pdf");
        assert_eq!(guess_mime("notes.txt"), "text/plain");
        assert_eq!(guess_mime("whatever"), link::OCTET_STREAM);
    }

    #[test]
    fn sizes_are_reported_the_way_a_person_reads_them() {
        assert_eq!(megabytes(ATTACHMENT_MAX_BYTES), "25.0 MB");
        assert_eq!(megabytes(1024 * 1024 * 3 / 2), "1.5 MB");
    }
}
