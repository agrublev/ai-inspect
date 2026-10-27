//! Loopback-only web UI for exploring local traces.

use std::io::{self, Read};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::bridge;
use crate::session::{self, Session};
use crate::trace::TraceStore;

const INDEX: &str = include_str!("../web/index.html");
const STYLES: &str = include_str!("../web/app.css");
const RESPONSIVE_STYLES: &str = include_str!("../web/responsive.css");
const SCRIPT: &str = include_str!("../web/app.js");
const MAX_JSON_BODY_BYTES: usize = 1024 * 1024;
const MAX_TRACE_EVENT_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_STASH_IMPORT_BODY_BYTES: usize = 16 * 1024 * 1024;

pub fn run(port: u16, projects_dir: &Path) -> Result<()> {
    let mut store = TraceStore::open(bridge::trace_db_path())
        .context("opening local trace database for web UI")?;
    import_existing_transcripts(&mut store, projects_dir)?;

    let address = format!("127.0.0.1:{port}");
    let server = Server::http(&address)
        .map_err(|error| anyhow::anyhow!("could not start web UI at {address}: {error}"))?;
    println!("Iris trace explorer listening at http://{address}");
    println!("Trace data stays local; use Ctrl-C to stop the web UI.");

    for mut request in server.incoming_requests() {
        let response = handle_request(&mut request, &mut store);
        if let Err(error) = request.respond(response) {
            eprintln!("web UI response failed: {error}");
        }
    }
    Ok(())
}

fn import_existing_transcripts(store: &mut TraceStore, projects_dir: &Path) -> Result<()> {
    if !projects_dir.exists() {
        return Ok(());
    }
    for path in session::discover(projects_dir) {
        let mut reader = Session::new(path.clone());
        reader
            .refresh()
            .with_context(|| format!("reading transcript {}", path.display()))?;
        for record in reader.take_new_records() {
            store
                .ingest_transcript(record)
                .with_context(|| format!("importing transcript {}", path.display()))?;
        }
    }
    Ok(())
}

fn handle_request(
    request: &mut Request,
    store: &mut TraceStore,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let method = request.method().clone();
    let url = request.url().to_owned();
    if method != Method::Get {
        if method == Method::Post || method == Method::Put {
            return handle_write_request(request, &method, &url, store);
        }
        return response(
            StatusCode(405),
            "application/json; charset=utf-8",
            json!({ "error": "method not allowed" }).to_string(),
        );
    }

    let (status, content_type, body) = match url
        .split_once('?')
        .map_or(url.as_str(), |(path, _)| path)
    {
        "/" => (
            StatusCode(200),
            "text/html; charset=utf-8",
            INDEX.to_owned(),
        ),
        "/app.css" => (
            StatusCode(200),
            "text/css; charset=utf-8",
            STYLES.to_owned(),
        ),
        "/responsive.css" => (
            StatusCode(200),
            "text/css; charset=utf-8",
            RESPONSIVE_STYLES.to_owned(),
        ),
        "/app.js" => (
            StatusCode(200),
            "text/javascript; charset=utf-8",
            SCRIPT.to_owned(),
        ),
        "/api/health" => (
            StatusCode(200),
            "application/json; charset=utf-8",
            json!({ "ok": true, "storage": "local" }).to_string(),
        ),
        "/api/traces/export" => {
            if query_value(&url, "format").as_deref() != Some("stash") {
                return json_response(StatusCode(400), json!({ "error": "format must be stash" }));
            }
            match store.export_stash_jsonl(query_value(&url, "id").as_deref()) {
                Ok(jsonl) => (
                    StatusCode(200),
                    "application/x-ndjson; charset=utf-8",
                    jsonl,
                ),
                Err(error) => return store_error(error),
            }
        }
        "/api/traces" => match store.list_traces() {
            Ok(traces) => (
                StatusCode(200),
                "application/json; charset=utf-8",
                json!({ "traces": traces }).to_string(),
            ),
            Err(error) => api_error(error),
        },
        "/api/datasets" => match store.list_datasets() {
            Ok(datasets) => (
                StatusCode(200),
                "application/json; charset=utf-8",
                json!({ "datasets": datasets }).to_string(),
            ),
            Err(error) => api_error(error),
        },
        "/api/dataset" => match query_value(&url, "name") {
            Some(name) => match store.list_datasets() {
                Ok(datasets) => {
                    let dataset = datasets.into_iter().find(|dataset| dataset["name"] == name);
                    match dataset {
                        Some(dataset) => match store.dataset_items(&name) {
                            Ok(items) => (
                                StatusCode(200),
                                "application/json; charset=utf-8",
                                json!({ "dataset": dataset, "items": items }).to_string(),
                            ),
                            Err(error) => api_error(error),
                        },
                        None => (
                            StatusCode(404),
                            "application/json; charset=utf-8",
                            json!({ "error": "dataset not found" }).to_string(),
                        ),
                    }
                }
                Err(error) => api_error(error),
            },
            None => (
                StatusCode(400),
                "application/json; charset=utf-8",
                json!({ "error": "dataset name is required" }).to_string(),
            ),
        },
        "/api/prompts" => match store.list_prompts() {
            Ok(prompts) => (
                StatusCode(200),
                "application/json; charset=utf-8",
                json!({ "prompts": prompts }).to_string(),
            ),
            Err(error) => api_error(error),
        },
        "/api/trace" => match query_value(&url, "id") {
            Some(trace_id) => match trace_detail(store, &trace_id) {
                Ok(Some(detail)) => (
                    StatusCode(200),
                    "application/json; charset=utf-8",
                    detail.to_string(),
                ),
                Ok(None) => (
                    StatusCode(404),
                    "application/json; charset=utf-8",
                    json!({ "error": "trace not found" }).to_string(),
                ),
                Err(error) => api_error(error),
            },
            None => (
                StatusCode(400),
                "application/json; charset=utf-8",
                json!({ "error": "trace id is required" }).to_string(),
            ),
        },
        _ => (
            StatusCode(404),
            "application/json; charset=utf-8",
            json!({ "error": "not found" }).to_string(),
        ),
    };
    response(status, content_type, body)
}

