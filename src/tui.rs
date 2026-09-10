use crate::cli::Options;
use crate::model::{Repository, State, Worktree};
use crate::{config, discover, git, terminal, ui};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

pub enum RunResult {
    Complete {
        outcome: ui::Outcome,
        scan_failed: bool,
    },
    Canceled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GroupKind {
    Chop,
    Review,
    Keep,
}

impl GroupKind {
    fn title(self) -> &'static str {
        match self {
            Self::Chop => "CHOP",
            Self::Review => "REVIEW",
            Self::Keep => "KEEP",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Chop => Color::Green,
            Self::Review => Color::Yellow,
            Self::Keep => Color::Cyan,
        }
    }

    fn description(self, dry_run: bool) -> &'static str {
        match self {
            Self::Chop if dry_run => "Clean linked worktrees. Chop would remove them.",
            Self::Chop => "Clean worktrees and Review items you chose to chop.",
            Self::Review => "These worktrees need your decision. Keep is the default.",
            Self::Keep => "Bare and current worktrees. Chop never removes them.",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlannedAction {
    Pending,
    Keep,
    Preserve,
    ForceChop,
}

impl PlannedAction {
    fn label(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Keep => "KEEP",
            Self::Preserve => "STASH + CHOP",
            Self::ForceChop => "CHOP ANYWAY",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Pending => Color::Yellow,
            Self::Keep => Color::Cyan,
            Self::Preserve => Color::Green,
            Self::ForceChop => Color::Red,
        }
    }
}

#[derive(Debug)]
struct Entry {
    repository: String,
    reference: String,
    path: String,
    reasons: Vec<String>,
    worktree: Worktree,
    action: PlannedAction,
}

#[derive(Debug)]
struct Group {
    kind: GroupKind,
    expanded: bool,
    entries: Vec<Entry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Row {
    Group(usize),
    Entry(usize, usize),
}

enum Overlay {
    Status {
        title: String,
        body: Vec<String>,
        scroll: u16,
    },
    Chop {
        group: usize,
        entry: usize,
        input: String,
        error: Option<String>,
        scroll: u16,
    },
    Confirm {
        input: String,
        error: Option<String>,
        scroll: u16,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum PlanDecision {
    Execute,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanelFocus {
    Worktrees,
    Details,
}

struct App {
    groups: Vec<Group>,
    selected: usize,
    detail_scroll: u16,
    dry_run: bool,
    accept_data_loss: bool,
    color: bool,
    notice: Option<String>,
    overlay: Option<Overlay>,
    scan_errors: Vec<String>,
    diffs: HashMap<PathBuf, Vec<String>>,
    diff_task: Option<(PathBuf, mpsc::Receiver<Result<String, String>>)>,
    focus: PanelFocus,
    list_area: Cell<Rect>,
    details_area: Cell<Rect>,
    list_offset: Cell<usize>,
}

struct RootEditor {
    roots: Vec<PathBuf>,
    selected: usize,
    input: String,
    message: Option<String>,
    current_dir: PathBuf,
    home: Option<PathBuf>,
    completions: Vec<String>,
    completion_selected: usize,
    editing_path: bool,
}

enum EditResult {
    Save(Vec<PathBuf>),
    Cancel,
}

#[derive(Clone, Copy)]
enum ResultKind {
    Good,
    Info,
    Bad,
}

struct ResultLine {
    kind: ResultKind,
    text: String,
}

struct BracketedPasteGuard;

struct MouseCaptureGuard;

impl MouseCaptureGuard {
    fn enable() -> std::io::Result<Self> {
        execute!(std::io::stdout(), EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
}

impl BracketedPasteGuard {
    fn enable() -> std::io::Result<Self> {
        execute!(std::io::stdout(), EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for BracketedPasteGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    }
}

pub fn run_all(options: &Options, current_dir: &Path) -> Result<RunResult, String> {
    let _paste = BracketedPasteGuard::enable()
        .map_err(|error| format!("Could not enable terminal paste: {error}"))?;
    let mut result = None;
    ratatui::run(|terminal| -> std::io::Result<()> {
        result = Some(run_all_session(terminal, options, current_dir));
        Ok(())
    })
    .map_err(|error| format!("Could not open chop: {error}"))?;
    result.unwrap_or_else(|| Err("Chop did not start.".to_owned()))
}

pub fn configure(current_dir: &Path) -> Result<bool, String> {
    let _paste = BracketedPasteGuard::enable()
        .map_err(|error| format!("Could not enable terminal paste: {error}"))?;
    let path = config::config_path()?;
    let current = config::read_roots(&path)?.unwrap_or_default();
    let mut result = None;
    ratatui::run(|terminal| -> std::io::Result<()> {
        result = Some(match edit_roots(terminal, current, current_dir)? {
            EditResult::Cancel => Ok(false),
            EditResult::Save(roots) => match config::write_roots(&path, &roots) {
                Ok(()) => {
                    wait_for_notice(
                        terminal,
                        "CONFIGURATION SAVED",
                        &saved_root_lines(&path, &roots),
                    )?;
                    Ok(true)
                }
                Err(error) => Err(error),
            },
        });
        Ok(())
    })
    .map_err(|error| format!("Could not open chop: {error}"))?;
    result.unwrap_or_else(|| Err("Chop did not start.".to_owned()))
}

fn run_all_session(
    terminal: &mut DefaultTerminal,
    options: &Options,
    current_dir: &Path,
) -> Result<RunResult, String> {
    let Some(roots) = resolve_roots(terminal, options, current_dir)? else {
        return Ok(RunResult::Canceled);
    };
    let scan_roots = roots.clone();
    let exhaustive = options.exhaustive;
    let Some(scan) = run_cancellable(
        move || discover::find_git_candidates(&scan_roots, exhaustive),
        |elapsed| render_scan(terminal, &roots, "Finding Git repositories", None, elapsed),
    )
    .map_err(|error| error.to_string())?
    else {
        return Ok(RunResult::Canceled);
    };
    let candidates = scan.candidates.clone();
    let inspect_current_dir = current_dir.to_path_buf();
    let inspect_roots = roots.clone();
    let Some((repositories, git_errors)) = run_cancellable(
        move || {
            git::inspect_repositories_with_options(
                &candidates,
                &inspect_current_dir,
                &inspect_roots,
                exhaustive,
            )
        },
        |elapsed| {
            render_scan(
                terminal,
                &roots,
                "Inspecting worktrees",
                Some(scan.candidates.len()),
                elapsed,
            )
        },
    )
    .map_err(|error| error.to_string())?
    else {
        return Ok(RunResult::Canceled);
    };

    let mut app = App::new(&repositories, options.dry_run, options.accept_data_loss);
    app.scan_errors.extend(
        scan.unreadable
            .iter()
            .map(|(path, error)| format!("{}: {}", terminal::path(path), terminal::text(error))),
    );
    app.scan_errors
        .extend(git_errors.iter().map(|error| terminal::text(error)));
    match app.run(terminal).map_err(|error| error.to_string())? {
        PlanDecision::Cancel => return Ok(RunResult::Canceled),
        PlanDecision::Execute => {}
    }

    let (outcome, result_lines, canceled) =
        execute_plan(terminal, &app, &scan.unreadable, &git_errors)
            .map_err(|error| error.to_string())?;
    show_results(terminal, &outcome, &result_lines, options.dry_run)
        .map_err(|error| error.to_string())?;
    if canceled {
        Ok(RunResult::Canceled)
    } else {
        Ok(RunResult::Complete {
            outcome,
            scan_failed: !scan.unreadable.is_empty() || !git_errors.is_empty(),
        })
    }
}

fn resolve_roots(
    terminal: &mut DefaultTerminal,
    options: &Options,
    current_dir: &Path,
) -> Result<Option<Vec<PathBuf>>, String> {
    if !options.roots.is_empty() {
        return config::normalize_roots(&options.roots, current_dir).map(Some);
    }
    let path = config::config_path()?;
    if let Some(roots) = config::read_roots(&path)? {
        return Ok(Some(roots));
    }

    match edit_roots(terminal, Vec::new(), current_dir).map_err(|error| error.to_string())? {
        EditResult::Cancel => Ok(None),
        EditResult::Save(roots) => {
            if !options.dry_run {
                config::write_roots(&path, &roots)?;
            }
            Ok(Some(roots))
        }
    }
}

fn edit_roots(
    terminal: &mut DefaultTerminal,
    roots: Vec<PathBuf>,
    current_dir: &Path,
) -> std::io::Result<EditResult> {
    let mut editor = RootEditor::new(roots, current_dir);
    loop {
        terminal.draw(|frame| editor.render(frame))?;
        match event::read()? {
            Event::Paste(value) if editor.editing_path => {
                editor.input.push_str(&value.replace(['\n', '\r'], ""));
                editor.refresh_completions();
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let Some(result) = editor.handle_key(key) {
                    return Ok(result);
                }
            }
            _ => {}
        }
    }
}

fn shortcut_lines(shortcuts: &[(&str, &str)], available_width: usize) -> Vec<Line<'static>> {
    let key_style = Style::default().fg(tone(Color::Rgb(198, 174, 146)));
    let action_style = Style::default().fg(tone(Color::Rgb(145, 145, 145)));
    let mut lines = vec![Line::default()];
    let mut line_width = 0;
    for (key, action) in shortcuts {
        let width = key.chars().count() + action.len() + 3;
        if line_width > 0 && line_width + 3 + width > available_width {
            lines.push(Line::default());
            line_width = 0;
        }
        let line = lines.last_mut().unwrap();
        if line_width > 0 {
            line.spans.push(Span::raw("   "));
            line_width += 3;
        }
        line.spans.push(Span::styled(format!("[{key}]"), key_style));
        line.spans
            .push(Span::styled(format!(" {action}"), action_style));
        line_width += width;
    }
    lines
}

fn complete_folder_path(
    input: &str,
    current_dir: &Path,
    home: Option<&Path>,
) -> Result<Vec<String>, String> {
    let input = if input == "~" { "~/" } else { input };
    let (prefix, fragment) = match input.rfind('/') {
        Some(index) => (&input[..=index], &input[index + 1..]),
        None => ("", input),
    };
    let parent = config::expand_home(Path::new(prefix), home);
    let parent = if parent.is_absolute() {
        parent
    } else {
        current_dir.join(parent)
    };
    let entries = std::fs::read_dir(&parent)
        .map_err(|error| format!("Could not read {}: {error}", terminal::path(&parent)))?;
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("Could not read folder: {error}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(fragment) && entry.path().is_dir() {
            matches.push(format!("{prefix}{name}/"));
        }
    }
    matches.sort();
    Ok(matches)
}

impl RootEditor {
    fn new(roots: Vec<PathBuf>, current_dir: &Path) -> Self {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let mut editor = Self {
            roots,
            selected: 0,
            input: if home.is_some() { "~/" } else { "./" }.to_owned(),
            message: None,
            current_dir: current_dir.to_path_buf(),
            home,
            completions: Vec::new(),
            completion_selected: 0,
            editing_path: true,
        };
        editor.refresh_completions();
        editor
    }

    fn refresh_completions(&mut self) {
        self.completion_selected = 0;
        self.message = None;
        match complete_folder_path(&self.input, &self.current_dir, self.home.as_deref()) {
            Ok(matches) => self.completions = matches,
            Err(error) => {
                self.completions.clear();
                self.message = Some(error);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<EditResult> {
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Some(EditResult::Cancel);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => match config::normalize_roots(&self.roots, &self.current_dir)
                {
                    Ok(roots) => return Some(EditResult::Save(roots)),
                    Err(error) => self.message = Some(error),
                },
                KeyCode::Char('r') => self.editing_path = !self.editing_path,
                KeyCode::Char('u') if self.editing_path => {
                    self.input.clear();
                    self.refresh_completions();
                }
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Tab if self.editing_path => {
                if let Some(value) = self.completions.get(self.completion_selected) {
                    self.input = value.clone();
                    self.refresh_completions();
                }
            }
            KeyCode::Up if self.editing_path => {
                self.completion_selected = self.completion_selected.saturating_sub(1);
            }
            KeyCode::Down if self.editing_path => {
                self.completion_selected =
                    (self.completion_selected + 1).min(self.completions.len().saturating_sub(1));
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.roots.len().saturating_sub(1));
            }
            KeyCode::Delete if !self.editing_path && !self.roots.is_empty() => {
                self.roots.remove(self.selected);
                self.selected = self.selected.min(self.roots.len().saturating_sub(1));
            }
            KeyCode::Enter if self.editing_path => {
                if self.input.is_empty() {
                    self.message = Some("Enter a folder path.".to_owned());
                } else {
                    match config::normalize_roots(&[PathBuf::from(&self.input)], &self.current_dir)
                    {
                        Ok(roots) => {
                            let root = &roots[0];
                            if !self.roots.contains(root) {
                                self.roots.push(root.clone());
                            }
                            self.selected = self
                                .roots
                                .iter()
                                .position(|value| value == root)
                                .unwrap_or(0);
                            self.input = if self.home.is_some() { "~/" } else { "./" }.to_owned();
                            self.refresh_completions();
                        }
                        Err(error) => self.message = Some(error),
                    }
                }
            }
            KeyCode::Backspace if self.editing_path => {
                self.input.pop();
                self.refresh_completions();
            }
            KeyCode::Char(character)
                if self.editing_path && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.push(character);
                self.refresh_completions();
            }
            _ => {}
        }
        None
    }

    fn render(&self, frame: &mut Frame) {
        let shortcuts = shortcut_lines(
            &[
                ("↑/↓", "select"),
                ("Tab", "complete"),
                ("Enter", "add"),
                ("Ctrl-U", "clear path"),
                ("Ctrl-R", "path/roots"),
                ("Delete", "remove root"),
                ("Ctrl-S", "save"),
                ("Esc", "cancel"),
            ],
            usize::from(frame.area().width),
        );
        let body = Layout::vertical([
            Constraint::Length(9),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(shortcuts.len() as u16),
        ])
        .split(frame.area());
        let mut intro = brand_header("FOLDERS");
        intro.extend([
            Line::from("Find and remove linked Git worktrees."),
            Line::from("Choose the folders that contain your Git repositories."),
        ]);
        frame.render_widget(Paragraph::new(intro), body[0]);
        let input = format!(
            "{}{}",
            visible_input(
                &terminal::text(&self.input),
                body[1].width.saturating_sub(3) as usize
            ),
            if self.editing_path { "█" } else { "" }
        );
        frame.render_widget(Paragraph::new(input).block(panel(" Folder path ")), body[1]);
        let matches: Vec<ListItem> = if self.completions.is_empty() {
            vec![ListItem::new("No matching folders")]
        } else {
            self.completions
                .iter()
                .map(|value| ListItem::new(terminal::text(value)))
                .collect()
        };
        let mut completion_state = ListState::default().with_selected(
            (self.editing_path && !self.completions.is_empty()).then_some(self.completion_selected),
        );
        frame.render_stateful_widget(
            List::new(matches)
                .block(panel(" Suggestions · Tab to complete "))
                .highlight_style(highlight()),
            body[2],
            &mut completion_state,
        );
        let items: Vec<ListItem> = if self.roots.is_empty() {
            vec![ListItem::new("No folders added yet")]
        } else {
            self.roots
                .iter()
                .map(|root| ListItem::new(terminal::path(root)))
                .collect()
        };
        let mut root_state = ListState::default()
            .with_selected((!self.editing_path && !self.roots.is_empty()).then_some(self.selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(panel(" Scan roots · Ctrl-R to select "))
                .highlight_style(highlight()),
            body[3],
            &mut root_state,
        );
        if let Some(message) = &self.message {
            frame.render_widget(
                Paragraph::new(terminal::text(message))
                    .style(Style::default().fg(tone(Color::Red))),
                body[4],
            );
        }
        frame.render_widget(Paragraph::new(shortcuts), body[5]);
    }
}

impl App {
    fn new(repositories: &[Repository], dry_run: bool, accept_data_loss: bool) -> Self {
        let mut groups = vec![
            Group {
                kind: GroupKind::Chop,
                expanded: true,
                entries: Vec::new(),
            },
            Group {
                kind: GroupKind::Review,
                expanded: true,
                entries: Vec::new(),
            },
            Group {
                kind: GroupKind::Keep,
                expanded: true,
                entries: Vec::new(),
            },
        ];
        for repository in repositories {
            let repository_name = repository_name(repository);
            for worktree in &repository.worktrees {
                if worktree.is_main && !worktree.flags.bare {
                    continue;
                }
                let (group, reasons) = classify(worktree, dry_run);
                groups[group].entries.push(Entry {
                    repository: repository_name.clone(),
                    reference: ui::worktree_reference(worktree),
                    path: terminal::path(&worktree.path),
                    reasons: reasons
                        .into_iter()
                        .map(|reason| terminal::text(&reason))
                        .collect(),
                    worktree: worktree.clone(),
                    action: PlannedAction::Pending,
                });
            }
        }
        let selected = groups
            .iter()
            .position(|group| !group.entries.is_empty())
            .map_or(0, |index| index + 1);
        Self {
            groups,
            selected,
            detail_scroll: 0,
            dry_run,
            accept_data_loss,
            color: terminal::color_enabled(),
            notice: None,
            overlay: None,
            scan_errors: Vec::new(),
            diffs: HashMap::new(),
            diff_task: None,
            focus: PanelFocus::Worktrees,
            list_area: Cell::new(Rect::default()),
            details_area: Cell::new(Rect::default()),
            list_offset: Cell::new(0),
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<PlanDecision> {
        let _mouse = MouseCaptureGuard::enable()?;
        loop {
            self.update_diff_preview();
            terminal.draw(|frame| self.render(frame))?;
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            let input = event::read()?;
            if let Event::Mouse(mouse) = input {
                self.handle_mouse(mouse);
                continue;
            }
            if let Event::Paste(value) = input {
                self.handle_paste(&value);
                continue;
            }
            let Event::Key(key) = input else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(decision) = self.handle_key(key) {
                return Ok(decision);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<PlanDecision> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(PlanDecision::Cancel);
        }
        if self.overlay.is_some() {
            return self.handle_overlay_key(key);
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(PlanDecision::Cancel),
            KeyCode::Char('c') if key.modifiers.is_empty() => return self.continue_plan(),
            KeyCode::Tab => {
                self.focus = if self.focus == PanelFocus::Worktrees {
                    PanelFocus::Details
                } else {
                    PanelFocus::Worktrees
                };
            }
            KeyCode::Down | KeyCode::Char('j') => self.scroll_focused(true, 1),
            KeyCode::Up => self.scroll_focused(false, 1),
            KeyCode::Char('1') => self.set_expanded(0, !self.groups[0].expanded),
            KeyCode::Char('2') => self.set_expanded(1, !self.groups[1].expanded),
            KeyCode::Char('3') => self.set_expanded(2, !self.groups[2].expanded),
            KeyCode::PageDown | KeyCode::Char(']') => {
                self.scroll_focused(true, 3);
            }
            KeyCode::PageUp | KeyCode::Char('[') => {
                self.scroll_focused(false, 3);
            }
            KeyCode::Home if self.focus == PanelFocus::Worktrees => {
                self.selected = self.selectable_rows().first().copied().unwrap_or(0);
                self.detail_scroll = 0;
            }
            KeyCode::End if self.focus == PanelFocus::Worktrees => {
                self.selected = self.selectable_rows().last().copied().unwrap_or(0);
                self.detail_scroll = 0;
            }
            KeyCode::Char('K') if self.selected_review().is_some() => {
                self.set_review_action(PlannedAction::Keep)
            }
            KeyCode::Char('k') => self.scroll_focused(false, 1),
            KeyCode::Char('v') => {
                if let Row::Entry(group, entry) = self.selected_row() {
                    self.diffs
                        .remove(&self.groups[group].entries[entry].worktree.path);
                    self.detail_scroll = 0;
                }
            }
            KeyCode::Char('p') => self.set_review_action(PlannedAction::Preserve),
            KeyCode::Char('x') => self.start_chop(),
            KeyCode::Char('u') => self.undo_chop(),
            KeyCode::Char('e') => self.show_scan_errors(),
            _ => {}
        }
        None
    }

    fn continue_plan(&mut self) -> Option<PlanDecision> {
        let pending = self.groups[1]
            .entries
            .iter()
            .filter(|entry| entry.action == PlannedAction::Pending)
            .count();
        if !self.dry_run && pending > 0 {
            self.notice = Some(format!(
                "Choose an action for {pending} Review worktree{}. Shift-K keeps one.",
                plural(pending)
            ));
            return None;
        }
        let preserve = self.groups[1]
            .entries
            .iter()
            .filter(|entry| entry.action == PlannedAction::Preserve)
            .count();
        let forced = self.groups[0]
            .entries
            .iter()
            .filter(|entry| entry.action == PlannedAction::ForceChop)
            .count();
        let count = self.groups[0].entries.len() + preserve;
        let consent_covers_plan = self.accept_data_loss && preserve == 0 && forced == 0;
        if self.dry_run || consent_covers_plan || count == 0 {
            Some(PlanDecision::Execute)
        } else {
            self.overlay = Some(Overlay::Confirm {
                input: String::new(),
                error: None,
                scroll: 0,
            });
            None
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> Option<PlanDecision> {
        let mut move_to_chop = None;
        let removal_count = self.removal_count();
        match &mut self.overlay {
            Some(Overlay::Status { scroll, .. }) => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.overlay = None,
                KeyCode::Down | KeyCode::PageDown | KeyCode::Char('j') => {
                    *scroll = scroll.saturating_add(3)
                }
                KeyCode::Up | KeyCode::PageUp | KeyCode::Char('k') => {
                    *scroll = scroll.saturating_sub(3)
                }
                _ => {}
            },
            Some(Overlay::Chop {
                group,
                entry,
                input,
                error,
                scroll,
            }) => match key.code {
                KeyCode::Esc => self.overlay = None,
                KeyCode::Down | KeyCode::PageDown => *scroll = scroll.saturating_add(3),
                KeyCode::Up | KeyCode::PageUp => *scroll = scroll.saturating_sub(3),
                KeyCode::Backspace => {
                    input.pop();
                    *error = None;
                }
                KeyCode::Enter => {
                    let expected = format!("CHOP {}", self.groups[*group].entries[*entry].path);
                    if input == &expected {
                        move_to_chop = Some((*group, *entry));
                    } else {
                        *error =
                            Some("The text does not match. The plan did not change.".to_owned());
                    }
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    input.push(character);
                    *error = None;
                }
                _ => {}
            },
            Some(Overlay::Confirm {
                input,
                error,
                scroll,
            }) => match key.code {
                KeyCode::Esc => self.overlay = None,
                KeyCode::Down | KeyCode::PageDown => *scroll = scroll.saturating_add(3),
                KeyCode::Up | KeyCode::PageUp => *scroll = scroll.saturating_sub(3),
                KeyCode::Backspace => {
                    input.pop();
                    *error = None;
                }
                KeyCode::Enter => {
                    let expected = format!("CHOP {removal_count}");
                    if input == &expected {
                        return Some(PlanDecision::Execute);
                    }
                    *error = Some("The text does not match. Nothing was removed.".to_owned());
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    input.push(character);
                    *error = None;
                }
                _ => {}
            },
            None => {}
        }
        if let Some((group, entry)) = move_to_chop {
            self.move_to_chop(group, entry);
        }
        None
    }

    fn handle_paste(&mut self, value: &str) {
        let value = value.replace(['\n', '\r'], "");
        match &mut self.overlay {
            Some(Overlay::Chop { input, error, .. })
            | Some(Overlay::Confirm { input, error, .. }) => {
                input.push_str(&value);
                *error = None;
            }
            _ => {}
        }
    }

    fn selected_review(&self) -> Option<(usize, usize)> {
        match self.selected_row() {
            Row::Entry(group, entry) if self.groups[group].kind == GroupKind::Review => {
                Some((group, entry))
            }
            _ => None,
        }
    }

    fn removal_count(&self) -> usize {
        self.groups[0].entries.len()
            + self.groups[1]
                .entries
                .iter()
                .filter(|entry| entry.action == PlannedAction::Preserve)
                .count()
    }

    fn set_review_action(&mut self, action: PlannedAction) {
        let Some((group, entry)) = self.selected_review() else {
            return;
        };
        if self.dry_run {
            self.notice = Some("Dry run keeps review worktrees unchanged.".to_owned());
            return;
        }
        if action == PlannedAction::Preserve
            && !ui::can_preserve(&self.groups[group].entries[entry].worktree)
        {
            self.notice = Some(
                "Chop cannot save this worktree safely. Keep it or use Chop anyway.".to_owned(),
            );
            return;
        }
        self.groups[group].entries[entry].action = action;
        self.notice = None;
    }

    fn start_chop(&mut self) {
        let Some((group, entry)) = self.selected_review() else {
            return;
        };
        if self.dry_run {
            self.notice = Some("Dry run keeps review worktrees unchanged.".to_owned());
            return;
        }
        self.overlay = Some(Overlay::Chop {
            group,
            entry,
            input: String::new(),
            error: None,
            scroll: 0,
        });
    }

    fn move_to_chop(&mut self, group: usize, entry: usize) {
        let mut item = self.groups[group].entries.remove(entry);
        item.action = PlannedAction::ForceChop;
        item.reasons
            .push("You chose Chop anyway. Local state will be deleted.".to_owned());
        self.groups[0].expanded = true;
        self.groups[0].entries.push(item);
        self.selected = self.groups[0].entries.len();
        self.overlay = None;
        self.notice = Some("Moved to Chop.".to_owned());
    }

    fn selected_forced_chop(&self) -> Option<(usize, usize)> {
        match self.selected_row() {
            Row::Entry(group, entry)
                if self.groups[group].kind == GroupKind::Chop
                    && self.groups[group].entries[entry].action == PlannedAction::ForceChop =>
            {
                Some((group, entry))
            }
            _ => None,
        }
    }

    fn undo_chop(&mut self) {
        let Some((group, entry)) = self.selected_forced_chop() else {
            return;
        };
        let mut item = self.groups[group].entries.remove(entry);
        item.action = PlannedAction::Pending;
        item.reasons.pop();
        self.groups[1].expanded = true;
        self.groups[1].entries.push(item);
        let target = Row::Entry(1, self.groups[1].entries.len() - 1);
        self.selected = self
            .rows()
            .iter()
            .position(|row| *row == target)
            .unwrap_or(0);
        self.notice = Some("Returned to Review.".to_owned());
    }

    fn update_diff_preview(&mut self) {
        if let Some((path, receiver)) = &self.diff_task {
            let result = match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("Diff preview stopped.".to_owned())),
            };
            if let Some(result) = result {
                let lines = match result {
                    Ok(text) => text.lines().map(terminal::text).collect(),
                    Err(error) => vec![format!("Could not load diff: {}", terminal::text(&error))],
                };
                self.diffs.insert(path.clone(), lines);
                self.diff_task = None;
            }
        }
        if self.diff_task.is_some() {
            return;
        }
        let Row::Entry(group, entry) = self.selected_row() else {
            return;
        };
        let worktree = &self.groups[group].entries[entry].worktree;
        if worktree.local_entries.is_empty()
            || !ui::can_view(worktree)
            || self.diffs.contains_key(&worktree.path)
        {
            return;
        }
        let worktree = worktree.clone();
        let (sender, receiver) = mpsc::channel();
        self.diff_task = Some((worktree.path.clone(), receiver));
        thread::spawn(move || {
            let _ = sender.send(git::diff_preview(&worktree));
        });
    }

    fn show_scan_errors(&mut self) {
        if self.scan_errors.is_empty() {
            self.notice = Some("The scan found no errors.".to_owned());
            return;
        }
        self.overlay = Some(Overlay::Status {
            title: " Scan errors ".to_owned(),
            body: self.scan_errors.clone(),
            scroll: 0,
        });
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (group_index, group) in self.groups.iter().enumerate() {
            rows.push(Row::Group(group_index));
            if group.expanded {
                rows.extend(
                    group
                        .entries
                        .iter()
                        .enumerate()
                        .map(|(entry_index, _)| Row::Entry(group_index, entry_index)),
                );
            }
        }
        rows
    }

    fn selected_row(&self) -> Row {
        self.rows()
            .get(self.selected)
            .copied()
            .unwrap_or(Row::Group(0))
    }

    fn selectable_rows(&self) -> Vec<usize> {
        self.rows()
            .iter()
            .enumerate()
            .filter_map(|(index, row)| matches!(row, Row::Entry(..)).then_some(index))
            .collect()
    }

    fn move_down(&mut self) {
        let rows = self.selectable_rows();
        let Some(selected) = rows.iter().copied().find(|index| *index > self.selected) else {
            return;
        };
        self.selected = selected;
        self.detail_scroll = 0;
        self.notice = None;
    }

    fn move_up(&mut self) {
        let rows = self.selectable_rows();
        let Some(selected) = rows
            .iter()
            .copied()
            .rev()
            .find(|index| *index < self.selected)
        else {
            return;
        };
        self.selected = selected;
        self.detail_scroll = 0;
        self.notice = None;
    }

    fn set_expanded(&mut self, group: usize, expanded: bool) {
        let previous = self.selected_row();
        self.groups[group].expanded = expanded;
        let rows = self.rows();
        self.selected = rows
            .iter()
            .position(|row| *row == previous && matches!(row, Row::Entry(..)))
            .or_else(|| rows.iter().position(|row| matches!(row, Row::Entry(..))))
            .unwrap_or(0);
        self.detail_scroll = 0;
        self.notice = None;
    }

    fn scroll_focused(&mut self, down: bool, amount: u16) {
        if self.focus == PanelFocus::Details {
            self.detail_scroll = if down {
                self.detail_scroll.saturating_add(amount)
            } else {
                self.detail_scroll.saturating_sub(amount)
            };
        } else {
            for _ in 0..amount {
                if down {
                    self.move_down();
                } else {
                    self.move_up();
                }
            }
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.overlay.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let position = ratatui::layout::Position::new(mouse.column, mouse.row);
                let list = self.list_area.get();
                if list.contains(position) {
                    self.focus = PanelFocus::Worktrees;
                    if mouse.row > list.y
                        && mouse.row + 1 < list.bottom()
                        && mouse.column > list.x
                        && mouse.column + 1 < list.right()
                    {
                        let index = self.list_offset.get() + usize::from(mouse.row - list.y - 1);
                        if matches!(self.rows().get(index), Some(Row::Entry(..)))
                            && self.selected != index
                        {
                            self.selected = index;
                            self.detail_scroll = 0;
                            self.notice = None;
                        }
                    }
                } else if self.details_area.get().contains(position) {
                    self.focus = PanelFocus::Details;
                }
            }
            MouseEventKind::ScrollDown => self.scroll_focused(true, 3),
            MouseEventKind::ScrollUp => self.scroll_focused(false, 3),
            _ => {}
        }
    }

    fn focused_panel(&self, title: &'static str, focus: PanelFocus) -> Block<'static> {
        if self.focus == focus {
            panel(title).border_style(Style::default().fg(tone(Color::Rgb(198, 174, 146))))
        } else {
            panel(title)
        }
    }

    fn shortcuts(&self) -> Vec<(&'static str, &'static str)> {
        let mut keys = vec![("Tab", "switch panel")];
        if self.focus == PanelFocus::Details {
            keys.extend([
                ("↑/↓ or j/k", "scroll details"),
                ("PgUp/PgDn", "scroll details"),
            ]);
        } else if !self.selectable_rows().is_empty() {
            keys.extend([
                ("↑/↓ or j/k", "move"),
                ("Home/End", "first/last"),
                ("PgUp/PgDn", "move faster"),
            ]);
        }
        if let Row::Entry(group, entry) = self.selected_row() {
            let worktree = &self.groups[group].entries[entry].worktree;
            if !worktree.local_entries.is_empty() && ui::can_view(worktree) {
                keys.push(("v", "refresh diff"));
            }
        }
        if let Some((group, entry)) = self.selected_review() {
            let worktree = &self.groups[group].entries[entry].worktree;
            if !self.dry_run {
                keys.push(("Shift-K", "keep"));
                if ui::can_preserve(worktree) {
                    keys.push(("p", "stash + chop"));
                }
                keys.push(("x", "chop anyway"));
            }
        }
        if self.selected_forced_chop().is_some() {
            keys.push(("u", "undo chop"));
        }
        for (index, group) in self.groups.iter().enumerate() {
            if !group.entries.is_empty() {
                keys.push(match (index, group.expanded) {
                    (0, true) => ("1", "collapse Chop"),
                    (0, false) => ("1", "expand Chop"),
                    (1, true) => ("2", "collapse Review"),
                    (1, false) => ("2", "expand Review"),
                    (_, true) => ("3", "collapse Keep"),
                    (_, false) => ("3", "expand Keep"),
                });
            }
        }
        if !self.scan_errors.is_empty() {
            keys.push(("e", "scan errors"));
        }
        keys.extend([("c", "continue"), ("q/Esc", "quit")]);
        keys
    }

    fn render(&self, frame: &mut Frame) {
        let footer_height = shortcut_lines(&self.shortcuts(), usize::from(frame.area().width)).len()
            as u16
            + u16::from(self.notice.is_some());
        let page = page(frame.area(), 8, footer_height);
        self.render_header(frame, page[0]);
        if page[1].width >= 92 {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
                .split(page[1]);
            self.render_list(frame, body[0]);
            self.render_details(frame, body[1]);
        } else {
            let body = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
                .split(page[1]);
            self.render_list(frame, body[0]);
            self.render_details(frame, body[1]);
        }
        self.render_footer(frame, page[2]);
        if let Some(overlay) = &self.overlay {
            self.render_overlay(frame, overlay);
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let counts = self
            .groups
            .iter()
            .map(|group| group.entries.len())
            .collect::<Vec<_>>();
        let mode = if self.dry_run { "DRY RUN" } else { "" };
        let mut lines = brand_header(mode);
        lines.push(Line::from(vec![
            badge(GroupKind::Chop, counts[0], self.color),
            Span::raw("   "),
            badge(GroupKind::Review, counts[1], self.color),
            Span::raw("   "),
            badge(GroupKind::Keep, counts[2], self.color),
            Span::raw("   "),
            summary_badge("ERRORS", self.scan_errors.len(), Color::Red),
        ]));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_list(&self, frame: &mut Frame, area: Rect) {
        self.list_area.set(area);
        let items = self
            .rows()
            .iter()
            .map(|row| match *row {
                Row::Group(group_index) => {
                    let group = &self.groups[group_index];
                    let marker = if group.expanded { "▼" } else { "▶" };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{marker} {}", group.kind.title()),
                            Style::default()
                                .fg(self.group_color(group.kind))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("  {}", group.entries.len()), muted()),
                    ]))
                }
                Row::Entry(group_index, entry_index) => {
                    let group = &self.groups[group_index];
                    let entry = &group.entries[entry_index];
                    let mut spans = vec![
                        Span::styled("  └ ", Style::default().fg(self.group_color(group.kind))),
                        Span::styled(entry.reference.clone(), foreground()),
                    ];
                    if group.kind == GroupKind::Review || entry.action == PlannedAction::ForceChop {
                        spans.push(Span::styled(
                            format!("  {}", entry.action.label()),
                            Style::default().fg(if self.color {
                                entry.action.color()
                            } else {
                                Color::Reset
                            }),
                        ));
                    } else {
                        spans.push(Span::styled(format!("  {}", entry.repository), muted()));
                    }
                    ListItem::new(Line::from(spans))
                }
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default()
            .with_selected(matches!(self.selected_row(), Row::Entry(..)).then_some(self.selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(self.focused_panel(" Worktrees ", PanelFocus::Worktrees))
                .highlight_style(highlight()),
            area,
            &mut state,
        );
        self.list_offset.set(state.offset());
    }

    fn render_details(&self, frame: &mut Frame, area: Rect) {
        self.details_area.set(area);
        let lines = match self.selected_row() {
            Row::Group(group) => self.group_details(group),
            Row::Entry(group, entry) => self.entry_details(group, entry),
        };
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .block(self.focused_panel(" Details ", PanelFocus::Details))
                .wrap(Wrap { trim: false })
                .scroll((self.detail_scroll, 0)),
            area,
        );
    }

    fn group_details(&self, group: usize) -> Vec<Line<'static>> {
        let group = &self.groups[group];
        vec![
            Line::from(Span::styled(
                group.kind.title(),
                Style::default()
                    .fg(self.group_color(group.kind))
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(group.kind.description(self.dry_run)),
            Line::from(""),
            Line::from(format!(
                "{} worktree{} in this group.",
                group.entries.len(),
                plural(group.entries.len())
            )),
        ]
    }

    fn entry_details(&self, group: usize, entry: usize) -> Vec<Line<'static>> {
        let group = &self.groups[group];
        let entry = &group.entries[entry];
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    group.kind.title(),
                    Style::default()
                        .fg(self.group_color(group.kind))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(entry.reference.clone(), heading()),
            ]),
            Line::from(""),
            detail_line("Repository", &entry.repository),
            detail_line("Path", &entry.path),
        ];
        if group.kind == GroupKind::Review || entry.action == PlannedAction::ForceChop {
            lines.push(detail_line("Action", entry.action.label()));
        }
        lines.extend([
            Line::from(""),
            Line::from(Span::styled("Why it is in this group", heading())),
        ]);
        lines.extend(
            entry
                .reasons
                .iter()
                .map(|reason| Line::from(format!("• {reason}"))),
        );
        if !entry.worktree.local_entries.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Changes", heading())));
            if !ui::can_view(&entry.worktree) {
                lines.push(Line::from(
                    "Diff preview is unavailable for this worktree's Git state.",
                ));
            } else if let Some(diff) = self.diffs.get(&entry.worktree.path) {
                lines.extend(diff.iter().map(|line| {
                    let color = if line.starts_with('+') {
                        Color::Green
                    } else if line.starts_with('-') {
                        Color::Red
                    } else if line.starts_with("@@") {
                        Color::Cyan
                    } else {
                        Color::Reset
                    };
                    Line::from(Span::styled(line.clone(), Style::default().fg(tone(color))))
                }));
            } else {
                lines.push(Line::from("Loading diff…"));
            }
        }
        lines
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        if let Some(notice) = &self.notice {
            lines.push(Line::from(Span::styled(
                notice.clone(),
                Style::default().fg(tone(Color::Yellow)),
            )));
        }
        lines.extend(shortcut_lines(&self.shortcuts(), usize::from(area.width)));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_overlay(&self, frame: &mut Frame, overlay: &Overlay) {
        let area = centered(frame.area(), 78, 90);
        frame.render_widget(Clear, area);
        match overlay {
            Overlay::Status {
                title,
                body,
                scroll,
            } => {
                let lines = body
                    .iter()
                    .map(|line| Line::from(line.clone()))
                    .collect::<Vec<_>>();
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(panel(title))
                        .wrap(Wrap { trim: false })
                        .scroll((*scroll, 0)),
                    area,
                );
            }
            Overlay::Chop {
                group,
                entry,
                input,
                error,
                scroll,
            } => {
                let item = &self.groups[*group].entries[*entry];
                let expected = format!("CHOP {}", item.path);
                let warning = vec![
                    Line::from(Span::styled("CHOP ANYWAY", danger_heading())),
                    Line::from(""),
                    Line::from("This moves the worktree into the Chop group."),
                    Line::from("Chop will delete changed, untracked, and ignored files."),
                    Line::from("Git cannot restore those files."),
                    Line::from(""),
                    Line::from("Type this text, then press Enter:"),
                    Line::from(Span::styled(expected, heading())),
                ];
                let input_width = area.width.saturating_sub(6) as usize;
                let shown_input = visible_input(&terminal::text(input), input_width);
                let input_lines = vec![
                    Line::from(vec![
                        Span::styled("> ", Style::default().fg(tone(Color::Red))),
                        Span::raw(shown_input),
                        Span::styled("█", Style::default().fg(tone(Color::Red))),
                    ]),
                    Line::from(Span::styled(error.clone().unwrap_or_default(), danger())),
                    Line::from(Span::styled(
                        "↑/↓ scroll instructions   Esc cancels",
                        muted(),
                    )),
                ];
                let sections = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(5), Constraint::Length(5)])
                    .split(area);
                frame.render_widget(
                    Paragraph::new(warning)
                        .block(panel(" Confirm Chop anyway "))
                        .wrap(Wrap { trim: false })
                        .scroll((*scroll, 0)),
                    sections[0],
                );
                frame.render_widget(
                    Paragraph::new(input_lines).block(panel(" Exact confirmation ")),
                    sections[1],
                );
            }
            Overlay::Confirm {
                input,
                error,
                scroll,
            } => {
                let count = self.removal_count();
                let expected = format!("CHOP {count}");
                let preserve = self.groups[1]
                    .entries
                    .iter()
                    .filter(|entry| entry.action == PlannedAction::Preserve)
                    .count();
                let forced = self.groups[0]
                    .entries
                    .iter()
                    .filter(|entry| entry.action == PlannedAction::ForceChop)
                    .count();
                let clean = self.groups[0].entries.len() - forced;
                let keep = self.groups[1].entries.len() - preserve;
                let mut text = vec![
                    Line::from(Span::styled("READY TO CHOP", heading())),
                    Line::from(""),
                    Line::from(format!(
                        "{count} worktree{} will be chopped.",
                        plural(count)
                    )),
                    Line::from(format!("{clean} of them are clean.")),
                    Line::from(format!(
                        "{preserve} review worktree{} will be saved and removed.",
                        plural(preserve)
                    )),
                    Line::from(format!(
                        "{forced} worktree{} will lose local state.",
                        plural(forced)
                    )),
                    Line::from(format!("{keep} review worktree{} will stay.", plural(keep))),
                    Line::from("Git branches will stay."),
                    Line::from(""),
                    Line::from(Span::styled("Chop candidates", heading())),
                ];
                text.extend(self.groups[0].entries.iter().map(|entry| {
                    let warning = if entry.action == PlannedAction::ForceChop {
                        "  LOCAL STATE WILL BE DELETED"
                    } else {
                        ""
                    };
                    Line::from(format!("• {}{warning}", entry.path))
                }));
                text.extend(
                    self.groups[1]
                        .entries
                        .iter()
                        .filter(|entry| entry.action == PlannedAction::Preserve)
                        .map(|entry| Line::from(format!("• {}  SAVE FIRST", entry.path))),
                );
                let input_lines = vec![
                    Line::from(vec![
                        Span::raw("Type "),
                        Span::styled(expected, heading()),
                        Span::raw(" and press Enter."),
                    ]),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("> ", Style::default().fg(tone(Color::Green))),
                        Span::raw(terminal::text(input)),
                        Span::styled("█", Style::default().fg(tone(Color::Green))),
                    ]),
                    Line::from(Span::styled(error.clone().unwrap_or_default(), danger())),
                    Line::from(Span::styled(
                        "↑/↓ scroll candidates   Esc returns to the plan",
                        muted(),
                    )),
                ];
                let sections = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(5), Constraint::Length(7)])
                    .split(area);
                frame.render_widget(
                    Paragraph::new(text)
                        .block(panel(" Confirm plan "))
                        .wrap(Wrap { trim: false })
                        .scroll((*scroll, 0)),
                    sections[0],
                );
                frame.render_widget(
                    Paragraph::new(input_lines).block(panel(" Type to confirm ")),
                    sections[1],
                );
            }
        }
    }

    fn group_color(&self, kind: GroupKind) -> Color {
        if self.color {
            kind.color()
        } else {
            Color::Reset
        }
    }
}

