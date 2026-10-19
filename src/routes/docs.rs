use std::collections::BTreeSet;
use std::fmt::Write as _;

use axum::http::StatusCode;
use serde_json::{Map, Value};

use crate::api_error::ErrorCode;

/// HTTP methods an OpenAPI path item can hold, in the order the reference lists them.
const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];
/// Prefix of a `$ref` that points into `components.schemas`.
const SCHEMA_PREFIX: &str = "#/components/schemas/";
/// Schema keywords that combine other schemas.
const COMPOSITIONS: [&str; 3] = ["oneOf", "anyOf", "allOf"];

/// The agent skill served at `GET /skill.md`, in Agent Skills format.
pub const SKILL: &str = include_str!("docs/skill.md");

/// Version of [`SKILL`], carried in its frontmatter and reported as `skill_version`.
pub const SKILL_VERSION: &str = "3";

/// SHA-256 of [`SKILL`] per version, oldest first.
#[cfg(test)]
const SKILL_FINGERPRINTS: [(&str, &str); 3] = [
    (
        "1",
        "c935bab15837c98040da423a9e4479bee899b60c4a8eb39a8cad0338d4c2e57e",
    ),
    (
        "2",
        "0c38f465cd3c16c276ae9cd157f578d1172cb4e03e20dce1a802b6ce1ff43f8f",
    ),
    (
        "3",
        "be8df8f76c0a50e7223b2096d3aab0d55c70c8f6b5b3fe23ed255896e91baea1",
    ),
];

/// The `llms.txt` index, with `{base}` standing for the server's base URL.
const LLMS_TXT: &str = include_str!("docs/llms.txt");

/// Renders the `llms.txt` index with absolute links under `base`.
pub fn render_llms_txt(base: &str) -> String {
    LLMS_TXT.replace("{base}", base)
}