fn handle_write_request(
    request: &mut Request,
    method: &Method,
    url: &str,
    store: &mut TraceStore,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let path = url.split_once('?').map_or(url, |(path, _)| path);
    let max_body_bytes = match path {
        "/api/events" => MAX_TRACE_EVENT_BODY_BYTES,
        "/api/traces/import" => MAX_STASH_IMPORT_BODY_BYTES,
        _ => MAX_JSON_BODY_BYTES,
    };
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| {
            header
                .value
                .as_str()
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
        });
    if path == "/api/traces/import"
        && content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/x-ndjson"))
    {
        let mut bytes = Vec::new();
        if let Err(error) = request
            .as_reader()
            .take((max_body_bytes + 1) as u64)
            .read_to_end(&mut bytes)
        {
            return json_response(
                StatusCode(400),
                json!({ "error": format!("could not read Stash import: {error}") }),
            );
        }
        if bytes.len() > max_body_bytes {
            return json_response(
                StatusCode(413),
                json!({ "error": "Stash import exceeds 16 MiB" }),
            );
        }
        let input = match String::from_utf8(bytes) {
            Ok(input) => input,
            Err(error) => {
                return json_response(
                    StatusCode(400),
                    json!({ "error": format!("Stash import must be UTF-8: {error}") }),
                );
            }
        };
        return match store.import_stash_jsonl(&input) {
            Ok(imported) => json_response(
                StatusCode(201),
                json!({ "imported": imported, "format": "stash" }),
            ),
            Err(error) => store_error(error),
        };
    }
    let content_type_is_json =
        content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json"));
    if !content_type_is_json {
        return json_response(
            StatusCode(415),
            json!({ "error": "content type must be application/json" }),
        );
    }
    if request
        .body_length()
        .is_some_and(|length| length > max_body_bytes)
    {
        return json_response(
            StatusCode(413),
            json!({ "error": format!("request body exceeds {} MiB", max_body_bytes / 1024 / 1024) }),
        );
    }
    let mut bytes = Vec::new();
    if let Err(error) = request
        .as_reader()
        .take((max_body_bytes + 1) as u64)
        .read_to_end(&mut bytes)
    {
        return json_response(
            StatusCode(400),
            json!({ "error": format!("could not read request body: {error}") }),
        );
    }
    if bytes.len() > max_body_bytes {
        return json_response(
            StatusCode(413),
            json!({ "error": format!("request body exceeds {} MiB", max_body_bytes / 1024 / 1024) }),
        );
    }
    let body = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(body) if body.is_object() => body,
        Ok(_) => {
            return json_response(
                StatusCode(400),
                json!({ "error": "request body must be a JSON object" }),
            );
        }
        Err(error) => {
            return json_response(
                StatusCode(400),
                json!({ "error": format!("invalid JSON: {error}") }),
            );
        }
    };
    write_request(method, url, &body, store)
}