fn execute_plan(
    terminal: &mut DefaultTerminal,
    app: &App,
    unreadable: &[(PathBuf, String)],
    git_errors: &[String],
) -> std::io::Result<(ui::Outcome, Vec<ResultLine>, bool)> {
    let mut outcome = ui::Outcome::default();
    let mut results = Vec::new();
    let total = app
        .groups
        .iter()
        .map(|group| group.entries.len())
        .sum::<usize>();
    let mut done = 0;
    for group in &app.groups {
        for entry in &group.entries {
            if cancellation_requested()? {
                return cancel_execution(terminal, outcome, results, done, total);
            }
            render_progress(terminal, done, total, &entry.path, &results)?;
            let mut cancel_after_current = false;
            match group.kind {
                GroupKind::Chop if app.dry_run => {
                    outcome.removed += 1;
                    results.push(result(ResultKind::Good, "Would remove", &entry.path));
                }
                GroupKind::Chop if entry.action == PlannedAction::ForceChop => {
                    let worktree = entry.worktree.clone();
                    let (removal, canceled) = run_git_operation(
                        terminal,
                        done,
                        total,
                        &entry.path,
                        &results,
                        move || git::force_remove(&worktree),
                    )?;
                    cancel_after_current = canceled;
                    match removal {
                        Ok(()) => {
                            outcome.removed += 1;
                            results.push(result(ResultKind::Good, "Chopped", &entry.path));
                        }
                        Err(error) => {
                            outcome.failed += 1;
                            results.push(result_error("Kept", &entry.path, &error));
                        }
                    }
                }
                GroupKind::Chop => {
                    let worktree = entry.worktree.clone();
                    let (removal, canceled) = run_git_operation(
                        terminal,
                        done,
                        total,
                        &entry.path,
                        &results,
                        move || git::remove_ready(&worktree),
                    )?;
                    cancel_after_current = canceled;
                    match removal {
                        Ok(()) => {
                            outcome.removed += 1;
                            results.push(result(ResultKind::Good, "Removed", &entry.path));
                        }
                        Err(error) => {
                            outcome.failed += 1;
                            results.push(result_error("Kept", &entry.path, &error));
                        }
                    }
                }
                GroupKind::Keep => {
                    outcome.kept += 1;
                    results.push(result(ResultKind::Info, "Kept", &entry.path));
                }
                GroupKind::Review if app.dry_run => {
                    outcome.kept += 1;
                    outcome.needs_choice += 1;
                    results.push(result(ResultKind::Info, "Would keep", &entry.path));
                }
                GroupKind::Review => match entry.action {
                    PlannedAction::Pending => {
                        outcome.kept += 1;
                        outcome.needs_choice += 1;
                        results.push(result(ResultKind::Info, "Needs choice", &entry.path));
                    }
                    PlannedAction::Keep => {
                        outcome.kept += 1;
                        results.push(result(ResultKind::Info, "Kept", &entry.path));
                    }
                    PlannedAction::Preserve => {
                        let worktree = entry.worktree.clone();
                        let (removal, canceled) = run_git_operation(
                            terminal,
                            done,
                            total,
                            &entry.path,
                            &results,
                            move || git::preserve_and_remove(&worktree),
                        )?;
                        cancel_after_current = canceled;
                        match removal {
                            Ok(saved) => {
                                outcome.preserved += 1;
                                outcome.removed += 1;
                                let detail = if saved.is_empty() {
                                    entry.path.clone()
                                } else {
                                    format!("{}  [{}]", entry.path, saved.join(", "))
                                };
                                results.push(ResultLine {
                                    kind: ResultKind::Good,
                                    text: format!("Saved + removed  {detail}"),
                                });
                            }
                            Err(error) => {
                                outcome.failed += 1;
                                let mut detail = error.message;
                                if !error.saved.is_empty() {
                                    detail.push_str(&format!(
                                        "; saved as {}",
                                        error.saved.join(", ")
                                    ));
                                }
                                results.push(result_error("Kept", &entry.path, &detail));
                            }
                        }
                    }
                    PlannedAction::ForceChop => {
                        let worktree = entry.worktree.clone();
                        let (removal, canceled) = run_git_operation(
                            terminal,
                            done,
                            total,
                            &entry.path,
                            &results,
                            move || git::force_remove(&worktree),
                        )?;
                        cancel_after_current = canceled;
                        match removal {
                            Ok(()) => {
                                outcome.removed += 1;
                                results.push(result(ResultKind::Good, "Chopped", &entry.path));
                            }
                            Err(error) => {
                                outcome.failed += 1;
                                results.push(result_error("Kept", &entry.path, &error));
                            }
                        }
                    }
                },
            }
            done += 1;
            if cancel_after_current {
                return cancel_execution(terminal, outcome, results, done, total);
            }
        }
    }
    for (path, error) in unreadable {
        results.push(ResultLine {
            kind: ResultKind::Bad,
            text: format!(
                "Could not scan  {}: {}",
                terminal::path(path),
                terminal::text(error)
            ),
        });
    }
    for error in git_errors {
        results.push(ResultLine {
            kind: ResultKind::Bad,
            text: format!("Could not inspect  {}", terminal::text(error)),
        });
    }
    render_progress(terminal, done, total, "Done", &results)?;
    Ok((outcome, results, false))
}

