# Commitment Issues (Rust)

A local terminal time tracker with daily task totals and Git commit context.

## Run

```sh
cargo run --release
# Optional data directory (defaults to ~/commitmentissues)
cargo run -- --data-dir /path/to/data
```

The app uses Ratatui, Crossterm, and Tokio. Click a task's **Start**, **Stop**, or **Switch** action, or use `↑`/`↓` and `Enter`. Press `n` to add a task, `r` to rename, `a` to archive or restore, `v` to show archived tasks, `[`/`]` to change the report day, and `q` to quit. `?` opens a help dialog. Press `t` to cycle through Midnight and Evergreen (dark), Arctic and Sandstone (light), and Default themes; `1`–`5` select a theme directly. The selected theme is stored in `theme.txt`.

The layout switches to stacked panels on narrower terminals and a compact view on short terminals. Task timers update every second. The Git panel shows commits for the selected local day and configured author, refreshing every 30 seconds. Git matches are day-and-author based and are not treated as task attribution.

Data is stored in `tasks.json`, `settings.json`, and `sessions.tsv` in the selected data directory. The Rust version reads and writes the task and settings document shape and session TSV columns used by the Go version. Git commits are collected from the configured repository using the configured author or local `user.name`.

## Code layout

- `src/data.rs` contains task/session data and compatible JSON/TSV persistence.
- `src/git.rs` collects commits from the configured local repository.
- `src/ui.rs` handles Ratatui rendering, keyboard actions, themes, and mouse hit areas.
- `src/app.rs` owns the Tokio terminal event loop.

## Build

```sh
cargo check
```
