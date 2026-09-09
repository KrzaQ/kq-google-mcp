//! The Sheets tools. Reads are ranges in A1 notation with the values
//! formatted as the sheet shows them; writes are append, overwrite, a new tab
//! and a new spreadsheet, and all four take `confirmed`.
//!
//! A spreadsheet is the one place where a wrong write is quietly destructive —
//! `sheets_update_range` overwrites whatever is in the range — so the preview
//! spells out the range and the rows before anything happens.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::dto::{self, Confirmable, PreviewOut};
use super::{Call, Gmcp, bad, capped};
use crate::domain::scope::Service;
use crate::google::{drive, sheets, text};

/// How many rows a preview prints before it starts counting instead.
const PREVIEW_ROWS: usize = 10;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SpreadsheetParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// The spreadsheet id, the long string in its Sheets URL
    pub spreadsheet_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadRangeParam {
    pub account: String,
    pub spreadsheet_id: String,
    /// A1 notation, e.g. "Q3!A1:D50" or just "Q3" for the whole tab
    pub range: String,
    /// Stop after this many rows; default 200, at most 2000
    pub max_rows: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AppendRowsParam {
    pub account: String,
    pub spreadsheet_id: String,
    /// The tab to append to, by title
    pub tab: String,
    /// The rows, each a list of cell values as strings. They are entered the
    /// way a person typing them would be: "=A1+B1" becomes a formula and
    /// "2026-09-08" a date.
    pub rows: Vec<Vec<String>>,
    /// Must be true to write. Call with false first, show the person the rows,
    /// and pass true only after they agree.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateRangeParam {
    pub account: String,
    pub spreadsheet_id: String,
    /// A1 notation. Everything in this range is overwritten.
    pub range: String,
    /// The rows to write, each a list of cell values as strings
    pub rows: Vec<Vec<String>>,
    /// Must be true to write. This overwrites what is there, so show the
    /// person the range and the rows first.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddTabParam {
    pub account: String,
    pub spreadsheet_id: String,
    /// The new tab's title
    pub title: String,
    /// Must be true to write.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SheetsCreateParam {
    pub account: String,
    pub title: String,
    /// Extra tabs to add after the first one, by title
    pub tabs: Option<Vec<String>>,
    /// The rows of the first tab, each a list of cell values as strings
    pub rows: Option<Vec<Vec<String>>>,
    /// The Drive folder to put it in; the account's root by default
    pub folder_id: Option<String>,
    /// Must be true to write. Call with false first and show the person the
    /// title and the rows.
    pub confirmed: bool,
}

#[tool_router(router = sheets_router, vis = "pub(crate)")]
impl Gmcp {
    #[tool(
        description = "The tabs of a spreadsheet with their titles, positions and row and column \
                       counts. Call this before reading or writing a range, so the range names a \
                       tab that exists."
    )]
    async fn sheets_list_tabs(
        &self,
        Parameters(p): Parameters<SpreadsheetParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::SpreadsheetOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Sheets).await?;
        let sheet = sheets::get(
            &self.google()?.client,
            connection.id,
            p.spreadsheet_id.trim(),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(spreadsheet_out(connection.label, sheet)))
    }

    #[tool(
        description = "Read a range in A1 notation and get the rows back as lists of strings, \
                       formatted the way the sheet shows them. Trailing empty cells are not \
                       padded, so rows can differ in length."
    )]
    async fn sheets_read_range(
        &self,
        Parameters(p): Parameters<ReadRangeParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::RangeOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Sheets).await?;
        let max = capped(p.max_rows, 200, 2000) as usize;
        // One more than asked for, so "there is more" is a fact and not a guess.
        let rows = sheets::values_get(
            &self.google()?.client,
            connection.id,
            p.spreadsheet_id.trim(),
            p.range.trim(),
            Some(max + 1),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        let truncated = rows.len() > max;
        let mut rows = rows;
        rows.truncate(max);
        Ok(Json(dto::RangeOut {
            account: connection.label,
            spreadsheet_id: p.spreadsheet_id.trim().to_string(),
            range: p.range.trim().to_string(),
            row_count: rows.len(),
            truncated,
            rows,
        }))
    }

    #[tool(
        description = "Add rows after the last used row of a tab. Needs confirmed=true: call \
                       once with confirmed=false, show the person the rows exactly as they will \
                       be written, and write only after they say yes."
    )]
    async fn sheets_append_rows(
        &self,
        Parameters(p): Parameters<AppendRowsParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::SheetWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Sheets).await?;
        let spreadsheet_id = p.spreadsheet_id.trim().to_string();
        let tab = p.tab.trim().to_string();
        if p.rows.is_empty() {
            return Err(bad("there is nothing to append: rows is empty"));
        }
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "append {} rows to the tab {tab:?} of spreadsheet {spreadsheet_id} in `{}`",
                    p.rows.len(),
                    connection.label
                ),
                preview_rows(&p.rows),
            ))));
        }
        let written = sheets::values_append(
            &self.google()?.client,
            connection.id,
            &spreadsheet_id,
            &tab,
            p.rows.clone(),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(write_out(
            connection.label,
            spreadsheet_id,
            written,
            format!("{} rows appended to {tab:?}", p.rows.len()),
        ))))
    }

    #[tool(
        description = "Write rows over exactly the given range, replacing whatever is there. \
                       Needs confirmed=true, and the preview is worth showing in full: this \
                       overwrites cells and there is no undo here."
    )]
    async fn sheets_update_range(
        &self,
        Parameters(p): Parameters<UpdateRangeParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::SheetWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Sheets).await?;
        let spreadsheet_id = p.spreadsheet_id.trim().to_string();
        let range = p.range.trim().to_string();
        if p.rows.is_empty() {
            return Err(bad("there is nothing to write: rows is empty"));
        }
        if !p.confirmed {
            let mut details = vec![format!(
                "everything currently in {range} is overwritten; there is no undo here"
            )];
            details.extend(preview_rows(&p.rows));
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "overwrite {range} of spreadsheet {spreadsheet_id} in `{}` with {} rows",
                    connection.label,
                    p.rows.len()
                ),
                details,
            ))));
        }
        let written = sheets::values_update(
            &self.google()?.client,
            connection.id,
            &spreadsheet_id,
            &range,
            p.rows.clone(),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(write_out(
            connection.label,
            spreadsheet_id,
            written,
            format!("{} rows written over {range}", p.rows.len()),
        ))))
    }

    #[tool(
        description = "Add a tab to a spreadsheet. Needs confirmed=true. Nothing here deletes or \
                       reorders a tab."
    )]
    async fn sheets_add_tab(
        &self,
        Parameters(p): Parameters<AddTabParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::SheetWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Sheets).await?;
        let spreadsheet_id = p.spreadsheet_id.trim().to_string();
        let title = p.title.trim().to_string();
        if title.is_empty() {
            return Err(bad("a tab needs a title"));
        }
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "add a tab called {title:?} to spreadsheet {spreadsheet_id} in `{}`",
                    connection.label
                ),
                vec!["the tab is added empty, at the end".into()],
            ))));
        }
        let tab = sheets::add_tab(
            &self.google()?.client,
            connection.id,
            &spreadsheet_id,
            &title,
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(dto::SheetWriteOut {
            account: connection.label,
            url: Some(format!(
                "https://docs.google.com/spreadsheets/d/{spreadsheet_id}/edit"
            )),
            spreadsheet_id,
            updated_range: Some(tab.title.clone()),
            updated_rows: Some(tab.rows),
            updated_cells: None,
            written: format!("tab {:?} added", tab.title),
        })))
    }

    #[tool(
        description = "Create a spreadsheet and return its id and URL. The rows, if any, go on \
                       the first tab; extra tabs are added empty. Needs confirmed=true after the \
                       person has seen the title and the rows."
    )]
    async fn sheets_create(
        &self,
        Parameters(p): Parameters<SheetsCreateParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::SheetWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Sheets).await?;
        let title = p.title.trim().to_string();
        if title.is_empty() {
            return Err(bad("a spreadsheet needs a title"));
        }
        let rows = p.rows.unwrap_or_default();
        let tabs: Vec<String> = p
            .tabs
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if !p.confirmed {
            let mut details = vec![format!(
                "{} rows on the first tab{}",
                rows.len(),
                if tabs.is_empty() {
                    String::new()
                } else {
                    format!(", plus empty tabs {}", tabs.join(", "))
                }
            )];
            details.extend(preview_rows(&rows));
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "create a spreadsheet called {title:?} in `{}`",
                    connection.label
                ),
                details,
            ))));
        }
        let client = &self.google()?.client;
        let csv = text::to_csv(&rows).map_err(|e| self.google_err_for(&connection, e))?;
        let file = drive::create_sheet_from_csv(
            client,
            connection.id,
            &title,
            &csv,
            p.folder_id
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty()),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        // The spreadsheet exists from here on. A tab that will not be added is
        // reported rather than raised: an error would drop the new file's id,
        // and the retry it invites would make a second spreadsheet.
        let mut missing: Vec<String> = Vec::new();
        let mut reason: Option<String> = None;
        for tab in &tabs {
            if let Err(e) = sheets::add_tab(client, connection.id, &file.id, tab).await {
                reason.get_or_insert_with(|| e.to_string());
                missing.push(tab.clone());
            }
        }
        Ok(Json(Confirmable::Done(dto::SheetWriteOut {
            account: connection.label,
            url: Some(format!(
                "https://docs.google.com/spreadsheets/d/{}/edit",
                file.id
            )),
            spreadsheet_id: file.id,
            updated_range: None,
            updated_rows: Some(rows.len() as i64),
            updated_cells: None,
            written: created_line(rows.len(), &tabs, &missing, reason.as_deref()),
        })))
    }
}

