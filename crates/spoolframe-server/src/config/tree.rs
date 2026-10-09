//! A UCL document as plain data, for merging the layers (P3-10) key by
//! key and writing the runtime layer back out.
//!
//! libucl's own priorities merge only whole top-level objects: a `plex {}`
//! in a later chunk replaces the earlier one rather than adding to it. So
//! each layer is parsed into a [`Table`] here, the tables are merged in
//! Rust, and the result is written out again as UCL for the typed parse
//! ([`super::ServerConfig`]) that every layer has always gone through.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use uclicious::raw::ObjectRef;
use uclicious::{DuplicateStrategy, Parser, Priority};

/// A UCL value. Objects are [`Table`]s; everything else is a leaf, an
/// array included: an array is one setting, replaced whole.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<Value>),
    Object(Table),
}

/// An object's keys, sorted, so a written document is the same for the
/// same settings.
pub type Table = BTreeMap<String, Value>;

/// Parse one UCL document. A repeated key is an error, as it is for the
/// typed parse.
pub fn parse(text: &str) -> Result<Table, String> {
    let mut parser = Parser::default();
    parser
        .add_chunk_full(
            text,
            Priority::default(),
            DuplicateStrategy::UCL_DUPLICATE_ERROR,
        )
        .map_err(|e| e.to_string())?;
    let root = parser.get_object().map_err(|e| e.to_string())?;
    match value(&root)? {
        Value::Object(table) => Ok(table),
        _ => Err("the document is not an object".into()),
    }
}

fn value(object: &ObjectRef) -> Result<Value, String> {
    Ok(if object.is_object() {
        let mut table = Table::new();
        for child in object.iter() {
            let key = child.key().ok_or("an object member without a key")?;
            table.insert(key, value(&child)?);
        }
        Value::Object(table)
    } else if object.is_array() {
        Value::Array(object.iter().map(|o| value(&o)).collect::<Result<_, _>>()?)
    } else if object.is_integer() {
        Value::Int(object.as_i64().ok_or("an integer out of range")?)
    } else if object.is_float() {
        Value::Float(object.as_f64().unwrap_or_default())
    } else if object.is_time() {
        Value::Float(object.as_time().unwrap_or_default())
    } else if object.is_string() {
        Value::String(object.as_string().unwrap_or_default())
    } else if object.is_boolean() {
        Value::Bool(object.as_bool().unwrap_or_default())
    } else {
        return Err(format!("unsupported UCL value {object:?}"));
    })
}

/// A key as UCL takes it bare, or quoted.
fn key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        name.to_owned()
    } else {
        serde_json::to_string(name).expect("a string serializes")
    }
}

impl Value {
    /// The value as UCL text: a string quoted and escaped, an object or an
    /// array on one line.
    pub fn to_ucl(&self) -> String {
        match self {
            Value::String(s) => serde_json::to_string(s).expect("a string serializes"),
            Value::Int(n) => n.to_string(),
            Value::Float(f) if f.fract() == 0.0 && f.is_finite() => format!("{f:.1}"),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Array(items) => {
                let items: Vec<String> = items.iter().map(Value::to_ucl).collect();
                format!("[{}]", items.join(", "))
            }
            Value::Object(table) => {
                let members: Vec<String> = table
                    .iter()
                    .map(|(k, v)| format!("{} = {};", key(k), v.to_ucl()))
                    .collect();
                format!("{{ {} }}", members.join(" "))
            }
        }
    }

    /// The value as JSON, for the admin API.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::String(s) => s.clone().into(),
            Value::Int(n) => (*n).into(),
            Value::Float(f) => (*f).into(),
            Value::Bool(b) => (*b).into(),
            Value::Array(items) => items.iter().map(Value::to_json).collect(),
            Value::Object(table) => table
                .iter()
                .map(|(k, v)| (k.clone(), v.to_json()))
                .collect::<serde_json::Map<_, _>>()
                .into(),
        }
    }

    /// A JSON value as UCL's; `null` has none. A `null` member of an
    /// object is left out: it is how serde writes a `None` field (the
    /// admin API's `PathMappingEntry.library`), and the typed parse reads
    /// a missing key as `None`.
    pub fn from_json(json: &serde_json::Value) -> Result<Self, String> {
        use serde_json::Value as J;
        Ok(match json {
            J::Null => return Err("null is not a value; unset the setting instead".into()),
            J::Bool(b) => Value::Bool(*b),
            J::Number(n) => match n.as_i64() {
                Some(i) => Value::Int(i),
                None => Value::Float(n.as_f64().ok_or("a number out of range")?),
            },
            J::String(s) => Value::String(s.clone()),
            J::Array(items) => Value::Array(
                items
                    .iter()
                    .map(Value::from_json)
                    .collect::<Result<_, _>>()?,
            ),
            J::Object(map) => Value::Object(
                map.iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| Ok((k.clone(), Value::from_json(v)?)))
                    .collect::<Result<_, String>>()?,
            ),
        })
    }
}

