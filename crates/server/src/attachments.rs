//! Build OpenAI content-parts from a user message + attachments (images, PDFs, text files).
//!
//! Mirrors `coworker/attachments.py`. Messages pass content either as a string
//! or an array of parts: `{"type": "text", ...}`, `{"type": "image_url",
//! "image_url": {"url": ...}}` (data: URLs work, and vision models read them),
//! and `{"type": "file", "file": {"filename", "file_data"}}` for PDFs. So
//! image/PDF attachments are just parts appended to the user turn — the
//! Anthropic/Gemini providers convert them to their own block shapes.
//!
//! `build_user_content` returns a plain string when there are no attachments
//! (back-compat with the text-only path), else the parts list.

use serde_json::{json, Value};

pub const MAX_ATTACHMENTS: usize = 8;
/// Data-URL length cap (~8–9 MB decoded); keeps a turn sane.
pub const MAX_IMAGE_CHARS: usize = 12_000_000;
/// Data-URL length cap (~10 MB decoded, the GUI's pick limit).
pub const MAX_PDF_CHARS: usize = 15_000_000;
/// Per text file, inlined.
pub const MAX_TEXT_CHARS: usize = 200_000;

fn is_data_image(url: &str) -> bool {
    url.starts_with("data:image/") && url.contains(";base64,")
}

fn is_data_pdf(url: &str) -> bool {
    url.starts_with("data:application/pdf;base64,")
}

/// Return a `String` (no attachments) or an array of OpenAI content-parts
/// (with attachments). Each attachment is `{"kind": "image"|"pdf"|"text",
/// "name"?, "data_url"? (image/pdf), "text"? (text)}`. Invalid/oversized
/// attachments are skipped rather than failing the turn — mirror of
/// `attachments.py::build_user_content`.
pub fn build_user_content(text: Option<&str>, attachments: Option<&[Value]>) -> Value {
    let text = text.unwrap_or("").trim().to_string();
    let Some(attachments) = attachments else {
        return json!(text);
    };
    if attachments.is_empty() {
        return json!(text);
    }

    let mut parts: Vec<Value> = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }

    let mut added = 0usize; // attachment parts that actually made it in
    for a in attachments.iter().take(MAX_ATTACHMENTS) {
        let Some(obj) = a.as_object() else {
            continue;
        };
        let kind = obj.get("kind").and_then(|v| v.as_str());
        match kind {
            Some("image") => {
                let url = obj.get("data_url").and_then(|v| v.as_str()).unwrap_or("");
                if is_data_image(url) && url.len() <= MAX_IMAGE_CHARS {
                    parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    added += 1;
                }
            }
            Some("pdf") => {
                let url = obj.get("data_url").and_then(|v| v.as_str()).unwrap_or("");
                if is_data_pdf(url) && url.len() <= MAX_PDF_CHARS {
                    let name = obj
                        .get("name")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("attachment.pdf");
                    parts.push(json!({
                        "type": "file",
                        "file": {"filename": name, "file_data": url}
                    }));
                    added += 1;
                }
            }
            Some("text") => {
                let body: String = obj
                    .get("text")
                    .map(|v| v.as_str().unwrap_or("").to_string())
                    .unwrap_or_default()
                    .chars()
                    .take(MAX_TEXT_CHARS)
                    .collect();
                let name = obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("attachment");
                if !body.is_empty() {
                    parts.push(json!({
                        "type": "text",
                        "text": format!("[Attached file: {name}]\n{body}")
                    }));
                    added += 1;
                }
            }
            _ => {}
        }
    }

    if added == 0 {
        // Every attachment was invalid/empty → just the text (possibly "").
        return json!(text);
    }
    json!(parts)
}