fn write_request(
    method: &Method,
    url: &str,
    body: &serde_json::Value,
    store: &mut TraceStore,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let path = url.split_once('?').map_or(url, |(path, _)| path);
    let result = match (method, path) {
        (Method::Post, "/api/events") => store
            .ingest_external_event(body)
            .map(|()| (StatusCode(201), json!({ "ok": true }))),
        (Method::Post, "/api/traces/import") => import_stash_traces(body, store),
        (Method::Post, "/api/datasets") => create_dataset(body, store),
        (Method::Put, "/api/dataset") => update_dataset(url, body, store),
        (Method::Post, "/api/dataset-item") => add_dataset_item(url, body, store),
        (Method::Put, "/api/dataset-item") => update_dataset_item(url, body, store),
        (Method::Post, "/api/prompts") => save_prompt(body, store),
        _ => {
            return json_response(StatusCode(404), json!({ "error": "not found" }));
        }
    };
    match result {
        Ok((status, body)) => json_response(status, body),
        Err(error) => store_error(error),
    }
}

fn import_stash_traces(
    body: &serde_json::Value,
    store: &mut TraceStore,
) -> io::Result<(StatusCode, serde_json::Value)> {
    let format = required_text(body, "format", "trace format", 32)?;
    if format != "stash" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "format must be stash",
        ));
    }
    let data = body
        .get("data")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "trace data must be a JSONL string",
            )
        })?;
    let imported = store.import_stash_jsonl(data)?;
    Ok((
        StatusCode(201),
        json!({ "imported": imported, "format": "stash" }),
    ))
}

fn create_dataset(
    body: &serde_json::Value,
    store: &TraceStore,
) -> io::Result<(StatusCode, serde_json::Value)> {
    let name = required_text(body, "name", "dataset name", 200)?;
    let description = optional_text(body, "description", 5_000)?;
    if store
        .list_datasets()?
        .iter()
        .any(|dataset| dataset["name"] == name)
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("dataset already exists: {name}"),
        ));
    }
    store.create_dataset(&name, &description)?;
    Ok((
        StatusCode(201),
        json!({ "dataset": { "name": name, "description": description, "items": 0 } }),
    ))
}

fn update_dataset(
    url: &str,
    body: &serde_json::Value,
    store: &TraceStore,
) -> io::Result<(StatusCode, serde_json::Value)> {
    let Some(current_name) = query_value(url, "name") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dataset name is required",
        ));
    };
    let name = required_text(body, "name", "dataset name", 200)?;
    let description = optional_text(body, "description", 5_000)?;
    store.update_dataset(&current_name, &name, &description)?;
    Ok((
        StatusCode(200),
        json!({ "dataset": { "name": name, "description": description } }),
    ))
}

fn add_dataset_item(
    url: &str,
    body: &serde_json::Value,
    store: &TraceStore,
) -> io::Result<(StatusCode, serde_json::Value)> {
    let Some(name) = query_value(url, "name") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dataset name is required",
        ));
    };
    let input = body.get("input").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "dataset item input is required",
        )
    })?;
    let expected = body.get("expected").filter(|value| !value.is_null());
    store.add_dataset_item(&name, input, expected)?;
    Ok((StatusCode(201), json!({ "ok": true })))
}

fn update_dataset_item(
    url: &str,
    body: &serde_json::Value,
    store: &TraceStore,
) -> io::Result<(StatusCode, serde_json::Value)> {
    let Some(name) = query_value(url, "name") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dataset name is required",
        ));
    };
    let item_id = query_value(url, "id")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "valid item id is required"))?;
    let input = body.get("input").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "dataset item input is required",
        )
    })?;
    let expected = body.get("expected").filter(|value| !value.is_null());
    store.update_dataset_item(&name, item_id, input, expected)?;
    Ok((StatusCode(200), json!({ "ok": true })))
}

fn save_prompt(
    body: &serde_json::Value,
    store: &TraceStore,
) -> io::Result<(StatusCode, serde_json::Value)> {
    let name = required_text(body, "name", "prompt name", 200)?;
    let template = required_template(body)?;
    let version = store.save_prompt(&name, &template)?;
    Ok((StatusCode(201), json!({ "name": name, "version": version })))
}

