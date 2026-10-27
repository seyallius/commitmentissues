use crate::{
    clockify::{ClockifyClient, SyncJob, SyncKind, SyncResult},
    data::{AppData, ClockifySync, Commit, Task},
    git::{collect_commits, new_session},
};
use chrono::{DateTime, Local, NaiveDate};
use crossterm::event::{KeyCode, MouseButton};
use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState},
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

pub(crate) struct Ui {
    pub(crate) data: AppData,
    pub(crate) root: PathBuf,
    pub(crate) selected: usize,
    pub(crate) day: NaiveDate,
    pub(crate) status: String,
    pub(crate) show_archived: bool,
    pub(crate) input: Option<(String, String)>,
    pub(crate) task_area: Rect,
    pub(crate) button_areas: Vec<(Rect, usize)>,
    pub(crate) task_rows: Vec<(Rect, usize)>,
    pub(crate) theme: usize,
    pub(crate) show_help: bool,
    pub(crate) git_commits: Vec<Commit>,
    pub(crate) git_status: String,
    pub(crate) click_targets: Vec<(Rect, ClickAction)>,
    pub(crate) context_menu: Option<(usize, u16, u16)>,
    pub(crate) last_row_click: Option<(usize, Instant)>,
    pub(crate) delete_confirmation: Option<usize>,
    pub(crate) clockify: Option<ClockifyClient>,
    pub(crate) clockify_sender: UnboundedSender<SyncResult>,
}

#[derive(Clone, Copy)]
pub(crate) enum ClickAction {
    PreviousDay,
    NextDay,
    NewTask,
    ToggleArchived,
    NextTheme,
    Help,
    Quit,
    SelectTask(usize),
    ActivateTask(usize),
    RenameTask(usize),
    ToggleTaskArchive(usize),
    DeleteTask(usize),
    DeleteConfirm(usize),
    DeleteCancel,
    SaveInput,
    CancelInput,
    CloseHelp,
    CloseContext,
}