fn cancel_execution(
    terminal: &mut DefaultTerminal,
    mut outcome: ui::Outcome,
    mut results: Vec<ResultLine>,
    done: usize,
    total: usize,
) -> std::io::Result<(ui::Outcome, Vec<ResultLine>, bool)> {
    let remaining = total - done;
    outcome.kept += remaining;
    results.push(ResultLine {
        kind: ResultKind::Info,
        text: format!(
            "Canceled  {remaining} remaining worktree{} stayed.",
            plural(remaining)
        ),
    });
    render_progress(terminal, done, total, "Canceled", &results)?;
    Ok((outcome, results, true))
}

fn result(kind: ResultKind, label: &str, path: &str) -> ResultLine {
    ResultLine {
        kind,
        text: format!("{label:<16}{path}"),
    }
}

fn result_error(label: &str, path: &str, error: &str) -> ResultLine {
    ResultLine {
        kind: ResultKind::Bad,
        text: format!("{label:<16}{path}: {}", terminal::text(error)),
    }
}

fn run_git_operation<T, F>(
    terminal: &mut DefaultTerminal,
    done: usize,
    total: usize,
    path: &str,
    results: &[ResultLine],
    task: F,
) -> std::io::Result<(T, bool)>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = sender.send(task());
    });
    let mut canceled = false;
    loop {
        match receiver.try_recv() {
            Ok(result) => {
                let _ = worker.join();
                return Ok((result, canceled));
            }
            Err(TryRecvError::Disconnected) => {
                let _ = worker.join();
                return Err(std::io::Error::other("Git operation stopped"));
            }
            Err(TryRecvError::Empty) => {}
        }
        let has_input = match event::poll(Duration::from_millis(50)) {
            Ok(value) => value,
            Err(error) => {
                let _ = worker.join();
                return Err(error);
            }
        };
        if has_input {
            let input = match event::read() {
                Ok(value) => value,
                Err(error) => {
                    let _ = worker.join();
                    return Err(error);
                }
            };
            if is_cancel_event(&input) && !canceled {
                canceled = true;
                if let Err(error) = render_progress(
                    terminal,
                    done,
                    total,
                    &format!("Cancel requested. Finishing safely: {path}"),
                    results,
                ) {
                    let _ = worker.join();
                    return Err(error);
                }
            }
        }
    }
}

