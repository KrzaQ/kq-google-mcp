//! The Sheets tools. Reads are ranges in A1 notation, as the sheet shows
//! them or as the formulas behind them; writes are append, overwrite, whole
//! rows in and out, a copy of a row's formatting, a new tab and a new
//! spreadsheet, and every one of them takes `confirmed`.
//!
//! A spreadsheet is the one place where a wrong write is quietly destructive —
//! `sheets_update_range` overwrites whatever is in the range — so the preview
//! spells out the range and the rows before anything happens.
//!
//! The quietest loss of all is a formula. A model that reads a column as the
//! numbers it displays and writes those numbers back replaces `=SUM(C2:C10)`
//! with a frozen total, and the sheet keeps looking correct while it has
//! stopped adding up. So `sheets_read_range` can read the formulas
//! themselves, and the preview of an overwrite says which cells in the target
//! range hold one today.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::dto::{self, Confirmable, PreviewOut};
use super::{Call, Gmcp, bad, capped};
use crate::db::Connection;
use crate::domain::scope::Service;
use crate::google::{drive, sheets, text};

/// How many rows a preview prints before it starts counting instead.
const PREVIEW_ROWS: usize = 10;

/// How many of the formulas an overwrite would replace the preview names
/// before it starts counting instead.
const PREVIEW_FORMULAS: usize = 5;

/// How long a formula is allowed to be in a preview line.
const FORMULA_CHARS: usize = 60;

/// The most rows one structural call may add, remove or reformat. A model
/// that means five and says five hundred is stopped here rather than in the
/// grid, where undoing it is the person's problem.
const MAX_ROWS_PER_CALL: u32 = 1000;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpreadsheetParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// The spreadsheet id, the long string in its Sheets URL
    pub spreadsheet_id: String,
}

/// What a cell should be read as. The default shows what the sheet shows.
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RenderParam {
    /// What the sheet displays: "1 234,50 zł", "8 Sep 2026"
    #[default]
    Formatted,
    /// The formula behind the cell, "=SUM(C2:C10)", or the literal value
    /// where there is no formula
    Formula,
    /// The underlying value, unformatted: 1234.5, and a date as its serial
    /// number
    Unformatted,
}

impl From<RenderParam> for sheets::Render {
    fn from(r: RenderParam) -> Self {
        match r {
            RenderParam::Formatted => sheets::Render::Formatted,
            RenderParam::Formula => sheets::Render::Formula,
            RenderParam::Unformatted => sheets::Render::Unformatted,
        }
    }
}

