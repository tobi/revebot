//! Attach CLI commands to a house that already owns the microVM.
//!
//! `Sandbox::start` refuses a namesake that is `Running`. The house process is
//! the owner; `revebot exec` / `tool` / `tui` become HTTP clients of that
//! process when `.reve/house.json` is ready. They must not boot a second VM
//! and must not stop the owner's guest on the way out.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::events::Event;
use crate::project::Project;
use crate::sandbox::{Output, Sandbox};

/// How long a client waits for a starting house to publish `status: "ready"`
/// and answer `/api/health`.
pub const READY_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize)]
pub struct HouseFile {
    pub pid: u32,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub sock: Option<PathBuf>,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub status: String,
}

/// Where a CLI command should send work.
#[derive(Debug)]
pub enum Target {
    /// Talk to the running house. Do not start or stop a microVM.
    House(HouseClient),
    /// No house, no leftover guest: boot a one-shot sandbox.
    OneShot,
    /// A namesake is Running and nobody holds `house.lock`.
    Orphan { name: String },
}

/// HTTP client for a ready house.
#[derive(Clone, Debug)]
pub struct HouseClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl HouseClient {
    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    fn from_file(file: &HouseFile) -> anyhow::Result<Self> {
        let bind = file
            .bind
            .as_deref()
            .map(str::trim)
            .filter(|bind| !bind.is_empty())
            .ok_or_else(|| anyhow::anyhow!("house.json has no bind address"))?;
        Ok(Self {
            http: reqwest::Client::builder().no_proxy().build()?,
            base: http_base(bind),
            token: file.token.clone(),
        })
    }

    async fn health(&self) -> anyhow::Result<()> {
        let url = format!("{}/api/health", self.base);
        let response = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .with_context(|| format!("health {url}"))?;
        if !response.status().is_success() {
            anyhow::bail!("health {url} returned {}", response.status());
        }
        Ok(())
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.token.is_empty() {
            request
        } else {
            request.header("Authorization", format!("Bearer {}", self.token))
        }
    }

    async fn send_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<(reqwest::StatusCode, Value)> {
        let url = format!("{}{path}", self.base);
        let mut request = self.http.request(method, &url);
        request = self.authorize(request);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.with_context(|| url.clone())?;
        let status = response.status();
        let value = match response.json::<Value>().await {
            Ok(value) => value,
            Err(_) => Value::Null,
        };
        if !status.is_success() {
            let error = value
                .pointer("/error")
                .and_then(Value::as_str)
                .or_else(|| value.pointer("/error/message").and_then(Value::as_str))
                .unwrap_or("request failed");
            anyhow::bail!("{path} returned {status}: {error}");
        }
        Ok((status, value))
    }

