//! `{{path}}` templates that map RPC messages onto action inputs and action
//! results onto response messages.
//!
//! * A string that is exactly one placeholder keeps the value's JSON type
//!   (`"{{facts}}"` stays an array).
//! * Placeholders inside longer strings are interpolated.
//! * A value whose placeholder is missing or empty is dropped; an object in an
//!   array that loses any key is dropped entirely (so optional links vanish
//!   instead of becoming `person:`).

use serde_json::{Map, Value};

/// Render `template` against `ctx`. `None` means "drop this value".
pub fn render(template: &Value, ctx: &Value) -> Option<Value> {
    match template {
        Value::String(s) => render_str(s, ctx),
        Value::Array(items) => Some(Value::Array(
            items
                .iter()
                .filter_map(|t| {
                    let r = render(t, ctx)?;
                    match (t, &r) {
                        (Value::Object(tm), Value::Object(rm)) if rm.len() < tm.len() => None,
                        _ => Some(r),
                    }
                })
                .collect(),
        )),
        Value::Object(map) => {
            let out: Map<String, Value> =
                map.iter().filter_map(|(k, t)| render(t, ctx).map(|v| (k.clone(), v))).collect();
            Some(Value::Object(out))
        }
        other => Some(other.clone()),
    }
}

fn render_str(s: &str, ctx: &Value) -> Option<Value> {
    if !s.contains("{{") {
        return Some(Value::String(s.to_string()));
    }
    let trimmed = s.trim();
    if let Some(inner) = trimmed.strip_prefix("{{").and_then(|r| r.strip_suffix("}}"))
        && !inner.contains("{{")
    {
        return lookup(ctx, inner.trim()).filter(|v| !is_empty(v)).cloned();
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}")?;
        let value = lookup(ctx, after[..end].trim()).filter(|v| !is_empty(v))?;
        match value {
            Value::String(v) => out.push_str(v),
            other => out.push_str(&other.to_string()),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Some(Value::String(out))
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// Dotted path lookup; numeric segments index arrays. `$` is the whole context.
pub fn lookup<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    if path == "$" || path.is_empty() {
        return Some(ctx);
    }
    path.split('.').try_fold(ctx, |cur, seg| match cur {
        Value::Object(m) => m.get(seg),
        Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_types_interpolation_and_drops() {
        let ctx = json!({ "ticket_id": "T-1", "priority": "high", "facts": ["a", "b"], "n": 3, "props": { "x": 1 } });
        let t = json!({
            "key": "{{ticket_id}}",
            "id": "issue:{{ticket_id}}",
            "facts": "{{facts}}",
            "count": "{{n}}",
            "nested": "{{props.x}}",
            "missing": "{{nope}}",
            "props": { "priority": "{{priority}}", "owner": "{{owner}}" },
            "links": [ { "rel": "assigned_to", "to": "person:{{assignee}}" }, { "rel": "belongs_to", "to": "project:{{ticket_id}}" } ],
            "fixed": 7
        });
        let r = render(&t, &ctx).unwrap();
        assert_eq!(r["key"], "T-1");
        assert_eq!(r["id"], "issue:T-1");
        assert_eq!(r["facts"], json!(["a", "b"]));
        assert_eq!(r["count"], 3);
        assert_eq!(r["nested"], 1);
        assert!(r.get("missing").is_none());
        assert_eq!(r["props"], json!({ "priority": "high" }));
        assert_eq!(r["links"], json!([{ "rel": "belongs_to", "to": "project:T-1" }]));
        assert_eq!(r["fixed"], 7);
    }
}