/// Renders the Markdown API reference from the OpenAPI document `spec` in its JSON form.
pub fn render_api_reference(spec: &Value) -> String {
    let mut out = String::new();
    let info = &spec["info"];
    let title = info["title"].as_str().unwrap_or("API");
    let _ = writeln!(out, "# {title} reference\n");
    if let Some(description) = info["description"].as_str() {
        let _ = writeln!(out, "{}\n", description.trim());
    }
    let codes = enum_values(&spec["components"]["schemas"]["ErrorCode"]);

    for tag in spec["tags"].as_array().into_iter().flatten() {
        let Some(name) = tag["name"].as_str() else {
            continue;
        };
        let _ = writeln!(out, "## {name}\n");
        if let Some(description) = tag["description"].as_str() {
            let _ = writeln!(out, "{}\n", description.trim());
        }
        for (method, path, operation) in operations(spec) {
            let tagged = operation["tags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|t| t == name));
            if tagged {
                render_operation(&mut out, spec, &codes, method, path, operation);
            }
        }
    }

    let _ = writeln!(out, "## Schemas\n");
    let schemas = &spec["components"]["schemas"];
    for name in referenced_schemas(spec) {
        let _ = writeln!(out, "### `{name}`\n");
        render_schema(&mut out, &schemas[name.as_str()]);
        out.push('\n');
    }
    out
}

/// Lists every operation as (method, path, operation), in path order.
pub fn operations(spec: &Value) -> Vec<(&'static str, &str, &Value)> {
    let mut found = Vec::new();
    for (path, item) in spec["paths"].as_object().into_iter().flatten() {
        for method in METHODS {
            if let Some(operation) = item.get(method) {
                found.push((method, path.as_str(), operation));
            }
        }
    }
    found
}

/// Collapses runs of whitespace, newlines included, into single spaces.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Renders one operation: heading, prose, parameters, request body and responses.
fn render_operation(
    out: &mut String,
    spec: &Value,
    codes: &[String],
    method: &str,
    path: &str,
    operation: &Value,
) {
    let _ = writeln!(out, "### {} {path}\n", method.to_uppercase());
    let id = operation["operationId"].as_str().unwrap_or_default();
    let summary = operation["summary"].as_str().map(one_line);
    let _ = writeln!(out, "Operation `{id}`. {}\n", summary.unwrap_or_default());
    if let Some(description) = operation["description"].as_str() {
        let _ = writeln!(out, "{}\n", description.trim());
    }
    let session_only = operation["security"][0].get("cookieAuth").is_some();
    let public = operation["security"]
        .as_array()
        .is_some_and(|requirements| {
            requirements
                .iter()
                .all(|r| r.as_object().is_some_and(Map::is_empty))
        });
    if session_only {
        let _ = writeln!(
            out,
            "Auth: the browser session only. A Bearer token gets 403 `session_required`.\n"
        );
    } else if public {
        let _ = writeln!(out, "Auth: none.\n");
    } else if let Some(scope) = operation["security"][0]["bearerAuth"][0].as_str() {
        let _ = writeln!(out, "Scope: `{scope}`.\n");
    }
    render_parameters(out, operation);
    render_request_body(out, spec, operation);
    render_responses(out, codes, operation);
}

/// Renders the parameter list of `operation`, when it has parameters.
fn render_parameters(out: &mut String, operation: &Value) {
    let Some(parameters) = operation["parameters"].as_array() else {
        return;
    };
    let _ = writeln!(out, "Parameters:\n");
    for parameter in parameters {
        let name = parameter["name"].as_str().unwrap_or_default();
        let place = parameter["in"].as_str().unwrap_or_default();
        let required = if parameter["required"].as_bool() == Some(true) {
            ", required"
        } else {
            ""
        };
        let _ = write!(
            out,
            "- `{name}` ({place}{required}): {}",
            type_label(&parameter["schema"])
        );
        if let Some(description) = parameter["description"].as_str() {
            let _ = write!(out, ". {}", one_line(description));
        }
        out.push('\n');
    }
    out.push('\n');
}

/// Renders the request body of `operation`, expanding a named schema into its fields.
fn render_request_body(out: &mut String, spec: &Value, operation: &Value) {
    let Some(body) = operation["requestBody"]["content"].as_object() else {
        return;
    };
    for (media, content) in body {
        let schema = &content["schema"];
        match schema_name(schema) {
            Some(name) => {
                let _ = writeln!(out, "Request body (`{media}`, `{name}`):\n");
                render_schema(out, &spec["components"]["schemas"][name]);
            }
            None => {
                let _ = writeln!(out, "Request body (`{media}`): {}", type_label(schema));
            }
        }
        out.push('\n');
    }
}

/// Renders the responses of `operation`, naming the error codes of each 4xx and 5xx.
fn render_responses(out: &mut String, codes: &[String], operation: &Value) {
    let _ = writeln!(out, "Responses:\n");
    for (status, response) in operation["responses"].as_object().into_iter().flatten() {
        let description = response["description"]
            .as_str()
            .map(one_line)
            .unwrap_or_default();
        if status.starts_with(['4', '5']) {
            let names_code = codes
                .iter()
                .any(|code| description.contains(&format!("`{code}`")));
            if names_code {
                let _ = writeln!(out, "- `{status}`: {description}");
            } else {
                let _ = writeln!(
                    out,
                    "- `{status}` ({}): {description}",
                    default_codes(status)
                );
            }
            continue;
        }
        let _ = write!(out, "- `{status}`: {description}");
        for (media, content) in response["content"].as_object().into_iter().flatten() {
            match content.get("schema") {
                Some(schema) => {
                    let _ = write!(out, ". Body (`{media}`): {}", type_label(schema));
                }
                None => {
                    let _ = write!(out, ". Body: `{media}`");
                }
            }
        }
        out.push('\n');
    }
    out.push('\n');
}

/// The codes the server sends with an error `status` when the response description names none.
fn default_codes(status: &str) -> String {
    let status = status
        .parse()
        .ok()
        .and_then(|status| StatusCode::from_u16(status).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let defaults = match status {
        StatusCode::UNAUTHORIZED => vec![
            ErrorCode::Unauthenticated,
            ErrorCode::TokenInvalid,
            ErrorCode::TokenExpired,
        ],
        status => vec![ErrorCode::for_status(status)],
    };
    defaults
        .into_iter()
        .filter_map(|code| serde_json::to_value(code).ok())
        .filter_map(|code| code.as_str().map(|code| format!("`{code}`")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders a schema as a description plus its enum values, fields or type.
fn render_schema(out: &mut String, schema: &Value) {
    if let Some(description) = schema["description"].as_str() {
        let _ = writeln!(out, "{}\n", one_line(description));
    }
    let values = enum_values(schema);
    if !values.is_empty() {
        let listed: Vec<String> = values.iter().map(|value| format!("`{value}`")).collect();
        let _ = writeln!(out, "One of {}.", listed.join(", "));
        return;
    }
    let Some(properties) = schema["properties"].as_object() else {
        let _ = writeln!(out, "Type: {}.", type_label(schema));
        return;
    };
    let required: BTreeSet<&str> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for (name, property) in properties {
        let _ = write!(out, "- `{name}`: {}", type_label(property));
        if required.contains(name.as_str()) {
            out.push_str(", required");
        }
        if let Some(description) = property_description(property) {
            let _ = write!(out, ". {}", one_line(description));
        }
        out.push('\n');
    }
}

/// A property's description, which utoipa puts on the `$ref` branch of a nullable reference.
fn property_description(property: &Value) -> Option<&str> {
    property["description"].as_str().or_else(|| {
        COMPOSITIONS
            .iter()
            .filter_map(|key| property[*key].as_array())
            .flatten()
            .find_map(|branch| branch["description"].as_str())
    })
}

/// A short readable name for a schema: its `$ref` name, its alternatives or its JSON type.
fn type_label(schema: &Value) -> String {
    if let Some(name) = schema_name(schema) {
        return format!("`{name}`");
    }
    for key in COMPOSITIONS {
        if let Some(branches) = schema[key].as_array() {
            let labels: Vec<String> = branches.iter().map(type_label).collect();
            return labels.join(" or ");
        }
    }
    let types: Vec<&str> = match &schema["type"] {
        Value::String(single) => vec![single.as_str()],
        Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if types.is_empty() {
        return "any".to_string();
    }
    let labels: Vec<String> = types
        .iter()
        .map(|kind| match *kind {
            "array" => format!("array of {}", type_label(&schema["items"])),
            kind => kind.to_string(),
        })
        .collect();
    labels.join(" or ")
}

/// The component schema name a `$ref` points at, if `schema` is one.
fn schema_name(schema: &Value) -> Option<&str> {
    schema["$ref"].as_str()?.strip_prefix(SCHEMA_PREFIX)
}

/// The string values of a schema's `enum`, empty when it has none.
fn enum_values(schema: &Value) -> Vec<String> {
    schema["enum"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Names of the schemas a reader needs beyond the inline request bodies: every schema a response
/// or a request body field refers to, followed through the schemas they refer to.
fn referenced_schemas(spec: &Value) -> BTreeSet<String> {
    let schemas = &spec["components"]["schemas"];
    let mut pending = Vec::new();
    for (_, _, operation) in operations(spec) {
        collect_refs(&operation["responses"], &mut pending);
        for content in operation["requestBody"]["content"]
            .as_object()
            .into_iter()
            .flatten()
        {
            let schema = &content.1["schema"];
            match schema_name(schema) {
                Some(name) => collect_refs(&schemas[name]["properties"], &mut pending),
                None => collect_refs(schema, &mut pending),
            }
        }
    }
    let mut found = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if !found.insert(name.clone()) {
            continue;
        }
        collect_refs(&schemas[name.as_str()], &mut pending);
    }
    found
}

/// Pushes the name of every component schema referenced anywhere inside `value` onto `out`.
fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(name) = schema_name(value) {
                out.push(name.to_string());
            }
            for child in map.values() {
                collect_refs(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_refs(child, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use utoipa::OpenApi;

    use super::{
        LLMS_TXT, METHODS, SKILL, SKILL_FINGERPRINTS, SKILL_VERSION, render_api_reference,
    };
    use crate::routes::ApiDoc;

    /// Every `METHOD /path` written in `text`, with any query string dropped.
    fn written_operations(text: &str) -> Vec<(String, String)> {
        let mut found = Vec::new();
        for method in METHODS {
            let marker = format!("{} /", method.to_uppercase());
            for (at, _) in text.match_indices(&marker) {
                let rest = &text[at + marker.len() - 1..];
                let end = rest
                    .find(|c: char| c.is_whitespace() || "`?),".contains(c))
                    .unwrap_or(rest.len());
                found.push((method.to_string(), rest[..end].to_string()));
            }
        }
        found
    }

    #[test]
    fn skill_should_match_the_fingerprint_of_its_version() {
        let actual = format!("{:x}", Sha256::digest(SKILL.as_bytes()));
        let recorded = SKILL_FINGERPRINTS
            .iter()
            .find_map(|(version, fingerprint)| (*version == SKILL_VERSION).then_some(*fingerprint))
            .expect("a fingerprint for the current skill version");
        assert!(
            recorded == actual,
            "The skill text changed. Bump SKILL_VERSION in src/routes/docs.rs and the \
             metadata.version in src/routes/docs/skill.md, then append \"{actual}\" to \
             SKILL_FINGERPRINTS, so installed copies see they are stale.\n\
             Recorded fingerprint for skill version {SKILL_VERSION}: {recorded}"
        );
    }

    #[test]
    fn skill_should_declare_its_name_description_and_version() {
        let frontmatter = SKILL
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .map(|(frontmatter, _)| frontmatter)
            .expect("frontmatter");
        assert!(frontmatter.lines().any(|line| line == "name: chipflow"));
        let description = frontmatter
            .lines()
            .find_map(|line| line.strip_prefix("description: "))
            .expect("description");
        assert!(description.contains("Use when"), "{description}");
        let version = format!("metadata:\n  version: \"{SKILL_VERSION}\"");
        assert!(frontmatter.contains(&version), "{frontmatter}");
        assert!(
            SKILL.lines().count() <= 150,
            "the skill runs past 150 lines"
        );
        assert!(
            LLMS_TXT.lines().count() <= 20,
            "llms.txt runs past 20 lines"
        );
    }

    #[test]
    fn skill_and_llms_txt_should_only_name_spec_operations() {
        let spec = serde_json::to_value(ApiDoc::openapi()).expect("spec");
        for (name, text) in [("skill.md", SKILL), ("llms.txt", LLMS_TXT)] {
            let written = written_operations(text);
            assert!(!written.is_empty(), "{name} names no operation");
            for (method, path) in written {
                assert!(
                    spec["paths"][&path].get(&method).is_some(),
                    "{name}: {} {path} is not in the spec",
                    method.to_uppercase()
                );
            }
        }
    }

    /// Every string value anywhere in `value`.
    fn collect_strings<'a>(value: &'a Value, found: &mut Vec<&'a str>) {
        match value {
            Value::String(text) => found.push(text),
            Value::Array(items) => items.iter().for_each(|item| collect_strings(item, found)),
            Value::Object(map) => map.values().for_each(|item| collect_strings(item, found)),
            _ => {}
        }
    }

    #[test]
    fn reference_should_have_a_heading_for_every_section_the_prose_points_to() {
        let spec = serde_json::to_value(ApiDoc::openapi()).expect("spec");
        let reference = render_api_reference(&spec);
        let mut texts = Vec::new();
        collect_strings(&spec, &mut texts);
        let mut pointed = Vec::new();
        for text in texts {
            for (opener, closer) in [("(see ", ")"), ("forms under ", " in the")] {
                for (at, _) in text.match_indices(opener) {
                    let rest = &text[at + opener.len()..];
                    if let Some((target, _)) = rest.split_once(closer) {
                        pointed.push(target.to_string());
                    }
                }
            }
        }
        assert!(pointed.contains(&"Due dates".to_string()), "{pointed:?}");
        assert!(pointed.contains(&"Claims".to_string()), "{pointed:?}");
        for target in pointed {
            let found = ["##", "###"]
                .iter()
                .any(|level| reference.contains(&format!("\n{level} {target}\n")));
            assert!(
                found,
                "the prose points to {target:?}, which has no heading"
            );
        }
    }
}