    pub async fn exec(
        &self,
        command: &str,
        cwd: Option<String>,
        timeout_seconds: Option<u64>,
    ) -> anyhow::Result<Output> {
        let mut body = serde_json::json!({ "command": command });
        if let Some(cwd) = cwd {
            insert_field(&mut body, "cwd", Value::String(cwd));
        }
        if let Some(timeout_seconds) = timeout_seconds {
            insert_field(&mut body, "timeout_seconds", Value::from(timeout_seconds));
        }
        let (_, value) = self
            .send_json(reqwest::Method::POST, "/api/exec", Some(&body))
            .await?;
        Ok(Output {
            stdout: string_field(&value, "stdout"),
            stderr: string_field(&value, "stderr"),
            exit_code: value
                .get("exit_code")
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
                .unwrap_or(0),
            success: value
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            cancelled: value
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    pub async fn tool(&self, name: &str, args: Map<String, Value>) -> anyhow::Result<String> {
        let body = serde_json::json!({ "name": name, "args": args });
        let (_, value) = self
            .send_json(reqwest::Method::POST, "/api/tool", Some(&body))
            .await?;
        if let Some(text) = value.get("result").and_then(Value::as_str) {
            return Ok(text.to_string());
        }
        if let Some(text) = value.get("text").and_then(Value::as_str) {
            return Ok(text.to_string());
        }
        Ok(value.to_string())
    }

    pub async fn prompt(
        &self,
        bot: &str,
        text: &str,
        log_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let path = format!("/api/bots/{bot}/messages");
        let mut body = serde_json::json!({ "text": text });
        if let Some(log_id) = log_id {
            insert_field(&mut body, "log_id", Value::String(log_id.to_string()));
        }
        let (_, value) = self
            .send_json(reqwest::Method::POST, &path, Some(&body))
            .await?;
        Ok(string_field(&value, "log_id"))
    }

    pub async fn abort(&self, bot: &str) -> anyhow::Result<()> {
        let path = format!("/api/bots/{bot}/abort");
        self.send_json(reqwest::Method::POST, &path, None).await?;
        Ok(())
    }

    pub async fn compact(
        &self,
        bot: &str,
        log_id: &str,
        instructions: Option<&str>,
    ) -> anyhow::Result<()> {
        let path = format!("/api/bots/{bot}/commands");
        let mut body = serde_json::json!({
            "command": "compact",
            "log_id": log_id,
        });
        if let Some(instructions) = instructions.filter(|text| !text.is_empty()) {
            insert_field(
                &mut body,
                "instructions",
                Value::String(instructions.to_string()),
            );
        }
        self.send_json(reqwest::Method::POST, &path, Some(&body))
            .await?;
        Ok(())
    }

    pub async fn log_id(&self, bot: &str) -> anyhow::Result<String> {
        let path = format!("/api/bots/{bot}/messages");
        let (_, value) = self.send_json(reqwest::Method::GET, &path, None).await?;
        Ok(string_field(&value, "log_id"))
    }

    fn events_url(&self, bot: &str) -> String {
        let ws = ws_base(&self.base);
        format!("{ws}/api/bots/{bot}/events?token={}", self.token)
    }

    /// Subscribe to the bot event stream. Hello/lagged frames are skipped.
    pub async fn subscribe(&self, bot: &str) -> anyhow::Result<mpsc::Receiver<Event>> {
        let url = self.events_url(bot);
        let (ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .with_context(|| format!("events {url}"))?;
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            use futures::StreamExt as _;
            let (_, mut read) = ws.split();
            while let Some(message) = read.next().await {
                let Ok(message) = message else {
                    break;
                };
                let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                if let Some("hello" | "lagged") = value.get("type").and_then(Value::as_str) {
                    continue;
                }
                if let Ok(event) = serde_json::from_value::<Event>(value)
                    && tx.send(event).await.is_err()
                {
                    break;
                }
            }
        });
        Ok(rx)
    }
}

/// Decide whether this process should attach, one-shot, or refuse an orphan.
pub async fn connect(project: &Project) -> anyhow::Result<Target> {
    let name = Sandbox::sandbox_name_for(&project.runtime.policy, project.workspace());
    let namesake = Sandbox::namesake_is_running(&name).await.then_some(name);
    resolve(&project.state_dir(), READY_WAIT, namesake).await
}

pub async fn resolve(
    state_dir: &Path,
    wait: Duration,
    namesake: Option<String>,
) -> anyhow::Result<Target> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match snapshot(state_dir) {
            Snapshot::Ready(file) => match HouseClient::from_file(&file) {
                Ok(client) => {
                    if client.health().await.is_ok() {
                        return Ok(Target::House(client));
                    }
                    if tokio::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "house at {} is not accepting requests; retry",
                            client.base()
                        );
                    }
                }
                Err(_) if tokio::time::Instant::now() >= deadline => {
                    anyhow::bail!("house is not accepting requests; retry");
                }
                Err(_) => {}
            },
            Snapshot::Starting => {
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!("house is starting; retry");
                }
            }
            Snapshot::Free => {
                return Ok(match namesake {
                    Some(name) => Target::Orphan { name },
                    None => Target::OneShot,
                });
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

enum Snapshot {
    Ready(HouseFile),
    Starting,
    Free,
}

fn snapshot(state_dir: &Path) -> Snapshot {
    if let Some(file) = read_house_file(state_dir)
        && file.status == "ready"
        && pid_alive(file.pid)
    {
        return Snapshot::Ready(file);
    }
    if lock_is_held(state_dir) {
        Snapshot::Starting
    } else {
        Snapshot::Free
    }
}

fn read_house_file(state_dir: &Path) -> Option<HouseFile> {
    let text = std::fs::read_to_string(state_dir.join("house.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn lock_is_held(state_dir: &Path) -> bool {
    let path = state_dir.join("house.lock");
    match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file.try_lock().is_err(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

fn pid_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return false;
    };
    rustix::process::test_kill_process(pid).is_ok()
}

fn http_base(bind: &str) -> String {
    let rewritten = bind.replace("0.0.0.0:", "127.0.0.1:");
    let rewritten = rewritten.replace("[::]:", "[::1]:");
    let trimmed = rewritten.trim().trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("http://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("https://{rest}")
    } else {
        format!("http://{trimmed}")
    }
}

fn ws_base(http: &str) -> String {
    if let Some(rest) = http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{http}")
    }
}

fn insert_field(body: &mut Value, key: &str, value: Value) {
    if let Some(object) = body.as_object_mut() {
        object.insert(key.to_string(), value);
    }
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{Target, http_base, lock_is_held, pid_alive, resolve, ws_base};
    use axum::Json;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Router, http::header};
    use serde_json::{Map, Value, json};
    use std::fs::OpenOptions;
    use std::time::Duration;
    use tokio::net::TcpListener;

    fn write_json(dir: &std::path::Path, body: &Value) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("house.json"),
            serde_json::to_vec_pretty(body).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn http_base_rewrites_wildcard_binds() {
        assert_eq!(http_base("0.0.0.0:7420"), "http://127.0.0.1:7420");
        assert_eq!(http_base("127.0.0.1:7420"), "http://127.0.0.1:7420");
        assert_eq!(http_base("http://127.0.0.1:7420/"), "http://127.0.0.1:7420");
        assert_eq!(ws_base("http://127.0.0.1:7420"), "ws://127.0.0.1:7420");
    }

    #[test]
    fn this_process_is_alive_and_a_bogus_pid_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(2_147_483_647));
    }

    #[test]
    fn a_locked_house_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!lock_is_held(dir.path()));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.path().join("house.lock"))
            .unwrap();
        file.try_lock().unwrap();
        assert!(lock_is_held(dir.path()));
        drop(file);
        assert!(!lock_is_held(dir.path()));
    }

    #[tokio::test]
    async fn no_house_and_no_namesake_is_a_one_shot() {
        let dir = tempfile::tempdir().unwrap();
        let target = resolve(dir.path(), Duration::from_millis(20), None)
            .await
            .unwrap();
        assert!(matches!(target, Target::OneShot));
    }

    #[tokio::test]
    async fn a_running_namesake_without_a_house_is_an_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let target = resolve(
            dir.path(),
            Duration::from_millis(20),
            Some("reve-orphan".into()),
        )
        .await
        .unwrap();
        match target {
            Target::Orphan { name } => assert_eq!(name, "reve-orphan"),
            Target::House(_) | Target::OneShot => panic!("expected orphan"),
        }
    }

    #[tokio::test]
    async fn a_dead_pid_ready_file_does_not_attach() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            &json!({
                "pid": 2_147_483_647u32,
                "bind": "127.0.0.1:9",
                "token": "secret",
                "status": "ready",
            }),
        );
        let target = resolve(dir.path(), Duration::from_millis(20), None)
            .await
            .unwrap();
        assert!(matches!(target, Target::OneShot));
    }

    #[tokio::test]
    async fn a_held_lock_without_ready_times_out_without_one_shot() {
        let dir = tempfile::tempdir().unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.path().join("house.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let error = resolve(dir.path(), Duration::from_millis(80), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("house is starting"));
        drop(lock);
    }

    #[tokio::test]
    async fn live_pid_without_health_does_not_one_shot() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            &json!({
                "pid": std::process::id(),
                "bind": "127.0.0.1:1",
                "token": "secret",
                "status": "ready",
            }),
        );
        let error = resolve(dir.path(), Duration::from_millis(80), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not accepting requests"));
    }

    #[derive(Clone)]
    struct Stub {
        token: String,
    }

    async fn stub_health() -> Json<Value> {
        Json(json!({ "ok": true }))
    }

    async fn stub_exec(
        State(stub): State<Stub>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(
            headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {}", stub.token)
        );
        assert_eq!(body["command"], "echo hi");
        Json(json!({
            "stdout": "hi\n",
            "stderr": "",
            "exit_code": 0,
            "success": true
        }))
    }

    async fn stub_tool(
        State(stub): State<Stub>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(
            headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {}", stub.token)
        );
        assert_eq!(body["name"], "example");
        Json(json!({ "result": "ok" }))
    }

    async fn stub_messages() -> Json<Value> {
        Json(json!({ "log_id": "log-1", "records": [] }))
    }

    #[tokio::test]
    async fn a_ready_house_attaches_and_runs_exec_and_tool() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stub = Stub {
            token: "secret".into(),
        };
        let app = Router::new()
            .route("/api/health", get(stub_health))
            .route("/api/exec", post(stub_exec))
            .route("/api/tool", post(stub_tool))
            .route(
                "/api/bots/{id}/messages",
                get(stub_messages).post(stub_prompt_ok),
            )
            .with_state(stub);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        write_json(
            dir.path(),
            &json!({
                "pid": std::process::id(),
                "bind": addr.to_string(),
                "token": "secret",
                "status": "ready",
            }),
        );
        let target = resolve(dir.path(), Duration::from_secs(2), None)
            .await
            .unwrap();
        let Target::House(client) = target else {
            panic!("expected attach");
        };
        assert_eq!(client.token(), "secret");
        let output = client.exec("echo hi", None, None).await.unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, "hi\n");
        assert_eq!(client.tool("example", Map::new()).await.unwrap(), "ok");
        assert_eq!(client.log_id("chief-of-staff").await.unwrap(), "log-1");
        assert_eq!(
            client
                .prompt("chief-of-staff", "hello", Some("log-1"))
                .await
                .unwrap(),
            "log-1"
        );
    }

    async fn stub_prompt_ok(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        assert_eq!(body["text"], "hello");
        (
            StatusCode::ACCEPTED,
            Json(json!({
                "log_id": "log-1",
                "operation_id": "op-1",
                "entry_id": "ent-1",
                "mode": "prompt"
            })),
        )
    }
}