impl Ui {
    pub(crate) fn handle_mouse(&mut self, button: MouseButton, point: Position) -> bool {
        if button == MouseButton::Right {
            if let Some((_, task_index)) =
                self.task_rows.iter().find(|(rect, _)| rect.contains(point))
            {
                self.selected = *task_index;
                self.context_menu = Some((*task_index, point.x, point.y));
                return false;
            }
            self.context_menu = None;
            return false;
        }
        if button != MouseButton::Left {
            return false;
        }
        if let Some((_, action)) = self
            .click_targets
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(point))
        {
            return self.perform_click(*action);
        }
        self.context_menu = None;
        false
    }

    fn perform_click(&mut self, action: ClickAction) -> bool {
        if !matches!(
            action,
            ClickAction::RenameTask(_)
                | ClickAction::ToggleTaskArchive(_)
                | ClickAction::DeleteTask(_)
                | ClickAction::DeleteConfirm(_)
                | ClickAction::DeleteCancel
                | ClickAction::CloseContext
        ) {
            self.context_menu = None;
        }
        match action {
            ClickAction::PreviousDay => {
                self.day = self.day.pred_opt().unwrap_or(self.day);
                self.refresh_git();
            }
            ClickAction::NextDay => {
                self.day = self.day.succ_opt().unwrap_or(self.day);
                self.refresh_git();
            }
            ClickAction::NewTask => self.input = Some(("New task".into(), String::new())),
            ClickAction::ToggleArchived => {
                self.show_archived = !self.show_archived;
            }
            ClickAction::NextTheme => {
                self.theme = (self.theme + 1) % 5;
                self.save_theme();
                self.status = format!("Theme: {}", palette_name(self.theme));
            }
            ClickAction::Help => self.show_help = true,
            ClickAction::Quit => return true,
            ClickAction::SelectTask(index) => {
                self.selected = index;
                let now = Instant::now();
                if self.last_row_click.is_some_and(|(previous, at)| {
                    previous == index && now.duration_since(at) <= Duration::from_millis(450)
                }) {
                    self.input = Some(("Rename task".into(), self.data.tasks[index].name.clone()));
                    self.last_row_click = None;
                } else {
                    self.last_row_click = Some((index, now));
                }
            }
            ClickAction::ActivateTask(index) => self.activate(index),
            ClickAction::RenameTask(index) => {
                self.selected = index;
                self.context_menu = None;
                self.input = Some(("Rename task".into(), self.data.tasks[index].name.clone()))
            }
            ClickAction::ToggleTaskArchive(index) => {
                self.data.tasks[index].archived = !self.data.tasks[index].archived;
                self.persist();
                self.context_menu = None;
            }
            ClickAction::DeleteTask(index) => {
                self.delete_confirmation = Some(index);
                self.context_menu = None;
            }
            ClickAction::DeleteConfirm(index) => {
                if self.data.running().is_some_and(|running| {
                    self.data.sessions[running].task_id == self.data.tasks[index].id
                }) {
                    self.status = "Stop the task before deleting it".into();
                } else {
                    let task_id = self.data.tasks[index].id.clone();
                    self.data
                        .sessions
                        .retain(|session| session.task_id != task_id);
                    self.data.clockify_sync.retain(|session_id, _| {
                        self.data.sessions.iter().any(|s| &s.id == session_id)
                    });
                    self.data.tasks.remove(index);
                    self.selected = self.selected.min(self.data.tasks.len().saturating_sub(1));
                    self.persist();
                    self.status = "Task and its recorded sessions deleted".into();
                }
                self.delete_confirmation = None;
            }
            ClickAction::DeleteCancel => self.delete_confirmation = None,
            ClickAction::SaveInput => self.save_input(),
            ClickAction::CancelInput => {
                self.input = None;
                self.status = "Cancelled".into();
            }
            ClickAction::CloseHelp => self.show_help = false,
            ClickAction::CloseContext => self.context_menu = None,
        }
        false
    }

    fn save_input(&mut self) {
        let Some((prompt, value)) = self.input.take() else {
            return;
        };
        if value.trim().is_empty() {
            self.status = "Name cannot be empty".into();
            return;
        }
        match prompt.as_str() {
            "New task" => {
                self.data.tasks.push(Task {
                    id: Uuid::new_v4().simple().to_string(),
                    name: value.trim().into(),
                    short_id: String::new(),
                    archived: false,
                });
                self.selected = self.data.tasks.len() - 1;
                self.persist();
                self.status = "Task created".into();
            }
            "Rename task" if self.selected < self.data.tasks.len() => {
                self.data.tasks[self.selected].name = value.trim().into();
                self.persist();
                self.status = "Task renamed".into();
            }
            _ => self.status = "Unknown action".into(),
        }
    }

    pub(crate) fn step_selection(&mut self, direction: isize) {
        let tasks = self.tasks();
        if tasks.is_empty() {
            return;
        }
        let current = tasks
            .iter()
            .position(|(index, _)| *index == self.selected)
            .unwrap_or(0) as isize;
        let next = (current + direction).clamp(0, tasks.len() as isize - 1) as usize;
        self.selected = tasks[next].0;
    }

    pub(crate) fn load_theme(root: &Path) -> usize {
        fs::read_to_string(root.join("theme.txt"))
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|value| *value < 5)
            .unwrap_or(4)
    }

    pub(crate) fn refresh_git(&mut self) {
        match collect_commits(&self.data.settings, self.day) {
            Ok(commits) => {
                self.git_status = if commits.is_empty() {
                    "No commits found for the configured author today".into()
                } else {
                    format!("{} commit(s) by the configured author today", commits.len())
                };
                self.git_commits = commits;
            }
            Err(error) => {
                self.git_status = format!("Git unavailable: {error:#}");
                self.git_commits.clear();
            }
        }
    }

    fn save_theme(&mut self) {
        if let Err(error) = fs::write(self.root.join("theme.txt"), format!("{}\n", self.theme)) {
            self.status = format!("Could not save theme: {error}");
        }
    }

    pub(crate) fn tasks(&self) -> Vec<(usize, Task)> {
        self.data
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| self.show_archived || !t.archived)
            .map(|(index, task)| (index, task.clone()))
            .collect()
    }
    pub(crate) fn activate(&mut self, index: usize) {
        if index >= self.data.tasks.len() {
            return;
        }
        self.selected = index;
        let now = Local::now();
        if self.data.tasks[index].archived {
            self.data.tasks[index].archived = false;
            self.status = "Task restored".into();
            self.persist();
            return;
        }
        let mut jobs = Vec::new();
        if let Some(running) = self.data.running() {
            if self.data.sessions[running].task_id == self.data.tasks[index].id {
                self.stop_session(running, now, &mut jobs);
                if self.status.is_empty() {
                    self.status = format!("Stopped {}", self.data.tasks[index].name);
                }
            } else {
                self.stop_session(running, now, &mut jobs);
                self.start_session(index, now, &mut jobs);
                if self.status.is_empty() {
                    self.status = format!("Switched to {}", self.data.tasks[index].name);
                }
            }
        } else {
            self.start_session(index, now, &mut jobs);
            self.status = format!("Started {}", self.data.tasks[index].name);
        }
        if self.persist() {
            for job in jobs {
                self.dispatch_sync(job);
            }
        }
    }
    fn start_session(&mut self, task_index: usize, now: DateTime<Local>, jobs: &mut Vec<SyncJob>) {
        let session = new_session(&self.data.tasks[task_index], now);
        if self.clockify.is_some() {
            self.data
                .clockify_sync
                .insert(session.id.clone(), ClockifySync::PendingStart);
            jobs.push(SyncJob::start(
                session.id.clone(),
                session.started_at,
                self.data.tasks[task_index].name.clone(),
            ));
        }
        self.data.sessions.push(session);
    }

    fn stop_session(
        &mut self,
        session_index: usize,
        now: DateTime<Local>,
        jobs: &mut Vec<SyncJob>,
    ) {
        self.status.clear();
        let session_id = self.data.sessions[session_index].id.clone();
        let task_id = self.data.sessions[session_index].task_id.clone();
        let started_at = self.data.sessions[session_index].started_at;
        let task_name = self.data.task_name(&task_id).to_owned();
        self.data.sessions[session_index].stopped_at = Some(now);
        let known: HashSet<String> = self
            .data
            .sessions
            .iter()
            .flat_map(|session| session.commits.iter().map(|commit| commit.hash.clone()))
            .collect();
        match collect_commits(&self.data.settings, now.date_naive()) {
            Ok(commits) => self.data.sessions[session_index].commits.extend(
                commits
                    .into_iter()
                    .filter(|commit| !known.contains(&commit.hash)),
            ),
            Err(error) => self.status.push_str(&format!(" · Git: {error}")),
        }

        let sync = self
            .data
            .clockify_sync
            .get(&session_id)
            .cloned()
            .unwrap_or_default();
        match sync {
            ClockifySync::PendingStart if self.clockify.is_some() => {
                self.data
                    .clockify_sync
                    .insert(session_id, ClockifySync::PendingStop { entry_id: None });
            }
            ClockifySync::PendingStart => {
                self.data.clockify_sync.insert(
                    session_id,
                    ClockifySync::FailedComplete {
                        error: "CLOCKIFY_API_KEY is not set".into(),
                    },
                );
                self.status = clockify_failure_prompt("save", "CLOCKIFY_API_KEY is not set");
            }
            ClockifySync::Running { entry_id } => {
                self.data.clockify_sync.insert(
                    session_id.clone(),
                    ClockifySync::PendingStop {
                        entry_id: Some(entry_id.clone()),
                    },
                );
                if self.clockify.is_some() {
                    jobs.push(SyncJob::stop(
                        session_id, entry_id, started_at, now, task_name,
                    ));
                } else {
                    self.data.clockify_sync.insert(
                        session_id,
                        ClockifySync::FailedStop {
                            entry_id,
                            error: "CLOCKIFY_API_KEY is not set".into(),
                        },
                    );
                    self.status = clockify_failure_prompt("stop", "CLOCKIFY_API_KEY is not set");
                }
            }
            ClockifySync::LocalOnly if self.clockify.is_some() => {
                self.data
                    .clockify_sync
                    .insert(session_id.clone(), ClockifySync::PendingComplete);
                jobs.push(SyncJob::complete(session_id, started_at, now, task_name));
            }
            ClockifySync::LocalOnly => {}
            ClockifySync::FailedStart { error } => {
                if self.clockify.is_some() {
                    self.data
                        .clockify_sync
                        .insert(session_id.clone(), ClockifySync::PendingComplete);
                    jobs.push(SyncJob::complete(session_id, started_at, now, task_name));
                    self.status = format!(
                        "Clockify start failed ({error}); retrying as a completed local interval…"
                    );
                } else {
                    self.data
                        .clockify_sync
                        .insert(session_id, ClockifySync::FailedComplete { error });
                    self.status = clockify_failure_prompt("start", "CLOCKIFY_API_KEY is not set");
                }
            }
            ClockifySync::FailedStop { entry_id, error } => {
                self.data
                    .clockify_sync
                    .insert(session_id, ClockifySync::FailedStop { entry_id, error });
                self.status = clockify_failure_prompt("stop", "the locally recorded session");
            }
            ClockifySync::FailedComplete { error } => {
                self.data
                    .clockify_sync
                    .insert(session_id, ClockifySync::FailedComplete { error });
                self.status = clockify_failure_prompt("save", "the locally recorded session");
            }
            ClockifySync::PendingStop { entry_id } => {
                self.data
                    .clockify_sync
                    .insert(session_id, ClockifySync::PendingStop { entry_id });
            }
            ClockifySync::PendingComplete => {}
            ClockifySync::Synced { entry_id } => {
                self.data
                    .clockify_sync
                    .insert(session_id, ClockifySync::Synced { entry_id });
            }
        }
    }

    fn dispatch_sync(&self, job: SyncJob) {
        let Some(client) = self.clockify.clone() else {
            return;
        };
        let sender = self.clockify_sender.clone();
        tokio::spawn(async move {
            let result = client.execute(job).await;
            let _ = sender.send(result);
        });
    }

    pub(crate) fn resume_clockify_sync(&mut self) {
        let mut jobs = Vec::new();
        for session in &self.data.sessions {
            let Some(stopped_at) = session.stopped_at else {
                if let Some(ClockifySync::PendingStart) = self.data.clockify_sync.get(&session.id) {
                    jobs.push(SyncJob::start(
                        session.id.clone(),
                        session.started_at,
                        self.data.task_name(&session.task_id).to_owned(),
                    ));
                }
                continue;
            };
            let Some(sync) = self.data.clockify_sync.get(&session.id) else {
                continue;
            };
            match sync {
                ClockifySync::PendingStop {
                    entry_id: Some(entry_id),
                } => jobs.push(SyncJob::stop(
                    session.id.clone(),
                    entry_id.clone(),
                    session.started_at,
                    stopped_at,
                    self.data.task_name(&session.task_id).to_owned(),
                )),
                ClockifySync::PendingStop { entry_id: None } | ClockifySync::PendingComplete => {
                    jobs.push(SyncJob::complete(
                        session.id.clone(),
                        session.started_at,
                        stopped_at,
                        self.data.task_name(&session.task_id).to_owned(),
                    ))
                }
                _ => {}
            }
        }
        for job in jobs {
            self.dispatch_sync(job);
        }
    }

    pub(crate) fn apply_clockify_result(&mut self, result: SyncResult) {
        let Some(session_index) = self
            .data
            .sessions
            .iter()
            .position(|session| session.id == result.session_id)
        else {
            return;
        };
        let session = &self.data.sessions[session_index];
        let task_name = self.data.task_name(&session.task_id).to_owned();
        let started_at = session.started_at;
        let stopped_at = session.stopped_at;
        let mut follow_up = None;
        match (result.kind, result.error) {
            (SyncKind::Start, None) => {
                let Some(entry_id) = result.entry_id else {
                    return;
                };
                if let Some(stopped_at) = stopped_at {
                    self.data.clockify_sync.insert(
                        result.session_id.clone(),
                        ClockifySync::PendingStop {
                            entry_id: Some(entry_id.clone()),
                        },
                    );
                    follow_up = Some(SyncJob::stop(
                        result.session_id,
                        entry_id,
                        started_at,
                        stopped_at,
                        task_name,
                    ));
                    self.status = "Clockify timer started and stopped; saving its end time…".into();
                } else {
                    self.data
                        .clockify_sync
                        .insert(result.session_id, ClockifySync::Running { entry_id });
                    self.status = "Clockify timer started".into();
                }
            }
            (SyncKind::Start, Some(error)) => {
                let state = if stopped_at.is_some() {
                    follow_up = Some(SyncJob::complete(
                        result.session_id.clone(),
                        started_at,
                        stopped_at.unwrap_or_else(Local::now),
                        task_name,
                    ));
                    ClockifySync::PendingComplete
                } else {
                    ClockifySync::FailedStart {
                        error: error.clone(),
                    }
                };
                self.data.clockify_sync.insert(result.session_id, state);
                self.status = if stopped_at.is_some() {
                    format!(
                        "Clockify start failed ({error}); retrying as a completed local interval…"
                    )
                } else {
                    format!(
                        "Clockify start failed: {error}. Local timer is running; stopping will retry the complete interval. If that fails, enter the interval manually in Clockify."
                    )
                };
            }
            (SyncKind::Stop, None) => {
                if let Some(entry_id) = result.entry_id {
                    self.data
                        .clockify_sync
                        .insert(result.session_id, ClockifySync::Synced { entry_id });
                }
                self.status = "Clockify timer saved".into();
            }
            (SyncKind::Stop, Some(error)) => {
                let Some(entry_id) = result.entry_id else {
                    return;
                };
                self.data.clockify_sync.insert(
                    result.session_id,
                    ClockifySync::FailedStop {
                        entry_id,
                        error: error.clone(),
                    },
                );
                self.status = clockify_failure_prompt("stop", &error);
            }
            (SyncKind::Complete, None) => {
                if let Some(entry_id) = result.entry_id {
                    self.data
                        .clockify_sync
                        .insert(result.session_id, ClockifySync::Synced { entry_id });
                }
                self.status = "Clockify timer saved".into();
            }
            (SyncKind::Complete, Some(error)) => {
                self.data.clockify_sync.insert(
                    result.session_id,
                    ClockifySync::FailedComplete {
                        error: error.clone(),
                    },
                );
                self.status = clockify_failure_prompt("save", &error);
            }
        }
        if self.persist()
            && let Some(job) = follow_up
        {
            self.dispatch_sync(job);
        }
    }

    pub(crate) fn retry_clockify(&mut self) {
        if self.clockify.is_none() {
            self.status =
                "Clockify retry needs CLOCKIFY_API_KEY; set it in your shell and restart the app"
                    .into();
            return;
        }
        let retry_index = self.data.sessions.iter().rposition(|session| {
            matches!(
                self.data
                    .clockify_sync
                    .get(&session.id)
                    .unwrap_or(&ClockifySync::LocalOnly),
                ClockifySync::LocalOnly
                    | ClockifySync::FailedStart { .. }
                    | ClockifySync::FailedStop { .. }
                    | ClockifySync::FailedComplete { .. }
            )
        });
        let Some(index) = retry_index else {
            self.status = "No failed or local-only session is available to retry".into();
            return;
        };
        let session = &self.data.sessions[index];
        let session_id = session.id.clone();
        let started_at = session.started_at;
        let stopped_at = session.stopped_at;
        let task_name = self.data.task_name(&session.task_id).to_owned();
        let previous = self
            .data
            .clockify_sync
            .get(&session_id)
            .cloned()
            .unwrap_or_default();
        let job = match previous {
            ClockifySync::FailedStop { entry_id, .. } if stopped_at.is_some() => {
                let stopped_at = stopped_at.unwrap_or_else(Local::now);
                self.data.clockify_sync.insert(
                    session_id.clone(),
                    ClockifySync::PendingStop {
                        entry_id: Some(entry_id.clone()),
                    },
                );
                SyncJob::stop(session_id, entry_id, started_at, stopped_at, task_name)
            }
            _ if stopped_at.is_some() => {
                let stopped_at = stopped_at.unwrap_or_else(Local::now);
                self.data
                    .clockify_sync
                    .insert(session_id.clone(), ClockifySync::PendingComplete);
                SyncJob::complete(session_id, started_at, stopped_at, task_name)
            }
            _ => {
                self.data
                    .clockify_sync
                    .insert(session_id.clone(), ClockifySync::PendingStart);
                SyncJob::start(session_id, started_at, task_name)
            }
        };
        if self.persist() {
            self.status = "Retrying Clockify sync…".into();
            self.dispatch_sync(job);
        }
    }

    pub(crate) fn persist(&mut self) -> bool {
        if let Err(e) = self.data.save(&self.root) {
            self.status = format!("Save failed: {e:#}");
            false
        } else {
            true
        }
    }
}

