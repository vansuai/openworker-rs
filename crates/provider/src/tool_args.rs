//! Tool-call argument parsing and salvage for malformed provider output.
//!
//! Some OpenAI-compatible and Anthropic-compatible backends (notably MiniMax) emit
//! tool inputs as partial JSON, XML `<parameter=` blocks, or flat `key=value` text.
//! This module normalizes those shapes into JSON objects the engine can execute.

use serde_json::{json, Map, Value};

/// Parse accumulated streaming JSON or a string `input` field into tool arguments.
pub fn parse_tool_arguments(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Value::Null;
    }
    if let Ok(v) = serde_json::from_str(trimmed) {
        return normalize_tool_input(v);
    }
    salvage_tool_args_from_text(trimmed)
        .map(Value::Object)
        .unwrap_or_else(|| json!({ "_raw": raw }))
}

/// Normalize a parsed `tool_use.input` value (object, string, or null).
pub fn normalize_tool_input(input: Value) -> Value {
    match input {
        Value::Object(mut map) => {
            merge_salvaged_raw(&mut map);
            let mut value = Value::Object(map);
            unwrap_repeated_elements(&mut value);
            value
        }
        Value::String(s) => parse_tool_arguments(&s),
        Value::Null => Value::Null,
        other => json!({ "_raw": other.to_string() }),
    }
}

/// MiniMax renders array parameters as XML repeated elements, which decode back as
/// `{"param": {"item": [...]}}`. Replaying that shape to the vendor adds another
/// wrapper level per turn (histories have been seen six deep), so collapse it here —
/// before the arguments are executed *and* before they are stored for the next turn.
fn unwrap_repeated_elements(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for child in map.values_mut() {
                unwrap_child(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                unwrap_child(child);
            }
        }
        _ => {}
    }
}

/// Collapse wrappers at one argument position. The arguments object itself is never
/// collapsed — `{"todos": [...]}` has to stay an object for the engine and the GUI.
fn unwrap_child(value: &mut Value) {
    for _ in 0..8 {
        if !unwrap_one_wrapper(value) {
            break;
        }
    }
    unwrap_repeated_elements(value);
}

/// Replace a bare `{"item": …}` / `{"items": …}` wrapper with its payload. Objects that
/// also carry sibling fields are real payloads (a todo item, a diff hunk) and are left
/// alone, as are wrappers whose payload is a scalar — `{"item": ""}` is an omitted
/// element, not an array.
fn unwrap_one_wrapper(value: &mut Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    if !map.keys().all(|k| k == "item" || k == "items") {
        return false;
    }
    let Some(child) = map.get("item").or_else(|| map.get("items")) else {
        return false;
    };
    if !child.is_array() && !child.is_object() {
        return false;
    }
    *value = child.clone();
    true
}

/// Best-effort recovery of `{path, content, …}` from non-JSON tool-input text.
pub fn salvage_tool_args_from_text(text: &str) -> Option<Map<String, Value>> {
    if let Some(m) = parse_xml_function_block(text) {
        return Some(m);
    }
    let mut map = parse_xml_parameters(text).unwrap_or_default();
    if let Some(kv) = parse_key_value_args(text) {
        for (k, v) in kv {
            map.entry(k).or_insert(v);
        }
    }
    if map.is_empty() {
        parse_artifact_path_hint(text).map(Some)?
    } else {
        Some(map)
    }
}

fn merge_salvaged_raw(map: &mut Map<String, Value>) {
    let raw = map.get("_raw").and_then(|v| v.as_str()).map(str::to_string);
    if let Some(raw) = raw {
        if let Some(salvaged) = salvage_tool_args_from_text(&raw) {
            for (k, v) in salvaged {
                map.entry(k).or_insert(v);
            }
        }
    }
}

/// `<function=NAME><parameter=KEY>VAL</parameter>…</function>`
fn parse_xml_function_block(text: &str) -> Option<Map<String, Value>> {
    let lower = text.to_lowercase();
    let fn_start = lower.find("<function")?;
    let body_start = text[fn_start..].find('>')? + fn_start + 1;
    let fn_end_lower = lower[body_start..].find("</function")?;
    let body = &text[body_start..body_start + fn_end_lower];
    let params = parse_xml_parameters(body)?;
    if params.is_empty() {
        None
    } else {
        Some(params)
    }
}

