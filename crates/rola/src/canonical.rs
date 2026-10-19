//! Canonical JSON: the one byte string a JSON value is signed as.
//!
//! A signature covers bytes, not meaning, so signer and verifier must turn the same value into
//! the same bytes. Here that is: object keys sorted by their UTF-16 code units (what JavaScript's
//! `sort()` does, so `@pam/core` writes the very same bytes), no whitespace, strings escaped as
//! `JSON.stringify` and `serde_json` both escape them, and integers only. Sorting is done here,
//! by hand, so it does not depend on whether some other crate in the build switched on
//! `serde_json/preserve_order`.

use serde_json::Value;

/// `value` as canonical JSON, or `None` when it holds a number that is not an integer (a float
/// has more than one spelling, so it has no place in signed bytes).
pub(crate) fn canonical(value: &Value) -> Option<String> {
    let mut out = String::new();
    write(value, &mut out)?;
    Some(out)
}

fn write(value: &Value, out: &mut String) -> Option<()> {
    match value {
        Value::Number(n) if n.is_f64() => return None,
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).ok()?);
                out.push(':');
                write(map.get(key)?, out)?;
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out)?;
            }
            out.push(']');
        }
        scalar => out.push_str(&serde_json::to_string(scalar).ok()?),
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_sorted_at_every_depth_and_nothing_is_padded() {
        let v = json!({"b": 1, "a": {"z": [true, null, "x"], "y": -2}});
        assert_eq!(
            canonical(&v).unwrap(),
            r#"{"a":{"y":-2,"z":[true,null,"x"]},"b":1}"#
        );
    }

    /// UTF-16 order, as JavaScript sorts: an astral character (a surrogate pair, 0xD8xx) comes
    /// BEFORE U+FFFD, although its code point is larger.
    #[test]
    fn keys_sort_like_javascript() {
        let v = json!({"\u{fffd}": 1, "\u{1f600}": 2});
        assert_eq!(canonical(&v).unwrap(), "{\"\u{1f600}\":2,\"\u{fffd}\":1}");
    }

    #[test]
    fn strings_are_escaped_like_json_stringify() {
        let v = json!({"k": "a\"b\\c\n\u{1}ñ"});
        assert_eq!(canonical(&v).unwrap(), "{\"k\":\"a\\\"b\\\\c\\n\\u0001ñ\"}");
    }

    #[test]
    fn a_float_has_no_canonical_form() {
        assert_eq!(canonical(&json!({"x": 1.5})), None);
        assert_eq!(canonical(&json!([1, 2.0])), None);
    }
}