fn clockify_failure_prompt(operation: &str, detail: &str) -> String {
    format!(
        "Clockify {operation} failed ({detail}). Local time is saved. Enter the full start-to-stop interval manually in Clockify; press c to retry sync if you prefer."
    )
}

pub(crate) fn handle_key(ui: &mut Ui, key: KeyCode) -> bool {
    if let Some((prompt, value)) = ui.input.take() {
        if key == KeyCode::Esc {
            ui.status = "Cancelled".into();
            return false;
        }
        if key == KeyCode::Enter {
            match prompt.as_str() {
                "New task" if !value.trim().is_empty() => {
                    ui.data.tasks.push(Task {
                        id: Uuid::new_v4().simple().to_string(),
                        name: value.trim().into(),
                        short_id: String::new(),
                        archived: false,
                    });
                    ui.selected = ui.data.tasks.len() - 1;
                    ui.persist();
                    ui.status = "Task created".into();
                }
                "Rename task" if !value.trim().is_empty() && ui.selected < ui.data.tasks.len() => {
                    ui.data.tasks[ui.selected].name = value.trim().into();
                    ui.persist();
                    ui.status = "Task renamed".into();
                }
                _ => ui.status = "Name cannot be empty".into(),
            }
            return false;
        }
        let mut edited = value;
        match key {
            KeyCode::Backspace => {
                edited.pop();
            }
            KeyCode::Char(ch) => edited.push(ch),
            _ => {}
        }
        ui.input = Some((prompt, edited));
        return false;
    }
    if ui.show_help {
        if matches!(key, KeyCode::Esc | KeyCode::Char('?') | KeyCode::F(1)) {
            ui.show_help = false;
        }
        return false;
    }
    match key {
        KeyCode::Char('q') => return true,
        KeyCode::Down | KeyCode::Char('j') => ui.step_selection(1),
        KeyCode::Up | KeyCode::Char('k') => ui.step_selection(-1),
        KeyCode::Enter | KeyCode::Char(' ') => ui.activate(ui.selected),
        KeyCode::Char('n') => ui.input = Some(("New task".into(), String::new())),
        KeyCode::Char('c') => ui.retry_clockify(),
        KeyCode::Char('r') if ui.selected < ui.data.tasks.len() => {
            ui.input = Some((
                "Rename task".into(),
                ui.data.tasks[ui.selected].name.clone(),
            ))
        }
        KeyCode::Char('a') if ui.selected < ui.data.tasks.len() => {
            ui.data.tasks[ui.selected].archived = !ui.data.tasks[ui.selected].archived;
            ui.persist();
            ui.status = "Archive state updated".into();
        }
        KeyCode::Char('v') => {
            ui.show_archived = !ui.show_archived;
            ui.status = if ui.show_archived {
                "Showing archived tasks"
            } else {
                "Hiding archived tasks"
            }
            .into();
        }
        KeyCode::Char(']') | KeyCode::Right => {
            ui.day = ui.day.succ_opt().unwrap_or(ui.day);
            ui.refresh_git();
        }
        KeyCode::Char('[') | KeyCode::Left => {
            ui.day = ui.day.pred_opt().unwrap_or(ui.day);
            ui.refresh_git();
        }
        KeyCode::Char('?') | KeyCode::F(1) => ui.show_help = !ui.show_help,
        KeyCode::Char('t') => {
            ui.theme = (ui.theme + 1) % 5;
            ui.save_theme();
            ui.status = format!("Theme: {}", palette_name(ui.theme));
        }
        KeyCode::Char(ch @ '1'..='5') => {
            ui.theme = (ch as usize) - ('1' as usize);
            ui.save_theme();
            ui.status = format!("Theme: {}", palette_name(ui.theme));
        }
        _ => {}
    }
    false
}