fn run_cancellable<T, F>(
    task: F,
    mut draw: impl FnMut(Duration) -> std::io::Result<()>,
) -> std::io::Result<Option<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let started = Instant::now();
    draw(Duration::ZERO)?;
    let mut last_draw = Instant::now();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(task());
    });
    loop {
        match receiver.try_recv() {
            Ok(result) => return Ok(Some(result)),
            Err(TryRecvError::Disconnected) => {
                return Err(std::io::Error::other("background task stopped"));
            }
            Err(TryRecvError::Empty) => {}
        }
        if last_draw.elapsed() >= Duration::from_millis(150) {
            draw(started.elapsed())?;
            last_draw = Instant::now();
        }
        if event::poll(Duration::from_millis(50))? {
            let event = event::read()?;
            if matches!(event, Event::Resize(..)) {
                draw(started.elapsed())?;
            }
            if is_cancel_event(&event) {
                return Ok(None);
            }
        }
    }
}

fn cancellation_requested() -> std::io::Result<bool> {
    while event::poll(Duration::ZERO)? {
        if is_cancel_event(&event::read()?) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_cancel_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key)
            if key.kind == KeyEventKind::Press
                && (is_ctrl_c(*key) || matches!(key.code, KeyCode::Esc | KeyCode::Char('q')))
    )
}

