use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{Client, Method, Response};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::env;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::webdriver::backend::{BackendSnapshot, BrowserBackend};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Deserialize)]
struct CreateTabResponse {
    #[serde(rename = "tabId")]
    tab_id: String,
}

#[derive(Debug, Deserialize)]
struct SnapshotResponse {
    url: String,
    snapshot: String,
}

#[derive(Debug, Deserialize)]
struct EvaluateResponse {
    result: Value,
}

/// Camofox's local REST API projected through agent-browser's backend
/// contract. The backend owns a spawned server when `AGENT_BROWSER_CAMOFOX_URL`
/// is unset and only attaches when that variable is provided.
pub struct CamofoxBackend {
    client: Client,
    base_url: String,
    user_id: String,
    tab_id: String,
    access_key: Option<String>,
    process: Option<Child>,
}

impl CamofoxBackend {
    /// Launch or attach to Camofox and create the isolated tab owned by this
    /// agent-browser session.
    pub async fn launch(session_id: &str, executable_path: Option<&str>) -> Result<Self, String> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| format!("Failed to create Camofox HTTP client: {error}"))?;
        let access_key = env::var("AGENT_BROWSER_CAMOFOX_ACCESS_KEY").ok();
        let user_id = format!("agent-browser-{session_id}");

        let (base_url, process) = match env::var("AGENT_BROWSER_CAMOFOX_URL") {
            Ok(url) if !url.trim().is_empty() => (url.trim_end_matches('/').to_string(), None),
            _ => {
                let listener = std::net::TcpListener::bind("127.0.0.1:0")
                    .map_err(|error| format!("Failed to reserve a Camofox port: {error}"))?;
                let port = listener
                    .local_addr()
                    .map_err(|error| format!("Failed to read reserved Camofox port: {error}"))?
                    .port();
                drop(listener);

                let executable = resolve_camofox_executable(executable_path);
                let mut command = Command::new(&executable);
                command
                    .env("CAMOFOX_BIND_HOST", "127.0.0.1")
                    .env("CAMOFOX_PORT", port.to_string())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if env::var_os("CAMOFOX_CRASH_REPORT_ENABLED").is_none() {
                    command.env("CAMOFOX_CRASH_REPORT_ENABLED", "false");
                }
                if let Some(ref key) = access_key {
                    command.env("CAMOFOX_ACCESS_KEY", key);
                }
                let child = command.spawn().map_err(|error| {
                    format!(
                        "Failed to launch Camofox with '{executable}': {error}. Install @askjo/camofox-browser or set AGENT_BROWSER_CAMOFOX_EXECUTABLE."
                    )
                })?;
                (format!("http://127.0.0.1:{port}"), Some(child))
            }
        };

        let mut backend = Self {
            client,
            base_url,
            user_id,
            tab_id: String::new(),
            access_key,
            process,
        };
        backend.wait_until_ready().await?;
        let created: CreateTabResponse = backend
            .request_json(
                Method::POST,
                "/tabs",
                Some(json!({
                    "userId": backend.user_id,
                    "sessionKey": session_id,
                })),
            )
            .await?;
        backend.tab_id = created.tab_id;
        Ok(backend)
    }

    async fn wait_until_ready(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Some(process) = self.process.as_mut() {
                if let Some(status) = process
                    .try_wait()
                    .map_err(|error| format!("Failed to inspect Camofox process: {error}"))?
                {
                    return Err(format!(
                        "Camofox exited during startup with status {status}"
                    ));
                }
            }

            match self.request(Method::GET, "/health", None).await {
                Ok(response) if response.status().is_success() => return Ok(()),
                _ if Instant::now() < deadline => {
                    tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
                }
                _ => {
                    if let Some(process) = self.process.as_mut() {
                        let _ = process.kill();
                        let _ = process.wait();
                    }
                    return Err(format!(
                        "Timed out waiting for Camofox at {}",
                        self.base_url
                    ));
                }
            }
        }
    }

    fn tab_path(&self, suffix: &str) -> String {
        format!("/tabs/{}{}", self.tab_id, suffix)
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Response, String> {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.base_url, path));
        if let Some(ref key) = self.access_key {
            request = request.bearer_auth(key);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request
            .send()
            .await
            .map_err(|error| format!("Camofox request failed: {error}"))
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, String> {
        let response = self.request(method, path, body).await?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("Failed to read Camofox response: {error}"))?;
        if !status.is_success() {
            let detail = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
            return Err(format!("Camofox returned {status}: {detail}"));
        }
        serde_json::from_slice(&bytes).map_err(|error| format!("Invalid Camofox response: {error}"))
    }

    async fn evaluate_value(&self, expression: &str) -> Result<Value, String> {
        let response: EvaluateResponse = self
            .request_json(
                Method::POST,
                &self.tab_path("/evaluate"),
                Some(json!({
                    "userId": self.user_id,
                    "expression": expression,
                })),
            )
            .await?;
        Ok(response.result)
    }

    async fn post_tab_action(&self, action: &str) -> Result<(), String> {
        let _: Value = self
            .request_json(
                Method::POST,
                &self.tab_path(action),
                Some(json!({ "userId": self.user_id })),
            )
            .await?;
        Ok(())
    }

    fn selector_payload(&self, selector: &str) -> Value {
        let normalized = selector.strip_prefix('@').unwrap_or(selector);
        if is_camofox_ref(normalized) {
            json!({ "userId": self.user_id, "ref": normalized })
        } else {
            json!({ "userId": self.user_id, "selector": selector })
        }
    }
}