#[derive(Clone, Copy)]
struct Palette {
    background: Color,
    border: Color,
    accent: Color,
    text: Color,
    muted: Color,
    selection: Color,
    success: Color,
}

fn palette(theme: usize) -> Palette {
    match theme {
        0 => Palette {
            background: Color::Rgb(12, 17, 27),
            border: Color::Rgb(39, 53, 74),
            accent: Color::Rgb(45, 212, 191),
            text: Color::Rgb(226, 232, 240),
            muted: Color::Rgb(130, 150, 173),
            selection: Color::Rgb(29, 44, 62),
            success: Color::Rgb(74, 222, 128),
        },
        1 => Palette {
            background: Color::Rgb(15, 25, 20),
            border: Color::Rgb(48, 76, 57),
            accent: Color::Rgb(134, 239, 172),
            text: Color::Rgb(220, 252, 231),
            muted: Color::Rgb(148, 163, 150),
            selection: Color::Rgb(31, 53, 39),
            success: Color::Rgb(134, 239, 172),
        },
        2 => Palette {
            background: Color::Rgb(248, 250, 252),
            border: Color::Rgb(203, 213, 225),
            accent: Color::Rgb(2, 132, 199),
            text: Color::Rgb(15, 23, 42),
            muted: Color::Rgb(100, 116, 139),
            selection: Color::Rgb(219, 234, 254),
            success: Color::Rgb(21, 128, 61),
        },
        3 => Palette {
            background: Color::Rgb(250, 246, 237),
            border: Color::Rgb(214, 198, 170),
            accent: Color::Rgb(180, 83, 9),
            text: Color::Rgb(68, 52, 36),
            muted: Color::Rgb(120, 104, 84),
            selection: Color::Rgb(239, 226, 205),
            success: Color::Rgb(63, 98, 18),
        },
        _ => Palette {
            background: Color::Reset,
            border: Color::DarkGray,
            accent: Color::Cyan,
            text: Color::Reset,
            muted: Color::Gray,
            selection: Color::Blue,
            success: Color::Green,
        },
    }
}