/// `<parameter=KEY>VAL</parameter>` and `<parameter name="KEY">VAL</parameter>`.
fn parse_xml_parameters(text: &str) -> Option<Map<String, Value>> {
    let mut map = Map::new();
    let lower = text.to_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("<parameter") {
        let start = search_from + rel;
        let tag_end = text[start..].find('>').map(|i| start + i)?;
        let tag = &text[start..=tag_end];
        let key = xml_param_key(tag)?;
        let content_start = tag_end + 1;
        let close_lower = lower[content_start..].find("</parameter")?;
        let val = text[content_start..content_start + close_lower].trim();
        if !key.is_empty() && !val.is_empty() {
            map.insert(key, json!(val));
        }
        search_from = content_start + close_lower + "</parameter".len();
    }
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

fn xml_param_key(tag: &str) -> Option<String> {
    // <parameter=path> or <parameter = path>
    if let Some(eq) = tag.find('=') {
        let after = tag[eq + 1..].trim();
        if after.starts_with('"') {
            // name="content"
            let rest = &after[1..];
            let end = rest.find('"')?;
            return Some(rest[..end].to_string());
        }
        let end = after.find(['>', ' ']).unwrap_or(after.len());
        let key = after[..end].trim();
        if !key.is_empty() {
            return Some(key.to_string());
        }
    }
    None
}

/// Flat `path=` / `content=` and `parameter name="content"=` patterns.
fn parse_key_value_args(text: &str) -> Option<Map<String, Value>> {
    let mut map = Map::new();

    if let Some(path) = extract_flat_kv(text, "path") {
        map.insert("path".into(), json!(path));
    }
    if let Some(content) = extract_flat_kv(text, "content") {
        map.insert("content".into(), json!(content));
    }
    if !map.contains_key("content") {
        if let Some(content) = extract_parameter_name_equals(text, "content") {
            map.insert("content".into(), json!(content));
        }
    }
    if !map.contains_key("path") {
        if let Some(path) = extract_parameter_name_equals(text, "path") {
            map.insert("path".into(), json!(path));
        }
    }

    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

/// `key=value` at a word boundary; value runs until the next ` word=` token or EOL.
fn extract_flat_kv(text: &str, key: &str) -> Option<String> {
    let pattern = format!("{key}=");
    let lower = text.to_lowercase();
    let key_lower = key.to_lowercase();
    let pattern_lower = format!("{key_lower}=");
    let start = lower.rfind(&pattern_lower)?;
    let val_start = start + pattern.len();
    let rest = text[val_start..].trim_start();
    if rest.is_empty() {
        return None;
    }
    // Stop before another ` word=` assignment (e.g. content before path).
    let mut end = rest.len();
    for (i, _) in rest.match_indices(' ') {
        let tail = rest[i + 1..].trim_start();
        if tail.contains('=') {
            let word = tail.split('=').next().unwrap_or("").trim();
            if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                end = i;
                break;
            }
        }
    }
    let val = rest[..end].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

/// `parameter name="content"=…` (MiniMax-style mangled output).
fn extract_parameter_name_equals(text: &str, key: &str) -> Option<String> {
    let needle = format!("parameter name=\"{key}\"=");
    let lower = text.to_lowercase();
    let needle_lower = needle.to_lowercase();
    let start = lower.find(&needle_lower)?;
    let val_start = start + needle.len();
    let rest = &text[val_start..];
    // Trim at trailing ` path=` / ` content=` if present and we're extracting content.
    let mut end = rest.len();
    for marker in [" path=", " content=", "\npath=", "\ncontent="] {
        if let Some(i) = rest.to_lowercase().find(marker) {
            end = end.min(i);
        }
    }
    let val = rest[..end].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

/// Pull `artifact:relative/path` out of mangled tool text as a path hint.
fn parse_artifact_path_hint(text: &str) -> Option<Map<String, Value>> {
    let lower = text.to_lowercase();
    let idx = lower.find("artifact:")?;
    let rest = &text[idx + "artifact:".len()..];
    let end = rest
        .find([']', '<', ' ', '\n'])
        .unwrap_or(rest.len());
    let path = rest[..end].trim();
    if path.is_empty() {
        return None;
    }
    let mut map = Map::new();
    map.insert("path".into(), json!(path));
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_json_passthrough() {
        let v = parse_tool_arguments(r#"{"path":"a.md","content":"hi"}"#);
        assert_eq!(v["path"], "a.md");
        assert_eq!(v["content"], "hi");
    }

    #[test]
    fn invalid_json_preserves_raw() {
        let v = parse_tool_arguments("not json at all");
        assert_eq!(v["_raw"], "not json at all");
    }

    #[test]
    fn xml_parameters_salvaged() {
        let raw = r#"<parameter=path>briefing.md</parameter><parameter=content># Title

Body</parameter>"#;
        let v = parse_tool_arguments(raw);
        assert_eq!(v["path"], "briefing.md");
        assert_eq!(v["content"], "# Title\n\nBody");
    }

    #[test]
    fn flat_key_value_salvaged() {
        let raw = r#"parameter name="content"=# Morning News

Details here path=briefing.md"#;
        let v = parse_tool_arguments(raw);
        assert_eq!(v["path"], "briefing.md");
        assert!(v["content"].as_str().unwrap().contains("Morning News"));
    }

    #[test]
    fn artifact_path_hint() {
        let raw = "write_file artifact:briefing.md]<]minimax[=[</artifact>";
        let v = parse_tool_arguments(raw);
        assert_eq!(v["path"], "briefing.md");
    }

    #[test]
    fn merge_raw_into_object() {
        let input = json!({ "_raw": "path=notes.md content=hello" });
        let v = normalize_tool_input(input);
        assert_eq!(v["path"], "notes.md");
        assert_eq!(v["content"], "hello");
    }

    /// MiniMax serializes array parameters as XML repeated elements, which arrive as
    /// `{"todos": {"item": [...]}}`. Replaying that shape back into the vendor adds
    /// another wrapper level per turn, so unwrap it on the way in.
    #[test]
    fn single_item_wrapper_unwrapped() {
        let v = parse_tool_arguments(
            r#"{"todos":{"item":[{"content":"a","status":"pending"}]}}"#,
        );
        assert_eq!(v["todos"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn nested_item_wrappers_unwrapped() {
        for depth in 2..=6 {
            let mut value = json!([{"content": "a", "status": "pending"}]);
            for _ in 0..depth {
                value = json!({"item": value});
            }
            let v = normalize_tool_input(json!({"todos": value}));
            assert_eq!(
                v["todos"].as_array().map(|a| a.len()),
                Some(1),
                "depth {depth}"
            );
        }
    }

    #[test]
    fn wrapper_unwrap_leaves_real_objects_alone() {
        // An object that also carries sibling fields is a todo item, not a wrapper.
        let v = parse_tool_arguments(
            r#"{"todos":[{"content":"a","status":"pending","item":"note"}]}"#,
        );
        let item = &v["todos"].as_array().unwrap()[0];
        assert_eq!(item["content"], "a");
        assert_eq!(item["item"], "note");
    }

    #[test]
    fn wrappers_inside_nested_objects_unwrapped() {
        let v = parse_tool_arguments(
            r#"{"plan":{"steps":{"item":[{"title":"a"}]}},"path":"briefing.md"}"#,
        );
        assert_eq!(v["plan"]["steps"].as_array().unwrap().len(), 1);
        assert_eq!(v["path"], "briefing.md");
    }

    /// The arguments object itself must stay an object: collapsing a bare single-key
    /// payload would turn the common `{"todos": […]}` into a top-level array.
    #[test]
    fn arguments_object_is_never_collapsed() {
        for raw in [
            r#"{"todos":[{"content":"a","status":"pending"}]}"#,
            r#"{"items":[{"content":"a","status":"pending"}]}"#,
        ] {
            let v = parse_tool_arguments(raw);
            assert!(v.is_object(), "{raw}");
            let key = if raw.starts_with(r#"{"todos"#) { "todos" } else { "items" };
            assert_eq!(v[key].as_array().unwrap().len(), 1);
        }
    }
}
