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
        let response = handle_request(&mut request, &store);
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

fn handle_request(request: &mut Request, store: &TraceStore) -> Response<std::io::Cursor<Vec<u8>>> {
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

    let (status, content_type, body) =
        match url.split_once('?').map_or(url.as_str(), |(path, _)| path) {
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
    store: &TraceStore,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let content_type_is_json = request.headers().iter().any(|header| {
        header.field.equiv("Content-Type")
            && header
                .value
                .as_str()
                .split(';')
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    });
    if !content_type_is_json {
        return json_response(
            StatusCode(415),
            json!({ "error": "content type must be application/json" }),
        );
    }
    if request
        .body_length()
        .is_some_and(|length| length > MAX_JSON_BODY_BYTES)
    {
        return json_response(
            StatusCode(413),
            json!({ "error": "request body exceeds 1 MiB" }),
        );
    }
    let mut bytes = Vec::new();
    if let Err(error) = request
        .as_reader()
        .take((MAX_JSON_BODY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
    {
        return json_response(
            StatusCode(400),
            json!({ "error": format!("could not read request body: {error}") }),
        );
    }
    if bytes.len() > MAX_JSON_BODY_BYTES {
        return json_response(
            StatusCode(413),
            json!({ "error": "request body exceeds 1 MiB" }),
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
    store: &TraceStore,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let path = url.split_once('?').map_or(url, |(path, _)| path);
    let result = match (method, path) {
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
        let store = TraceStore::open(dir.path().join("traces.db")).unwrap();
        let created = write_request(
            &Method::Post,
            "/api/datasets",
            &json!({ "name": "support", "description": "initial" }),
            &store,
        );
        assert_eq!(created.status_code(), StatusCode(201));

        let duplicate = write_request(
            &Method::Post,
            "/api/datasets",
            &json!({ "name": "support" }),
            &store,
        );
        assert_eq!(duplicate.status_code(), StatusCode(409));

        let added = write_request(
            &Method::Post,
            "/api/dataset-item?name=support",
            &json!({ "input": { "question": "before" }, "expected": "answer" }),
            &store,
        );
        assert_eq!(added.status_code(), StatusCode(201));
        let item_id = store.dataset_items("support").unwrap()[0]["item_id"]
            .as_i64()
            .unwrap();

        let renamed = write_request(
            &Method::Put,
            "/api/dataset?name=support",
            &json!({ "name": "support-v2", "description": "updated" }),
            &store,
        );
        assert_eq!(renamed.status_code(), StatusCode(200));
        let updated_item = write_request(
            &Method::Put,
            &format!("/api/dataset-item?name=support-v2&id={item_id}"),
            &json!({ "input": { "question": "after" }, "expected": "better answer" }),
            &store,
        );
        assert_eq!(updated_item.status_code(), StatusCode(200));
        let item = store.dataset_items("support-v2").unwrap().remove(0);
        assert_eq!(item["input"]["question"], "after");
        assert_eq!(item["expected"], "better answer");

        let first_version = write_request(
            &Method::Post,
            "/api/prompts",
            &json!({ "name": "helper", "template": " initial\n" }),
            &store,
        );
        assert_eq!(first_version.status_code(), StatusCode(201));
        let second_version = write_request(
            &Method::Post,
            "/api/prompts",
            &json!({ "name": "helper", "template": " revised " }),
            &store,
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
}
