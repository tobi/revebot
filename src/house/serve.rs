//! HTTP + WebSocket surface for the house.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::net::{TcpListener, UnixListener};

use super::{CreateSpec, House};
use crate::sandbox::ExecOptions;

#[derive(Clone)]
struct AppState {
    house: Arc<House>,
}

pub async fn serve(house: House) -> anyhow::Result<()> {
    let house = Arc::new(house);
    let state = AppState {
        house: house.clone(),
    };
    let app = router(state);

    let addr: SocketAddr = house
        .bind()
        .parse()
        .unwrap_or_else(|_| "127.0.0.1:7420".parse().unwrap());
    let tcp = TcpListener::bind(addr).await?;

    let sock = house.sock().clone();
    let _ = std::fs::remove_file(&sock);
    if let Some(parent) = sock.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let unix = UnixListener::bind(&sock)?;
    house.write_house_json()?;

    let tcp_app = app.clone();
    let unix_app = app;
    tokio::select! {
        result = axum::serve(tcp, tcp_app.into_make_service()) => result?,
        result = axum::serve(unix, unix_app.into_make_service()) => result?,
    }
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/icon.svg", get(icon))
        .route("/api/health", get(health))
        .route("/api/events", get(house_events_ws))
        .route("/api/bots", get(list_bots).post(create_bot))
        .route(
            "/api/bots/{id}",
            get(get_bot).patch(patch_bot).delete(delete_bot),
        )
        .route(
            "/api/bots/{id}/messages",
            get(list_messages).post(post_message),
        )
        .route("/api/bots/{id}/attachments", post(post_attachment))
        .route(
            "/api/bots/{id}/attachments/{uid}/{name}",
            get(get_attachment),
        )
        .route("/api/bots/{id}/soul", get(get_soul).put(put_soul))
        .route("/api/bots/{id}/abort", post(abort_bot))
        .route("/api/models", get(list_models))
        .route("/api/bots/{id}/skills", get(list_skills))
        .route("/api/bots/{id}/secrets", post(complete_secret))
        .route("/api/bots/{id}/events", get(events_ws))
        .route("/api/exec", post(exec))
        .route("/api/tool", post(tool))
        .route("/api/routines", get(list_routines))
        .route("/api/routines/{id}/run", post(run_routine))
        .layer(DefaultBodyLimit::max(
            super::attach::MAX_BYTES as usize + 1024 * 1024,
        ))
        .with_state(state)
}

fn authorized(headers: &HeaderMap, query: &QueryAuth, house: &House) -> bool {
    if let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let token = value.strip_prefix("Bearer ").unwrap_or(value);
        return token == house.token();
    }
    query.token.as_deref() == Some(house.token())
}

#[derive(Default, Deserialize)]
struct QueryAuth {
    token: Option<String>,
}

async fn index(State(state): State<AppState>) -> impl IntoResponse {
    let page = crate::web::page(state.house.token(), state.house.bind());
    (
        [(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, no-cache, must-revalidate"),
        )],
        Html(page),
    )
}

async fn manifest() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/manifest+json"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        include_str!("../web/manifest.webmanifest"),
    )
}

async fn service_worker() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/javascript; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (
                HeaderName::from_static("service-worker-allowed"),
                HeaderValue::from_static("/"),
            ),
        ],
        include_str!("../web/sw.js"),
    )
}

async fn icon() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("image/svg+xml"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            ),
        ],
        include_str!("../web/icon.svg"),
    )
}

async fn health(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({ "ok": true }))
}

fn deny() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized" })),
    )
        .into_response()
}

async fn list_bots(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    Json(json!({ "bots": state.house.profile_views() })).into_response()
}

async fn get_bot(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    match state
        .house
        .profile_views()
        .into_iter()
        .find(|p| p["id"].as_str() == Some(id.as_str()))
    {
        Some(p) => Json(p).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response(),
    }
}

async fn create_bot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
    Json(body): Json<CreateSpec>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    match state.house.create_bot(body).await {
        Ok(profile) => (StatusCode::CREATED, Json(profile)).into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn patch_bot(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
    Json(patch): Json<Value>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    match state.house.patch_bot(&id, patch).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct DeleteBody {
    confirm: bool,
}

async fn delete_bot(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
    Json(body): Json<DeleteBody>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    if !body.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "confirm required" })),
        )
            .into_response();
    }
    match state.house.delete_bot(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Default, Deserialize)]
struct MessageQuery {
    token: Option<String>,
    before: Option<u64>,
    limit: Option<usize>,
}