/// `table` as a UCL document: one member per line, nested objects
/// indented.
pub fn to_document(table: &Table) -> String {
    let mut out = String::new();
    write_table(&mut out, table, 0);
    out
}

fn write_table(out: &mut String, table: &Table, depth: usize) {
    let indent = "  ".repeat(depth);
    for (k, v) in table {
        match v {
            Value::Object(inner) => {
                let _ = writeln!(out, "{indent}{} {{", key(k));
                write_table(out, inner, depth + 1);
                let _ = writeln!(out, "{indent}}}");
            }
            leaf => {
                let _ = writeln!(out, "{indent}{} = {};", key(k), leaf.to_ucl());
            }
        }
    }
}

/// Every leaf of `table` with its dotted key (`plex.url`), in key order.
/// An array is a leaf: it is set and replaced whole. So is an object that
/// is one setting (`network.wan`).
pub fn leaves(table: &Table) -> Vec<(String, &Value)> {
    let mut out = Vec::new();
    collect(table, "", &mut out);
    out
}

/// `prefix.key`, or `key` at the top.
pub fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

fn collect<'t>(table: &'t Table, prefix: &str, out: &mut Vec<(String, &'t Value)>) {
    for (k, v) in table {
        let path = join(prefix, k);
        match v {
            Value::Object(inner) if super::settings::setting(&path).is_none() => {
                collect(inner, &path, out)
            }
            leaf => out.push((path, leaf)),
        }
    }
}

/// What a string names to be read from the environment: `${SF_ENV_…}`
/// (PX-22). Only this prefix is read, so a value cannot reach `HOME` or
/// the server's own variables.
pub const ENV_PREFIX: &str = "SF_ENV_";