const CHOPPING_FRAMES: [[&str; 9]; 4] = [
    [
        r"       /\          .------.",
        r"      /  \         |     /",
        r"     / /\ \        |____/",
        r"    /______\          ||",
        r"       ||             ||",
        r"       ||             ||",
        r"       ||",
        r"      /__\",
        r"   __________",
    ],
    [
        r"       /\",
        r"      /  \       .------.",
        r"     / /\ \      |     /",
        r"    /______\     |____/",
        r"       ||           \",
        r"       ||            \",
        r"       ||             \",
        r"      /__\",
        r"   __________",
    ],
    [
        r"       /\",
        r"      /  \",
        r"     / /\ \",
        r"    /______\",
        r"       ||   /|",
        r"       ||  < |==========",
        r"       ||   \|",
        r"      /__\",
        r"   __________",
    ],
    [
        r"       /\",
        r"      /  \",
        r"     / /\ \",
        r"    /______\  '",
        r"       || /|    .",
        r"       |X< |==========",
        r"       || \|  `",
        r"      /__\  .",
        r"   __________",
    ],
];

fn render_scan(
    terminal: &mut DefaultTerminal,
    roots: &[PathBuf],
    stage: &str,
    candidates: Option<usize>,
    elapsed: Duration,
) -> std::io::Result<()> {
    terminal.draw(|frame| draw_scan(frame, roots, stage, candidates, elapsed))?;
    Ok(())
}

