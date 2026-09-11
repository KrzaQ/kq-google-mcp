//! Sheets: tabs, ranges and the one structural change.

use super::*;
use crate::google::sheets;

#[tokio::test]
async fn a_spreadsheet_reports_its_tabs_and_dimensions() {
    let h = harness().await;
    h.mount_json(
        "GET",
        &format!("/v4/spreadsheets/{SHEET}"),
        fixture("sheets_spreadsheet.json"),
    )
    .await;
    let spreadsheet = sheets::get(&h.client, CONNECTION, SHEET).await.unwrap();
    assert_eq!(spreadsheet.title, "Support hours 2026");
    assert_eq!(spreadsheet.tabs.len(), 2);
    assert_eq!(spreadsheet.tabs[0].title, "September");
    assert_eq!(spreadsheet.tabs[0].rows, 200);
    assert_eq!(spreadsheet.tabs[1].sheet_id, 1298374);
    assert!(spreadsheet.url.contains(SHEET));
}

#[tokio::test]
async fn a_range_is_read_written_and_appended() {
    let h = harness().await;
    h.mount_json(
        "GET",
        &format!("/v4/spreadsheets/{SHEET}/values/September%21A1%3AD4"),
        fixture("sheets_values.json"),
    )
    .await;
    let read = sheets::values_get(
        &h.client,
        CONNECTION,
        SHEET,
        "September!A1:D4",
        sheets::Render::Formatted,
        None,
    )
    .await
    .unwrap();
    assert_eq!(read.rows.len(), 4);
    assert_eq!(read.range, "September!A1:D4");
    assert_eq!(
        read.rows[1],
        ["2026-09-01", "Phoenix", "1.5", "Restarted the exporter"]
    );
    let capped = sheets::values_get(
        &h.client,
        CONNECTION,
        SHEET,
        "September!A1:D4",
        sheets::Render::Formatted,
        Some(2),
    )
    .await
    .unwrap();
    assert_eq!(capped.rows.len(), 2);

    h.mount_json(
        "POST",
        &format!("/v4/spreadsheets/{SHEET}/values/September%21A%3AD:append"),
        fixture("sheets_append.json"),
    )
    .await;
    let appended = sheets::values_append(
        &h.client,
        CONNECTION,
        SHEET,
        "September!A:D",
        vec![vec![
            "2026-09-08".into(),
            "Aurora".into(),
            "1".into(),
            "Called back".into(),
        ]],
    )
    .await
    .unwrap();
    assert_eq!(appended.updated_range, "September!A5:D5");
    assert_eq!(appended.updated_cells, 4);
    let request = h
        .last(
            "POST",
            &format!("/v4/spreadsheets/{SHEET}/values/September%21A%3AD:append"),
        )
        .await;
    let query: std::collections::HashMap<_, _> = request.url.query_pairs().into_owned().collect();
    assert_eq!(query["valueInputOption"], "USER_ENTERED");
    assert_eq!(query["insertDataOption"], "INSERT_ROWS");

    h.mount_json(
        "PUT",
        &format!("/v4/spreadsheets/{SHEET}/values/September%21A2%3AD2"),
        fixture("sheets_update.json"),
    )
    .await;
    let updated = sheets::values_update(
        &h.client,
        CONNECTION,
        SHEET,
        "September!A2:D2",
        vec![vec![
            "2026-09-01".into(),
            "Phoenix".into(),
            "2".into(),
            "Fixed".into(),
        ]],
    )
    .await
    .unwrap();
    assert_eq!(updated.updated_range, "September!A2:D2");
    assert_eq!(updated.updated_rows, 1);
}

#[tokio::test]
async fn a_tab_is_added() {
    let h = harness().await;
    h.mount_json(
        "POST",
        &format!("/v4/spreadsheets/{SHEET}:batchUpdate"),
        fixture("sheets_add_sheet.json"),
    )
    .await;
    let tab = sheets::add_tab(&h.client, CONNECTION, SHEET, "October")
        .await
        .unwrap();
    assert_eq!(tab.title, "October");
    assert_eq!(tab.sheet_id, 774411);
    assert_eq!(tab.index, 2);
    let body = h
        .last_body("POST", &format!("/v4/spreadsheets/{SHEET}:batchUpdate"))
        .await;
    assert_eq!(
        body,
        json!({"requests": [{"addSheet": {"properties": {"title": "October"}}}]})
    );
}

#[tokio::test]
async fn each_render_mode_asks_google_for_it_and_answers_strings() {
    let h = harness().await;
    let at = format!("/v4/spreadsheets/{SHEET}/values/September%21A1%3AD2");
    h.mount_json("GET", &at, fixture("sheets_values_unformatted.json"))
        .await;

    for (render, option) in [
        (sheets::Render::Formatted, "FORMATTED_VALUE"),
        (sheets::Render::Formula, "FORMULA"),
        (sheets::Render::Unformatted, "UNFORMATTED_VALUE"),
    ] {
        let read = sheets::values_get(
            &h.client,
            CONNECTION,
            SHEET,
            "September!A1:D2",
            render,
            None,
        )
        .await
        .unwrap();
        assert_eq!(read.rows.len(), 2);
        let query: std::collections::HashMap<_, _> = h
            .last("GET", &at)
            .await
            .url
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(query["valueRenderOption"], option);
        assert_eq!(query["majorDimension"], "ROWS");
    }

    // UNFORMATTED_VALUE sends numbers and booleans as JSON scalars. Every
    // mode still answers rows of strings, so a caller's shape does not change
    // with the mode it asked for.
    let read = sheets::values_get(
        &h.client,
        CONNECTION,
        SHEET,
        "September!A1:D2",
        sheets::Render::Unformatted,
        None,
    )
    .await
    .unwrap();
    assert_eq!(read.rows[1], ["46266", "Phoenix", "1.5", "TRUE"]);
}
