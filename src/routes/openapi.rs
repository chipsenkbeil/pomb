use std::collections::HashSet;

use axum::http::Method;
use serde_json::Value;
use utoipa::OpenApi;

use super::ApiDoc;
use super::docs::{one_line, operations as all_operations, render_api_reference};
use super::slot::{QUEUE_WAIT_SECS, SEND_SECS};
use crate::db::{Db, MAX_API_TOKENS_PER_ACCOUNT};
use crate::models::{AuthScope, RateLimits};
use crate::{AppState, auth};

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
fn spec_should_pin_cookie_only_operations_to_cookie_auth_and_no_bearer_scope() {
    let spec = spec();
    let reference = render_api_reference(&spec);
    let mut cookie_only = Vec::new();
    let mut public = Vec::new();
    for (name, operation) in operations(&spec) {
        let id = operation["operationId"].as_str().unwrap_or_default();
        let requirements = operation["security"].as_array().expect("security");
        let uses_cookie = requirements.iter().any(|r| r.get("cookieAuth").is_some());
        if uses_cookie {
            assert_eq!(requirements.len(), 1, "{name}");
            assert!(!is_bearer_secured(&spec, operation), "{name} has a scope");
            cookie_only.push(id);
        }
        if requirements
            .iter()
            .all(|r| r.as_object().is_some_and(|o| o.is_empty()))
        {
            public.push(id);
        }
    }
    cookie_only.sort_unstable();
    assert_eq!(
        cookie_only,
        vec![
            "approve_account",
            "create_api_token",
            "create_invite",
            "delete_account",
            "delete_member_account",
            "finish_reauth",
            "get_backups",
            "list_accounts",
            "list_api_tokens",
            "list_invites",
            "list_passkeys",
            "regenerate_recovery_code",
            "register_passkey",
            "remove_passkey",
            "revoke_api_token",
            "revoke_invite",
            "run_backup",
            "sign_out_other_sessions",
            "start_passkey_registration",
            "start_reauth",
            "suspend_account",
            "unsuspend_account",
            "update_admin_settings",
            "update_backup_settings",
        ]
    );
    public.sort_unstable();
    assert_eq!(
        public,
        vec![
            "finish_recovery",
            "finish_registration",
            "finish_setup",
            "finish_sign_in",
            "get_api_reference",
            "get_calendar_feed",
            "get_llms_txt",
            "get_openapi_spec",
            "get_skill",
            "get_version",
            "poll_device_token",
            "start_device_auth",
            "start_recovery",
            "start_registration",
            "start_setup",
            "start_sign_in",
        ]
    );
    let scheme = &spec["components"]["securitySchemes"]["cookieAuth"];
    assert_eq!(scheme["type"], "apiKey");
    assert_eq!(scheme["in"], "cookie");
    assert_eq!(scheme["name"], "session");
    assert_eq!(
        reference.matches("Auth: the browser session only.").count(),
        24
    );
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

#[test]
fn spec_should_reference_error_body_from_every_error_response() {
    let spec = spec();
    for (name, operation) in operations(&spec) {
        let responses = operation["responses"].as_object().expect("responses");
        for (status, response) in responses {
            if !status.starts_with(['4', '5']) {
                continue;
            }
            let schema = &response["content"]["application/json"]["schema"]["$ref"];
            assert_eq!(
                schema, "#/components/schemas/ErrorBody",
                "{name} {status} does not reference ErrorBody"
            );
        }
    }
}

#[test]
fn spec_should_describe_the_rate_limits() {
    let spec = spec();
    let per_second = RateLimits::DEFAULT_TOKEN_PER_SECOND;
    let burst = RateLimits::DEFAULT_TOKEN_BURST;
    let per_ip = RateLimits::DEFAULT_IP_PER_MINUTE;
    let squash = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let description = squash(spec["info"]["description"].as_str().expect("description"));
    let expected = [
        format!("{per_second} requests per second with a burst of {burst}"),
        format!("{per_ip} requests per minute on `POST /api/v1/auth/device` and {per_ip} on"),
        "429 `rate_limited`".to_string(),
        "`Retry-After`".to_string(),
    ];
    for needed in expected {
        assert!(description.contains(&needed), "missing {needed}");
    }
}

#[test]
fn spec_should_enumerate_snake_case_error_codes() {
    let spec = spec();
    let body = &spec["components"]["schemas"]["ErrorBody"];
    assert_eq!(
        body["properties"]["code"]["$ref"],
        "#/components/schemas/ErrorCode"
    );
    let codes: Vec<_> = spec["components"]["schemas"]["ErrorCode"]["enum"]
        .as_array()
        .expect("ErrorCode enum")
        .iter()
        .map(|code| code.as_str().expect("code string"))
        .collect();
    for required in [
        "token_invalid",
        "token_expired",
        "unauthenticated",
        "route_not_found",
    ] {
        assert!(codes.contains(&required), "missing {required}");
    }
    let unique: HashSet<_> = codes.iter().collect();
    assert_eq!(unique.len(), codes.len(), "duplicate code");
    for code in codes {
        let snake =
            code.chars().all(|c| c.is_ascii_lowercase() || c == '_') && !code.starts_with('_');
        assert!(snake, "{code} is not snake_case");
    }
}

/// Distinctive sentences, one per API concept rule, that live in the spec's tag, operation and
/// info descriptions.
const CONCEPT_RULES: [(&str, &str); 14] = [
    ("due date forms", "Seconds run 00 to 59 in every form"),
    (
        "due date DST",
        "occurs twice at a daylight-saving fall-back",
    ),
    ("claims advisory", "Claims are advisory"),
    ("claim fence", "goes up by one on every new claim"),
    ("claim lease", "Expiry is lazy, with no background job"),
    (
        "claim heartbeat",
        "a heartbeat reuses the lease's previous length",
    ),
    (
        "claim release",
        "any fence the task has issued releases with 200",
    ),
    (
        "readiness",
        "A blocker in any Done column, whatever its name, no longer blocks",
    ),
    ("timer stop", "less a 2-second grace"),
    ("timer stop cap", "at most one hour past its duration"),
    (
        "time entries",
        "Manual entries count toward time spent, never toward pomodori",
    ),
    ("fields", "so a client can create or adopt"),
    ("metadata", "The merge is per top-level key"),
    ("error codes", "Match on `code`, a stable snake_case"),
];

/// Joins a source file's lines with comment markers and string continuations stripped, so a
/// sentence wrapped across doc-comment or literal lines reads as one line.
fn flattened(source: &str) -> String {
    let lines = source.lines().map(|line| {
        let line = line.trim();
        let line = ["///", "//!", "//"]
            .iter()
            .find_map(|marker| line.strip_prefix(marker))
            .unwrap_or(line);
        line.strip_suffix('\\').unwrap_or(line).trim()
    });
    one_line(&lines.collect::<Vec<_>>().join(" "))
}

/// Reads every source file under `dir` except this lint file, whose rule list would match itself.
fn sources(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read source dir")
        .map(|entry| entry.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            sources(&path, out);
        } else if !path.ends_with("routes/openapi.rs") {
            let text = std::fs::read_to_string(&path).expect("read source");
            out.push((path.display().to_string(), flattened(&text)));
        }
    }
}