fn palette_name(theme: usize) -> &'static str {
    ["Midnight", "Evergreen", "Arctic", "Sandstone", "Default"][theme.min(4)]
}

pub(crate) fn render(frame: &mut ratatui::Frame, ui: &mut Ui) {
    let area = frame.area();
    let colors = palette(ui.theme);
    ui.click_targets.clear();
    if area.width < 30 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("Resize terminal to at least 30 × 8")
                .alignment(ratatui::layout::Alignment::Center)
                .style(Style::default().fg(colors.text).bg(colors.background)),
            area,
        );
        ui.button_areas.clear();
        ui.task_rows.clear();
        return;
    }
    if area.width < 62 || area.height < 16 {
        render_compact(frame, ui, colors);
        return;
    }
    frame.render_widget(
        Block::default().style(Style::default().bg(colors.background)),
        area,
    );
    let compact = area.height < 22;
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(if compact { 1 } else { 2 })
        .constraints(if compact {
            [
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(1),
            ]
        } else {
            [
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(7),
                Constraint::Length(2),
            ]
        })
        .split(area);
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "  ◈  COMMITMENT ISSUES",
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "   ·   FOCUS, MADE VISIBLE",
            Style::default().fg(colors.muted),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(colors.border)),
    );
    frame.render_widget(title, layout[0]);
    let date_controls = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(28),
            Constraint::Length(5),
            Constraint::Min(8),
        ])
        .split(layout[1]);
    draw_button(
        frame,
        ui,
        date_controls[0],
        "‹ Prev",
        colors,
        ClickAction::PreviousDay,
        false,
    );
    let selected_day_label = if ui.day == Local::now().date_naive() {
        " TODAY "
    } else {
        " SELECTED "
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                selected_day_label,
                Style::default()
                    .bg(colors.accent)
                    .fg(colors.background)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} ", ui.day.format("%a, %b %-d, %Y")),
                Style::default()
                    .fg(colors.background)
                    .bg(colors.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        date_controls[1],
    );
    draw_button(
        frame,
        ui,
        date_controls[2],
        "Next ›",
        colors,
        ClickAction::NextDay,
        false,
    );
    frame.render_widget(
        Paragraph::new(ui.status.as_str()).style(Style::default().fg(colors.muted)),
        date_controls[3],
    );
    let split = if area.width >= 112 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(59), Constraint::Percentage(41)])
            .split(layout[2])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(layout[2])
    };
    ui.task_area = split[0];
    ui.button_areas.clear();
    ui.task_rows.clear();
    render_tasks(frame, ui, split[0], colors);
    render_report(frame, ui, split[1], colors);
    let footer_buttons = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(20); 5])
        .split(layout[3]);
    for (area, (label, action)) in footer_buttons.iter().copied().zip([
        ("＋ New task", ClickAction::NewTask),
        (
            if ui.show_archived {
                "◉ Hide archived"
            } else {
                "◯ Show archived"
            },
            ClickAction::ToggleArchived,
        ),
        ("◈ Theme", ClickAction::NextTheme),
        ("? Help", ClickAction::Help),
        ("× Quit", ClickAction::Quit),
    ]) {
        draw_button(frame, ui, area, label, colors, action, false);
    }
    if let Some((task_index, x, y)) = ui.context_menu {
        render_context_menu(frame, ui, colors, task_index, x, y);
    }
    if let Some((prompt, value)) = ui.input.clone() {
        render_input(frame, ui, &prompt, &value, colors);
    } else if let Some(index) = ui.delete_confirmation {
        render_delete_confirmation(frame, ui, colors, index);
    } else if ui.show_help {
        render_help(frame, ui, colors);
    }
}