impl RenderParam {
    fn as_str(self) -> &'static str {
        match self {
            RenderParam::Formatted => "formatted",
            RenderParam::Formula => "formula",
            RenderParam::Unformatted => "unformatted",
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadRangeParam {
    pub account: String,
    pub spreadsheet_id: String,
    /// A1 notation, e.g. "Q3!A1:D50" or just "Q3" for the whole tab
    pub range: String,
    /// Stop after this many rows; default 200, at most 2000
    pub max_rows: Option<u32>,
    /// How to read each cell: "formatted" (the default), "formula" or
    /// "unformatted". Read with "formula" before you copy cells anywhere.
    pub render: Option<RenderParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct InsertRowsParam {
    pub account: String,
    pub spreadsheet_id: String,
    /// The tab to insert into, by title
    pub tab: String,
    /// The row number the first new row takes, counting from 1 as A1 notation
    /// does: 5 puts the new rows where row 5 is today and moves it down.
    pub at_row: u32,
    /// How many rows to insert
    pub count: u32,
    /// Must be true to write. Call with false first and show the person where
    /// the rows go.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteRowsParam {
    pub account: String,
    pub spreadsheet_id: String,
    /// The tab to delete from, by title
    pub tab: String,
    /// The first row to delete, counting from 1
    pub from_row: u32,
    /// How many rows to delete
    pub count: u32,
    /// Must be true to delete. Call with false first: the preview shows what
    /// is in those rows today, which is the only guard against deleting the
    /// wrong ones.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CopyFormatParam {
    pub account: String,
    pub spreadsheet_id: String,
    /// The tab, by title. The row copied and the rows written are both on it.
    pub tab: String,
    /// The row whose formatting is copied, counting from 1
    pub from_row: u32,
    /// The first row to give that formatting to, counting from 1
    pub to_row: u32,
    /// How many rows to give it to, starting at to_row
    pub count: u32,
    /// Must be true to write.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddTabParam {
    pub account: String,
    pub spreadsheet_id: String,
    /// The new tab's title
    pub title: String,
    /// Must be true to write.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
        description = "Read a range in A1 notation and get the rows back as lists of strings. \
                       `render` decides what a cell is: \"formatted\" (the default) is what the \
                       sheet displays, \"formula\" is the formula behind it, \"unformatted\" is \
                       the raw value. Read with render=\"formula\" whenever you are about to \
                       copy, move or rewrite cells: a cell that displays a number may hold a \
                       formula, and writing the number back freezes it. Trailing empty cells are \
                       not padded, so rows can differ in length."
    )]
    async fn sheets_read_range(
        &self,
        Parameters(p): Parameters<ReadRangeParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::RangeOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Sheets).await?;
        let max = capped(p.max_rows, 200, 2000) as usize;
        let render = p.render.unwrap_or_default();
        // One more than asked for, so "there is more" is a fact and not a guess.
        let read = sheets::values_get(
            &self.google()?.client,
            connection.id,
            p.spreadsheet_id.trim(),
            p.range.trim(),
            render.into(),
            Some(max + 1),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        let mut rows = read.rows;
        let truncated = rows.len() > max;
        rows.truncate(max);
        Ok(Json(dto::RangeOut {
            account: connection.label,
            spreadsheet_id: p.spreadsheet_id.trim().to_string(),
            range: p.range.trim().to_string(),
            render: render.as_str().to_string(),
            row_count: rows.len(),
            truncated,
            rows,
        }))
    }

    #[tool(
        description = "Add rows after the last used row of a tab. Needs confirmed=true: call \
                       once with confirmed=false, show the person the rows exactly as they will \
                       be written, and write only after they say yes. A write carries values \
                       and no formatting: rows appended under a formatted table arrive plain, so \
                       currency, dates and borders do not follow. Say so before you append, and \
                       use sheets_copy_format afterwards to make the new rows match the ones \
                       above."
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
                       overwrites cells and there is no undo here. The preview also reads the \
                       range as formulas and says which cells hold one today, because writing a \
                       displayed value over a formula replaces the formula with a fixed number \
                       and the sheet goes on looking right. A write carries values and no \
                       formatting; the cells keep the formatting they already had."
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
            // What is there now, as formulas rather than as the numbers they
            // show. Only as many rows as are being written are read, because
            // those are the only cells an update touches.
            let current = sheets::values_get(
                &self.google()?.client,
                connection.id,
                &spreadsheet_id,
                &range,
                sheets::Render::Formula,
                Some(p.rows.len()),
            )
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
            let mut details = vec![
                format!("everything currently in {range} is overwritten; there is no undo here"),
                formula_warning(&formulas_overwritten(&current, &p.rows)),
            ];
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

    /// The tab a `tab` argument names, with its numeric id. Every structural
    /// change below needs that id and a model only ever has the title, so the
    /// lookup lives here once and answers an unknown title with the titles
    /// that do exist.
    async fn tab(
        &self,
        connection: &Connection,
        spreadsheet_id: &str,
        title: &str,
    ) -> Result<sheets::Tab, ErrorData> {
        let title = title.trim();
        if title.is_empty() {
            return Err(bad("name the tab to change, by title"));
        }
        let spreadsheet = sheets::get(&self.google()?.client, connection.id, spreadsheet_id)
            .await
            .map_err(|e| self.google_err_for(connection, e))?;
        spreadsheet
            .tabs
            .iter()
            .find(|t| t.title.eq_ignore_ascii_case(title))
            .cloned()
            .ok_or_else(|| {
                bad(format!(
                    "there is no tab called {title:?} in that spreadsheet; it has {}",
                    spreadsheet
                        .tabs
                        .iter()
                        .map(|t| format!("{:?}", t.title))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    #[tool(
        description = "Insert empty rows into a tab, moving everything below them down. Rows are \
                       numbered from 1 as in A1 notation, so at_row=5 puts the new rows where \
                       row 5 is today. The new rows take the formatting of the row above them, \
                       except at row 1, where there is none to take. Needs confirmed=true."
    )]
    async fn sheets_insert_rows(
        &self,
        Parameters(p): Parameters<InsertRowsParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::SheetWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Sheets).await?;
        let spreadsheet_id = p.spreadsheet_id.trim().to_string();
        let count = row_count(p.count)?;
        let at_row = row_number(p.at_row, "at_row")?;
        let tab = self.tab(&connection, &spreadsheet_id, &p.tab).await?;
        let last = at_row + count - 1;
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "insert {count} empty rows at row {at_row} of the tab {:?} \
                     in spreadsheet {spreadsheet_id} in `{}`",
                    tab.title, connection.label
                ),
                vec![
                    format!(
                        "rows {at_row} to {last} become empty; what is row {at_row} today \
                         becomes row {}. Nothing is overwritten",
                        at_row + count
                    ),
                    if at_row > 1 {
                        format!("the new rows take the formatting of row {}", at_row - 1)
                    } else {
                        "inserted at the top, the new rows carry no formatting".into()
                    },
                ],
            ))));
        }
        sheets::insert_rows(
            &self.google()?.client,
            connection.id,
            &spreadsheet_id,
            tab.sheet_id,
            at_row,
            count,
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(row_out(
            connection.label,
            spreadsheet_id,
            &tab.title,
            at_row,
            last,
            count,
            format!("{count} rows inserted at row {at_row} of {:?}", tab.title),
        ))))
    }

    #[tool(
        description = "Delete whole rows from a tab, moving everything below them up. Rows are \
                       numbered from 1. Needs confirmed=true, and the preview shows what those \
                       rows hold today — read it to the person, because it is the only thing \
                       standing between a miscounted row number and lost data."
    )]
    async fn sheets_delete_rows(
        &self,
        Parameters(p): Parameters<DeleteRowsParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::SheetWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Sheets).await?;
        let spreadsheet_id = p.spreadsheet_id.trim().to_string();
        let count = row_count(p.count)?;
        let from_row = row_number(p.from_row, "from_row")?;
        let tab = self.tab(&connection, &spreadsheet_id, &p.tab).await?;
        let last = from_row + count - 1;
        if !p.confirmed {
            let doomed = sheets::values_get(
                &self.google()?.client,
                connection.id,
                &spreadsheet_id,
                &rows_range(&tab.title, from_row, last),
                sheets::Render::Formatted,
                Some(count as usize),
            )
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
            let mut details = vec![
                format!(
                    "rows {from_row} to {last} are deleted and everything below them moves up; \
                     there is no undo here"
                ),
                if doomed.rows.is_empty() {
                    "those rows are empty today".to_string()
                } else {
                    "they hold this today:".to_string()
                },
            ];
            details.extend(preview_rows(&doomed.rows));
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "delete rows {from_row} to {last} of the tab {:?} \
                     in spreadsheet {spreadsheet_id} in `{}`",
                    tab.title, connection.label
                ),
                details,
            ))));
        }
        sheets::delete_rows(
            &self.google()?.client,
            connection.id,
            &spreadsheet_id,
            tab.sheet_id,
            from_row,
            count,
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(row_out(
            connection.label,
            spreadsheet_id,
            &tab.title,
            from_row,
            last,
            count,
            format!("rows {from_row} to {last} of {:?} deleted", tab.title),
        ))))
    }

    #[tool(
        description = "Copy the formatting of one whole row onto other whole rows of the same \
                       tab: fonts, colours, borders, number and date formats. Values and \
                       formulas are not copied and are not disturbed. Whole rows only — this is \
                       the tool for \"make these rows look like the one above\" after an append, \
                       and there is deliberately no way here to format a single cell, a column \
                       or a rectangle. Needs confirmed=true."
    )]
    async fn sheets_copy_format(
        &self,
        Parameters(p): Parameters<CopyFormatParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::SheetWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Sheets).await?;
        let spreadsheet_id = p.spreadsheet_id.trim().to_string();
        let count = row_count(p.count)?;
        let from_row = row_number(p.from_row, "from_row")?;
        let to_row = row_number(p.to_row, "to_row")?;
        let tab = self.tab(&connection, &spreadsheet_id, &p.tab).await?;
        let last = to_row + count - 1;
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "give rows {to_row} to {last} of the tab {:?} in spreadsheet \
                     {spreadsheet_id} in `{}` the formatting of row {from_row}",
                    tab.title, connection.label
                ),
                vec![
                    "only the formatting travels: what those rows say stays exactly as it is"
                        .into(),
                    format!("any formatting rows {to_row} to {last} have of their own is replaced"),
                ],
            ))));
        }
        sheets::copy_row_format(
            &self.google()?.client,
            connection.id,
            &spreadsheet_id,
            tab.sheet_id,
            from_row,
            to_row,
            count,
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(row_out(
            connection.label,
            spreadsheet_id,
            &tab.title,
            to_row,
            last,
            count,
            format!(
                "rows {to_row} to {last} of {:?} now have the formatting of row {from_row}",
                tab.title
            ),
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

/// A row number as the tools take it: 1-based, because every range these
/// tools speak is A1 notation and a model that has just read `C7` should be
/// able to say 7.
fn row_number(value: u32, name: &str) -> Result<u32, ErrorData> {
    if value == 0 {
        return Err(bad(format!(
            "{name} counts from 1, the way the row numbers down the side of the sheet do; \
             there is no row 0"
        )));
    }
    Ok(value)
}

/// How many rows one structural call may touch.
fn row_count(value: u32) -> Result<u32, ErrorData> {
    match value {
        0 => Err(bad("count must be at least 1")),
        n if n > MAX_ROWS_PER_CALL => Err(bad(format!(
            "count is at most {MAX_ROWS_PER_CALL} rows in one call; \
             ask the person before doing anything on that scale"
        ))),
        n => Ok(n),
    }
}

/// Whole rows of one tab in A1 notation, `'September'!5:7`. The title is
/// quoted always and its own quotes are doubled, because a tab called
/// `Q3 2026` or `Anna's` is a perfectly ordinary tab.
fn rows_range(tab: &str, from: u32, to: u32) -> String {
    format!("'{}'!{from}:{to}", tab.replace('\'', "''"))
}

/// What a structural change answers: which rows it touched, as a range a
/// person can look at.
fn row_out(
    account: String,
    spreadsheet_id: String,
    tab: &str,
    from: u32,
    to: u32,
    count: u32,
    written: String,
) -> dto::SheetWriteOut {
    dto::SheetWriteOut {
        account,
        url: Some(format!(
            "https://docs.google.com/spreadsheets/d/{spreadsheet_id}/edit"
        )),
        spreadsheet_id,
        updated_range: Some(rows_range(tab, from, to)),
        updated_rows: Some(count as i64),
        updated_cells: None,
        written,
    }
}

/// The cells an update would overwrite that hold a formula today, as
/// `C2 =SUM(C2:C10)` lines. Only the cells actually being written are
/// considered: a formula one column to the right of the new values survives
/// the write and does not belong in the warning.
fn formulas_overwritten(current: &sheets::RangeValues, writing: &[Vec<String>]) -> Vec<String> {
    let anchor = anchor(&current.range);
    let mut found = Vec::new();
    for (r, row) in current.rows.iter().enumerate() {
        let width = writing.get(r).map(Vec::len).unwrap_or(0);
        for (c, value) in row.iter().take(width).enumerate() {
            if !value.starts_with('=') {
                continue;
            }
            let reference = match anchor {
                Some((column, row_one)) => a1_cell(column + c, row_one + r as u32),
                None => format!("row {} cell {}", r + 1, c + 1),
            };
            found.push(format!("{reference} {}", shorten(value)));
        }
    }
    found
}

/// The preview line about formulas. It says the good news too: a model that
/// is told nothing would have to guess whether the check happened.
fn formula_warning(found: &[String]) -> String {
    if found.is_empty() {
        return "no cell being written over holds a formula today".to_string();
    }
    let shown = found.len().min(PREVIEW_FORMULAS);
    let mut line = format!(
        "{} of the cells being written over hold a formula today, and writing a value there \
         replaces the formula with a fixed number that stops recalculating: {}",
        found.len(),
        found[..shown].join(", ")
    );
    if found.len() > shown {
        line.push_str(&format!(", and {} more", found.len() - shown));
    }
    line.push_str(
        ". Read them with sheets_read_range render=\"formula\" and keep the formulas \
         unless the person asked for fixed values",
    );
    line
}

/// The top left cell of a range Google resolved, as a 0-based column and a
/// 1-based row: `September!B2:D5` is `(1, 2)`. `None` when the range is not
/// in that shape, which is why the caller has a fallback.
fn anchor(range: &str) -> Option<(usize, u32)> {
    let cell = range.rsplit('!').next()?.split(':').next()?;
    let letters: String = cell
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .flat_map(char::to_uppercase)
        .collect();
    let digits = &cell[letters.len()..];
    if letters.is_empty() || digits.is_empty() {
        return None;
    }
    let mut column: usize = 0;
    for letter in letters.bytes() {
        column = column * 26 + (letter - b'A') as usize + 1;
    }
    Some((column - 1, digits.parse().ok()?))
}

/// A cell's A1 reference from a 0-based column and a 1-based row.
fn a1_cell(column: usize, row: u32) -> String {
    let mut letters = String::new();
    let mut left = column + 1;
    while left > 0 {
        let digit = (left - 1) % 26;
        letters.insert(0, (b'A' + digit as u8) as char);
        left = (left - 1) / 26;
    }
    format!("{letters}{row}")
}

/// A formula short enough to read in a preview line.
fn shorten(formula: &str) -> String {
    if formula.chars().count() <= FORMULA_CHARS {
        return formula.to_string();
    }
    let kept: String = formula.chars().take(FORMULA_CHARS).collect();
    format!("{kept}…")
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
