//! Clockify API operations used by the local-first timer.
//!
//! Credentials come only from `CLOCKIFY_API_KEY`; this module never writes them
//! to application data. Network errors are returned to the UI so local tracking
//! can continue and the user can retry later.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
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

#[derive(Debug)]
pub(crate) struct SyncJob {
    pub(crate) session_id: String,
    pub(crate) kind: SyncKind,
    pub(crate) entry_id: Option<String>,
    pub(crate) started_at: DateTime<Local>,
    pub(crate) stopped_at: Option<DateTime<Local>>,
    pub(crate) description: String,
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
    #[serde(rename = "type")]
    entry_type: &'static str,
}

#[derive(Deserialize)]
struct TimeEntryResponse {
    id: String,
}

impl ClockifyClient {
    /// Builds a client when `CLOCKIFY_API_KEY` is set and non-empty.
    pub(crate) fn from_env() -> Result<Option<Self>> {
        let api_key = match std::env::var("CLOCKIFY_API_KEY") {
            Ok(value) if !value.trim().is_empty() => value.trim().to_owned(),
            Ok(_) | Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => bail!("CLOCKIFY_API_KEY is not valid Unicode"),
        };
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("create Clockify HTTP client")?;
        Ok(Some(Self {
            api_key: Arc::from(api_key),
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
            },
            Err(error) => SyncResult {
                session_id,
                kind,
                entry_id: existing_entry_id,
                error: Some(format!("{error:#}")),
            },
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
        let entry_id = job
            .entry_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .context("Clockify stop job has no time-entry ID")?;
        let identity = self.identity().await?;
        let payload = TimeEntryRequest {
            start: job.started_at.to_rfc3339(),
            end: Some(end.to_rfc3339()),
            description: &job.description,
            entry_type: "REGULAR",
        };
        let response = self
            .http
            .put(format!(
                "{}/workspaces/{}/time-entries/{}",
                self.api_base, identity.workspace_id, entry_id
            ))
            .header("X-Api-Key", self.api_key.as_ref())
            .json(&payload)
            .send()
            .await
            .context("stop the Clockify time entry")?;
        check_status(response, "stop the time entry").await?;
        Ok(())
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
        let started = client
            .execute(SyncJob::start(
                "session-1".into(),
                start,
                "Implement feature".into(),
            ))
            .await;
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
        let retried = client
            .execute(SyncJob::complete(
                "session-2".into(),
                start,
                stop,
                "Review feature".into(),
            ))
            .await;
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
            requests[2]
                .head
                .starts_with("PUT /api/v1/workspaces/workspace-1/time-entries/entry-1 HTTP/1.1")
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
        assert!(start_body.get("end").is_none());
        assert_eq!(start_body["type"], "REGULAR");
        let stop_body: Value = serde_json::from_str(&requests[2].body).unwrap();
        assert_eq!(stop_body["end"], stop.to_rfc3339());
        assert_eq!(stop_body["start"], start.to_rfc3339());
        assert_eq!(stop_body["description"], "Implement feature");
        let retry_body: Value = serde_json::from_str(&requests[3].body).unwrap();
        assert_eq!(retry_body["start"], start.to_rfc3339());
        assert_eq!(retry_body["end"], stop.to_rfc3339());
        assert_eq!(retry_body["description"], "Review feature");
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
