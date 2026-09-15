//! What a tool call looks like in the log. The arguments are worth keeping —
//! they answer "why did the model draft that" — but the bodies are not: they
//! are the mail itself, and the log is the portal's main page. So every field
//! that carries content is replaced by its size, and the whole thing is cut
//! at 4 KB. Tool output is never logged at all.

use serde_json::Value;

use super::limits::AUDIT_ARGS_MAX_BYTES;

/// Fields whose value is content rather than a parameter, at any depth.
///
/// A field belongs here when it carries what the person wrote rather than
/// how the tool should behave. `code` is a listing, `cells` are the words in
/// a table row, and `replace` is the new wording of a sentence; all three
/// were being written to the log in full, which the first paragraph of this
/// file says they should not be.
const CONTENT_FIELDS: [&str; 8] = [
    "body", "text", "html", "rows", "markdown", "code", "cells", "replace",
];
/// What a cut log line ends with.
const CUT: char = '…';

/// The tool's JSON arguments with every content field summarised and the
/// result cut to [`AUDIT_ARGS_MAX_BYTES`].
pub fn strip_args(args: Value) -> String {
    let mut out = serde_json::to_string(&strip(args)).expect("a Value always serialises");
    if out.len() > AUDIT_ARGS_MAX_BYTES {
        let mut cut = AUDIT_ARGS_MAX_BYTES - CUT.len_utf8();
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push(CUT);
    }
    out
}

fn strip(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(name, value)| {
                    let value = if CONTENT_FIELDS.contains(&name.as_str()) {
                        summarise(value)
                    } else {
                        strip(value)
                    };
                    (name, value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(strip).collect()),
        other => other,
    }
}

/// The size of a content field, in whatever unit it is measured in: rows for
/// a list of rows, characters for everything else. An absent field stays
/// absent rather than becoming "<0 chars>".
fn summarise(value: Value) -> Value {
    match value {
        Value::Null => Value::Null,
        Value::Array(items) => Value::String(format!("<{} rows>", items.len())),
        Value::String(s) => Value::String(format!("<{} chars>", s.chars().count())),
        other => Value::String(format!("<{} chars>", other.to_string().chars().count())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn content_fields_become_their_size_at_any_depth() {
        let args = json!({
            "account": "work",
            "to": ["someone@example.com"],
            "subject": "Re: the thing",
            "body": "Dear friend,\nhere is the thing.",
            "html": "<p>Dear friend</p>",
            "draft": { "body": "nested", "note": { "text": "deeper" } },
            "updates": [{ "markdown": "# heading" }, { "rows": [[1, 2], [3, 4], [5, 6]] }]
        });
        let stripped: Value = serde_json::from_str(&strip_args(args)).unwrap();
        assert_eq!(stripped["account"], json!("work"));
        assert_eq!(stripped["subject"], json!("Re: the thing"));
        assert_eq!(stripped["to"], json!(["someone@example.com"]));
        assert_eq!(stripped["body"], json!("<31 chars>"));
        assert_eq!(stripped["html"], json!("<18 chars>"));
        assert_eq!(stripped["draft"]["body"], json!("<6 chars>"));
        assert_eq!(stripped["draft"]["note"]["text"], json!("<6 chars>"));
        assert_eq!(stripped["updates"][0]["markdown"], json!("<9 chars>"));
        assert_eq!(stripped["updates"][1]["rows"], json!("<3 rows>"));

        // A listing, a table row and a replacement are content too: they are
        // what the person wrote, not how the tool should behave. All three
        // used to reach the log in full.
        let docs = strip_args(json!({
            "doc_id": "1AbC",
            "after_paragraph": 12,
            "code": "fn main() {\n    println!(\"hi\");\n}",
            "cells": ["Mistral-7B", "7 mld"],
            "replace": "a whole rewritten sentence",
            "font": "Courier New",
        }));
        let docs: Value = serde_json::from_str(&docs).unwrap();
        assert_eq!(docs["doc_id"], json!("1AbC"));
        assert_eq!(docs["after_paragraph"], json!(12));
        assert_eq!(docs["font"], json!("Courier New"), "a font is a parameter");
        assert_eq!(docs["code"], json!("<33 chars>"));
        assert_eq!(docs["cells"], json!("<2 rows>"));
        assert_eq!(docs["replace"], json!("<26 chars>"));
    }

    #[test]
    fn sizes_are_counted_in_characters_not_bytes() {
        let stripped = strip_args(json!({ "body": "zażółć gęślą jaźń" }));
        assert_eq!(stripped, r#"{"body":"<17 chars>"}"#);
    }

    #[test]
    fn a_content_field_that_is_not_a_string_is_still_replaced() {
        let stripped: Value =
            serde_json::from_str(&strip_args(json!({ "body": null, "rows": 5, "text": {} })))
                .unwrap();
        assert_eq!(stripped["body"], Value::Null);
        assert_eq!(stripped["rows"], json!("<1 chars>"));
        assert_eq!(stripped["text"], json!("<2 chars>"));
    }

    #[test]
    fn the_whole_thing_is_cut_at_four_kilobytes() {
        // A parameter, not content: nothing summarises it, so the cap does.
        let long = strip_args(json!({ "query": "x".repeat(10_000) }));
        assert_eq!(long.len(), AUDIT_ARGS_MAX_BYTES);
        assert!(long.ends_with(CUT));
        assert!(long.starts_with(r#"{"query":"xxx"#));

        // The cut never lands inside a character.
        let wide = strip_args(json!({ "query": "ł".repeat(10_000) }));
        assert!(wide.len() <= AUDIT_ARGS_MAX_BYTES);
        assert!(wide.ends_with(CUT));
        assert!(std::str::from_utf8(wide.as_bytes()).is_ok());

        // Anything that fits is left exactly as it is.
        let short = strip_args(json!({ "account": "work" }));
        assert_eq!(short, r#"{"account":"work"}"#);
    }
}