/// `table` with every `${SF_ENV_NAME}` in its strings replaced by that
/// variable, as `env` has it. Anything else, `${HOME}` included, stays as
/// written. The dotted key and the variable of the first one `env` does
/// not have, if any.
pub fn expand_env(
    table: &Table,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<Table, (String, String)> {
    let mut out = Table::new();
    for (k, v) in table {
        out.insert(k.clone(), expand_value(v, k, env)?);
    }
    Ok(out)
}

fn expand_value(
    value: &Value,
    path: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<Value, (String, String)> {
    Ok(match value {
        Value::String(s) => {
            Value::String(expand_str(s, env).map_err(|var| (path.to_owned(), var))?)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| expand_value(v, path, env))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(table) => {
            let mut out = Table::new();
            for (k, v) in table {
                out.insert(k.clone(), expand_value(v, &join(path, k), env)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

/// `s` with its `${SF_ENV_…}` references replaced, or the first variable
/// `env` does not have.
fn expand_str(s: &str, env: &impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let name = after
            .find('}')
            .map(|end| &after[..end])
            .filter(|name| is_env_name(name));
        match name {
            Some(name) => {
                out.push_str(&env(name).ok_or_else(|| name.to_owned())?);
                rest = &after[name.len() + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// `SF_ENV_` and at least one letter, digit or `_` after it.
fn is_env_name(name: &str) -> bool {
    name.strip_prefix(ENV_PREFIX).is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Whether `s` is one reference and nothing else: `${SF_ENV_NAME}`.
pub fn is_env_reference(s: &str) -> bool {
    s.strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .is_some_and(is_env_name)
}

/// The value at dotted `key`, if `table` has one.
pub fn get<'t>(table: &'t Table, key: &str) -> Option<&'t Value> {
    let (first, rest) = match key.split_once('.') {
        Some((first, rest)) => (first, Some(rest)),
        None => (key, None),
    };
    match (table.get(first)?, rest) {
        (value, None) => Some(value),
        (Value::Object(inner), Some(rest)) => get(inner, rest),
        _ => None,
    }
}

/// Set dotted `key` to `value`, making the objects on the way.
pub fn set(table: &mut Table, key: &str, value: Value) {
    match key.split_once('.') {
        None => {
            table.insert(key.to_owned(), value);
        }
        Some((first, rest)) => {
            let entry = table
                .entry(first.to_owned())
                .or_insert_with(|| Value::Object(Table::new()));
            if !matches!(entry, Value::Object(_)) {
                *entry = Value::Object(Table::new());
            }
            if let Value::Object(inner) = entry {
                set(inner, rest, value);
            }
        }
    }
}

/// Remove dotted `key`, and any object left empty by it. Whether there
/// was one.
pub fn remove(table: &mut Table, key: &str) -> bool {
    match key.split_once('.') {
        None => table.remove(key).is_some(),
        Some((first, rest)) => {
            let Some(Value::Object(inner)) = table.get_mut(first) else {
                return false;
            };
            let removed = remove(inner, rest);
            if inner.is_empty() {
                table.remove(first);
            }
            removed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_reads_into_nested_tables() {
        let table = parse(
            r#"
            name = "Den";
            plex { url = "http://nas:32400"; path_mapping [ { from = "/a"; to = "/b"; } ] }
            transcode { max_jobs = 3; }
            admin { insecure_cookies = true; }
            "#,
        )
        .unwrap();
        assert_eq!(get(&table, "name"), Some(&Value::String("Den".into())));
        assert_eq!(get(&table, "transcode.max_jobs"), Some(&Value::Int(3)));
        assert_eq!(
            get(&table, "admin.insecure_cookies"),
            Some(&Value::Bool(true))
        );
        let keys: Vec<String> = leaves(&table).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            keys,
            [
                "admin.insecure_cookies",
                "name",
                "plex.path_mapping",
                "plex.url",
                "transcode.max_jobs"
            ]
        );
    }

    #[test]
    fn a_repeated_key_is_an_error() {
        assert!(parse("a = 1; a = 2;").is_err());
        assert!(parse("o { a = 1; a = 2; }").is_err());
    }

    #[test]
    fn a_written_document_reads_back_the_same() {
        let table = parse(
            r#"
            name = "Den \"quoted\" \\ and\nnewline";
            plex { url = "http://nas:32400"; path_mapping [ { library = "Kids' films"; from = "/a b"; to = "/c"; } ] }
            transcode { max_jobs = 3; }
            ratio = 0.5;
            whole = 2.0;
            "with space" = true;
            "#,
        )
        .unwrap();
        let text = to_document(&table);
        assert_eq!(parse(&text).unwrap(), table, "{text}");
    }

    #[test]
    fn set_and_remove_make_and_drop_the_objects_on_the_way() {
        let mut table = Table::new();
        set(&mut table, "plex.url", Value::String("http://a".into()));
        set(
            &mut table,
            "plex.watch_sync.user",
            Value::String("me".into()),
        );
        assert_eq!(
            to_document(&table),
            "plex {\n  url = \"http://a\";\n  watch_sync {\n    user = \"me\";\n  }\n}\n"
        );
        assert!(remove(&mut table, "plex.watch_sync.user"));
        assert!(!remove(&mut table, "plex.watch_sync.user"));
        assert_eq!(to_document(&table), "plex {\n  url = \"http://a\";\n}\n");
        assert!(remove(&mut table, "plex.url"));
        assert!(table.is_empty());
    }

    fn env(name: &str) -> Option<String> {
        match name {
            "SF_ENV_SECRET" => Some("s3cret".into()),
            "SF_ENV_HOST" => Some("nas".into()),
            "HOME" => Some("/root".into()),
            _ => None,
        }
    }

    #[test]
    fn sf_env_references_expand_anywhere_in_a_string() {
        let table = parse(
            r#"
            plex { url = "http://${SF_ENV_HOST}:32400"; }
            auth { oidc [ { name = "Pocket"; client_secret = "${SF_ENV_SECRET}"; } ] }
            "#,
        )
        .unwrap();
        let expanded = expand_env(&table, &env).unwrap();
        assert_eq!(
            get(&expanded, "plex.url"),
            Some(&Value::String("http://nas:32400".into()))
        );
        let Some(Value::Array(oidc)) = get(&expanded, "auth.oidc") else {
            panic!("no auth.oidc");
        };
        let Value::Object(p) = &oidc[0] else { panic!() };
        assert_eq!(
            p.get("client_secret"),
            Some(&Value::String("s3cret".into()))
        );
    }

    #[test]
    fn only_the_sf_env_prefix_is_read() {
        let table = parse(r#"name = "${HOME} ${SF_ENV_} $SF_ENV_HOST ${SF_ENV_HOST";"#).unwrap();
        assert_eq!(expand_env(&table, &env).unwrap(), table);
    }

    #[test]
    fn an_unset_sf_env_variable_is_an_error_naming_it_and_its_key() {
        let table = parse(r#"telemetry { otlp { headers { key = "${SF_ENV_NOPE}"; } } }"#).unwrap();
        assert_eq!(
            expand_env(&table, &env),
            Err((
                "telemetry.otlp.headers.key".to_owned(),
                "SF_ENV_NOPE".to_owned()
            ))
        );
    }

    #[test]
    fn a_reference_is_one_variable_and_nothing_else() {
        assert!(is_env_reference("${SF_ENV_OIDC_SECRET}"));
        assert!(!is_env_reference("x${SF_ENV_OIDC_SECRET}"));
        assert!(!is_env_reference("${HOME}"));
        assert!(!is_env_reference("hunter2"));
    }

    #[test]
    fn json_goes_both_ways() {
        let json = serde_json::json!([{ "from": "/a", "to": "/b" }]);
        let value = Value::from_json(&json).unwrap();
        assert_eq!(value.to_json(), json);
        assert!(Value::from_json(&serde_json::Value::Null).is_err());
    }

    /// serde writes a `None` field as `null`: in an object it is a key
    /// left out, as the typed parse reads a missing one.
    #[test]
    fn a_null_member_of_an_object_is_left_out() {
        let json = serde_json::json!([{ "library": null, "from": "/a", "to": "/b" }]);
        assert_eq!(
            Value::from_json(&json).unwrap().to_json(),
            serde_json::json!([{ "from": "/a", "to": "/b" }])
        );
        assert!(Value::from_json(&serde_json::json!([null])).is_err());
    }
}
