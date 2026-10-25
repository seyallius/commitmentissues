use crate::{
    data::AppData,
    ui::{Ui, handle_key, render},
};
use anyhow::{Context, Result};
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
use std::{io, path::PathBuf, time::Duration};
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
    Ok(PathBuf::from(std::env::var("HOME").context("HOME is not set")?).join("commitmentissues"))
}

async fn run_ui(data: AppData, root: PathBuf) -> Result<()> {
    let theme = Ui::load_theme(&root);
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
        status: "Ready · click a task action or press ? for help".into(),
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
    };
    ui.refresh_git();
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
