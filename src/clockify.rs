//! Clockify API operations used by the local-first timer.
//!
//! Credentials are read from the user's TOML config and never emitted in errors.
//! Network errors are returned to the UI so local tracking can continue and the
//! user can retry later.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::OnceCell;

const DEFAULT_API_BASE: &str = "https://api.clockify.me/api/v1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone)]
pub(crate) struct ClockifyClient {
    api_key: Arc<str>,
    api_base: Arc<str>,
    http: Client,
    identity: Arc<OnceCell<Identity>>,
}

#[derive(Clone, Debug)]
struct Identity {
    workspace_id: String,
    user_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SyncJob {
    pub(crate) session_id: String,
    pub(crate) kind: SyncKind,
    pub(crate) entry_id: Option<String>,
    pub(crate) started_at: DateTime<Local>,
    pub(crate) stopped_at: Option<DateTime<Local>>,
    pub(crate) description: String,
    pub(crate) project_id: Option<String>,
    pub(crate) task_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ClockifyProject {
    pub(crate) id: String,
    pub(crate) name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ClockifyTask {
    pub(crate) id: String,
    pub(crate) name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncKind {
    Start,
    Stop,
    Complete,
}

#[derive(Debug)]
pub(crate) struct SyncResult {
    pub(crate) session_id: String,
    pub(crate) kind: SyncKind,
    pub(crate) entry_id: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) config: Option<ClockifyConfigResult>,
}

#[derive(Debug)]
pub(crate) enum ClockifyConfigResult {
    Projects(Result<Vec<ClockifyProject>, String>),
    ProjectCreated(Result<ClockifyProject, String>),
    Tasks(Result<Vec<ClockifyTask>, String>),
    TaskCreated(usize, Result<ClockifyTask, String>),
}

#[derive(Deserialize)]
struct UserResponse {
    id: String,
    #[serde(rename = "activeWorkspace")]
    active_workspace: Option<String>,
    #[serde(rename = "defaultWorkspace")]
    default_workspace: Option<String>,
}

#[derive(Serialize)]
struct TimeEntryRequest<'a> {
    start: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<String>,
    description: &'a str,
    #[serde(rename = "projectId", skip_serializing_if = "Option::is_none")]
    project_id: Option<&'a str>,
    #[serde(rename = "taskId", skip_serializing_if = "Option::is_none")]
    task_id: Option<&'a str>,
    #[serde(rename = "type")]
    entry_type: &'static str,
}

#[derive(Deserialize)]
struct TimeEntryResponse {
    id: String,
}

#[derive(Deserialize, Default)]
struct AppConfig {
    #[serde(default)]
    clockify: ClockifyCredentials,
}

#[derive(Deserialize, Default)]
struct ClockifyCredentials {
    #[serde(default)]
    api_key: String,
}

impl ClockifyClient {
    /// Builds a client when the TOML config contains a non-empty API key.
    pub(crate) fn from_config(path: &Path) -> Result<Option<Self>> {
        let source = match std::fs::read_to_string(path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)?.permissions().mode();
            if mode & 0o077 != 0 {
                bail!(
                    "{} has permissions that expose the API key; run chmod 600 {}",
                    path.display(),
                    path.display()
                );
            }
        }
        let config: AppConfig = toml::from_str(&source)
            .with_context(|| format!("parse Clockify settings in {}", path.display()))?;
        let api_key = config.clockify.api_key.trim();
        if api_key.is_empty() {
            return Ok(None);
        }
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("create Clockify HTTP client")?;
        Ok(Some(Self {
            api_key: Arc::from(api_key.to_owned()),
            api_base: Arc::from(DEFAULT_API_BASE),
            http,
            identity: Arc::new(OnceCell::new()),
        }))
    }

    #[cfg(test)]
    pub(crate) fn for_test(api_key: &str, api_base: String) -> Self {
        Self {
            api_key: Arc::from(api_key),
            api_base: Arc::from(api_base),
            http: Client::new(),
            identity: Arc::new(OnceCell::new()),
        }
    }

    pub(crate) async fn list_projects(&self) -> Result<Vec<ClockifyProject>> {
        let identity = self.identity().await?;
        let response = self
            .http
            .get(format!(
                "{}/workspaces/{}/projects?archived=false",
                self.api_base, identity.workspace_id
            ))
            .header("X-Api-Key", self.api_key.as_ref())
            .send()
            .await
            .context("list Clockify projects")?;
        check_status(response, "list projects")
            .await?
            .json()
            .await
            .context("read Clockify projects")
    }

    pub(crate) async fn create_project(&self, name: &str) -> Result<ClockifyProject> {
        let identity = self.identity().await?;
        let response = self
            .http
            .post(format!(
                "{}/workspaces/{}/projects",
                self.api_base, identity.workspace_id
            ))
            .header("X-Api-Key", self.api_key.as_ref())
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .context("create Clockify project")?;
        check_status(response, "create project")
            .await?
            .json()
            .await
            .context("read created Clockify project")
    }

    pub(crate) async fn list_tasks(&self, project_id: &str) -> Result<Vec<ClockifyTask>> {
        let identity = self.identity().await?;
        let response = self
            .http
            .get(format!(
                "{}/workspaces/{}/projects/{}/tasks?is-active=true",
                self.api_base, identity.workspace_id, project_id
            ))
            .header("X-Api-Key", self.api_key.as_ref())
            .send()
            .await
            .context("list Clockify project tasks")?;
        check_status(response, "list project tasks")
            .await?
            .json()
            .await
            .context("read Clockify tasks")
    }

    pub(crate) async fn create_task(&self, project_id: &str, name: &str) -> Result<ClockifyTask> {
        let identity = self.identity().await?;
        let response = self
            .http
            .post(format!(
                "{}/workspaces/{}/projects/{}/tasks",
                self.api_base, identity.workspace_id, project_id
            ))
            .header("X-Api-Key", self.api_key.as_ref())
            .json(&serde_json::json!({ "name": name, "status": "ACTIVE" }))
            .send()
            .await
            .context("create Clockify project task")?;
        check_status(response, "create project task")
            .await?
            .json()
            .await
            .context("read created Clockify task")
    }

    async fn identity(&self) -> Result<&Identity> {
        self.identity
            .get_or_try_init(|| async {
                let response = self
                    .http
                    .get(format!("{}/user", self.api_base))
                    .header("X-Api-Key", self.api_key.as_ref())
                    .send()
                    .await
                    .context("connect to Clockify")?;
                let response = check_status(response, "identify the Clockify user").await?;
                let user: UserResponse = response
                    .json()
                    .await
                    .context("read Clockify user details")?;
                let workspace_id = user
                    .active_workspace
                    .filter(|value| !value.is_empty())
                    .or(user.default_workspace)
                    .filter(|value| !value.is_empty())
                    .context("Clockify account has no active or default workspace")?;
                if user.id.is_empty() {
                    bail!("Clockify returned an empty user ID");
                }
                Ok(Identity {
                    workspace_id,
                    user_id: user.id,
                })
            })
            .await
    }

    /// Runs a start, stop, or completed-entry request without blocking the TUI.
    pub(crate) async fn execute(&self, job: SyncJob) -> SyncResult {
        let session_id = job.session_id.clone();
        let kind = job.kind;
        let existing_entry_id = job.entry_id.clone();
        let outcome = match job.kind {
            SyncKind::Start => self.start(&job).await,
            SyncKind::Stop => match self.stop(&job).await {
                Ok(()) => Ok(None),
                Err(error) => Err(error),
            },
            SyncKind::Complete => self.create_completed(&job).await.map(Some),
        };
        match outcome {
            Ok(entry_id) => SyncResult {
                session_id,
                kind,
                entry_id: entry_id.or(existing_entry_id),
                error: None,
                config: None,
            },
            Err(error) => SyncResult {
                session_id,
                kind,
                entry_id: existing_entry_id,
                error: Some(format!("{error:#}")),
                config: None,
            },
        }
    }

    pub(crate) fn project_list(result: Result<Vec<ClockifyProject>>) -> SyncResult {
        Self::config_result(ClockifyConfigResult::Projects(
            result.map_err(|e| format!("{e:#}")),
        ))
    }

    pub(crate) fn project_created(result: Result<ClockifyProject>) -> SyncResult {
        Self::config_result(ClockifyConfigResult::ProjectCreated(
            result.map_err(|e| format!("{e:#}")),
        ))
    }

    pub(crate) fn task_list(result: Result<Vec<ClockifyTask>>) -> SyncResult {
        Self::config_result(ClockifyConfigResult::Tasks(
            result.map_err(|e| format!("{e:#}")),
        ))
    }

    pub(crate) fn task_created(index: usize, result: Result<ClockifyTask>) -> SyncResult {
        Self::config_result(ClockifyConfigResult::TaskCreated(
            index,
            result.map_err(|e| format!("{e:#}")),
        ))
    }

    fn config_result(config: ClockifyConfigResult) -> SyncResult {
        SyncResult {
            session_id: String::new(),
            kind: SyncKind::Start,
            entry_id: None,
            error: None,
            config: Some(config),
        }
    }

    async fn start(&self, job: &SyncJob) -> Result<Option<String>> {
        self.create_entry(job, None).await.map(Some)
    }

    async fn create_completed(&self, job: &SyncJob) -> Result<String> {
        let stopped_at = job
            .stopped_at
            .context("cannot create a completed Clockify entry for a running session")?;
        self.create_entry(job, Some(stopped_at)).await
    }

    async fn create_entry(&self, job: &SyncJob, end: Option<DateTime<Local>>) -> Result<String> {
        let identity = self.identity().await?;
        let payload = TimeEntryRequest {
            start: job.started_at.to_rfc3339(),
            end: end.map(|value| value.to_rfc3339()),
            description: &job.description,
            project_id: job.project_id.as_deref(),
            task_id: job.task_id.as_deref(),
            entry_type: "REGULAR",
        };
        let response = self
            .http
            .post(format!(
                "{}/workspaces/{}/user/{}/time-entries",
                self.api_base, identity.workspace_id, identity.user_id
            ))
            .header("X-Api-Key", self.api_key.as_ref())
            .json(&payload)
            .send()
            .await
            .context("send the time entry to Clockify")?;
        let response = check_status(response, "create the time entry").await?;
        let entry: TimeEntryResponse = response
            .json()
            .await
            .context("read the created Clockify time entry")?;
        if entry.id.is_empty() {
            bail!("Clockify returned an empty time-entry ID");
        }
        Ok(entry.id)
    }

    async fn stop(&self, job: &SyncJob) -> Result<()> {
        let end = job
            .stopped_at
            .context("cannot stop a Clockify entry without a local stop time")?;
        let _entry_id = job
            .entry_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .context("Clockify stop job has no time-entry ID")?;
        let identity = self.identity().await?;
        let response = self
            .http
            .patch(format!(
                "{}/workspaces/{}/user/{}/time-entries",
                self.api_base, identity.workspace_id, identity.user_id
            ))
            .header("X-Api-Key", self.api_key.as_ref())
            .json(&serde_json::json!({ "end": end.to_rfc3339() }))
            .send()
            .await
            .context("stop the Clockify time entry")?;
        check_status(response, "stop the time entry").await?;
        Ok(())
    }
}

#[cfg(test)]
mod credential_config_tests {
    use super::ClockifyClient;
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    fn temp_config() -> PathBuf {
        std::env::temp_dir().join(format!("commitmentissues-config-{}.toml", Uuid::new_v4()))
    }

