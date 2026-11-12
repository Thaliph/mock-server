//! Read-only YAML rendering of a JSON value (spec §7.5, explorer shell): deterministic, every
//! string double-quoted in JSON syntax so that no recorded text can change the document's shape.

use serde_json::Value;

/// The YAML text of `value`, ending with a newline.
pub(super) fn render(_value: &Value) -> String {
    todo!()
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
  y: {}
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
    n: 1
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
            "e: \"\"\nn: \"12\"\ns: \"<img src=x onerror=alert(1)>\\nnext: line\"\nt: \"true\"\nz: \"null\"\n"
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
}
