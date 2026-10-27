use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Task {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) short_id: String,
    #[serde(default)]
    pub(crate) archived: bool,
    #[serde(default)]
    pub(crate) clockify_task_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Commit {
    pub(crate) hash: String,
    pub(crate) subject: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) task_id: String,
    pub(crate) started_at: DateTime<Local>,
    pub(crate) stopped_at: Option<DateTime<Local>>,
    #[serde(default)]
    pub(crate) duration_override_ns: Option<i64>,
    #[serde(default)]
    pub(crate) corrected: bool,
    #[serde(default)]
    pub(crate) commits: Vec<Commit>,
}

/// Tracks remote delivery separately so local session records stay compatible.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ClockifySync {
    #[default]
    LocalOnly,
    PendingStart,
    Running {
        entry_id: String,
    },
    PendingStop {
        entry_id: Option<String>,
    },
    PendingComplete,
    FailedStart {
        error: String,
    },
    FailedStop {
        entry_id: String,
        error: String,
    },
    FailedComplete {
        error: String,
    },
    Synced {
        entry_id: String,
    },
}

#[derive(Default)]
pub(crate) struct Settings {
    pub(crate) repository_path: String,
    pub(crate) author_override: String,
    pub(crate) export_directory: String,
    pub(crate) clockify_project_id: Option<String>,
    pub(crate) clockify_project_name: Option<String>,
}

#[derive(Default)]
pub(crate) struct AppData {
    pub(crate) tasks: Vec<Task>,
    pub(crate) sessions: Vec<Session>,
    pub(crate) clockify_sync: BTreeMap<String, ClockifySync>,
    pub(crate) settings: Settings,
}

impl AppData {
    pub(crate) fn load(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("create data directory {}", root.display()))?;
        let tasks_doc: Option<Versioned<TaskDoc>> = read_json(root.join("tasks.json"))?;
        let settings_doc: Option<Versioned<SettingsDoc>> = read_json(root.join("settings.json"))?;
        let clockify_doc: Option<Versioned<ClockifyDoc>> = read_json(root.join("clockify.json"))?;
        let clockify_sync = match clockify_doc {
            Some(document) if document.version == 1 => document.data.states,
            Some(document) => bail!(
                "unsupported clockify.json version {}; preserve the file and update the app",
                document.version
            ),
            None => BTreeMap::new(),
        };
        let sessions = read_sessions(&root.join("sessions.tsv"))?;
        let mut data = Self {
            tasks: tasks_doc
                .and_then(|document| document.map(|data| data.tasks))
                .unwrap_or_default(),
            sessions,
            clockify_sync,
            settings: settings_doc
                .and_then(|document| document.map(|data| data))
                .map(|d| Settings {
                    repository_path: d.repository_path,
                    author_override: d.author_override,
                    export_directory: d.export_directory,
                    clockify_project_id: d.clockify_project_id,
                    clockify_project_name: d.clockify_project_name,
                })
                .unwrap_or_default(),
        };
        if data.settings.repository_path.is_empty() {
            data.settings.repository_path = std::env::current_dir()?.display().to_string();
        }
        let mut ids = HashSet::new();
        for task in &data.tasks {
            if task.id.is_empty() || task.name.trim().is_empty() || !ids.insert(task.id.clone()) {
                bail!("tasks.json contains an invalid or duplicate task");
            }
        }
        for session in &data.sessions {
            if !ids.contains(&session.task_id) {
                bail!(
                    "session {} references missing task {}",
                    session.id,
                    session.task_id
                );
            }
        }
        for session_id in data.clockify_sync.keys() {
            if !data
                .sessions
                .iter()
                .any(|session| &session.id == session_id)
            {
                bail!("clockify.json references missing session {session_id}");
            }
        }
        Ok(data)
    }

    pub(crate) fn save(&self, root: &Path) -> Result<()> {
        atomic_write(
            root.join("tasks.json"),
            &serde_json::to_vec_pretty(&Versioned {
                version: 1,
                data: TaskDoc {
                    tasks: self.tasks.clone(),
                },
            })?,
        )?;
        atomic_write(
            root.join("settings.json"),
            &serde_json::to_vec_pretty(&Versioned {
                version: 1,
                data: SettingsDoc {
                    repository_path: self.settings.repository_path.clone(),
                    author_override: self.settings.author_override.clone(),
                    export_directory: self.settings.export_directory.clone(),
                    clockify_project_id: self.settings.clockify_project_id.clone(),
                    clockify_project_name: self.settings.clockify_project_name.clone(),
                },
            })?,
        )?;
        write_sessions(&root.join("sessions.tsv"), &self.sessions)?;
        atomic_write(
            root.join("clockify.json"),
            &serde_json::to_vec_pretty(&Versioned {
                version: 1,
                data: ClockifyDoc {
                    states: self.clockify_sync.clone(),
                },
            })?,
        )?;
        Ok(())
    }

    pub(crate) fn running(&self) -> Option<usize> {
        self.sessions.iter().position(|s| s.stopped_at.is_none())
    }
    pub(crate) fn task_name(&self, id: &str) -> &str {
        self.tasks
            .iter()
            .find(|t| t.id == id)
            .map_or("Unknown task", |t| &t.name)
    }
    pub(crate) fn elapsed_today(
        &self,
        id: &str,
        day: NaiveDate,
        now: DateTime<Local>,
    ) -> chrono::Duration {
        self.sessions
            .iter()
            .filter(|s| s.task_id == id)
            .map(|s| {
                if let Some(override_ns) = s.duration_override_ns {
                    return if s.started_at.date_naive() == day {
                        chrono::Duration::nanoseconds(override_ns)
                    } else {
                        chrono::Duration::zero()
                    };
                }
                let start = day
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_local_timezone(Local)
                    .single()
                    .unwrap();
                let end = (day.succ_opt().unwrap())
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_local_timezone(Local)
                    .single()
                    .unwrap();
                let stop = s.stopped_at.unwrap_or(now);
                let from = s.started_at.max(start);
                let until = stop.min(end);
                if until > from {
                    until - from
                } else {
                    chrono::Duration::zero()
                }
            })
            .sum()
    }
}