fn required_text(
    body: &serde_json::Value,
    key: &str,
    label: &str,
    max_bytes: usize,
) -> io::Result<String> {
    let value = body
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{label} is required"))
        })?;
    if value.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} exceeds {max_bytes} bytes"),
        ));
    }
    Ok(value.to_owned())
}

fn required_template(body: &serde_json::Value) -> io::Result<String> {
    let template = body
        .get("template")
        .and_then(serde_json::Value::as_str)
        .filter(|template| !template.trim().is_empty())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "prompt template is required")
        })?;
    if template.len() > MAX_JSON_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("prompt template exceeds {MAX_JSON_BODY_BYTES} bytes"),
        ));
    }
    Ok(template.to_owned())
}

fn optional_text(body: &serde_json::Value, key: &str, max_bytes: usize) -> io::Result<String> {
    let value = body
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if value.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{key} exceeds {max_bytes} bytes"),
        ));
    }
    Ok(value.to_owned())
}

fn store_error(error: io::Error) -> Response<std::io::Cursor<Vec<u8>>> {
    let status = match error.kind() {
        io::ErrorKind::InvalidInput => StatusCode(400),
        io::ErrorKind::NotFound => StatusCode(404),
        io::ErrorKind::AlreadyExists => StatusCode(409),
        _ => StatusCode(500),
    };
    json_response(status, json!({ "error": error.to_string() }))
}

fn json_response(
    status: StatusCode,
    body: serde_json::Value,
) -> Response<std::io::Cursor<Vec<u8>>> {
    response(status, "application/json; charset=utf-8", body.to_string())
}

fn trace_detail(store: &TraceStore, trace_id: &str) -> std::io::Result<Option<serde_json::Value>> {
    let trace = store
        .list_traces()?
        .into_iter()
        .find(|trace| trace["trace_id"].as_str() == Some(trace_id));
    let Some(trace) = trace else {
        return Ok(None);
    };
    Ok(Some(json!({
        "trace": trace,
        "spans": store.spans_for_trace(trace_id)?,
        "events": store.events_for_trace(trace_id)?,
        "evaluations": store.evaluations(Some(trace_id))?,
    })))
}

fn query_value(url: &str, key: &str) -> Option<String> {
    url.split_once('?')?.1.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key)
            .then(|| decode_query_component(value))
            .flatten()
    })
}

fn decode_query_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' => {
                let high = *bytes.get(index + 1)?;
                let low = *bytes.get(index + 2)?;
                decoded.push(hex_digit(high)? * 16 + hex_digit(low)?);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn api_error(error: std::io::Error) -> (StatusCode, &'static str, String) {
    (
        StatusCode(500),
        "application/json; charset=utf-8",
        json!({ "error": error.to_string() }).to_string(),
    )
}

fn response(
    status: StatusCode,
    content_type: &str,
    body: String,
) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(body.into_bytes())
        .with_status_code(status)
        .with_header(header("Content-Type", content_type))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Referrer-Policy", "no-referrer"))
        .with_header(header(
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'",
        ))
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).expect("static HTTP response header is valid")
}

#[cfg(test)]
mod tests {
    use super::{query_value, write_request};
    use crate::trace::TraceStore;
    use serde_json::json;
    use tempfile::TempDir;
    use tiny_http::{Method, StatusCode};

    #[test]
    fn query_value_reads_named_parameter_without_matching_prefixes() {
        assert_eq!(
            query_value("/api/trace?other=x&id=abc", "id"),
            Some("abc".into())
        );
        assert_eq!(
            query_value("/api/dataset?name=golden%20set%2Bv1", "name"),
            Some("golden set+v1".into())
        );
        assert_eq!(query_value("/api/trace?id2=abc", "id"), None);
        assert_eq!(query_value("/api/trace", "id"), None);
        assert_eq!(query_value("/api/dataset?name=%GG", "name"), None);
    }