#[async_trait]
impl BrowserBackend for CamofoxBackend {
    async fn navigate(&self, url: &str) -> Result<(), String> {
        let _: Value = self
            .request_json(
                Method::POST,
                &self.tab_path("/navigate"),
                Some(json!({ "userId": self.user_id, "url": url })),
            )
            .await?;
        Ok(())
    }

    async fn get_url(&self) -> Result<String, String> {
        self.evaluate_value("location.href")
            .await?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "Camofox returned a non-string URL".to_string())
    }

    async fn get_title(&self) -> Result<String, String> {
        self.evaluate_value("document.title")
            .await?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "Camofox returned a non-string title".to_string())
    }

    async fn get_content(&self) -> Result<String, String> {
        self.evaluate_value("document.documentElement.outerHTML")
            .await?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "Camofox returned non-string page content".to_string())
    }

    async fn evaluate(&self, script: &str) -> Result<Value, String> {
        self.evaluate_value(script).await
    }

    async fn screenshot(&self) -> Result<String, String> {
        let response = self
            .request(
                Method::GET,
                &format!(
                    "{}?userId={}",
                    self.tab_path("/screenshot"),
                    urlencoding::encode(&self.user_id)
                ),
                None,
            )
            .await?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("Failed to read Camofox screenshot: {error}"))?;
        if !status.is_success() {
            return Err(format!(
                "Camofox screenshot failed with {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        Ok(STANDARD.encode(bytes))
    }

    async fn click(&self, selector: &str) -> Result<(), String> {
        let _: Value = self
            .request_json(
                Method::POST,
                &self.tab_path("/click"),
                Some(self.selector_payload(selector)),
            )
            .await?;
        Ok(())
    }

    async fn fill(&self, selector: &str, value: &str) -> Result<(), String> {
        let mut payload = self.selector_payload(selector);
        if let Some(object) = payload.as_object_mut() {
            object.insert("text".to_string(), Value::String(value.to_string()));
            object.insert("mode".to_string(), Value::String("fill".to_string()));
        }
        let _: Value = self
            .request_json(Method::POST, &self.tab_path("/type"), Some(payload))
            .await?;
        Ok(())
    }

    async fn close(&mut self) -> Result<(), String> {
        let result = if self.tab_id.is_empty() {
            Ok(())
        } else {
            self.request_json::<Value>(
                Method::DELETE,
                &format!("/sessions/{}", urlencoding::encode(&self.user_id)),
                None,
            )
            .await
            .map(|_| ())
        };
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait();
        }
        result
    }

    async fn back(&self) -> Result<(), String> {
        self.post_tab_action("/back").await
    }

    async fn forward(&self) -> Result<(), String> {
        self.post_tab_action("/forward").await
    }

    async fn reload(&self) -> Result<(), String> {
        self.post_tab_action("/refresh").await
    }

    async fn get_cookies(&self) -> Result<Value, String> {
        Err(self.unsupported_error("cookies_get"))
    }

    async fn snapshot(&self) -> Result<BackendSnapshot, String> {
        let response: SnapshotResponse = self
            .request_json(
                Method::GET,
                &format!(
                    "{}?userId={}",
                    self.tab_path("/snapshot"),
                    urlencoding::encode(&self.user_id)
                ),
                None,
            )
            .await?;
        Ok(BackendSnapshot {
            snapshot: normalize_snapshot_refs(&response.snapshot),
            origin: response.url,
            refs: Map::new(),
        })
    }

    fn backend_type(&self) -> &str {
        "camofox"
    }
}

impl Drop for CamofoxBackend {
    fn drop(&mut self) {
        if let Some(process) = self.process.as_mut() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

fn is_camofox_ref(value: &str) -> bool {
    value
        .strip_prefix('e')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

fn resolve_camofox_executable(explicit: Option<&str>) -> String {
    if let Some(path) = explicit {
        return path.to_string();
    }
    if let Ok(path) = env::var("AGENT_BROWSER_CAMOFOX_EXECUTABLE") {
        if !path.trim().is_empty() {
            return path;
        }
    }
    let adjacent = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .map(|directory| directory.join(executable_name("camofox-browser")));
    if let Some(path) = adjacent.filter(|path| path.is_file()) {
        return path.to_string_lossy().into_owned();
    }
    "camofox-browser".to_string()
}

fn executable_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.cmd")
    } else {
        base.to_string()
    }
}

fn normalize_snapshot_refs(snapshot: &str) -> String {
    let mut output = String::with_capacity(snapshot.len());
    let mut remaining = snapshot;
    while let Some(index) = remaining.find("[e") {
        let (before, candidate) = remaining.split_at(index);
        output.push_str(before);
        let closing = candidate.find(']');
        match closing {
            Some(end) if is_camofox_ref(&candidate[1..end]) => {
                output.push_str("[@");
                output.push_str(&candidate[1..=end]);
                remaining = &candidate[end + 1..];
            }
            _ => {
                output.push_str("[e");
                remaining = &candidate[2..];
            }
        }
    }
    output.push_str(remaining);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_camofox_element_refs() {
        assert!(is_camofox_ref("e1"));
        assert!(is_camofox_ref("e204"));
        assert!(!is_camofox_ref("@e1"));
        assert!(!is_camofox_ref("email"));
        assert!(!is_camofox_ref("e"));
    }

    #[test]
    fn normalizes_snapshot_refs_without_touching_other_brackets() {
        let snapshot = "- link \"Home\" [e1]\n- text: [example]\n- button [e22]";
        assert_eq!(
            normalize_snapshot_refs(snapshot),
            "- link \"Home\" [@e1]\n- text: [example]\n- button [@e22]"
        );
    }
}