#[derive(Serialize, Deserialize)]
struct Versioned<T> {
    pub(crate) version: u32,
    #[serde(flatten)]
    pub(crate) data: T,
}
#[derive(Serialize, Deserialize)]
struct TaskDoc {
    tasks: Vec<Task>,
}
#[derive(Serialize, Deserialize)]
struct SettingsDoc {
    #[serde(default)]
    repository_path: String,
    #[serde(default)]
    author_override: String,
    #[serde(default)]
    export_directory: String,
    #[serde(default)]
    clockify_project_id: Option<String>,
    #[serde(default)]
    clockify_project_name: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct ClockifyDoc {
    #[serde(default)]
    states: BTreeMap<String, ClockifySync>,
}

impl<T> Versioned<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Option<U> {
        if self.version == 1 {
            Some(f(self.data))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn clockify_delivery_state_round_trips_without_changing_session_tsv() {
        let root = std::env::temp_dir().join(format!("commitmentissues-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let now = Local.with_ymd_and_hms(2026, 10, 27, 9, 30, 0).unwrap();
        let task = Task {
            id: "task-1".into(),
            name: "Write tests".into(),
            short_id: String::new(),
            archived: false,
            clockify_task_id: Some("remote-task-1".into()),
        };
        let session = Session {
            id: "session-1".into(),
            task_id: task.id.clone(),
            started_at: now,
            stopped_at: None,
            duration_override_ns: None,
            corrected: false,
            commits: Vec::new(),
        };
        let mut data = AppData {
            tasks: vec![task],
            sessions: vec![session],
            settings: Settings {
                clockify_project_id: Some("remote-project-1".into()),
                clockify_project_name: Some("Project One".into()),
                ..Settings::default()
            },
            ..AppData::default()
        };
        data.clockify_sync.insert(
            "session-1".into(),
            ClockifySync::Running {
                entry_id: "clockify-entry-1".into(),
            },
        );
        data.save(&root).unwrap();

        let loaded = AppData::load(&root).unwrap();
        assert_eq!(
            loaded.clockify_sync.get("session-1"),
            Some(&ClockifySync::Running {
                entry_id: "clockify-entry-1".into(),
            })
        );
        assert_eq!(
            loaded.settings.clockify_project_id.as_deref(),
            Some("remote-project-1")
        );
        assert_eq!(
            loaded.settings.clockify_project_name.as_deref(),
            Some("Project One")
        );
        assert_eq!(
            loaded.tasks[0].clockify_task_id.as_deref(),
            Some("remote-task-1")
        );
        let sessions_tsv = fs::read_to_string(root.join("sessions.tsv")).unwrap();
        assert_eq!(sessions_tsv.lines().next().unwrap().split('\t').count(), 9);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_without_clockify_fields_remain_loadable() {
        let legacy: Versioned<SettingsDoc> = serde_json::from_str(
            r#"{"version":1,"repository_path":"repo","author_override":"","export_directory":""}"#,
        )
        .unwrap();
        assert_eq!(legacy.data.clockify_project_id, None);
        assert_eq!(legacy.data.clockify_project_name, None);
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(value))
}

fn atomic_write(path: PathBuf, contents: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    {
        let mut file = File::create(&temp)?;
        file.write_all(contents)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    fs::rename(&temp, &path).with_context(|| format!("replace {}", path.display()))
}

fn read_sessions(path: &Path) -> Result<Vec<Session>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut reader = csv::ReaderBuilder::new().delimiter(b'\t').from_path(path)?;
    let mut grouped: Vec<Session> = Vec::new();
    for row in reader.records() {
        let row = row?;
        let id = row.get(0).unwrap_or_default().to_owned();
        let started =
            DateTime::parse_from_rfc3339(row.get(2).unwrap_or_default())?.with_timezone(&Local);
        let stopped = match row.get(3).unwrap_or_default() {
            "" => None,
            value => Some(DateTime::parse_from_rfc3339(value)?.with_timezone(&Local)),
        };
        let override_ns = match row.get(5).unwrap_or_default() {
            "" => None,
            value => Some(value.parse()?),
        };
        let session = if let Some(existing) = grouped.iter_mut().find(|s| s.id == id) {
            existing
        } else {
            grouped.push(Session {
                id,
                task_id: row.get(1).unwrap_or_default().into(),
                started_at: started,
                stopped_at: stopped,
                duration_override_ns: override_ns,
                corrected: row.get(6).unwrap_or("false").parse()?,
                commits: Vec::new(),
            });
            grouped.last_mut().unwrap()
        };
        let hash = row.get(7).unwrap_or_default();
        if !hash.is_empty() {
            session.commits.push(Commit {
                hash: hash.into(),
                subject: row.get(8).unwrap_or_default().into(),
            });
        }
    }
    Ok(grouped)
}

fn write_sessions(path: &Path, sessions: &[Session]) -> Result<()> {
    let temp = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let mut writer = csv::WriterBuilder::new()
        .delimiter(b'\t')
        .from_path(&temp)?;
    writer.write_record([
        "session_id",
        "task_id",
        "start_time",
        "stop_time",
        "elapsed_ns",
        "duration_override_ns",
        "corrected",
        "commit_hash",
        "commit_subject",
    ])?;
    for s in sessions {
        let commits: Vec<Option<&Commit>> = if s.commits.is_empty() {
            vec![None]
        } else {
            s.commits.iter().map(Some).collect()
        };
        for commit in commits {
            let elapsed = s
                .stopped_at
                .map(|end| {
                    (end - s.started_at)
                        .num_nanoseconds()
                        .unwrap_or_default()
                        .to_string()
                })
                .unwrap_or_default();
            writer.write_record([
                s.id.as_str(),
                s.task_id.as_str(),
                &s.started_at.to_rfc3339(),
                &s.stopped_at.map(|d| d.to_rfc3339()).unwrap_or_default(),
                &elapsed,
                &s.duration_override_ns
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                if s.corrected { "true" } else { "false" },
                commit.map_or("", |c| &c.hash),
                commit.map_or("", |c| &c.subject),
            ])?;
        }
    }
    writer.flush()?;
    drop(writer);
    fs::rename(temp, path)?;
    Ok(())
}
