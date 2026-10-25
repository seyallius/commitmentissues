use crate::data::{Commit, Session, Settings, Task};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use std::process::Command;
use uuid::Uuid;

pub(crate) fn new_session(task: &Task, now: DateTime<Local>) -> Session {
    Session {
        id: Uuid::new_v4().simple().to_string(),
        task_id: task.id.clone(),
        started_at: now,
        stopped_at: None,
        duration_override_ns: None,
        corrected: false,
        commits: Vec::new(),
    }
}

pub(crate) fn collect_commits(settings: &Settings, day: NaiveDate) -> Result<Vec<Commit>> {
    let repo = if settings.repository_path.is_empty() {
        "."
    } else {
        &settings.repository_path
    };
    let author = if !settings.author_override.is_empty() {
        settings.author_override.clone()
    } else {
        let output = Command::new("git")
            .args(["-C", repo, "config", "--get", "user.name"])
            .output()
            .context("run git config")?;
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    if author.is_empty() {
        bail!("Git author is not configured");
    }
    let midnight = Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .context("resolve local midnight")?;
    let next_midnight = Local
        .from_local_datetime(
            &day.succ_opt()
                .context("resolve next local day")?
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .single()
        .context("resolve next local midnight")?;
    let output = Command::new("git")
        .args([
            "-C",
            repo,
            "log",
            &format!("--since={}", midnight.to_rfc3339()),
            &format!("--before={}", next_midnight.to_rfc3339()),
            &format!("--author={author}"),
            "--format=%H%x00%s",
        ])
        .output()
        .context("run git log")?;
    if !output.status.success() {
        bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            line.split_once('\0').map(|(hash, subject)| Commit {
                hash: hash.into(),
                subject: subject.into(),
            })
        })
        .collect())
}