    fn write_private(path: &PathBuf, value: &str) {
        fs::write(path, value).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn absent_or_blank_config_keeps_local_only_mode() {
        let path = temp_config();
        assert!(ClockifyClient::from_config(&path).unwrap().is_none());
        write_private(&path, "[clockify]\napi_key = \"  \"\n");
        assert!(ClockifyClient::from_config(&path).unwrap().is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reads_api_key_from_toml() {
        let path = temp_config();
        write_private(&path, "[clockify]\napi_key = \"test-key\"\n");
        assert!(ClockifyClient::from_config(&path).unwrap().is_some());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_malformed_toml_without_revealing_contents() {
        let path = temp_config();
        write_private(&path, "[clockify\napi_key = \"secret-test-value\"");
        let error = match ClockifyClient::from_config(&path) {
            Ok(_) => panic!("malformed config unexpectedly loaded"),
            Err(error) => error.to_string(),
        };
        assert!(!error.contains("secret-test-value"));
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_config_readable_by_group_or_others() {
        let path = temp_config();
        fs::write(&path, "[clockify]\napi_key = \"test-key\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ClockifyClient::from_config(&path).is_err());
        fs::remove_file(path).unwrap();
    }
}

async fn check_status(response: Response, operation: &str) -> Result<Response> {
    let status = response.status();
    if !status.is_success() {
        bail!("Clockify could not {operation} (HTTP {})", status.as_u16());
    }
    Ok(response)
}

impl SyncJob {
    pub(crate) fn start(
        session_id: String,
        started_at: DateTime<Local>,
        description: String,
    ) -> Self {
        Self {
            session_id,
            kind: SyncKind::Start,
            entry_id: None,
            started_at,
            stopped_at: None,
            description,
            project_id: None,
            task_id: None,
        }
    }

    pub(crate) fn complete(
        session_id: String,
        started_at: DateTime<Local>,
        stopped_at: DateTime<Local>,
        description: String,
    ) -> Self {
        Self {
            session_id,
            kind: SyncKind::Complete,
            entry_id: None,
            started_at,
            stopped_at: Some(stopped_at),
            description,
            project_id: None,
            task_id: None,
        }
    }

    pub(crate) fn stop(
        session_id: String,
        entry_id: String,
        started_at: DateTime<Local>,
        stopped_at: DateTime<Local>,
        description: String,
    ) -> Self {
        Self {
            session_id,
            kind: SyncKind::Stop,
            entry_id: Some(entry_id),
            started_at,
            stopped_at: Some(stopped_at),
            description,
            project_id: None,
            task_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::Value;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        thread,
    };

    #[tokio::test]
    async fn start_stop_and_retry_create_use_clockify_api_contract() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in [
                (
                    "200 OK",
                    r#"{"id":"user-1","activeWorkspace":"workspace-1"}"#,
                ),
                ("201 Created", r#"{"id":"entry-1"}"#),
                ("200 OK", r#"{"id":"entry-1"}"#),
                ("201 Created", r#"{"id":"entry-2"}"#),
            ] {
                let (stream, _) = listener.accept().unwrap();
                let (request, mut stream) = read_request(stream);
                requests.push(request);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        let client = ClockifyClient::for_test("test-secret", format!("http://{address}/api/v1"));
        let start = Local.with_ymd_and_hms(2026, 10, 27, 9, 30, 0).unwrap();
        let stop = Local.with_ymd_and_hms(2026, 10, 27, 10, 0, 0).unwrap();
        let mut start_job = SyncJob::start("session-1".into(), start, "Implement feature".into());
        start_job.project_id = Some("project-1".into());
        start_job.task_id = Some("task-1".into());
        let started = client.execute(start_job).await;
        assert_eq!(started.error, None);
        assert_eq!(started.entry_id.as_deref(), Some("entry-1"));
        let stopped = client
            .execute(SyncJob::stop(
                "session-1".into(),
                "entry-1".into(),
                start,
                stop,
                "Implement feature".into(),
            ))
            .await;
        assert_eq!(stopped.error, None);
        let mut retry_job =
            SyncJob::complete("session-2".into(), start, stop, "Review feature".into());
        retry_job.project_id = Some("project-1".into());
        retry_job.task_id = Some("task-2".into());
        let retried = client.execute(retry_job).await;
        assert_eq!(retried.error, None);
        assert_eq!(retried.entry_id.as_deref(), Some("entry-2"));

        let requests = server.join().unwrap();
        assert!(requests[0].head.starts_with("GET /api/v1/user HTTP/1.1"));
        assert!(
            requests[1].head.starts_with(
                "POST /api/v1/workspaces/workspace-1/user/user-1/time-entries HTTP/1.1"
            )
        );
        assert!(
            requests[2].head.starts_with(
                "PATCH /api/v1/workspaces/workspace-1/user/user-1/time-entries HTTP/1.1"
            )
        );
        assert!(
            requests[3].head.starts_with(
                "POST /api/v1/workspaces/workspace-1/user/user-1/time-entries HTTP/1.1"
            )
        );
        for request in &requests {
            assert!(
                request
                    .head
                    .to_ascii_lowercase()
                    .contains("x-api-key: test-secret")
            );
        }
        let start_body: Value = serde_json::from_str(&requests[1].body).unwrap();
        assert_eq!(start_body["description"], "Implement feature");
        assert_eq!(start_body["projectId"], "project-1");
        assert_eq!(start_body["taskId"], "task-1");
        assert!(start_body.get("end").is_none());
        assert_eq!(start_body["type"], "REGULAR");
        let stop_body: Value = serde_json::from_str(&requests[2].body).unwrap();
        assert_eq!(stop_body, serde_json::json!({ "end": stop.to_rfc3339() }));
        let retry_body: Value = serde_json::from_str(&requests[3].body).unwrap();
        assert_eq!(retry_body["start"], start.to_rfc3339());
        assert_eq!(retry_body["end"], stop.to_rfc3339());
        assert_eq!(retry_body["description"], "Review feature");
        assert_eq!(retry_body["projectId"], "project-1");
        assert_eq!(retry_body["taskId"], "task-2");
    }

    #[tokio::test]
    async fn api_error_does_not_include_the_credential_in_result() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let (_, mut stream) = read_request(stream);
            let body = "invalid key";
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let client = ClockifyClient::for_test("test-secret", format!("http://{address}/api/v1"));
        let now = Local.with_ymd_and_hms(2026, 10, 27, 9, 30, 0).unwrap();
        let result = client
            .execute(SyncJob::start("session-1".into(), now, "Task".into()))
            .await;
        let error = result.error.unwrap();
        assert!(error.contains("HTTP 401"));
        assert!(!error.contains("test-secret"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn project_and_task_configuration_uses_workspace_endpoints() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in [
                (
                    "200 OK",
                    r#"{"id":"user-1","activeWorkspace":"workspace-1"}"#,
                ),
                ("200 OK", r#"[{"id":"project-1","name":"Existing"}]"#),
                ("201 Created", r#"{"id":"project-2","name":"New Project"}"#),
                ("200 OK", r#"[{"id":"task-1","name":"Existing Task"}]"#),
                ("201 Created", r#"{"id":"task-2","name":"New Task"}"#),
            ] {
                let (stream, _) = listener.accept().unwrap();
                let (request, mut stream) = read_request(stream);
                requests.push(request);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        let client = ClockifyClient::for_test("test-secret", format!("http://{address}/api/v1"));
        assert_eq!(client.list_projects().await.unwrap()[0].name, "Existing");
        assert_eq!(
            client.create_project("New Project").await.unwrap().id,
            "project-2"
        );
        assert_eq!(
            client.list_tasks("project-1").await.unwrap()[0].name,
            "Existing Task"
        );
        assert_eq!(
            client
                .create_task("project-1", "New Task")
                .await
                .unwrap()
                .id,
            "task-2"
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1]
                .head
                .starts_with("GET /api/v1/workspaces/workspace-1/projects?archived=false HTTP/1.1")
        );
        assert!(
            requests[2]
                .head
                .starts_with("POST /api/v1/workspaces/workspace-1/projects HTTP/1.1")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&requests[2].body).unwrap()["name"],
            "New Project"
        );
        assert!(requests[3].head.starts_with(
            "GET /api/v1/workspaces/workspace-1/projects/project-1/tasks?is-active=true HTTP/1.1"
        ));
        assert!(
            requests[4].head.starts_with(
                "POST /api/v1/workspaces/workspace-1/projects/project-1/tasks HTTP/1.1"
            )
        );
        let task_body: Value = serde_json::from_str(&requests[4].body).unwrap();
        assert_eq!(task_body["name"], "New Task");
        assert_eq!(task_body["status"], "ACTIVE");
    }

    struct CapturedRequest {
        head: String,
        body: String,
    }

    fn read_request(stream: TcpStream) -> (CapturedRequest, TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap();
                }
            }
            head.push_str(&line);
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        (
            CapturedRequest {
                head,
                body: String::from_utf8(body).unwrap(),
            },
            stream,
        )
    }
}
