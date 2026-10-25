use crate::{
    data::{AppData, Commit, Task},
    git::{collect_commits, new_session},
};
use chrono::{Local, NaiveDate};
use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState},
};
use std::{collections::HashSet, fs, path::PathBuf};
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
}

impl Ui {
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

    pub(crate) fn load_theme(root: &PathBuf) -> usize {
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
            return;
        }
        if let Some(running) = self.data.running() {
            if self.data.sessions[running].task_id == self.data.tasks[index].id {
                self.data.sessions[running].stopped_at = Some(now);
                self.status = format!("Stopped {}", self.data.tasks[index].name);
                let known: HashSet<String> = self
                    .data
                    .sessions
                    .iter()
                    .flat_map(|s| s.commits.iter().map(|c| c.hash.clone()))
                    .collect();
                match collect_commits(&self.data.settings, now.date_naive()) {
                    Ok(commits) => {
                        let fresh: Vec<_> = commits
                            .into_iter()
                            .filter(|c| !known.contains(&c.hash))
                            .collect();
                        self.data.sessions[running].commits.extend(fresh);
                    }
                    Err(error) => self.status.push_str(&format!(" · Git: {error}")),
                }
            } else {
                self.data.sessions[running].stopped_at = Some(now);
                let known: HashSet<String> = self
                    .data
                    .sessions
                    .iter()
                    .flat_map(|session| session.commits.iter().map(|commit| commit.hash.clone()))
                    .collect();
                if let Ok(commits) = collect_commits(&self.data.settings, now.date_naive()) {
                    self.data.sessions[running].commits.extend(
                        commits
                            .into_iter()
                            .filter(|commit| !known.contains(&commit.hash)),
                    );
                }
                self.data
                    .sessions
                    .push(new_session(&self.data.tasks[index], now));
                self.status = format!("Switched to {}", self.data.tasks[index].name);
            }
        } else {
            self.data
                .sessions
                .push(new_session(&self.data.tasks[index], now));
            self.status = format!("Started {}", self.data.tasks[index].name);
        }
        self.persist();
    }
    pub(crate) fn persist(&mut self) {
        if let Err(e) = self.data.save(&self.root) {
            self.status = format!("Save failed: {e:#}");
        }
    }
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
    let status = Paragraph::new(Line::from(vec![
        Span::styled(
            " TODAY  ",
            Style::default()
                .bg(colors.accent)
                .fg(colors.background)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", ui.day.format("%A, %B %-d, %Y")),
            Style::default().fg(colors.text),
        ),
        Span::styled(
            format!("          {}", ui.status),
            Style::default().fg(colors.muted),
        ),
    ]));
    frame.render_widget(status, layout[1]);
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
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            " n ",
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("new   "),
        Span::styled("Enter", Style::default().fg(colors.accent)),
        Span::raw(" start/stop   "),
        Span::styled("← →", Style::default().fg(colors.accent)),
        Span::raw(" day   "),
        Span::styled(
            format!("t {}", palette_name(ui.theme)),
            Style::default().fg(colors.accent),
        ),
        Span::raw(" themes   "),
        Span::styled("?", Style::default().fg(colors.accent)),
        Span::raw(" help   "),
        Span::styled("q", Style::default().fg(colors.accent)),
        Span::raw(" quit   ·   Mouse enabled"),
    ]))
    .style(Style::default().fg(colors.muted));
    frame.render_widget(footer, layout[3]);
    if let Some((prompt, value)) = &ui.input {
        render_input(frame, prompt, value, colors);
    } else if ui.show_help {
        render_help(frame, colors);
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
        ui.button_areas.push((
            Rect::new(area.x + area.width.saturating_sub(13), row_y, 12, 1),
            *task_index,
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

fn render_input(frame: &mut ratatui::Frame, prompt: &str, value: &str, colors: Palette) {
    let area = centered(frame.area(), 60, 20);
    frame.render_widget(Clear, area);
    let text = Paragraph::new(vec![
        Line::from(prompt),
        Line::from(""),
        Line::from(format!("› {value}_")),
        Line::from(""),
        Line::from(Span::styled(
            "Enter save · Esc cancel",
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
}

fn render_help(frame: &mut ratatui::Frame, colors: Palette) {
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
    let title = Paragraph::new(Span::styled(
        "◈ COMMITMENT ISSUES",
        Style::default()
            .fg(colors.accent)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(title, chunks[0]);
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
    frame.render_widget(
        Paragraph::new("↑↓ select · Enter start/stop · ? help · q quit")
            .style(Style::default().fg(colors.accent)),
        chunks[3],
    );
    if ui.show_help {
        render_help(frame, colors);
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
