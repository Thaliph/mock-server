//! Read-only YAML rendering of a JSON value (spec §7.5, explorer shell): deterministic, every
//! string double-quoted in JSON syntax so that no recorded text can change the document's shape.

use serde_json::{Map, Value};

/// The YAML text of `value`, ending with a newline.
pub(crate) fn render(value: &Value) -> String {
    let mut out = String::new();
    match value {
        Value::Object(map) if !map.is_empty() => write_map(&mut out, map, 0, false),
        Value::Array(items) if !items.is_empty() => write_list(&mut out, items, 0),
        _ => {
            out.push_str(&scalar(value));
            out.push('\n');
        }
    }
    out
}

/// A value written on one line: a scalar, or an empty container.
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(text) => quoted(text),
        Value::Array(_) => "[]".to_string(),
        Value::Object(_) => "{}".to_string(),
    }
}

/// JSON syntax is valid YAML: escapes keep a newline or a quote inside its string. YAML 1.1 also
/// reads U+0085, U+2028 and U+2029 as line breaks, which JSON leaves raw: they are escaped too.
fn quoted(text: &str) -> String {
    let json = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    json.replace('\u{85}', "\\u0085")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn key(name: &str) -> String {
    let mut chars = name.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'))
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n"
        );
    if plain {
        name.to_string()
    } else {
        quoted(name)
    }
}

fn nested(value: &Value) -> bool {
    match value {
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => false,
    }
}

/// The entries of `map` with their keys at column `indent`; with `dash` the first one follows
/// a `- ` that starts at `indent - 2`.
fn write_map(out: &mut String, map: &Map<String, Value>, indent: usize, dash: bool) {
    for (i, (name, value)) in map.iter().enumerate() {
        if dash && i == 0 {
            out.push_str(&" ".repeat(indent - 2));
            out.push_str("- ");
        } else {
            out.push_str(&" ".repeat(indent));
        }
        out.push_str(&key(name));
        out.push(':');
        match value {
            Value::Object(inner) if nested(value) => {
                out.push('\n');
                write_map(out, inner, indent + 2, false);
            }
            Value::Array(items) if nested(value) => {
                out.push('\n');
                write_list(out, items, indent + 2);
            }
            _ => {
                out.push(' ');
                out.push_str(&scalar(value));
                out.push('\n');
            }
        }
    }
}

fn write_list(out: &mut String, items: &[Value], indent: usize) {
    for item in items {
        match item {
            Value::Object(map) if nested(item) => write_map(out, map, indent + 2, true),
            Value::Array(inner) if nested(item) => {
                out.push_str(&" ".repeat(indent));
                out.push_str("-\n");
                write_list(out, inner, indent + 2);
            }
            _ => {
                out.push_str(&" ".repeat(indent));
                out.push_str("- ");
                out.push_str(&scalar(item));
                out.push('\n');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::render;

    #[test]
    fn exp_01_maps_lists_and_scalars_render_in_byte_order() {
        let value = json!({
            "b": [1, "two", null, true, 1.5],
            "a": {"y": {}, "x": [], "z": {"k": "v"}},
            "list": [{"n": 1, "m": 2}, ["x", "y"]],
        });
        let expected = "\
a:
  x: []
  \"y\": {}
  z:
    k: \"v\"
b:
  - 1
  - \"two\"
  - null
  - true
  - 1.5
list:
  - m: 2
    \"n\": 1
  -
    - \"x\"
    - \"y\"
";
        assert_eq!(render(&value), expected);
        assert_eq!(render(&value), render(&value.clone()));
    }

    #[test]
    fn exp_01_strings_are_quoted_so_that_they_stay_strings() {
        let hostile = "<img src=x onerror=alert(1)>\nnext: line";
        let out = render(&json!({"s": hostile, "t": "true", "n": "12", "z": "null", "e": ""}));
        assert_eq!(
            out,
            "e: \"\"\n\"n\": \"12\"\ns: \"<img src=x onerror=alert(1)>\\nnext: line\"\nt: \"true\"\nz: \"null\"\n"
        );
        assert_eq!(
            out.lines().count(),
            5,
            "a newline in a string never starts a new line"
        );
    }

    #[test]
    fn exp_01_keys_are_quoted_unless_plain() {
        let out = render(&json!({
            "app.kubernetes.io/name": 1,
            "true": 2,
            "null": 3,
            "has space": 4,
            "a: b": 5,
            "": 6,
            "1x": 7,
            "-x": 8,
        }));
        assert_eq!(
            out,
            "\"\": 6\n\"-x\": 8\n\"1x\": 7\n\"a: b\": 5\napp.kubernetes.io/name: 1\n\"has space\": 4\n\"null\": 3\n\"true\": 2\n"
        );
    }

    #[test]
    fn exp_01_an_empty_value_is_one_line() {
        assert_eq!(render(&json!({})), "{}\n");
        assert_eq!(render(&json!("x")), "\"x\"\n");
    }

    #[test]
    fn exp_01_yaml_1_1_line_breaks_are_escaped_in_strings_and_keys() {
        let out = render(&json!({"s": "a\u{85}b\u{2028}c\u{2029}d", "k\u{2028}": 1}));
        assert_eq!(out, "\"k\\u2028\": 1\ns: \"a\\u0085b\\u2028c\\u2029d\"\n");
        assert!(!out.contains(['\u{85}', '\u{2028}', '\u{2029}']));
        assert_eq!(out.lines().count(), 2);
    }
}
