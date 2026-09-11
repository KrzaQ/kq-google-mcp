//! Sheets: the tabs of a spreadsheet, a range read, two range writes and the
//! one structural change this release makes, adding a tab.
//!
//! Creating a whole spreadsheet is not here on purpose: a new Sheet is made
//! through Drive, by uploading CSV and letting Drive convert it, which fills
//! it in the same call. See [`super::drive::create_sheet_from_csv`].

use serde::{Deserialize, Serialize};

use super::client::{Client, Result, urlencode};

/// Sheets is served from its own host, never from `www.googleapis.com`.
const SHEETS: &str = "sheets";

/// How values are written. `USER_ENTERED` is what a person typing into the
/// grid would get: `2026-09-08` becomes a date and `=SUM(A1:A2)` a formula,
/// which is what someone asking a model to add rows means.
const USER_ENTERED: &str = "USER_ENTERED";

/// How a cell comes back from a read. `Formatted` is what the sheet shows and
/// what a person reading the answer would see, so it stays the default.
///
/// `Formula` is the one that prevents a quiet loss: a model that reads a
/// column as display strings and writes them back replaces `=SUM(C2:C10)`
/// with a fixed number, and the sheet keeps looking right while being wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Render {
    #[default]
    Formatted,
    Formula,
    Unformatted,
}

impl Render {
    /// Google's own word for this mode, as `valueRenderOption` takes it.
    pub fn as_option(self) -> &'static str {
        match self {
            Render::Formatted => "FORMATTED_VALUE",
            Render::Formula => "FORMULA",
            Render::Unformatted => "UNFORMATTED_VALUE",
        }
    }
}

/// What a read answered: the range Google resolved the request to, and the
/// rows. The range is not decoration — it names the top left cell, which is
/// how a caller turns a row and column index back into an A1 reference.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RangeValues {
    pub range: String,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Spreadsheet {
    pub spreadsheet_id: String,
    pub title: String,
    pub url: String,
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tab {
    pub sheet_id: i64,
    pub title: String,
    pub index: i64,
    pub rows: i64,
    pub columns: i64,
}

/// What a write answered, for the tool to report back.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WriteResult {
    pub updated_range: String,
    pub updated_rows: i64,
    pub updated_cells: i64,
}

/// `spreadsheets.get` without any cell data: tabs and their dimensions, which
/// is what a model needs before it reads or writes a range.
pub async fn get(client: &Client, connection_id: i64, spreadsheet_id: &str) -> Result<Spreadsheet> {
    let request = client
        .service(SHEETS)
        .get(&format!("v4/spreadsheets/{}", urlencode(spreadsheet_id)))?
        .query(&[
            ("includeGridData", "false"),
            (
                "fields",
                "spreadsheetId,spreadsheetUrl,properties(title),sheets(properties)",
            ),
        ]);
    let wire: WireSpreadsheet = client.json(connection_id, request).await?;
    Ok(wire.into())
}

/// `spreadsheets.values.get`, in A1 notation. [`Render`] decides what a cell
/// is: what the sheet shows, the formula behind it, or the raw value.
///
/// Every mode answers with rows of strings. `UNFORMATTED_VALUE` sends numbers
/// and booleans as JSON scalars rather than strings, so the cells are read as
/// [`serde_json::Value`] and rendered here; a caller's shape does not change
/// with the mode it asked for.
pub async fn values_get(
    client: &Client,
    connection_id: i64,
    spreadsheet_id: &str,
    range: &str,
    render: Render,
    max_rows: Option<usize>,
) -> Result<RangeValues> {
    let request = client
        .service(SHEETS)
        .get(&format!(
            "v4/spreadsheets/{}/values/{}",
            urlencode(spreadsheet_id),
            urlencode(range)
        ))?
        .query(&[
            ("valueRenderOption", render.as_option()),
            ("majorDimension", "ROWS"),
        ]);
    let wire: WireValueRange = client.json(connection_id, request).await?;
    let mut rows: Vec<Vec<String>> = wire
        .values
        .into_iter()
        .map(|row| row.into_iter().map(cell).collect())
        .collect();
    if let Some(max) = max_rows {
        rows.truncate(max);
    }
    Ok(RangeValues {
        range: wire.range,
        rows,
    })
}