fn draw_scan(
    frame: &mut Frame,
    roots: &[PathBuf],
    stage: &str,
    candidates: Option<usize>,
    elapsed: Duration,
) {
    let area = frame.area();
    let width = area.width.min(64);
    let height = area.height.min(26);
    let content = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let wood = Style::default().fg(tone(Color::Rgb(184, 130, 79)));
    let cycle = [0, 0, 1, 2, 3, 3, 2, 1];
    let index = cycle[(elapsed.as_millis() / 150 % cycle.len() as u128) as usize];
    let mut lines = brand_header(stage);
    lines.insert(lines.len() - 1, Line::from(""));
    lines.push(Line::from(""));
    lines.extend(
        CHOPPING_FRAMES[index]
            .iter()
            .map(|line| Line::from(Span::styled(*line, wood))),
    );
    lines.push(Line::from(Span::styled(
        format!(
            "{}s elapsed · No worktrees are being removed",
            elapsed.as_secs()
        ),
        Style::default().fg(tone(Color::Gray)),
    )));
    if let Some(count) = candidates {
        lines.push(Line::from(format!("{count} Git candidates found")));
    }
    lines.push(Line::from(""));
    for root in roots.iter().take(3) {
        lines.push(Line::from(format!("  {}", terminal::path(root))));
    }
    if roots.len() > 3 {
        lines.push(Line::from(format!("  +{} more roots", roots.len() - 3)));
    }
    let sections = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(content);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(shortcut_lines(
            &[("Esc / q / Ctrl-C", "cancel")],
            usize::from(width),
        )),
        sections[1],
    );
}

fn render_progress(
    terminal: &mut DefaultTerminal,
    done: usize,
    total: usize,
    current: &str,
    results: &[ResultLine],
) -> std::io::Result<()> {
    terminal.draw(|frame| {
        let page = page(frame.area(), 8, 2);
        let mut header = brand_header("CHOPPING");
        header.push(Line::from(format!("{done} of {total} checked")));
        frame.render_widget(Paragraph::new(header), page[0]);
        let recent = results.iter().rev().take(12).collect::<Vec<_>>();
        let mut lines = vec![
            Line::from(Span::styled(terminal::text(current), heading())),
            Line::from(""),
        ];
        lines.extend(recent.into_iter().rev().map(|item| result_line(item)));
        frame.render_widget(
            Paragraph::new(lines)
                .block(panel(" Progress "))
                .wrap(Wrap { trim: false }),
            page[1],
        );
        frame.render_widget(
            Paragraph::new(" Chop checks current Git state before each removal. ")
                .style(Style::default().fg(tone(Color::Gray))),
            page[2],
        );
    })?;
    Ok(())
}

fn draw_results(
    frame: &mut Frame,
    outcome: &ui::Outcome,
    results: &[ResultLine],
    dry_run: bool,
    scroll: u16,
) {
    let shortcuts = shortcut_lines(
        &[("↑/↓", "scroll"), ("Enter / q / Esc", "close")],
        usize::from(frame.area().width),
    );
    let page = page(frame.area(), 8, shortcuts.len() as u16);
    let removed = if dry_run { "WOULD REMOVE" } else { "REMOVED" };
    let mut header = brand_header("RESULTS");
    header.push(Line::from(vec![
        summary_badge(removed, outcome.removed, Color::Green),
        Span::raw("  "),
        summary_badge("SAVED", outcome.preserved, Color::Cyan),
        Span::raw("  "),
        summary_badge("KEPT", outcome.kept, GroupKind::Keep.color()),
        Span::raw("  "),
        summary_badge("FAILED", failure_count(results), Color::Red),
    ]));
    frame.render_widget(Paragraph::new(header), page[0]);
    let lines = if results.is_empty() {
        vec![Line::from("No linked worktrees matched this scan.")]
    } else {
        results.iter().map(result_line).collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" Worktree summary "))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        page[1],
    );
    frame.render_widget(Paragraph::new(shortcuts), page[2]);
}

fn show_results(
    terminal: &mut DefaultTerminal,
    outcome: &ui::Outcome,
    results: &[ResultLine],
    dry_run: bool,
) -> std::io::Result<()> {
    let mut scroll = 0_u16;
    loop {
        terminal.draw(|frame| draw_results(frame, outcome, results, dry_run, scroll))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if is_ctrl_c(key) {
            return Ok(());
        }
        match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Down | KeyCode::PageDown | KeyCode::Char('j') => {
                scroll = scroll.saturating_add(3)
            }
            KeyCode::Up | KeyCode::PageUp | KeyCode::Char('k') => scroll = scroll.saturating_sub(3),
            _ => {}
        }
    }
}

fn wait_for_notice(
    terminal: &mut DefaultTerminal,
    stage: &str,
    lines: &[String],
) -> std::io::Result<()> {
    loop {
        terminal.draw(|frame| {
            let page = page(frame.area(), 8, 2);
            frame.render_widget(Paragraph::new(brand_header(stage)), page[0]);
            frame.render_widget(
                Paragraph::new(lines.iter().cloned().map(Line::from).collect::<Vec<_>>())
                    .block(panel(" Saved "))
                    .wrap(Wrap { trim: false }),
                page[1],
            );
            frame.render_widget(
                Paragraph::new(shortcut_lines(
                    &[("Enter / q / Esc", "close")],
                    usize::from(page[2].width),
                )),
                page[2],
            );
        })?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Press
            && (is_ctrl_c(key)
                || matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')))
        {
            return Ok(());
        }
    }
}

fn saved_root_lines(path: &Path, roots: &[PathBuf]) -> Vec<String> {
    let mut lines = vec![format!("Saved to {}", terminal::path(path)), String::new()];
    lines.extend(
        roots
            .iter()
            .map(|root| format!("✓ {}", terminal::path(root))),
    );
    lines
}

fn brand_header(stage: &str) -> Vec<Line<'static>> {
    let wood = Style::default().fg(tone(Color::Rgb(184, 130, 79)));
    let rings = Style::default().fg(tone(Color::Rgb(232, 190, 133)));
    vec![
        Line::from(Span::styled(r" /-------\       _", wood)),
        Line::from(Span::styled(r" |       |    __| |_  ___  _ __", wood)),
        Line::from(vec![
            Span::styled(" |  ", wood),
            Span::styled("(@)", rings),
            Span::styled(r"  |   / _| ' \/ _ \| '_ \", wood),
        ]),
        Line::from(Span::styled(r" |       |   \__|_||_\___/| .__/", wood)),
        Line::from(Span::styled(r" \-------/                |_|", wood)),
        Line::from(Span::styled(
            stage.to_owned(),
            heading().fg(tone(Color::Rgb(198, 174, 146))),
        )),
    ]
}

fn page(area: Rect, header: u16, footer: u16) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header),
            Constraint::Min(5),
            Constraint::Length(footer),
        ])
        .split(area)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height) / 2),
            Constraint::Percentage(height),
            Constraint::Percentage((100 - height) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width) / 2),
            Constraint::Percentage(width),
            Constraint::Percentage((100 - width) / 2),
        ])
        .split(vertical[1])[1]
}

fn visible_input(input: &str, width: usize) -> String {
    let characters = input.chars().collect::<Vec<_>>();
    if characters.len() <= width {
        return input.to_owned();
    }
    if width <= 1 {
        return "…".to_owned();
    }
    format!(
        "…{}",
        characters[characters.len() - (width - 1)..]
            .iter()
            .collect::<String>()
    )
}

fn panel<'a>(title: &'a str) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tone(Color::DarkGray)))
        .title(title)
}

fn badge(kind: GroupKind, count: usize, color: bool) -> Span<'static> {
    let style = if color {
        Style::default()
            .fg(Color::Black)
            .bg(kind.color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    Span::styled(format!(" {} {count} ", kind.title()), style)
}

fn summary_badge(label: &str, count: usize, color: Color) -> Span<'static> {
    let style = if terminal::color_enabled() {
        Style::default()
            .fg(Color::Black)
            .bg(color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    Span::styled(format!(" {label} {count} "), style)
}

fn result_line(item: &ResultLine) -> Line<'static> {
    let color = match item.kind {
        ResultKind::Good => Color::Green,
        ResultKind::Info => Color::Cyan,
        ResultKind::Bad => Color::Red,
    };
    Line::from(Span::styled(
        item.text.clone(),
        Style::default().fg(tone(color)),
    ))
}

fn failure_count(results: &[ResultLine]) -> usize {
    results
        .iter()
        .filter(|item| matches!(item.kind, ResultKind::Bad))
        .count()
}

fn detail_line(label: &'static str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), muted()),
        Span::raw(value.to_owned()),
    ])
}