    #[test]
    fn web_writes_create_and_edit_datasets_and_append_prompt_versions() {
        let dir = TempDir::new().unwrap();
        let mut store = TraceStore::open(dir.path().join("traces.db")).unwrap();
        let created = write_request(
            &Method::Post,
            "/api/datasets",
            &json!({ "name": "support", "description": "initial" }),
            &mut store,
        );
        assert_eq!(created.status_code(), StatusCode(201));

        let duplicate = write_request(
            &Method::Post,
            "/api/datasets",
            &json!({ "name": "support" }),
            &mut store,
        );
        assert_eq!(duplicate.status_code(), StatusCode(409));

        let added = write_request(
            &Method::Post,
            "/api/dataset-item?name=support",
            &json!({ "input": { "question": "before" }, "expected": "answer" }),
            &mut store,
        );
        assert_eq!(added.status_code(), StatusCode(201));
        let item_id = store.dataset_items("support").unwrap()[0]["item_id"]
            .as_i64()
            .unwrap();

        let renamed = write_request(
            &Method::Put,
            "/api/dataset?name=support",
            &json!({ "name": "support-v2", "description": "updated" }),
            &mut store,
        );
        assert_eq!(renamed.status_code(), StatusCode(200));
        let updated_item = write_request(
            &Method::Put,
            &format!("/api/dataset-item?name=support-v2&id={item_id}"),
            &json!({ "input": { "question": "after" }, "expected": "better answer" }),
            &mut store,
        );
        assert_eq!(updated_item.status_code(), StatusCode(200));
        let item = store.dataset_items("support-v2").unwrap().remove(0);
        assert_eq!(item["input"]["question"], "after");
        assert_eq!(item["expected"], "better answer");

        let first_version = write_request(
            &Method::Post,
            "/api/prompts",
            &json!({ "name": "helper", "template": " initial\n" }),
            &mut store,
        );
        assert_eq!(first_version.status_code(), StatusCode(201));
        let second_version = write_request(
            &Method::Post,
            "/api/prompts",
            &json!({ "name": "helper", "template": " revised " }),
            &mut store,
        );
        assert_eq!(second_version.status_code(), StatusCode(201));
        assert_eq!(
            store.get_prompt("helper", Some(1)).unwrap().unwrap()["template"],
            " initial\n"
        );
        assert_eq!(
            store.get_prompt("helper", Some(2)).unwrap().unwrap()["template"],
            " revised "
        );
    }

    #[test]
    fn web_ingests_validated_trace_events_for_the_explorer() {
        let dir = TempDir::new().unwrap();
        let mut store = TraceStore::open(dir.path().join("traces.db")).unwrap();
        let event = json!({
            "source_id":"openrouter:run:0","session_id":"openrouter:run",
            "trace_id":"openrouter:run","span_id":"openrouter:run",
            "parent_span_id":null,"timestamp":"2026-10-27T10:00:00Z",
            "event_type":"trace.input","name":"OpenRouter agent run","status":"running",
            "input":{"input":"hello"},"output":null,
            "metadata":{"source":"openrouter-agent"}
        });

        let accepted = write_request(&Method::Post, "/api/events", &event, &mut store);
        assert_eq!(accepted.status_code(), StatusCode(201));
        let invalid = write_request(
            &Method::Post,
            "/api/events",
            &json!({ "source_id": "missing-required-fields" }),
            &mut store,
        );
        assert_eq!(invalid.status_code(), StatusCode(400));
        let traces = store.list_traces().unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0]["trace_id"], "openrouter:run");
    }

    #[test]
    fn web_imports_and_exports_stash_jsonl() {
        let dir = TempDir::new().unwrap();
        let mut store = TraceStore::open(dir.path().join("traces.db")).unwrap();
        let stash_line = json!({
            "id":"web-round-trip",
            "title":"Web format test",
            "metadata":{"origin":"fixture"},
            "steps":[
                {"role":"user","content":"hello"},
                {"role":"assistant","content":"world"}
            ]
        })
        .to_string();
        let imported = write_request(
            &Method::Post,
            "/api/traces/import",
            &json!({ "format": "stash", "data": stash_line }),
            &mut store,
        );
        assert_eq!(imported.status_code(), StatusCode(201));

        let exported = store
            .export_stash_jsonl(Some("stash:web-round-trip"))
            .unwrap();
        let round_trip: serde_json::Value = serde_json::from_str(exported.trim()).unwrap();
        assert_eq!(round_trip["id"], "web-round-trip");
        assert_eq!(round_trip["steps"][0]["content"], "hello");
        assert_eq!(round_trip["steps"][1]["content"], "world");

        let unsupported = write_request(
            &Method::Post,
            "/api/traces/import",
            &json!({ "format": "openai_chat", "data": "{}" }),
            &mut store,
        );
        assert_eq!(unsupported.status_code(), StatusCode(400));
        assert_eq!(store.list_traces().unwrap().len(), 1);
    }
}