fn render_tasks(frame: &mut ratatui::Frame, ui: &mut Ui, area: Rect, colors: Palette) {
    let tasks = ui.tasks();
    let running = ui
        .data
        .running()
        .map(|i| ui.data.sessions[i].task_id.clone());
    let header = Row::new(["TASK", "TODAY", "STATE", "ACTION"])
        .style(
            Style::default()
                .fg(colors.muted)
                .add_modifier(Modifier::BOLD),
        )
        .height(1);
    let mut rows = Vec::new();
    let visible_capacity = area.height.saturating_sub(3) as usize;
    let selected_position = tasks
        .iter()
        .position(|(index, _)| *index == ui.selected)
        .unwrap_or(0);
    let offset = selected_position.saturating_sub(visible_capacity.saturating_sub(1));
    for (index, (task_index, task)) in tasks.iter().enumerate().skip(offset).take(visible_capacity)
    {
        let elapsed = ui.data.elapsed_today(&task.id, ui.day, Local::now());
        let state = if task.archived {
            "ARCHIVED"
        } else if running.as_deref() == Some(&task.id) {
            "● RUNNING"
        } else if running.is_some() {
            "SWITCH"
        } else {
            "READY"
        };
        let action = if task.archived {
            "Restore"
        } else if running.as_deref() == Some(&task.id) {
            "Stop"
        } else if running.is_some() {
            "Switch"
        } else {
            "Start"
        };
        let style = if *task_index == ui.selected {
            Style::default()
                .fg(colors.text)
                .bg(colors.selection)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors.text)
        };
        let short = if task.short_id.is_empty() {
            &task.id[..task.id.len().min(6)]
        } else {
            &task.short_id
        };
        let row = Row::new([
            format!("{}  {}", task.name, short),
            format_duration(elapsed),
            state.into(),
            format!("[ {action} ]"),
        ])
        .style(style)
        .height(1);
        rows.push(row);
        let row_y = area.y + 2 + (index - offset) as u16;
        let action_x = area.x + 1 + area.width.saturating_sub(2) * 42 / 100 + 22;
        ui.button_areas
            .push((Rect::new(action_x, row_y, 12, 1), *task_index));
        ui.click_targets.push((
            Rect::new(area.x + 1, row_y, area.width.saturating_sub(2), 1),
            ClickAction::SelectTask(*task_index),
        ));
        ui.click_targets.push((
            Rect::new(action_x, row_y, 12, 1),
            ClickAction::ActivateTask(*task_index),
        ));
        ui.task_rows.push((
            Rect::new(area.x + 1, row_y, area.width.saturating_sub(2), 1),
            *task_index,
        ));
    }
    if rows.is_empty() {
        rows.push(
            Row::new(["No tasks yet · press n to add your first task", "", "", ""]).height(2),
        );
    }
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(42),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title("  YOUR TASKS  ")
            .title_style(
                Style::default()
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .borders(Borders::ALL)
            .border_style(Style::default().fg(colors.border)),
    )
    .row_highlight_style(Style::default());
    let mut state = TableState::default();
    if selected_position >= offset && selected_position < offset + visible_capacity {
        state.select(Some(selected_position - offset));
    }
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_report(frame: &mut ratatui::Frame, ui: &Ui, area: Rect, colors: Palette) {
    let now = Local::now();
    let total: chrono::Duration = ui
        .data
        .tasks
        .iter()
        .map(|t| ui.data.elapsed_today(&t.id, ui.day, now))
        .sum();
    let sessions = ui
        .data
        .sessions
        .iter()
        .filter(|s| {
            s.started_at.date_naive() == ui.day
                || s.stopped_at.is_some_and(|at| at.date_naive() == ui.day)
        })
        .count();
    if area.height < 12 {
        let mut compact = vec![Line::from(Span::styled(
            format!("TODAY  {} · {sessions} sessions", format_duration(total)),
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD),
        ))];
        if let Some(running) = ui.data.running().map(|i| &ui.data.sessions[i]) {
            compact.push(Line::from(format!(
                "{} · {} · {}",
                ui.data.task_name(&running.task_id),
                running.started_at.format("%H:%M:%S"),
                format_duration(now - running.started_at)
            )));
        }
        compact.push(Line::from(Span::styled(
            "GIT",
            Style::default()
                .fg(colors.muted)
                .add_modifier(Modifier::BOLD),
        )));
        compact.push(Line::from(ui.git_status.as_str()));
        for commit in ui.git_commits.iter().take(2) {
            compact.push(Line::from(format!(
                "{} {}",
                &commit.hash[..commit.hash.len().min(7)],
                commit.subject
            )));
        }
        frame.render_widget(
            Paragraph::new(compact)
                .block(
                    Block::default()
                        .title("  DAY + GIT  ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(colors.border))
                        .padding(ratatui::widgets::Padding::horizontal(1)),
                )
                .wrap(ratatui::widgets::Wrap { trim: true }),
            area,
        );
        return;
    }
    let mut lines = vec![
        Line::from(Span::styled(
            "DAILY OVERVIEW",
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format_duration(total),
            Style::default()
                .fg(colors.text)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("across {sessions} sessions"),
            Style::default().fg(colors.muted),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "TIME BY TASK",
            Style::default()
                .fg(colors.muted)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    for task in &ui.data.tasks {
        let duration = ui.data.elapsed_today(&task.id, ui.day, now);
        if duration.num_seconds() == 0
            && task.id
                != ui
                    .data
                    .running()
                    .map(|i| ui.data.sessions[i].task_id.as_str())
                    .unwrap_or("")
        {
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled("  ▪ ", Style::default().fg(colors.accent)),
            Span::styled(task.name.clone(), Style::default().fg(colors.text)),
            Span::raw("  "),
            Span::styled(
                format_duration(duration),
                Style::default().fg(colors.success),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "GIT CONTEXT",
        Style::default()
            .fg(colors.muted)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        &ui.git_status,
        Style::default().fg(colors.muted),
    )));
    for commit in ui
        .git_commits
        .iter()
        .take(if area.height < 16 { 2 } else { 5 })
    {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", &commit.hash[..commit.hash.len().min(7)]),
                Style::default().fg(colors.accent),
            ),
            Span::styled(commit.subject.clone(), Style::default().fg(colors.text)),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "Author and date match; task attribution is not inferred.",
        Style::default().fg(colors.muted),
    )));
    if let Some(running) = ui.data.running().map(|i| &ui.data.sessions[i]) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "FOCUS SESSION",
            Style::default()
                .fg(colors.success)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!(
            "{} · started {} · elapsed {}",
            ui.data.task_name(&running.task_id),
            running.started_at.format("%H:%M:%S"),
            format_duration(Local::now() - running.started_at)
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("  DAY AT A GLANCE  ")
                    .title_style(
                        Style::default()
                            .fg(colors.accent)
                            .add_modifier(Modifier::BOLD),
                    )
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(colors.border))
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            )
            .wrap(ratatui::widgets::Wrap { trim: true }),
        area,
    );
}

fn draw_button(
    frame: &mut ratatui::Frame,
    ui: &mut Ui,
    area: Rect,
    label: &str,
    colors: Palette,
    action: ClickAction,
    bordered: bool,
) {
    let widget = Paragraph::new(label)
        .alignment(ratatui::layout::Alignment::Center)
        .style(Style::default().fg(colors.accent).bg(colors.background));
    let widget = if bordered {
        widget.block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(colors.border)),
        )
    } else {
        widget
    };
    frame.render_widget(widget, area);
    ui.click_targets.push((area, action));
}