fn heading() -> Style {
    Style::default()
        .fg(tone(Color::White))
        .add_modifier(Modifier::BOLD)
}

fn danger_heading() -> Style {
    Style::default()
        .fg(tone(Color::Red))
        .add_modifier(Modifier::BOLD)
}

fn foreground() -> Style {
    Style::default().fg(tone(Color::White))
}

fn muted() -> Style {
    Style::default().fg(tone(Color::DarkGray))
}

fn danger() -> Style {
    Style::default().fg(tone(Color::Red))
}

fn highlight() -> Style {
    if terminal::color_enabled() {
        Style::default()
            .bg(Color::Rgb(38, 45, 56))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    }
}

fn tone(color: Color) -> Color {
    if terminal::color_enabled() {
        color
    } else {
        Color::Reset
    }
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn classify(worktree: &Worktree, dry_run: bool) -> (usize, Vec<String>) {
    match worktree.state() {
        State::Ready => (
            0,
            vec![
                "This linked worktree is clean.".to_owned(),
                if dry_run {
                    "Chop would remove its directory.".to_owned()
                } else {
                    "Chop can remove its directory after confirmation.".to_owned()
                },
                "The Git branch will stay.".to_owned(),
            ],
        ),
        State::NeedsChoice(reasons) => (1, reasons),
        State::Protected(reasons) => (2, reasons),
    }
}

fn repository_name(repository: &Repository) -> String {
    let path = repository
        .worktrees
        .iter()
        .find(|worktree| worktree.is_main)
        .map(|worktree| worktree.path.as_path())
        .or_else(|| {
            (repository
                .common_dir
                .file_name()
                .is_some_and(|name| name == ".git"))
            .then(|| repository.common_dir.parent())
            .flatten()
        })
        .unwrap_or(repository.common_dir.as_path());
    path.file_name()
        .map(|name| terminal::text(&name.to_string_lossy()))
        .unwrap_or_else(|| terminal::path(path))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::{
        App, EditResult, Overlay, PlanDecision, PlannedAction, ResultKind, ResultLine, RootEditor,
        Row, failure_count, is_cancel_event, visible_input,
    };
    use crate::model::{Repository, Worktree, WorktreeFlags};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    #[test]
    fn path_completion_adds_and_saves_selected_folder() {
        let directory =
            std::env::temp_dir().join(format!("chop-completion-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("Work/nested")).unwrap();
        std::fs::create_dir_all(directory.join("Workspace")).unwrap();
        std::fs::write(directory.join("World-file"), "").unwrap();
        assert_eq!(
            super::complete_folder_path("~/Wo", &directory, Some(&directory)).unwrap(),
            vec!["~/Work/", "~/Workspace/"]
        );
        assert_eq!(
            super::complete_folder_path("Work/n", &directory, None).unwrap(),
            vec!["Work/nested/"]
        );
        let mut editor = RootEditor::new(Vec::new(), &directory);
        editor.input = format!("{}/Wo", directory.display());
        editor.refresh_completions();
        assert_eq!(editor.completions.len(), 2);
        editor.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        editor.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(editor.input.ends_with("/Workspace/"));
        assert!(editor.roots.is_empty());
        editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let root = directory.join("Workspace").canonicalize().unwrap();
        assert_eq!(editor.roots, vec![root.clone()]);
        let Some(EditResult::Save(roots)) =
            editor.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        else {
            panic!("expected saved roots")
        };
        assert_eq!(roots, vec![root]);
        editor.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        editor.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert!(editor.roots.is_empty());
        assert!(
            editor
                .handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(editor.message.is_some());
        assert!(matches!(
            editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(EditResult::Cancel)
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_path_clears_completions_and_does_not_add_a_root() {
        let directory = std::env::current_dir().unwrap();
        let mut editor = RootEditor::new(Vec::new(), &directory);
        editor.input = "/this-folder-does-not-exist/chop/".to_owned();
        editor.refresh_completions();
        assert!(editor.completions.is_empty());
        editor.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(editor.roots.is_empty());
        assert!(editor.message.is_some());
    }

    #[test]
    fn folder_editor_renders_autocomplete_without_setup_banner() {
        let editor = RootEditor::new(Vec::new(), &std::env::current_dir().unwrap());
        for (width, height) in [(100, 30), (60, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| editor.render(frame)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("Folder path"));
            assert!(text.contains("Suggestions"));
            assert!(text.contains("Scan roots"));
            assert!(text.contains("[Ctrl-S] save"));
            assert!(text.contains(r"\__|_||_\___/| .__/"));
            assert!(text.contains("Find and remove linked Git worktrees."));
            assert!(!text.contains("FIRST SETUP"));
        }
    }

    #[test]
    fn results_share_setup_and_plan_artwork_at_common_terminal_sizes() {
        let editor = RootEditor::new(Vec::new(), &std::env::current_dir().unwrap());
        let app = App::new(&[repository_with_each_state()], false, false);
        let outcome = crate::ui::Outcome {
            removed: 1,
            preserved: 2,
            kept: 3,
            ..Default::default()
        };
        let results = [ResultLine {
            kind: ResultKind::Bad,
            text: "Could not remove /example/worktree".to_owned(),
        }];
        for (width, height) in [(60, 24), (80, 24), (112, 38)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| editor.render(frame)).unwrap();
            let setup = terminal.backend().buffer().clone();
            terminal.draw(|frame| app.render(frame)).unwrap();
            let plan = terminal.backend().buffer().clone();
            for dry_run in [false, true] {
                terminal
                    .draw(|frame| super::draw_results(frame, &outcome, &results, dry_run, 0))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                for y in 0..5 {
                    for x in 0..width {
                        assert_eq!(buffer[(x, y)], setup[(x, y)]);
                        assert_eq!(buffer[(x, y)], plan[(x, y)]);
                    }
                }
                let text = terminal.backend().to_string();
                for label in [
                    "RESULTS",
                    "SAVED 2",
                    "KEPT 3",
                    "FAILED 1",
                    "Could not remove",
                    "[↑/↓] scroll",
                    "close",
                ] {
                    assert!(text.contains(label), "missing {label} at {width}x{height}");
                }
                assert!(text.contains(if dry_run {
                    "WOULD REMOVE 1"
                } else {
                    "REMOVED 1"
                }));
            }
            terminal
                .draw(|frame| super::draw_results(frame, &Default::default(), &[], false, 0))
                .unwrap();
            assert!(
                terminal
                    .backend()
                    .to_string()
                    .contains("No linked worktrees matched this scan.")
            );
        }
    }

    #[test]
    fn groups_worktrees_by_action() {
        let app = App::new(&[repository_with_each_state()], false, false);
        assert_eq!(app.groups[0].entries.len(), 1);
        assert_eq!(app.groups[1].entries.len(), 1);
        assert_eq!(app.groups[2].entries.len(), 1);
        assert_eq!(app.rows().len(), 6);
    }

    #[test]
    fn main_worktree_is_not_in_any_group() {
        let app = App::new(&[repository_with_each_state()], false, false);

        assert!(
            app.groups
                .iter()
                .flat_map(|group| &group.entries)
                .all(|entry| entry.path != "/repo")
        );
    }

    #[test]
    fn collapsing_a_group_selects_a_worktree_and_can_be_reopened() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.selected = 1;
        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert!(!app.groups[0].expanded);
        assert_eq!(app.selected_row(), Row::Entry(1, 0));
        assert_eq!(app.rows().len(), 5);
        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert!(app.groups[0].expanded);
        assert_eq!(app.selected_row(), Row::Entry(1, 0));
    }

    #[test]
    fn scan_animation_changes_pose_and_keeps_scan_information_visible() {
        for (width, height) in [(60, 24), (80, 24), (112, 38)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let roots = vec![PathBuf::from("/example/repos")];
            terminal
                .draw(|frame| {
                    super::draw_scan(
                        frame,
                        &roots,
                        "Inspecting worktrees",
                        Some(95),
                        std::time::Duration::ZERO,
                    )
                })
                .unwrap();
            let initial = terminal.backend().to_string();
            terminal
                .draw(|frame| {
                    super::draw_scan(
                        frame,
                        &roots,
                        "Inspecting worktrees",
                        Some(95),
                        std::time::Duration::from_millis(600),
                    )
                })
                .unwrap();
            let impact = terminal.backend().to_string();
            assert_ne!(initial, impact);
            for text in [
                "Inspecting worktrees",
                "95 Git candidates found",
                "/example/repos",
                "cancel",
            ] {
                assert!(initial.contains(text));
                assert!(impact.contains(text));
            }
            assert!(impact.contains("|X< |=========="));
            for millis in [0, 300, 450, 600] {
                terminal
                    .draw(|frame| {
                        super::draw_scan(
                            frame,
                            &roots,
                            "Inspecting worktrees",
                            Some(95),
                            std::time::Duration::from_millis(millis),
                        )
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let rows: Vec<String> = (0..height)
                    .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                    .collect();
                let stage = rows
                    .iter()
                    .position(|row| row.contains("Inspecting worktrees"))
                    .unwrap();
                assert!(rows[stage - 1].trim().is_empty());
                assert!(rows[stage + 1].trim().is_empty());
                for (offset, outline) in [r"/\", r"/  \", r"/ /\ \", r"/______\"].iter().enumerate()
                {
                    assert!(rows[stage + 2 + offset].trim_start().starts_with(outline));
                }
            }
        }
    }

    #[test]
    fn mouse_focus_routes_scrolling_and_skips_group_titles() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let details = app.details_area.get();
        let mouse = |kind, column, row| super::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(mouse(
            super::MouseEventKind::Down(super::MouseButton::Left),
            details.x + 1,
            details.y + 1,
        ));
        assert_eq!(app.focus, super::PanelFocus::Details);
        let selected = app.selected;
        app.handle_mouse(mouse(
            super::MouseEventKind::ScrollDown,
            details.x + 1,
            details.y + 1,
        ));
        assert_eq!(app.selected, selected);
        assert_eq!(app.detail_scroll, 3);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.detail_scroll, 4);
        let list = app.list_area.get();
        app.handle_mouse(mouse(
            super::MouseEventKind::Down(super::MouseButton::Left),
            list.x + 1,
            list.y + 1,
        ));
        assert_eq!(app.focus, super::PanelFocus::Worktrees);
        assert_eq!(app.selected, selected);
        app.handle_mouse(mouse(
            super::MouseEventKind::ScrollDown,
            list.x + 1,
            list.y + 1,
        ));
        assert_eq!(app.selected_row(), Row::Entry(2, 0));
        app.handle_mouse(mouse(
            super::MouseEventKind::ScrollDown,
            list.x + 1,
            list.y + 1,
        ));
        assert_eq!(app.selected_row(), Row::Entry(2, 0));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, super::PanelFocus::Details);
    }

    #[test]
    fn navigation_skips_titles_and_stops_at_both_ends() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        assert_eq!(app.selected_row(), Row::Entry(0, 0));
        for expected in [Row::Entry(1, 0), Row::Entry(2, 0), Row::Entry(2, 0)] {
            app.move_down();
            assert_eq!(app.selected_row(), expected);
        }
        app.detail_scroll = 3;
        app.move_down();
        assert_eq!(app.detail_scroll, 3);
        for expected in [Row::Entry(1, 0), Row::Entry(0, 0), Row::Entry(0, 0)] {
            app.move_up();
            assert_eq!(app.selected_row(), expected);
        }
        for group in 0..3 {
            app.set_expanded(group, false);
        }
        assert!(app.selectable_rows().is_empty());
        app.move_down();
        app.move_up();
        app.set_expanded(1, true);
        assert_eq!(app.selected_row(), Row::Entry(1, 0));
    }

    #[test]
    fn details_include_cached_diff_and_refresh_clears_it() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.selected = 3;
        let path = app.groups[1].entries[0].worktree.path.clone();
        app.diffs.insert(
            path.clone(),
            vec!["-old line".to_owned(), "+new line".to_owned()],
        );
        let text: String = app
            .entry_details(1, 0)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(text.contains("-old line"));
        assert!(text.contains("+new line"));
        assert!(app.shortcuts().contains(&("v", "refresh diff")));
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(!app.diffs.contains_key(&path));
    }

    #[test]
    fn footer_actions_follow_selection_and_dry_run() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        assert!(
            !app.shortcuts()
                .iter()
                .any(|(key, _)| *key == "Shift-K" || *key == "u")
        );
        app.move_down();
        assert!(app.shortcuts().contains(&("Shift-K", "keep")));
        assert!(app.shortcuts().contains(&("x", "chop anyway")));
        app.dry_run = true;
        assert!(
            !app.shortcuts()
                .iter()
                .any(|(key, _)| ["Shift-K", "p", "x"].contains(key))
        );
        app.dry_run = false;
        app.move_to_chop(1, 0);
        assert!(app.shortcuts().contains(&("u", "undo chop")));
        assert!(!app.shortcuts().contains(&("x", "chop anyway")));
    }

    #[test]
    fn details_explain_keep_without_internal_state_names() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.selected = 5;
        let backend = TestBackend::new(110, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let output = terminal.backend().to_string();
        assert!(output.contains("KEEP"));
        assert!(output.contains("This worktree contains your current directory."));
        assert!(!output.contains("PROTECTED"));
    }

    #[test]
    fn review_action_is_part_of_the_plan() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.selected = 3;
        app.handle_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT));
        assert_eq!(app.groups[1].entries[0].action, PlannedAction::Keep);
    }

    #[test]
    fn lowercase_k_moves_without_changing_a_review_action() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.selected = 3;

        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));

        assert_eq!(app.selected, 1);
        assert_eq!(app.groups[1].entries[0].action, PlannedAction::Pending);
    }

    #[test]
    fn failure_count_includes_scan_errors() {
        let results = [
            ResultLine {
                kind: ResultKind::Good,
                text: "Removed one".to_owned(),
            },
            ResultLine {
                kind: ResultKind::Bad,
                text: "Could not scan two".to_owned(),
            },
        ];

        assert_eq!(failure_count(&results), 1);
    }

    #[test]
    fn clean_worktrees_need_exact_batch_confirmation() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.groups[1].entries[0].action = PlannedAction::Keep;
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
                .is_none()
        );
        assert!(matches!(app.overlay, Some(Overlay::Confirm { .. })));
    }

    #[test]
    fn wrong_batch_confirmation_does_not_execute() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.groups[1].entries[0].action = PlannedAction::Keep;
        app.continue_plan();
        if let Some(Overlay::Confirm { input, .. }) = &mut app.overlay {
            input.push_str("CHOP 2");
        }

        let decision = app.handle_overlay_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(decision, None);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Confirm { error: Some(_), .. })
        ));
    }

    #[test]
    fn exact_batch_confirmation_executes() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.groups[1].entries[0].action = PlannedAction::Keep;
        app.continue_plan();
        if let Some(Overlay::Confirm { input, .. }) = &mut app.overlay {
            input.push_str("CHOP 1");
        }

        let decision = app.handle_overlay_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(decision, Some(PlanDecision::Execute));
    }

    #[test]
    fn batch_confirmation_shows_a_candidate_and_navigation() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.groups[1].entries[0].action = PlannedAction::Keep;
        app.continue_plan();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| app.render(frame)).unwrap();

        let output = terminal.backend().to_string();
        assert!(output.contains("/worktrees/clean"));
        assert!(output.contains("scroll candidates"));
    }

    #[test]
    fn pending_review_worktree_blocks_execution() {
        let mut app = App::new(&[repository_with_each_state()], false, false);

        let decision = app.continue_plan();

        assert_eq!(decision, None);
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Choose an action"))
        );
        assert!(app.overlay.is_none());
    }

    #[test]
    fn preserve_only_plan_needs_final_confirmation() {
        let mut app = App::new(&[repository_with_each_state()], false, true);
        app.groups[0].entries.clear();
        app.groups[1].entries[0].action = PlannedAction::Preserve;

        let decision = app.continue_plan();

        assert_eq!(decision, None);
        assert_eq!(app.removal_count(), 1);
        assert!(matches!(app.overlay, Some(Overlay::Confirm { .. })));
    }

    #[test]
    fn ctrl_c_escape_and_q_cancel_background_work() {
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let quit = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));

        assert!(is_cancel_event(&ctrl_c));
        assert!(is_cancel_event(&escape));
        assert!(is_cancel_event(&quit));
        assert!(!is_cancel_event(&Event::Resize(80, 24)));
    }

    #[test]
    fn long_confirmation_input_keeps_its_end_visible() {
        assert_eq!(
            visible_input("CHOP /a/very/long/worktree", 10),
            "…/worktree"
        );
        assert_eq!(visible_input("CHOP /short", 20), "CHOP /short");
    }

    #[test]
    fn chop_anyway_needs_the_full_path_and_moves_the_worktree() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.selected = 3;
        app.start_chop();
        if let Some(Overlay::Chop { input, .. }) = &mut app.overlay {
            input.push_str("CHOP /worktrees/review");
        }

        app.handle_overlay_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.groups[1].entries.is_empty());
        assert_eq!(app.groups[0].entries.len(), 2);
        assert_eq!(app.groups[0].entries[1].action, PlannedAction::ForceChop);
    }

    #[test]
    fn wrong_chop_anyway_text_does_not_move_the_worktree() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.selected = 3;
        app.start_chop();
        if let Some(Overlay::Chop { input, .. }) = &mut app.overlay {
            input.push_str("CHOP /worktrees/other");
        }

        app.handle_overlay_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.groups[0].entries.len(), 1);
        assert_eq!(app.groups[1].entries.len(), 1);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Chop { error: Some(_), .. })
        ));
    }

    #[test]
    fn forced_chop_can_return_to_review_before_execution() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        app.selected = 3;
        app.start_chop();
        if let Some(Overlay::Chop { input, .. }) = &mut app.overlay {
            input.push_str("CHOP /worktrees/review");
        }
        app.handle_overlay_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        app.undo_chop();

        assert_eq!(app.groups[0].entries.len(), 1);
        assert_eq!(app.groups[1].entries.len(), 1);
        assert_eq!(app.groups[1].entries[0].action, PlannedAction::Pending);
    }

    #[test]
    fn control_c_cancels() {
        let mut app = App::new(&[repository_with_each_state()], false, false);
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .is_some()
        );
    }

    #[test]
    fn details_escape_control_characters_in_git_reasons() {
        let mut repository = repository_with_each_state();
        repository.worktrees[2].flags.locked = Some("\x1b]52;bad\u{7}".to_owned());
        let mut app = App::new(&[repository], false, false);
        app.selected = 3;
        let backend = TestBackend::new(110, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let output = terminal.backend().to_string();
        assert!(output.contains("\\x1b]52;bad\\u{7}"));
        assert!(!output.contains("\x1b]52;bad"));
    }

    fn repository_with_each_state() -> Repository {
        let main = Worktree {
            common_dir: PathBuf::from("/repo/.git"),
            path: PathBuf::from("/repo"),
            head: "0123456789abcdef".to_owned(),
            branch: Some("refs/heads/main".to_owned()),
            flags: WorktreeFlags::default(),
            is_main: true,
            exists: true,
            contains_current_dir: false,
            local_entries: Vec::new(),
            operations: Vec::new(),
            has_initialized_submodules: false,
            has_nested_repositories: false,
            has_hidden_index_flags: false,
            has_executable_filters: false,
            detached_references: Vec::new(),
            unreferenced_private_commits: Vec::new(),
            inspection_error: None,
        };
        let mut clean = main.clone();
        clean.path = PathBuf::from("/worktrees/clean");
        clean.branch = Some("refs/heads/clean".to_owned());
        clean.is_main = false;
        let mut review = clean.clone();
        review.path = PathBuf::from("/worktrees/review");
        review.branch = Some("refs/heads/review".to_owned());
        review.local_entries.push(" M file".to_owned());
        let mut current = clean.clone();
        current.path = PathBuf::from("/worktrees/current");
        current.branch = Some("refs/heads/current".to_owned());
        current.contains_current_dir = true;
        Repository {
            common_dir: PathBuf::from("/repo/.git"),
            worktrees: vec![main, clean, review, current],
        }
    }
}