/// One cell as a string. A boolean is spelled the way the grid spells it, an
/// empty cell is an empty string, and a number keeps the digits Google sent.
fn cell(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s,
        serde_json::Value::Bool(true) => "TRUE".to_string(),
        serde_json::Value::Bool(false) => "FALSE".to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `spreadsheets.values.append`, which adds rows after the last used one.
pub async fn values_append(
    client: &Client,
    connection_id: i64,
    spreadsheet_id: &str,
    range: &str,
    rows: Vec<Vec<String>>,
) -> Result<WriteResult> {
    let request = client
        .service(SHEETS)
        .post(&format!(
            "v4/spreadsheets/{}/values/{}:append",
            urlencode(spreadsheet_id),
            urlencode(range)
        ))?
        .query(&[
            ("valueInputOption", USER_ENTERED),
            ("insertDataOption", "INSERT_ROWS"),
        ])
        .json(&ValueRange {
            range: range.to_string(),
            major_dimension: "ROWS",
            values: rows,
        });
    let wire: WireAppendReply = client.json(connection_id, request).await?;
    Ok(wire.updates.into())
}

/// `spreadsheets.values.update`, which writes over exactly the range given.
pub async fn values_update(
    client: &Client,
    connection_id: i64,
    spreadsheet_id: &str,
    range: &str,
    rows: Vec<Vec<String>>,
) -> Result<WriteResult> {
    let request = client
        .service(SHEETS)
        .put(&format!(
            "v4/spreadsheets/{}/values/{}",
            urlencode(spreadsheet_id),
            urlencode(range)
        ))?
        .query(&[("valueInputOption", USER_ENTERED)])
        .json(&ValueRange {
            range: range.to_string(),
            major_dimension: "ROWS",
            values: rows,
        });
    let wire: WireUpdateReply = client.json(connection_id, request).await?;
    Ok(wire.into())
}

/// `spreadsheets.batchUpdate` with one `addSheet`. The only structural change
/// this release makes; nothing here deletes or reorders a tab.
pub async fn add_tab(
    client: &Client,
    connection_id: i64,
    spreadsheet_id: &str,
    title: &str,
) -> Result<Tab> {
    let request = client
        .service(SHEETS)
        .post(&format!(
            "v4/spreadsheets/{}:batchUpdate",
            urlencode(spreadsheet_id)
        ))?
        .json(&BatchUpdate {
            requests: vec![SheetRequest {
                add_sheet: AddSheet {
                    properties: NewSheetProperties {
                        title: title.to_string(),
                    },
                },
            }],
        });
    let wire: WireBatchReply = client.json(connection_id, request).await?;
    let properties = wire
        .replies
        .into_iter()
        .find_map(|r| r.add_sheet.map(|a| a.properties))
        .unwrap_or_default();
    Ok(properties.into())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireSpreadsheet {
    spreadsheet_id: String,
    spreadsheet_url: String,
    properties: WireSpreadsheetProperties,
    sheets: Vec<WireSheet>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireSpreadsheetProperties {
    title: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireSheet {
    properties: WireSheetProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireSheetProperties {
    sheet_id: i64,
    title: String,
    index: i64,
    grid_properties: WireGridProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireGridProperties {
    row_count: i64,
    column_count: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireValueRange {
    /// The range Google resolved the request to, such as `September!A2:D5`.
    range: String,
    /// Cells arrive as JSON scalars under `UNFORMATTED_VALUE`, so they are
    /// taken as they come and rendered by [`cell`].
    values: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireAppendReply {
    updates: WireUpdateReply,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireUpdateReply {
    updated_range: String,
    updated_rows: i64,
    updated_cells: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireBatchReply {
    replies: Vec<WireReply>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireReply {
    add_sheet: Option<WireAddSheetReply>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireAddSheetReply {
    properties: WireSheetProperties,
}

impl From<WireSpreadsheet> for Spreadsheet {
    fn from(w: WireSpreadsheet) -> Self {
        Spreadsheet {
            spreadsheet_id: w.spreadsheet_id,
            title: w.properties.title,
            url: w.spreadsheet_url,
            tabs: w.sheets.into_iter().map(|s| s.properties.into()).collect(),
        }
    }
}

impl From<WireSheetProperties> for Tab {
    fn from(p: WireSheetProperties) -> Self {
        Tab {
            sheet_id: p.sheet_id,
            title: p.title,
            index: p.index,
            rows: p.grid_properties.row_count,
            columns: p.grid_properties.column_count,
        }
    }
}

impl From<WireUpdateReply> for WriteResult {
    fn from(w: WireUpdateReply) -> Self {
        WriteResult {
            updated_range: w.updated_range,
            updated_rows: w.updated_rows,
            updated_cells: w.updated_cells,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValueRange {
    range: String,
    major_dimension: &'static str,
    values: Vec<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct BatchUpdate {
    requests: Vec<SheetRequest>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SheetRequest {
    add_sheet: AddSheet,
}

#[derive(Debug, Serialize)]
struct AddSheet {
    properties: NewSheetProperties,
}

#[derive(Debug, Serialize)]
struct NewSheetProperties {
    title: String,
}