fn render_context_menu(
    frame: &mut ratatui::Frame,
    ui: &mut Ui,
    colors: Palette,
    task_index: usize,
    x: u16,
    y: u16,
) {
    let bounds = frame.area();
    let width = 24.min(bounds.width.saturating_sub(2));
    let height = 6.min(bounds.height.saturating_sub(2));
    let left = x
        .min(bounds.right().saturating_sub(width + 1))
        .max(bounds.x + 1);
    let top = y
        .min(bounds.bottom().saturating_sub(height + 1))
        .max(bounds.y + 1);
    let area = Rect::new(left, top, width, height);
    frame.render_widget(Clear, area);
    let task = &ui.data.tasks[task_index];
    let archive_label = if task.archived {
        "Restore task"
    } else {
        "Archive task"
    };
    let actions = [
        ("Rename / modify", ClickAction::RenameTask(task_index)),
        (archive_label, ClickAction::ToggleTaskArchive(task_index)),
        ("Delete task…", ClickAction::DeleteTask(task_index)),
        ("Dismiss", ClickAction::CloseContext),
    ];
    let rows: Vec<Line> = actions
        .iter()
        .map(|(label, _)| Line::from(format!("  {label}")))
        .collect();
    frame.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .title(" TASK OPTIONS ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(colors.accent))
                .style(Style::default().bg(colors.background)),
        ),
        area,
    );
    for (index, (label, action)) in actions.into_iter().enumerate() {
        let hit = Rect::new(
            area.x + 1,
            area.y + 1 + index as u16,
            area.width.saturating_sub(2),
            1,
        );
        ui.click_targets.push((hit, action));
        let _ = label;
    }
}