async fn list_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<MessageQuery>,
) -> Response {
    let auth = QueryAuth { token: q.token };
    if !authorized(&headers, &auth, &state.house) {
        return deny();
    }
    let limit = q.limit.unwrap_or(80);
    match state.house.transcript_page(&id, q.before, limit).await {
        Ok((entries, has_more)) => {
            let oldest_seq = entries.first().map(|e| e.seq);
            Json(json!({
                "messages": entries,
                "has_more": has_more,
                "oldest_seq": oldest_seq
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn get_soul(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    match state.house.bot_soul(&id) {
        Ok(text) => Json(json!({ "text": text })).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct SoulBody {
    text: String,
}

async fn put_soul(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
    Json(body): Json<SoulBody>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    match state.house.set_bot_soul(&id, &body.text).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    Json(json!({ "models": state.house.configured_models() })).into_response()
}

#[derive(Deserialize)]
struct SecretBody {
    accept: bool,
    #[serde(default)]
    env: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    hosts: Vec<String>,
    header: Option<String>,
    prefix: Option<String>,
}

async fn complete_secret(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
    Json(body): Json<SecretBody>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    let kind = if body.accept {
        match super::secret::SecretKind::parse(&body.kind) {
            Ok(k) => k,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
            }
        }
    } else {
        super::secret::SecretKind::Paste
    };
    let decision = super::secret::SecretDecision {
        accept: body.accept,
        env: body.env,
        kind,
        source: body.source,
        value: body.value,
        hosts: body.hosts,
        header: body.header,
        prefix: body.prefix,
    };
    match state.house.complete_secret(&id, decision).await {
        Ok(status) => Json(json!({ "ok": true, "status": status })).into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn list_skills(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    if !state.house.ready_profiles().iter().any(|p| p.id == id) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response();
    }
    Json(json!({ "skills": state.house.skills_for(&id) })).into_response()
}

#[derive(Deserialize)]
struct PostMessage {
    text: String,
}

async fn post_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
    Json(body): Json<PostMessage>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    match state.house.prompt(&id, &body.text).await {
        Ok(ack) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "operation_id": ack.operation_id,
                "entry_id": ack.entry_id,
                "mode": ack.mode
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn post_attachment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
    mut multipart: Multipart,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    let mut name = String::from("file");
    let mut mime: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        };
        if field.name() != Some("file") && bytes.is_some() {
            continue;
        }
        if let Some(file_name) = field.file_name() {
            name = file_name.to_string();
        }
        if let Some(ct) = field.content_type() {
            mime = Some(ct.to_string());
        }
        match field.bytes().await {
            Ok(buf) => bytes = Some(buf.to_vec()),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        }
    }
    let Some(bytes) = bytes else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "file required" })),
        )
            .into_response();
    };
    match state
        .house
        .save_attachment(&id, &name, &bytes, mime.as_deref())
    {
        Ok(saved) => (
            StatusCode::CREATED,
            Json(json!({
                "id": saved.id,
                "name": saved.name,
                "path": saved.path,
                "bytes": saved.bytes,
                "mime": saved.mime,
                "tag": super::attach::tag(&saved),
                "url": format!(
                    "/api/bots/{}/attachments/{}/{}",
                    urlencoding_path(&id),
                    urlencoding_path(&saved.id),
                    urlencoding_path(&saved.name)
                )
            })),
        )
            .into_response(),
        Err(e) => {
            let status = if e.to_string().contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(json!({ "error": e.to_string() }))).into_response()
        }
    }
}

fn urlencoding_path(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn get_attachment(
    State(state): State<AppState>,
    Path((id, uid, name)): Path<(String, String, String)>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    match state.house.read_attachment(&id, &uid, &name) {
        Ok((saved, bytes)) => {
            let mut response = Response::new(Body::from(bytes));
            let headers = response.headers_mut();
            if let Ok(value) = HeaderValue::from_str(&saved.mime) {
                headers.insert(header::CONTENT_TYPE, value);
            }
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=3600"),
            );
            let disp = if saved.mime.starts_with("image/") {
                "inline"
            } else {
                "attachment"
            };
            if let Ok(value) = HeaderValue::from_str(&format!(
                "{disp}; filename=\"{}\"",
                saved.name.replace('"', "")
            )) {
                headers.insert(header::CONTENT_DISPOSITION, value);
            }
            response
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn abort_bot(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    match state.house.abort(&id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn house_events_ws(
    State(state): State<AppState>,
    Query(q): Query<QueryAuth>,
    ws: WebSocketUpgrade,
) -> Response {
    if q.token.as_deref() != Some(state.house.token()) {
        return deny();
    }
    let rx = state.house.subscribe_house();
    ws.on_upgrade(move |socket| push_events(socket, rx))
        .into_response()
}

async fn events_ws(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<QueryAuth>,
    ws: WebSocketUpgrade,
) -> Response {
    if q.token.as_deref() != Some(state.house.token()) {
        return deny();
    }
    let Ok(rx) = state.house.subscribe(&id) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response();
    };
    ws.on_upgrade(move |socket| push_events(socket, rx))
        .into_response()
}

async fn push_events(
    mut socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<crate::events::Event>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                let Ok(text) = serde_json::to_string(&event) else {
                    continue;
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                if socket
                    .send(Message::Text("{\"type\":\"lagged\"}".into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

async fn list_routines(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    Json(json!({ "routines": state.house.routines() })).into_response()
}

async fn run_routine(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    match state.house.run_routine(&id).await {
        Ok(bots) => Json(json!({ "ok": true, "bots": bots })).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ExecBody {
    command: String,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
}

async fn exec(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
    Json(body): Json<ExecBody>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    let timeout = body.timeout_seconds.map(std::time::Duration::from_secs);
    let result = state
        .house
        .sandbox()
        .exec(
            &body.command,
            ExecOptions {
                cwd: body.cwd,
                timeout,
                ..Default::default()
            },
            None,
        )
        .await;
    state.house.external_effect_finished();
    match result {
        Ok(out) => Json(json!({
            "stdout": out.stdout,
            "stderr": out.stderr,
            "exit_code": out.exit_code,
            "success": out.success
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ToolBody {
    name: String,
    #[serde(default)]
    args: Map<String, Value>,
}

async fn tool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<QueryAuth>,
    Json(body): Json<ToolBody>,
) -> Response {
    if !authorized(&headers, &q, &state.house) {
        return deny();
    }
    let result = state
        .house
        .project()
        .runtime
        .call_tool(&body.name, body.args, state.house.sandbox())
        .await;
    state.house.external_effect_finished();
    match result {
        Ok(text) => Json(json!({ "result": text })).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