/// Flatten message content (string or parts) to text — for titles, previews,
/// search. Images render as `image_placeholder` (pass "" to drop them, e.g.
/// for clean titles) — mirror of `attachments.py::content_to_text`.
pub fn content_to_text(content: &Value, image_placeholder: &str) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    let mut out: Vec<String> = Vec::new();
    for part in parts {
        let Some(obj) = part.as_object() else {
            continue;
        };
        match obj.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                    out.push(t.to_string());
                }
            }
            Some("image_url") => {
                if !image_placeholder.is_empty() {
                    out.push(image_placeholder.to_string());
                }
            }
            Some("file") => {
                if !image_placeholder.is_empty() {
                    out.push("[pdf]".to_string());
                }
            }
            _ => {}
        }
    }
    out.join(" ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_attachments_returns_text_string() {
        assert_eq!(build_user_content(Some("hello"), None), json!("hello"));
        assert_eq!(build_user_content(Some("  hi  "), None), json!("hi"));
        assert_eq!(build_user_content(Some(""), Some(&[])), json!(""));
    }

    #[test]
    fn image_parts_skipped_when_invalid() {
        let atts = json!([
            {"kind": "image", "data_url": "https://example.com/x.png"}, // not a data URL
            {"kind": "image", "data_url": "data:image/png;base64,AAAA"}, // ok
            {"kind": "image", "data_url": "data:image/png;base64,BB"}, // ok
        ]);
        let atts = atts.as_array().unwrap();
        let out = build_user_content(Some("see"), Some(atts));
        let parts = out.as_array().expect("parts list");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(parts[2]["image_url"]["url"], "data:image/png;base64,BB");
    }

    #[test]
    fn all_invalid_falls_back_to_text() {
        let atts = json!([
            {"kind": "image", "data_url": "data:image/png;base64,AAAA"},
            {"kind": "pdf", "data_url": "https://x/f.pdf"},
        ]);
        let atts = atts.as_array().unwrap();
        let out = build_user_content(Some("only text"), Some(atts));
        // The image part is valid here, so expect parts; adjust: make both invalid.
        let _ = out;
        let bad = json!([
            {"kind": "image", "data_url": "https://x/i.png"},
            {"kind": "pdf", "data_url": "not-a-data-url"},
        ]);
        let bad = bad.as_array().unwrap();
        assert_eq!(build_user_content(Some("only text"), Some(bad)), json!("only text"));
    }

    #[test]
    fn pdf_part_shape() {
        let atts = json!([
            {"kind": "pdf", "data_url": "data:application/pdf;base64,JVBERi0", "name": "spec.pdf"}
        ]);
        let atts = atts.as_array().unwrap();
        let out = build_user_content(None, Some(atts));
        let parts = out.as_array().unwrap();
        assert_eq!(parts[0]["type"], "file");
        assert_eq!(parts[0]["file"]["filename"], "spec.pdf");
        assert_eq!(parts[0]["file"]["file_data"], "data:application/pdf;base64,JVBERi0");
        // No text part when text is empty.
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn pdf_default_filename() {
        let atts = json!([{"kind": "pdf", "data_url": "data:application/pdf;base64,xx"}]);
        let atts = atts.as_array().unwrap();
        let out = build_user_content(Some(""), Some(atts));
        assert_eq!(out[0]["file"]["filename"], "attachment.pdf");
    }

    #[test]
    fn text_attachment_inlined_with_cap() {
        let body = "x".repeat(200_005);
        let atts = json!([{"kind": "text", "name": "notes.txt", "text": body}]);
        let atts = atts.as_array().unwrap();
        let out = build_user_content(None, Some(atts));
        let part = &out[0];
        assert_eq!(part["type"], "text");
        let text = part["text"].as_str().unwrap();
        assert!(text.starts_with("[Attached file: notes.txt]\n"));
        assert!(text.len() <= 200_000 + "[Attached file: notes.txt]\n".len());
        // Default name fallback.
        let atts2 = json!([{"kind": "text", "text": "hello"}]);
        let atts2 = atts2.as_array().unwrap();
        let out2 = build_user_content(None, Some(atts2));
        assert_eq!(out2[0]["text"], "[Attached file: attachment]\nhello");
    }

    #[test]
    fn max_attachments_capped() {
        let mut atts = Vec::new();
        for i in 0..12 {
            atts.push(json!({"kind": "text", "text": format!("file {i}")}));
        }
        let out = build_user_content(None, Some(&atts));
        let parts = out.as_array().unwrap();
        // 8 text parts (no leading text part).
        assert_eq!(parts.len(), MAX_ATTACHMENTS);
    }

    #[test]
    fn content_to_text_flattens_parts() {
        let content = json!([
            {"type": "text", "text": "hello"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,x"}},
            {"type": "file", "file": {"filename": "a.pdf", "file_data": "data:..."}},
            {"type": "text", "text": "world"},
        ]);
        assert_eq!(content_to_text(&content, "[image]"), "hello [image] [pdf] world");
        // Drop images for clean titles.
        assert_eq!(content_to_text(&content, ""), "hello world");
        // String content passes through.
        assert_eq!(content_to_text(&json!("just text"), "[image]"), "just text");
        // Non-string/non-list → empty.
        assert_eq!(content_to_text(&json!(42), "[image]"), "");
    }
}