fn render_input(
    frame: &mut ratatui::Frame,
    ui: &mut Ui,
    prompt: &str,
    value: &str,
    colors: Palette,
) {
    let area = centered(frame.area(), 60, 20);
    frame.render_widget(Clear, area);
    let text = Paragraph::new(vec![
        Line::from(prompt),
        Line::from(""),
        Line::from(format!("› {value}_")),
        Line::from(""),
        Line::from(Span::styled(
            "Enter saves · Esc cancels",
            Style::default().fg(colors.muted),
        )),
    ])
    .block(
        Block::default()
            .title("  QUICK ACTION  ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(colors.accent))
            .style(Style::default().bg(colors.background))
            .padding(ratatui::widgets::Padding::uniform(1)),
    );
    frame.render_widget(text, area);
    let save = Rect::new(
        area.x + 2,
        area.bottom().saturating_sub(3),
        18.min(area.width.saturating_sub(4)),
        1,
    );
    let cancel = Rect::new(
        area.right().saturating_sub(20),
        area.bottom().saturating_sub(3),
        18.min(area.width.saturating_sub(4)),
        1,
    );
    draw_button(
        frame,
        ui,
        save,
        "Save",
        colors,
        ClickAction::SaveInput,
        true,
    );
    draw_button(
        frame,
        ui,
        cancel,
        "Cancel",
        colors,
        ClickAction::CancelInput,
        true,
    );
}

fn render_delete_confirmation(
    frame: &mut ratatui::Frame,
    ui: &mut Ui,
    colors: Palette,
    index: usize,
) {
    let area = centered(frame.area(), 58, 9);
    frame.render_widget(Clear, area);
    let message = format!(
        "Delete {} and its recorded sessions?",
        ui.data.tasks[index].name
    );
    frame.render_widget(
        Paragraph::new(message).block(
            Block::default()
                .title("  CONFIRM DELETE  ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(colors.accent))
                .style(Style::default().bg(colors.background))
                .padding(ratatui::widgets::Padding::uniform(1)),
        ),
        area,
    );
    let yes = Rect::new(area.x + 4, area.bottom().saturating_sub(3), 16, 1);
    let no = Rect::new(
        area.right().saturating_sub(20),
        area.bottom().saturating_sub(3),
        16,
        1,
    );
    draw_button(
        frame,
        ui,
        yes,
        "Delete",
        colors,
        ClickAction::DeleteConfirm(index),
        true,
    );
    draw_button(
        frame,
        ui,
        no,
        "Cancel",
        colors,
        ClickAction::DeleteCancel,
        true,
    );
}

fn render_help(frame: &mut ratatui::Frame, ui: &mut Ui, colors: Palette) {
    let area = centered(frame.area(), 72, 21);
    frame.render_widget(Clear, area);
    let help = vec![
        Line::from(Span::styled(
            "Quick guide",
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("↑ / ↓, j / k", Style::default().fg(colors.accent)),
            Span::raw("   Select a task"),
        ]),
        Line::from(vec![
            Span::styled("Enter / Space", Style::default().fg(colors.accent)),
            Span::raw(" Start, stop, or switch the selected task"),
        ]),
        Line::from(vec![
            Span::styled("Mouse", Style::default().fg(colors.accent)),
            Span::raw("          Click a row or its action button"),
        ]),
        Line::from(vec![
            Span::styled("n / r / a / v", Style::default().fg(colors.accent)),
            Span::raw("   New, rename, archive, show archived"),
        ]),
        Line::from(vec![
            Span::styled("← / →", Style::default().fg(colors.accent)),
            Span::raw("        Review the previous or next day"),
        ]),
        Line::from(vec![
            Span::styled("t / 1–5", Style::default().fg(colors.accent)),
            Span::raw("        Cycle themes or select one directly"),
        ]),
        Line::from(vec![
            Span::styled("c", Style::default().fg(colors.accent)),
            Span::raw("             Retry the latest local or failed Clockify session"),
        ]),
        Line::from(vec![
            Span::styled("? / F1 / Esc", Style::default().fg(colors.accent)),
            Span::raw("  Close this help"),
        ]),
        Line::from(vec![
            Span::styled("q", Style::default().fg(colors.accent)),
            Span::raw("             Quit"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(help).block(
            Block::default()
                .title("  HELP  ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(colors.accent))
                .style(Style::default().bg(colors.background))
                .padding(ratatui::widgets::Padding::uniform(1)),
        ),
        area,
    );
    ui.click_targets.push((area, ClickAction::CloseHelp));
}

fn render_compact(frame: &mut ratatui::Frame, ui: &mut Ui, colors: Palette) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    let day_label = if ui.day == Local::now().date_naive() {
        "TODAY"
    } else {
        "SELECTED"
    };
    let title = Paragraph::new(Span::styled(
        format!("‹ {day_label}: {} ›", ui.day.format("%a %b %-d")),
        Style::default()
            .fg(colors.background)
            .bg(colors.accent)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(title, chunks[0]);
    ui.click_targets.push((
        Rect::new(chunks[0].x, chunks[0].y, 1, 1),
        ClickAction::PreviousDay,
    ));
    ui.click_targets.push((
        Rect::new(chunks[0].right().saturating_sub(1), chunks[0].y, 1, 1),
        ClickAction::NextDay,
    ));
    let task = ui.data.tasks.get(ui.selected).cloned();
    let running = ui
        .data
        .running()
        .map(|index| ui.data.sessions[index].task_id.clone());
    let action = task
        .as_ref()
        .map(|task| {
            if task.archived {
                "Restore"
            } else if running.as_deref() == Some(task.id.as_str()) {
                "Stop"
            } else if running.is_some() {
                "Switch"
            } else {
                "Start"
            }
        })
        .unwrap_or("No task");
    let task_text = if let Some(task) = &task {
        let elapsed = ui.data.elapsed_today(&task.id, ui.day, Local::now());
        format!(
            "{}  ·  {}  ·  {}  ·  [ {} ]",
            task.name,
            format_duration(elapsed),
            if running.as_deref() == Some(task.id.as_str()) {
                "RUNNING"
            } else {
                "READY"
            },
            action
        )
    } else {
        "No tasks · n creates one".to_owned()
    };
    frame.render_widget(
        Paragraph::new(task_text).block(
            Block::default()
                .title(" TASK ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(colors.border))
                .style(Style::default().fg(colors.text).bg(colors.background)),
        ),
        chunks[1],
    );
    if let Some(task) = task {
        ui.task_area = chunks[1];
        ui.button_areas = vec![(
            Rect::new(
                chunks[1].x + chunks[1].width.saturating_sub(12),
                chunks[1].y + 1,
                11,
                1,
            ),
            ui.selected,
        )];
        ui.task_rows = vec![(
            Rect::new(
                chunks[1].x + 1,
                chunks[1].y + 1,
                chunks[1].width.saturating_sub(2),
                1,
            ),
            ui.selected,
        )];
        ui.click_targets
            .push((ui.task_rows[0].0, ClickAction::SelectTask(ui.selected)));
        ui.click_targets
            .push((ui.button_areas[0].0, ClickAction::ActivateTask(ui.selected)));
        drop(task);
    } else {
        ui.button_areas.clear();
        ui.task_rows.clear();
    }
    let running_line = ui
        .data
        .running()
        .map(|index| {
            let session = &ui.data.sessions[index];
            format!(
                "Started {} · {} elapsed",
                session.started_at.format("%H:%M:%S"),
                format_duration(Local::now() - session.started_at)
            )
        })
        .unwrap_or_else(|| format!("{} · {}", ui.day.format("%a %b %-d"), ui.git_status));
    frame.render_widget(
        Paragraph::new(running_line).style(Style::default().fg(colors.muted)),
        chunks[2],
    );
    let controls = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(20); 5])
        .split(chunks[3]);
    for (button, (label, action)) in controls.iter().copied().zip([
        ("＋", ClickAction::NewTask),
        ("◯", ClickAction::ToggleArchived),
        ("◈", ClickAction::NextTheme),
        ("?", ClickAction::Help),
        ("×", ClickAction::Quit),
    ]) {
        draw_button(frame, ui, button, label, colors, action, false);
    }
    if let Some((task_index, x, y)) = ui.context_menu {
        render_context_menu(frame, ui, colors, task_index, x, y);
    }
    if ui.show_help {
        render_help(frame, ui, colors);
    } else if let Some(index) = ui.delete_confirmation {
        render_delete_confirmation(frame, ui, colors, index);
    } else if let Some((prompt, value)) = ui.input.clone() {
        render_input(frame, ui, &prompt, &value, colors);
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn format_duration(duration: chrono::Duration) -> String {
    let minutes = duration.num_minutes();
    let seconds = duration.num_seconds() % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else if minutes < 60 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{}h {:02}m {:02}s", minutes / 60, minutes % 60, seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        clockify::{SyncKind, SyncResult},
        data::{Session, Settings},
    };
    use tokio::sync::mpsc;

    #[test]
    fn clockify_failure_keeps_local_time_and_retries_complete_interval_on_stop() {
        let root = std::env::temp_dir().join(format!("commitmentissues-ui-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let task = Task {
            id: "task-1".into(),
            name: "Write tests".into(),
            short_id: String::new(),
            archived: false,
        };
        let session = Session {
            id: "session-1".into(),
            task_id: task.id.clone(),
            started_at: Local::now() - chrono::Duration::minutes(5),
            stopped_at: None,
            duration_override_ns: None,
            corrected: false,
            commits: Vec::new(),
        };
        let (clockify_sender, _) = mpsc::unbounded_channel();
        let mut ui = Ui {
            data: AppData {
                tasks: vec![task],
                sessions: vec![session],
                clockify_sync: [("session-1".into(), ClockifySync::PendingStart)]
                    .into_iter()
                    .collect(),
                settings: Settings {
                    repository_path: root.display().to_string(),
                    ..Settings::default()
                },
            },
            root: root.clone(),
            selected: 0,
            day: Local::now().date_naive(),
            status: String::new(),
            show_archived: false,
            input: None,
            task_area: Rect::default(),
            button_areas: Vec::new(),
            task_rows: Vec::new(),
            theme: 4,
            show_help: false,
            git_commits: Vec::new(),
            git_status: String::new(),
            click_targets: Vec::new(),
            context_menu: None,
            last_row_click: None,
            delete_confirmation: None,
            clockify: None,
            clockify_sender,
        };

        ui.apply_clockify_result(SyncResult {
            session_id: "session-1".into(),
            kind: SyncKind::Start,
            entry_id: None,
            error: Some("Clockify returned HTTP 401".into()),
        });
        assert!(ui.data.running().is_some());
        assert!(ui.status.contains("timer is running"));

        ui.activate(0);
        assert!(ui.data.running().is_none());
        assert!(ui.data.sessions[0].stopped_at.is_some());
        assert!(matches!(
            ui.data.clockify_sync.get("session-1"),
            Some(ClockifySync::FailedComplete { .. })
        ));
        assert!(ui.status.contains("press c to retry"));

        let next_session = new_session(
            &ui.data.tasks[0],
            Local::now() - chrono::Duration::minutes(2),
        );
        let next_session_id = next_session.id.clone();
        ui.data.sessions.push(next_session);
        ui.data.clockify_sync.insert(
            next_session_id.clone(),
            ClockifySync::FailedStart {
                error: "HTTP 503".into(),
            },
        );
        ui.clockify = Some(ClockifyClient::for_test(
            "test-secret",
            "http://127.0.0.1:1/api/v1".into(),
        ));
        let stopped_at = Local::now();
        let mut jobs = Vec::new();
        ui.stop_session(1, stopped_at, &mut jobs);
        assert!(matches!(
            ui.data.clockify_sync.get(&next_session_id),
            Some(ClockifySync::PendingComplete)
        ));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, SyncKind::Complete);
        assert_eq!(jobs[0].stopped_at, Some(stopped_at));
        let _ = fs::remove_dir_all(root);
    }
}