#[test]
fn concept_rules_should_appear_once_in_the_source_and_in_the_reference() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    assert!(
        files
            .iter()
            .any(|(name, _)| name.ends_with("docs/skill.md")),
        "the served skill sits under src/"
    );
    let reference = one_line(&render_api_reference(&spec()));
    for (rule, sentence) in CONCEPT_RULES {
        let hits: Vec<_> = files
            .iter()
            .flat_map(|(name, text)| text.matches(sentence).map(move |_| name.as_str()))
            .collect();
        assert_eq!(hits.len(), 1, "{rule}: {sentence:?} found in {hits:?}");
        assert!(
            reference.contains(sentence),
            "{rule} is missing from /docs/api.md"
        );
    }
}

#[test]
fn reference_should_list_every_operation_with_its_summary() {
    let spec = spec();
    let reference = render_api_reference(&spec);
    for (method, path, operation) in all_operations(&spec) {
        let name = format!("{} {path}", method.to_uppercase());
        let heading = format!("\n### {name}\n");
        let at = reference
            .find(&heading)
            .unwrap_or_else(|| panic!("{name} has no section"));
        let section = &reference[at + heading.len()..];
        let section = &section[..section.find("\n### ").unwrap_or(section.len())];
        let summary = operation["summary"].as_str().unwrap_or_default();
        assert!(!summary.trim().is_empty(), "{name} has no summary");
        let id = operation["operationId"].as_str().expect("operationId");
        let opening = format!("Operation `{id}`. {}", one_line(summary));
        assert!(section.contains(&opening), "{name}: {opening:?}");
        assert!(section.contains("Responses:"), "{name} lists no responses");
    }
}

#[test]
fn reference_should_name_the_error_codes_of_each_error_response() {
    let reference = render_api_reference(&spec());
    for needed in [
        "- `409`: `claim_held`: task claimed by",
        "- `401` (`unauthenticated`, `token_invalid`, `token_expired`):",
        "- `404` (`not_found`): Task not found",
        "- `409`: `field_key_exists`: field key exists",
    ] {
        assert!(reference.contains(needed), "missing {needed:?}");
    }
}

