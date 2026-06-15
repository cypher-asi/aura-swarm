//! Application state for the deploy TUI.

use std::collections::VecDeque;
use std::path::PathBuf;

use chrono::{DateTime, Local};

use crate::runner::RunEvent;
use crate::scripts::DeployScript;

/// Maximum number of raw output lines kept in the scrollback ring buffer.
const RAW_CAP: usize = 8000;
/// Maximum number of curated progress items retained.
const PROGRESS_CAP: usize = 4000;

/// Which top-level screen is active.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    /// Browse and select a script to run.
    Picker,
    /// A script is running.
    Running,
    /// A script finished (success or failure); output stays visible.
    Finished,
}

/// Which right-column sub-pane currently receives scroll input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Progress,
    Raw,
}

/// One curated progress line (mirrors a `_lib.sh` log call).
#[derive(Clone, Debug)]
pub struct ProgressItem {
    pub level: String,
    pub step: String,
    pub text: String,
}

/// All TUI state.
pub struct App {
    pub deploy_dir: PathBuf,
    pub scripts: Vec<DeployScript>,
    pub selected: usize,
    pub screen: Screen,

    // Argument entry on the picker.
    pub args_input: String,
    pub editing_args: bool,

    // Active / last run.
    pub running_script: Option<String>,
    pub progress: Vec<ProgressItem>,
    pub raw: VecDeque<String>,
    pub progress_scroll: usize, // lines scrolled up from the bottom
    pub raw_scroll: usize,      // lines scrolled up from the bottom
    pub focus: Focus,
    pub started_at: Option<DateTime<Local>>,
    pub finished_at: Option<DateTime<Local>>,
    pub exit_code: Option<i32>,

    // Live infra-state snapshot.
    pub watch_cmd: String,
    pub snapshot: String,
    pub snapshot_at: Option<DateTime<Local>>,

    pub spinner_tick: usize,
    pub error: Option<String>,
    pub should_quit: bool,
    /// Set by input handling to ask `main` to spawn the selected script.
    pub request_run: bool,
}

impl App {
    pub fn new(deploy_dir: PathBuf, scripts: Vec<DeployScript>, watch_cmd: String) -> Self {
        Self {
            deploy_dir,
            scripts,
            selected: 0,
            screen: Screen::Picker,
            args_input: String::new(),
            editing_args: false,
            running_script: None,
            progress: Vec::new(),
            raw: VecDeque::new(),
            progress_scroll: 0,
            raw_scroll: 0,
            focus: Focus::Raw,
            started_at: None,
            finished_at: None,
            exit_code: None,
            watch_cmd,
            snapshot: String::new(),
            snapshot_at: None,
            spinner_tick: 0,
            error: None,
            should_quit: false,
            request_run: false,
        }
    }

    pub fn selected_script(&self) -> Option<&DeployScript> {
        self.scripts.get(self.selected)
    }

    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn select_next(&mut self) {
        if self.selected + 1 < self.scripts.len() {
            self.selected += 1;
        }
    }

    /// Reset per-run state and mark a run as started.
    pub fn begin_run(&mut self, script_name: String) {
        self.running_script = Some(script_name);
        self.progress.clear();
        self.raw.clear();
        self.progress_scroll = 0;
        self.raw_scroll = 0;
        self.started_at = Some(Local::now());
        self.finished_at = None;
        self.exit_code = None;
        self.error = None;
        self.screen = Screen::Running;
        self.focus = Focus::Raw;
    }

    /// Return to the picker, discarding the finished run's view.
    pub fn back_to_picker(&mut self) {
        self.screen = Screen::Picker;
        self.running_script = None;
    }

    /// Apply a streamed run event to the state.
    pub fn apply_event(&mut self, event: RunEvent) {
        match event {
            RunEvent::Progress { level, step, text } => {
                self.progress.push(ProgressItem { level, step, text });
                if self.progress.len() > PROGRESS_CAP {
                    self.progress.remove(0);
                }
            }
            RunEvent::Raw(line) => {
                self.raw.push_back(line);
                while self.raw.len() > RAW_CAP {
                    self.raw.pop_front();
                }
            }
            RunEvent::Watch(cmd) => {
                if !cmd.trim().is_empty() {
                    self.watch_cmd = cmd;
                    self.snapshot.clear();
                    self.snapshot_at = None;
                }
            }
            RunEvent::Exited(code) => {
                self.exit_code = code;
                self.finished_at = Some(Local::now());
                self.screen = Screen::Finished;
            }
        }
    }

    pub fn set_snapshot(&mut self, text: String) {
        self.snapshot = text;
        self.snapshot_at = Some(Local::now());
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Progress => Focus::Raw,
            Focus::Raw => Focus::Progress,
        };
    }

    pub fn scroll_up(&mut self, n: usize) {
        match self.focus {
            Focus::Progress => self.progress_scroll = self.progress_scroll.saturating_add(n),
            Focus::Raw => self.raw_scroll = self.raw_scroll.saturating_add(n),
        }
    }

    pub fn scroll_down(&mut self, n: usize) {
        match self.focus {
            Focus::Progress => self.progress_scroll = self.progress_scroll.saturating_sub(n),
            Focus::Raw => self.raw_scroll = self.raw_scroll.saturating_sub(n),
        }
    }

    pub fn jump_to_tail(&mut self) {
        match self.focus {
            Focus::Progress => self.progress_scroll = 0,
            Focus::Raw => self.raw_scroll = 0,
        }
    }

    /// Human-readable elapsed time for the active/last run.
    pub fn elapsed(&self) -> String {
        let Some(start) = self.started_at else {
            return String::new();
        };
        let end = self.finished_at.unwrap_or_else(Local::now);
        let secs = (end - start).num_seconds().max(0);
        let (m, s) = (secs / 60, secs % 60);
        if m > 0 {
            format!("{m}m{s:02}s")
        } else {
            format!("{s}s")
        }
    }
}
