use std::io::Cursor;
use std::path::Path;

use jaq_core::load::{Arena, Error as LoadError, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data, unwrap_valr};
use jaq_fmts::Format;
use jaq_json::Val;
use json_pretty_compact::PrettyCompactFormatter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryFocus {
    Query,
    Output,
}

impl QueryFocus {
    #[must_use]
    pub const fn toggle(self) -> Self {
        match self {
            Self::Query => Self::Output,
            Self::Output => Self::Query,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Query => "Query",
            Self::Output => "Output",
        }
    }
}

#[must_use]
pub const fn format_name(fmt: Format) -> &'static str {
    match fmt {
        Format::Json => "JSON",
        Format::Yaml => "YAML",
        Format::Toml => "TOML",
        Format::Xml => "XML",
        Format::Cbor => "CBOR",
        Format::Csv => "CSV",
        Format::Tsv => "TSV",
        Format::Raw | Format::Raw0 => "Raw",
        _ => "Data",
    }
}

/// Parse raw input bytes into `jaq_json::Val` items using `jaq-fmts`.
///
/// # Errors
/// Returns an error if the format name is unknown or if parsing fails.
pub fn parse_inputs(
    bytes: &[u8],
    path: Option<&Path>,
    format_override: Option<&str>,
) -> anyhow::Result<(Vec<Val>, Format)> {
    let fmt = if let Some(name) = format_override {
        Format::parse(&name.to_ascii_lowercase()).ok_or_else(|| {
            anyhow::anyhow!("unknown format '{name}'. Supported: {}", Format::ALL)
        })?
    } else if let Some(p) = path {
        Format::determine(p).unwrap_or(Format::Json)
    } else {
        Format::Json
    };

    let text = std::str::from_utf8(bytes).unwrap_or("");
    let cursor = Cursor::new(bytes);
    let mut vals = Vec::new();
    for item in jaq_fmts::read::read(fmt, cursor, text, false) {
        let val = item.map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", format_name(fmt)))?;
        vals.push(val);
    }
    Ok((vals, fmt))
}

/// Compile and evaluate a `jaq` filter against parsed inputs, formatting results as compact JSON.
///
/// # Errors
/// Returns a concise single-line error message if loading, compiling, or running the filter fails.
pub fn evaluate_query(inputs: &[Val], filter_code: &str) -> Result<String, String> {
    let code = if filter_code.trim().is_empty() {
        "."
    } else {
        filter_code
    };

    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let program = File { code, path: () };

    let modules = loader
        .load(&arena, program)
        .map_err(|errs| format_load_errors(&errs))?;
    let filter = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|errs| format_compile_errors(&errs))?;

    let mut out_parts = Vec::new();
    for input in inputs {
        let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
        for item in filter.id.run((ctx, input.clone())).map(unwrap_valr) {
            let val = item.map_err(|e| concise_error(&format!("{e}")))?;
            out_parts.push(format_compact_json(&val)?);
        }
    }

    Ok(out_parts.join("\n"))
}

fn format_compact_json(val: &Val) -> Result<String, String> {
    let raw = val.to_string();
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(raw);
    };
    let mut buf = Vec::new();
    let formatter = PrettyCompactFormatter::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    serde::Serialize::serialize(&parsed, &mut ser).map_err(|e| concise_error(&e.to_string()))?;
    String::from_utf8(buf).map_err(|e| concise_error(&e.to_string()))
}

fn format_load_errors(errs: &jaq_core::load::Errors<&str, ()>) -> String {
    for (_, err) in errs {
        match err {
            LoadError::Io(items) => {
                if let Some((path, msg)) = items.first() {
                    return format!("{path}: {msg}");
                }
            }
            LoadError::Lex(items) => {
                if let Some((expect, got)) = items.first() {
                    return format!("expected {}, got '{got}'", expect.as_str());
                }
            }
            LoadError::Parse(items) => {
                if let Some((expect, got)) = items.first() {
                    return format!("expected {}, got '{got}'", expect.as_str());
                }
            }
        }
    }
    "syntax error".to_owned()
}

fn format_compile_errors(errs: &jaq_core::compile::Errors<&str, ()>) -> String {
    errs.iter()
        .flat_map(|(_, file_errs)| file_errs.iter())
        .map(|(name, undefined)| format!("undefined {}: {name}", undefined.as_str()))
        .next()
        .unwrap_or_else(|| "compile error".to_owned())
}

fn concise_error(msg: &str) -> String {
    let first_line = msg.lines().next().unwrap_or("query error").trim();
    first_line.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_and_eval_json_compact() {
        let input = br#"{"name":{"first":"Ada","last":"Lovelace"},"nums":[1,2,3]}"#;
        let (vals, fmt) = parse_inputs(input, Some(Path::new("data.json")), None).unwrap();
        assert_eq!(format_name(fmt), "JSON");

        let out = evaluate_query(&vals, ".").unwrap();
        assert!(out.contains(r#""nums": [ 1, 2, 3 ]"#) || out.contains("[1, 2, 3]"));

        let name = evaluate_query(&vals, ".name.first").unwrap();
        assert_eq!(name, r#""Ada""#);
    }

    #[test]
    fn test_parse_yaml_and_toml() {
        let yaml = b"user:\n  name: alice\n  roles:\n    - admin\n";
        let (vals, fmt) = parse_inputs(yaml, Some(Path::new("cfg.yaml")), None).unwrap();
        assert_eq!(format_name(fmt), "YAML");
        assert_eq!(evaluate_query(&vals, ".user.name").unwrap(), r#""alice""#);

        let toml = b"[package]\nname = \"q\"\n";
        let (vals, fmt) = parse_inputs(toml, Some(Path::new("Cargo.toml")), None).unwrap();
        assert_eq!(format_name(fmt), "TOML");
        assert_eq!(evaluate_query(&vals, ".package.name").unwrap(), r#""q""#);
    }

    #[test]
    fn test_parse_format_override_and_errors() {
        let (vals, fmt) = parse_inputs(b"a: 42\n", None, Some("yaml")).unwrap();
        assert_eq!(format_name(fmt), "YAML");
        assert_eq!(evaluate_query(&vals, ".a").unwrap(), "42");

        assert!(evaluate_query(&vals, ".[").is_err());
        assert!(evaluate_query(&vals, "unknown_fn").is_err());
        assert!(parse_inputs(b"{}", None, Some("nope")).is_err());
    }
}