/// What `sheets_create` says it did, including the tabs it could not add. The
/// spreadsheet is named as made either way, because it was.
fn created_line(rows: usize, tabs: &[String], missing: &[String], reason: Option<&str>) -> String {
    let added = tabs.len() - missing.len();
    let mut line = format!("created with {rows} rows");
    if added > 0 {
        line.push_str(&format!(" and {added} extra tabs"));
    }
    if !missing.is_empty() {
        line.push_str(&format!(
            ". The spreadsheet was made, but the {} could not be added ({}); \
             add them with sheets_add_tab rather than creating the spreadsheet again",
            missing
                .iter()
                .map(|t| format!("tab {t:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            reason.unwrap_or("no reason given")
        ));
    }
    line
}

fn spreadsheet_out(account: String, sheet: sheets::Spreadsheet) -> dto::SpreadsheetOut {
    dto::SpreadsheetOut {
        account,
        spreadsheet_id: sheet.spreadsheet_id,
        title: sheet.title,
        url: sheet.url,
        tabs: sheet.tabs.into_iter().map(Into::into).collect(),
    }
}

fn write_out(
    account: String,
    spreadsheet_id: String,
    written: sheets::WriteResult,
    summary: String,
) -> dto::SheetWriteOut {
    dto::SheetWriteOut {
        account,
        url: Some(format!(
            "https://docs.google.com/spreadsheets/d/{spreadsheet_id}/edit"
        )),
        spreadsheet_id,
        updated_range: Some(written.updated_range),
        updated_rows: Some(written.updated_rows),
        updated_cells: Some(written.updated_cells),
        written: summary,
    }
}

/// The rows a preview prints, as the person would read them.
fn preview_rows(rows: &[Vec<String>]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .take(PREVIEW_ROWS)
        .map(|row| row.join(" | "))
        .collect();
    if rows.len() > PREVIEW_ROWS {
        out.push(format!("… and {} more rows", rows.len() - PREVIEW_ROWS));
    }
    out
}
