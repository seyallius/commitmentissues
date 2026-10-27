use crate::{
    clockify::ClockifyClient,
    data::AppData,
    ui::{Ui, handle_key, render},
};
use anyhow::{Context, Result, bail};
use chrono::Local;
use crossterm::{
    event::{Event, EventStream, KeyEventKind, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Position, Rect},
};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio::time::{Instant, interval, interval_at};

pub(crate) async fn run() -> Result<()> {
    let root = parse_data_dir()?;
    let data = AppData::load(&root)?;
    run_ui(data, root).await
}

fn parse_data_dir() -> Result<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(index) = args
        .iter()
        .position(|arg| arg == "--data-dir" || arg == "-data-dir")
    {
        return args
            .get(index + 1)
            .map(PathBuf::from)
            .context("--data-dir requires a path");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("HOME or USERPROFILE is not set")?;
    default_data_dir(&home)
}

fn default_data_dir(home: &Path) -> Result<PathBuf> {
    let current = home.join(".commitmentissues");
    let legacy = home.join("commitmentissues");
    match (current.exists(), legacy.exists()) {
        (false, true) => fs::rename(&legacy, &current).with_context(|| {
            format!(
                "move existing data directory {} to {}",
                legacy.display(),
                current.display()
            )
        })?,
        (true, true) => bail!(
            "both {} and {} exist; choose a data directory explicitly with --data-dir",
            current.display(),
            legacy.display()
        ),
        _ => {}
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::default_data_dir;
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    fn temp_home() -> PathBuf {
        std::env::temp_dir().join(format!("commitmentissues-home-{}", Uuid::new_v4()))
    }

    #[test]
    fn migrates_legacy_directory_when_hidden_directory_is_absent() {
        let home = temp_home();
        let legacy = home.join("commitmentissues");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("tasks.json"), "[]").unwrap();

        let result = default_data_dir(&home).unwrap();

        assert_eq!(result, home.join(".commitmentissues"));
        assert!(!legacy.exists());
        assert_eq!(fs::read_to_string(result.join("tasks.json")).unwrap(), "[]");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn refuses_to_choose_between_existing_directories() {
        let home = temp_home();
        let current = home.join(".commitmentissues");
        let legacy = home.join("commitmentissues");
        fs::create_dir_all(&current).unwrap();
        fs::create_dir_all(&legacy).unwrap();
        fs::write(current.join("tasks.json"), "current").unwrap();
        fs::write(legacy.join("tasks.json"), "legacy").unwrap();

        assert!(default_data_dir(&home).is_err());
        assert_eq!(
            fs::read_to_string(current.join("tasks.json")).unwrap(),
            "current"
        );
        assert_eq!(
            fs::read_to_string(legacy.join("tasks.json")).unwrap(),
            "legacy"
        );
        fs::remove_dir_all(home).unwrap();
    }
}

async fn run_ui(data: AppData, root: PathBuf) -> Result<()> {
    let theme = Ui::load_theme(&root);
    let (clockify, clockify_status) = match ClockifyClient::from_config(&root.join("config.toml")) {
        Ok(Some(client)) => (
            Some(client),
            "Ready · Clockify configured; local records remain authoritative".to_owned(),
        ),
        Ok(None) => (
            None,
            "Ready · local tracking; add [clockify].api_key to config.toml to enable Clockify"
                .to_owned(),
        ),
        Err(error) => (
            None,
            format!("Clockify unavailable; local tracking continues: {error:#}"),
        ),
    };
    let (clockify_sender, mut clockify_results) = mpsc::unbounded_channel();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut ui = Ui {
        data,
        root,
        selected: 0,
        day: Local::now().date_naive(),
        status: clockify_status,
        show_archived: false,
        input: None,
        task_area: Rect::default(),
        button_areas: Vec::new(),
        task_rows: Vec::new(),
        theme,
        show_help: false,
        git_commits: Vec::new(),
        git_status: "Loading Git context…".into(),
        click_targets: Vec::new(),
        context_menu: None,
        last_row_click: None,
        delete_confirmation: None,
        clockify,
        clockify_sender,
        clockify_projects: Vec::new(),
        clockify_tasks: Vec::new(),
        clockify_menu: false,
        clockify_task_mode: false,
        clockify_loading: false,
    };
    ui.refresh_git();
    ui.resume_clockify_sync();
    let mut events = EventStream::new();
    let mut ticker = interval(Duration::from_secs(1));
    let mut git_refresh = interval_at(
        Instant::now() + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    let result = loop {
        terminal.draw(|frame| render(frame, &mut ui))?;
        tokio::select! {
            _ = ticker.tick() => {},
            _ = git_refresh.tick() => ui.refresh_git(),
            Some(result) = clockify_results.recv() => ui.apply_clockify_result(result),
            maybe_event = events.next() => match maybe_event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => { if handle_key(&mut ui, key.code) { break Ok(()); } },
                Some(Ok(Event::Mouse(mouse))) => match mouse.kind {
                    MouseEventKind::Down(button) => {
                        if ui.handle_mouse(button, Position::new(mouse.column, mouse.row)) { break Ok(()); }
                    },
                    MouseEventKind::ScrollDown => ui.step_selection(1),
                    MouseEventKind::ScrollUp => ui.step_selection(-1),
                    _ => {}
                },
                Some(Ok(_)) => {}, Some(Err(error)) => { ui.status = format!("Terminal event error: {error}"); }, None => break Ok(()),
            }
        }
    };
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    ui.data.save(&ui.root)?;
    result
}
