# Commitment Issues (Rust)

A local terminal time tracker with daily task totals and Git commit context.

## Run

```sh
cargo run --release
# Optional data directory (defaults to ~/commitmentissues)
cargo run -- --data-dir /path/to/data
```

The app uses Ratatui, Crossterm, and Tokio. Click a task row to select it, double-click its name to rename it, or click its **Start**, **Stop**, or **Switch** action. Right-click a task for rename, archive/restore, and delete options; deletion asks for confirmation and refuses to remove a running task. Click the date arrows and footer buttons to navigate days, create tasks, show archived tasks, change themes, open help, or quit. The input and confirmation dialogs have clickable buttons too. Right-click support depends on the terminal sending right-button mouse events.

Keyboard shortcuts remain available: `↑`/`↓` or `j`/`k` select a task, `Enter` starts or stops it, `n` creates a task, `r` renames, `a` archives or restores, `v` shows archived tasks, `[`/`]` changes the report day, and `q` quits. `?` opens a help dialog. Press `t` to cycle through Midnight and Evergreen (dark), Arctic and Sandstone (light), and Default themes; `1`–`5` select a theme directly. The selected theme is stored in `theme.txt`.

The layout switches to stacked panels on narrower terminals and a compact view on short terminals. Task timers update every second. The Git panel shows commits for the selected local day and configured author, refreshing every 30 seconds. Git matches are day-and-author based and are not treated as task attribution.

## Optional Clockify sync

Clockify sync is optional. In the WSL shell, set `CLOCKIFY_API_KEY` before starting the app:

```sh
export CLOCKIFY_API_KEY="your-clockify-api-key"
cargo run --release
```

The app discovers the API key's user and active workspace from Clockify. Starting a local task creates an active Clockify time entry with the task name as its description; stopping updates that entry's end time. No Clockify project is selected. The API key is read from the process environment and is never written to `settings.json`, `sessions.tsv`, or the app's sync-state file. If no key is set, the app remains a local-only tracker.

Local records are saved before requests run, and network requests do not block the TUI. Clockify failures leave local time intact. Press `c` to retry the latest failed or local-only session. A failed start is retried as the complete local interval once it has been stopped. If a retry still fails, the status message tells you to add that session's full start-to-stop interval in Clockify manually. Pending sync state and Clockify entry IDs are stored in `clockify.json`; the session TSV columns are unchanged.

Clockify's default global API endpoint is used. If the workspace is restricted to a regional endpoint, sync may fail and local tracking will continue; regional API base selection is not configurable yet.

Data is stored in `tasks.json`, `settings.json`, and `sessions.tsv` in the selected data directory. The Rust version reads and writes the task and settings document shape and session TSV columns used by the Go version. Git commits are collected from the configured repository using the configured author or local `user.name`.

## Code layout

- `src/data.rs` contains task/session data and compatible JSON/TSV persistence.
- `src/clockify.rs` sends optional Clockify time-entry requests and reports results to the local-first UI.
- `src/git.rs` collects commits from the configured local repository.
- `src/ui.rs` handles Ratatui rendering, keyboard actions, themes, and mouse hit areas.
- `src/app.rs` owns the Tokio terminal event loop.

## Build

```sh
cargo check
```