#[test]
fn spec_should_give_every_secured_operation_exactly_one_scope() {
    let spec = spec();
    for (name, operation) in operations(&spec) {
        if !is_bearer_secured(&spec, operation) {
            continue;
        }
        let requirements = operation["security"]
            .as_array()
            .expect("per-operation security");
        let scopes: Vec<_> = requirements
            .iter()
            .flat_map(|requirement| requirement["bearerAuth"].as_array().into_iter().flatten())
            .collect();
        assert_eq!(scopes.len(), 1, "{name}: {requirements:?}");
        let scope = scopes[0].as_str().unwrap_or_default();
        assert!(
            AuthScope::parse(scope).is_some(),
            "{name}: unknown scope {scope:?}"
        );
    }
}

#[test]
fn spec_should_list_a_scope_for_every_secured_write_operation() {
    let spec = spec();
    for (method, path, operation) in all_operations(&spec) {
        if method != "get" && is_bearer_secured(&spec, operation) {
            let verb = Method::from_bytes(method.to_uppercase().as_bytes()).expect("method");
            assert!(
                auth::listed_scope(&verb, path).is_some(),
                "{method} {path} has no ROUTE_SCOPES entry, so the spec would say admin"
            );
        }
    }
}

#[test]
fn spec_should_name_the_scope_each_operation_needs() {
    let spec = spec();
    let scope_of = |method: &str, path: &str| {
        spec["paths"][path][method]["security"][0]["bearerAuth"][0]
            .as_str()
            .map(str::to_string)
    };
    for (method, path, scope) in [
        ("get", "/api/boards", "read"),
        ("get", "/api/boards/{id}/calendar-feed", "admin"),
        ("post", "/api/tasks", "work"),
        ("post", "/api/tasks/{id}/claim", "work"),
        ("post", "/api/timer/stop", "work"),
        ("delete", "/api/boards/{id}", "admin"),
        ("post", "/api/boards", "provision"),
        ("post", "/api/columns", "provision"),
        ("post", "/api/swimlanes", "provision"),
        ("post", "/api/boards/{id}/fields", "provision"),
        ("patch", "/api/columns/{id}", "admin"),
        ("delete", "/api/tasks/{id}", "admin"),
    ] {
        assert_eq!(
            scope_of(method, path).as_deref(),
            Some(scope),
            "{method} {path}"
        );
    }
    assert_eq!(scope_of("get", "/api/calendar-feed/{token}"), None);
}

#[test]
fn reference_should_show_each_operations_scope() {
    let reference = render_api_reference(&spec());
    let section = |heading: &str| {
        let at = reference
            .find(heading)
            .unwrap_or_else(|| panic!("no section {heading}"));
        let rest = &reference[at + heading.len()..];
        rest[..rest.find("\n### ").unwrap_or(rest.len())].to_string()
    };
    assert!(section("\n### POST /api/tasks\n").contains("Scope: `work`."));
    assert!(section("\n### DELETE /api/boards/{id}\n").contains("Scope: `admin`."));
    assert!(section("\n### GET /api/boards\n").contains("Scope: `read`."));
    assert!(section("\n### GET /skill.md\n").contains("Auth: none."));
}

#[test]
fn spec_should_list_is_done_on_column_create() {
    let spec = spec();
    assert_eq!(
        spec["components"]["schemas"]["CreateColumnInput"]["properties"]["is_done"]["type"][0],
        "boolean"
    );
}

#[test]
fn spec_should_name_the_upload_download_and_device_limits_the_server_enforces() {
    let spec = spec().to_string();
    for phrase in [
        format!("takes {} uploads at once", AppState::UPLOADS_AT_ONCE),
        format!("waits more than {QUEUE_WAIT_SECS} seconds"),
        format!("holds {MAX_API_TOKENS_PER_ACCOUNT} tokens already"),
        format!("{} uploads are running", AppState::UPLOADS_AT_ONCE),
        format!("sends {} downloads at once", AppState::DOWNLOADS_AT_ONCE),
        format!("{} downloads are running", AppState::DOWNLOADS_AT_ONCE),
        format!("longer than {SEND_SECS} seconds to read the file"),
        format!(
            "{} device requests from this client network, or {} in all",
            Db::MAX_PENDING_DEVICE_AUTHS_PER_NETWORK,
            Db::MAX_PENDING_DEVICE_AUTHS
        ),
    ] {
        assert!(spec.contains(&phrase), "the spec no longer says {phrase:?}");
    }
}
