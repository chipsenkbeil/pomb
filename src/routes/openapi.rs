use std::collections::HashSet;

use serde_json::Value;
use utoipa::OpenApi;

use super::ApiDoc;

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

fn spec() -> Value {
    serde_json::to_value(ApiDoc::openapi()).expect("openapi json")
}

fn operations(spec: &Value) -> Vec<(String, &Value)> {
    let paths = spec["paths"].as_object().expect("paths");
    let mut found = Vec::new();
    for (path, item) in paths {
        for method in METHODS {
            if let Some(operation) = item.get(method) {
                found.push((format!("{} {path}", method.to_uppercase()), operation));
            }
        }
    }
    found
}

fn string_leaves<'a>(value: &'a Value, path: &str, out: &mut Vec<(String, &'a str)>) {
    match value {
        Value::String(text) => out.push((path.to_string(), text)),
        Value::Object(map) => {
            for (key, child) in map {
                string_leaves(child, &format!("{path}/{key}"), out);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                string_leaves(child, &format!("{path}/{index}"), out);
            }
        }
        _ => {}
    }
}

/// Pairs each `#[utoipa::path]` handler name with the `operation_id` its attribute sets.
fn annotated_handlers(source: &str) -> Vec<(String, Option<String>)> {
    let mut found = Vec::new();
    let mut open: Option<Option<String>> = None;
    for line in source.lines().map(str::trim_start) {
        if line.starts_with("#[utoipa::path(") {
            open = Some(None);
        } else if let Some(id) = &mut open {
            if let Some(rest) = line.strip_prefix("operation_id = \"") {
                *id = rest.split('"').next().map(str::to_string);
            } else if let Some(handler) = line.strip_prefix("async fn ") {
                let name = handler.split('(').next().unwrap_or_default();
                found.push((name.to_string(), id.take()));
                open = None;
            }
        }
    }
    found
}

fn is_bearer_secured(spec: &Value, operation: &Value) -> bool {
    let security = operation.get("security").unwrap_or(&spec["security"]);
    security
        .as_array()
        .is_some_and(|requirements| requirements.iter().any(|r| r.get("bearerAuth").is_some()))
}

#[test]
fn spec_should_give_every_operation_a_unique_snake_case_operation_id() {
    let spec = spec();
    let mut seen = HashSet::new();
    for (name, operation) in operations(&spec) {
        let id = operation["operationId"].as_str().unwrap_or_default();
        let snake = id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        assert!(!id.is_empty() && snake, "{name}: operationId {id:?}");
        assert!(seen.insert(id.to_string()), "{name}: duplicate {id}");
    }
}

#[test]
fn spec_should_expose_exactly_the_operation_ids_the_annotations_set() {
    let spec = spec();
    let spec_ids: HashSet<_> = operations(&spec)
        .into_iter()
        .map(|(_, operation)| operation["operationId"].as_str().expect("operationId"))
        .collect();

    let annotations = annotated_handlers(include_str!("../routes.rs"));
    let missing: Vec<_> = annotations
        .iter()
        .filter(|(_, id)| id.is_none())
        .map(|(handler, _)| handler.as_str())
        .collect();
    assert!(missing.is_empty(), "no explicit operation_id: {missing:?}");

    let explicit: HashSet<_> = annotations
        .iter()
        .filter_map(|(_, id)| id.as_deref())
        .collect();
    assert_eq!(
        annotations.len(),
        spec_ids.len(),
        "one operation per annotation"
    );
    assert_eq!(explicit, spec_ids);
}

#[test]
fn spec_should_mark_board_id_required_on_column_and_swimlane_creates() {
    let spec = spec();
    for schema in ["CreateColumnInput", "CreateSwimlaneInput"] {
        let required = spec["components"]["schemas"][schema]["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{schema} has no required list"));
        assert!(
            required.iter().any(|name| name == "board_id"),
            "{schema} does not require board_id"
        );
    }
}

#[test]
fn spec_should_not_return_html_from_any_operation() {
    let spec = spec();
    for (name, operation) in operations(&spec) {
        let responses = operation["responses"].as_object().expect("responses");
        for (status, response) in responses {
            let content = response.get("content").and_then(Value::as_object);
            let html = content.is_some_and(|types| types.keys().any(|t| t.contains("html")));
            assert!(!html, "{name} {status} returns HTML");
        }
    }
}

#[test]
fn spec_should_not_mention_tracker_ids() {
    let spec = spec();
    let mut texts = Vec::new();
    string_leaves(&spec, "", &mut texts);
    let hits: Vec<_> = texts
        .iter()
        .filter(|(_, text)| text.contains("KF-"))
        .map(|(path, text)| format!("{path}: {text}"))
        .collect();
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

#[test]
fn spec_should_declare_401_on_every_bearer_secured_operation() {
    let spec = spec();
    for (name, operation) in operations(&spec) {
        if is_bearer_secured(&spec, operation) {
            assert!(
                operation["responses"].get("401").is_some(),
                "{name} is Bearer-secured but declares no 401"
            );
        }
    }
}

#[test]
fn spec_should_mark_the_calendar_feed_public() {
    let spec = spec();
    let feed = &spec["paths"]["/api/calendar-feed/{token}"]["get"];
    let security = feed["security"].as_array().expect("explicit security");
    assert!(
        security
            .iter()
            .all(|r| r.as_object().is_some_and(|o| o.is_empty()))
    );
    assert!(!is_bearer_secured(&spec, feed));
}

#[test]
fn spec_should_report_the_crate_version_and_a_license() {
    let spec = spec();
    assert_eq!(spec["info"]["version"], env!("CARGO_PKG_VERSION"));
    let license = spec["info"]["license"]["name"].as_str().unwrap_or_default();
    assert!(!license.is_empty(), "info.license.name is set");
}

#[test]
fn spec_should_declare_every_tag_it_uses() {
    let spec = spec();
    let declared: HashSet<_> = spec["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .filter_map(|tag| tag["name"].as_str())
        .collect();
    for (name, operation) in operations(&spec) {
        for tag in operation["tags"].as_array().expect("operation tags") {
            let tag = tag.as_str().expect("tag name");
            assert!(declared.contains(tag), "{name} uses undeclared tag {tag}");
        }
    }
}

#[test]
fn spec_should_not_register_unreferenced_schemas() {
    const QUERY_ONLY: [&str; 2] = ["TimeSpentQuery", "TimerLogQuery"];
    let spec = spec();
    let text = spec.to_string();
    for name in spec["components"]["schemas"]
        .as_object()
        .expect("schemas")
        .keys()
    {
        let reference = format!("#/components/schemas/{name}\"");
        assert!(
            QUERY_ONLY.contains(&name.as_str()) || text.contains(&reference),
            "schema {name} is not referenced by any operation or schema"
        );
    }
}
