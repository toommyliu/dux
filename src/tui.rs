use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{
    ops,
    scan::{EntryKind, EntryNode, SortKey, human_size, sorted_children},
    scan::{ScanControl, SizeMode},
    search,
};

const SEARCH_RESULT_LIMIT: usize = 2_000;
const TABLE_COLUMN_SPACING: u16 = 1;

pub fn run(root: EntryNode) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = App::new(root).run(&mut terminal);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    result
}

struct App {
    root: EntryNode,
    path: Vec<usize>,
    selected: usize,
    sort: SortKey,
    reversed: bool,
    filter: String,
    filtering: bool,
    search_query: String,
    searching: bool,
    search_results: Option<SearchView>,
    active_search: Option<Receiver<SearchOutcome>>,
    selected_targets: HashMap<PathBuf, DeleteTarget>,
    cursor_by_path: HashMap<PathBuf, usize>,
    pending_delete: Option<Vec<DeleteTarget>>,
    active_delete: Option<Receiver<DeleteOutcome>>,
    active_refresh: Option<Receiver<RefreshOutcome>>,
    refresh_control: Option<ScanControl>,
    showing_full_path: bool,
    status: Option<String>,
    table_state: TableState,
}

#[derive(Clone, Debug)]
struct SearchEntry {
    indices: Vec<usize>,
}

#[derive(Clone, Debug)]
struct SearchView {
    query: String,
    entries: Vec<SearchEntry>,
    total_matched: usize,
}

struct SearchOutcome {
    query: String,
    paths: Vec<PathBuf>,
    total_matched: usize,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct VisibleItem {
    path: PathBuf,
    indices: Vec<usize>,
    name: String,
    kind: EntryKind,
    size: u64,
    file_count: u64,
    dir_count: u64,
    error_count: u64,
}

#[derive(Clone, Debug)]
struct DeleteTarget {
    path: PathBuf,
    indices: Vec<usize>,
    size: u64,
    file_count: u64,
    dir_count: u64,
    error_count: u64,
}

struct DeleteOutcome {
    total: usize,
    removed: usize,
    removed_targets: Vec<DeleteTarget>,
    errors: Vec<String>,
}

struct RefreshOutcome {
    indices: Vec<usize>,
    result: Result<EntryNode, crate::scan::ScanCanceled>,
}

impl App {
    fn new(root: EntryNode) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        Self {
            root,
            path: Vec::new(),
            selected: 0,
            sort: SortKey::Size,
            reversed: false,
            filter: String::new(),
            filtering: false,
            search_query: String::new(),
            searching: false,
            search_results: None,
            active_search: None,
            selected_targets: HashMap::new(),
            cursor_by_path: HashMap::new(),
            pending_delete: None,
            active_delete: None,
            active_refresh: None,
            refresh_control: None,
            showing_full_path: false,
            status: None,
            table_state,
        }
    }

    fn run(mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        loop {
            self.poll_delete();
            self.poll_search();
            self.poll_refresh();
            terminal.draw(|frame| self.draw(frame))?;

            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind == KeyEventKind::Press && self.handle_key(key) {
                            break;
                        }
                    }
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    _ => {}
                }

                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) => {
                            if key.kind == KeyEventKind::Press && self.handle_key(key) {
                                return Ok(());
                            }
                        }
                        Event::Mouse(mouse) => self.handle_mouse(mouse),
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .areas(area);

        self.draw_header(frame, header);
        self.draw_table(frame, body);
        self.draw_footer(frame, footer);
        self.draw_full_path(frame, area);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let node = self.current_node();
        let direction = if self.reversed { "reversed" } else { "normal" };
        let header_path = shorten_middle(
            &node.path.display().to_string(),
            area.width.saturating_sub(7) as usize,
        );
        let title = Line::from(vec![
            Span::styled(
                "dux",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(header_path, Style::default().add_modifier(Modifier::BOLD)),
        ]);
        let stats = Line::from(vec![
            Span::raw(format!(
                "{} total  {} files  {} dirs",
                human_size(node.size),
                node.file_count,
                node.dir_count.saturating_sub(1)
            )),
            Span::raw(format!(
                "  sort: {} {}  filter: {}",
                self.sort.label(),
                direction,
                if self.filter.is_empty() {
                    "<none>"
                } else {
                    &self.filter
                }
            )),
            Span::raw(format!("  selected: {}", self.selected_targets.len())),
            Span::raw(match &self.search_results {
                Some(results) => format!(
                    "  search: {} ({}/{})",
                    results.query,
                    results.entries.len(),
                    results.total_matched
                ),
                None => String::new(),
            }),
        ]);

        frame.render_widget(
            Paragraph::new(vec![title, stats]).block(Block::new().borders(Borders::BOTTOM)),
            area,
        );
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let base_path = self.current_node().path.clone();
        let (name_width, path_width) = table_text_widths(area.width);
        let rows = self
            .visible_items()
            .into_iter()
            .map(|item| {
                let name = shorten_end(&item.name, name_width as usize);
                let path =
                    shorten_middle(&display_path(&base_path, &item.path), path_width as usize);
                let mark = if self.selected_targets.contains_key(&item.path) {
                    "*"
                } else {
                    ""
                };
                let style = match item.kind {
                    EntryKind::Directory => Style::default().fg(Color::Blue),
                    EntryKind::Error => Style::default().fg(Color::Red),
                    EntryKind::Symlink => Style::default().fg(Color::Magenta),
                    _ => Style::default(),
                };

                Row::new([
                    Cell::from(mark),
                    Cell::from(human_size(item.size)),
                    Cell::from(kind_label(&item.kind)),
                    Cell::from(name),
                    Cell::from(path),
                ])
                .style(style)
            })
            .collect::<Vec<_>>();

        let selected = if rows.is_empty() {
            None
        } else {
            Some(self.selected.min(rows.len() - 1))
        };
        self.table_state.select(selected);

        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(9),
                Constraint::Length(5),
                Constraint::Length(name_width),
                Constraint::Length(path_width),
            ],
        )
        .column_spacing(TABLE_COLUMN_SPACING)
        .header(
            Row::new(["", "Size", "Kind", "Name", "Path"]).style(
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("  ");

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let prompt = if let Some(paths) = &self.pending_delete {
            if paths.len() == 1 {
                format!(
                    "Move {} to trash? y confirm  n/esc cancel",
                    paths[0].path.display()
                )
            } else {
                format!(
                    "Move {} selected items to trash? y confirm  n/esc cancel",
                    paths.len()
                )
            }
        } else if self.searching {
            format!("?{}", self.search_query)
        } else if self.filtering {
            format!("/{}", self.filter)
        } else if self.active_refresh.is_some() {
            let progress = self.refresh_control.as_ref().map(ScanControl::snapshot);
            if let Some(progress) = progress {
                format!(
                    "Refreshing... {} scanned  {} files  {} dirs  {} errors  esc cancel",
                    progress.scanned, progress.files, progress.dirs, progress.errors
                )
            } else {
                "Refreshing... esc cancel".to_string()
            }
        } else if self.active_search.is_some() {
            "Searching descendants...".to_string()
        } else {
            "space mark  a mark visible  u unmark all  d trash  o reveal  p path  R refresh  ? search  / filter  c clear  q quit"
                .to_string()
        };

        let mut lines = vec![Line::from(prompt)];
        if let Some(status) = &self.status {
            lines.push(Line::from(status.clone()));
        }

        let busy = self.active_delete.is_some()
            || self.active_refresh.is_some()
            || self.searching
            || self.active_search.is_some()
            || self.filtering;
        let style = if self.pending_delete.is_some() {
            Style::default().fg(Color::Red)
        } else if busy {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Gray)
        };

        frame.render_widget(
            Paragraph::new(lines)
                .style(style)
                .block(Block::new().borders(Borders::TOP)),
            area,
        );
    }

    fn draw_full_path(&self, frame: &mut Frame, area: Rect) {
        if !self.showing_full_path {
            return;
        }

        let path = self
            .selected_item()
            .map(|item| item.path.display().to_string())
            .unwrap_or_else(|| self.current_node().path.display().to_string());
        let width = area.width.saturating_sub(8).clamp(24, 100);
        let height = ((path.chars().count() as u16 / width.max(1)) + 4)
            .min(area.height.saturating_sub(2))
            .max(5);
        let popup = centered_rect(width, height, area);
        let paragraph = Paragraph::new(path)
            .block(
                Block::new()
                    .title(" Full path ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false });

        frame.render_widget(Clear, popup);
        frame.render_widget(paragraph, popup);
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.showing_full_path {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('p') | KeyCode::Char('q') => {
                    self.showing_full_path = false;
                }
                _ => {}
            }
            return false;
        }

        if self.pending_delete.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.confirm_delete(),
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.pending_delete = None;
                    self.status = Some("Delete canceled".to_string());
                }
                _ => {}
            }

            return false;
        }

        if self.active_refresh.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    if let Some(control) = &self.refresh_control {
                        control.cancel();
                    }
                    self.status = Some("Canceling refresh...".to_string());
                }
                _ => {}
            }

            return false;
        }

        if self.searching {
            match key.code {
                KeyCode::Esc => self.searching = false,
                KeyCode::Enter => self.start_search(),
                KeyCode::Backspace => {
                    self.search_query.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.search_query.clear();
                }
                KeyCode::Char(char) => {
                    self.search_query.push(char);
                }
                _ => {}
            }

            return false;
        }

        if self.filtering {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.selected = 0;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.clear();
                    self.selected = 0;
                }
                KeyCode::Char(char) => {
                    self.filter.push(char);
                    self.selected = 0;
                }
                _ => {}
            }

            return false;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => true,
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                false
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                self.open_selected();
                false
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                self.open_parent();
                false
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                false
            }
            KeyCode::Char('?') => {
                self.searching = true;
                self.search_query.clear();
                false
            }
            KeyCode::Char('c') => {
                self.clear_constraints();
                false
            }
            KeyCode::Char('d') => {
                self.stage_delete();
                false
            }
            KeyCode::Char('o') => {
                self.open_targets_in_file_manager();
                false
            }
            KeyCode::Char('R') => {
                self.start_refresh();
                false
            }
            KeyCode::Char('p') => {
                self.showing_full_path = true;
                false
            }
            KeyCode::Char(' ') => {
                self.toggle_selected();
                false
            }
            KeyCode::Char('a') => {
                self.select_visible();
                false
            }
            KeyCode::Char('u') => {
                self.selected_targets.clear();
                self.status = Some("Cleared selection".to_string());
                false
            }
            KeyCode::Char('s') => {
                self.sort = SortKey::Size;
                self.selected = 0;
                self.save_current_cursor();
                false
            }
            KeyCode::Char('n') => {
                self.sort = SortKey::Name;
                self.selected = 0;
                self.save_current_cursor();
                false
            }
            KeyCode::Char('t') => {
                self.sort = SortKey::Kind;
                self.selected = 0;
                self.save_current_cursor();
                false
            }
            KeyCode::Char('r') => {
                self.reversed = !self.reversed;
                self.selected = 0;
                self.save_current_cursor();
                false
            }
            _ => false,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.pending_delete.is_some() || self.searching || self.filtering {
            return;
        }

        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_selection(1),
            MouseEventKind::ScrollUp => self.move_selection(-1),
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.visible_count();
        if count == 0 {
            self.selected = 0;
            return;
        }

        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(count.saturating_sub(1));
        self.save_current_cursor();
    }

    fn clear_constraints(&mut self) {
        let had_filter = !self.filter.is_empty();
        let had_search = self.search_results.is_some();

        self.filter.clear();
        self.filtering = false;
        self.search_query.clear();
        self.searching = false;
        self.search_results = None;
        self.selected = 0;

        self.status = Some(match (had_filter, had_search) {
            (true, true) => "Cleared filter and search".to_string(),
            (true, false) => "Cleared filter".to_string(),
            (false, true) => "Cleared search".to_string(),
            (false, false) => "No filter or search to clear".to_string(),
        });
    }

    fn current_node(&self) -> &EntryNode {
        let mut node = &self.root;
        for index in &self.path {
            if let Some(child) = node.children.get(*index) {
                node = child;
            }
        }
        node
    }

    fn visible_count(&self) -> usize {
        self.visible_items().len()
    }

    fn open_selected(&mut self) {
        let Some(item) = self.visible_items().get(self.selected).cloned() else {
            return;
        };

        self.save_current_cursor();
        if item.kind == EntryKind::Directory {
            self.path = item.indices;
            self.filter.clear();
            self.search_results = None;
            self.restore_current_cursor();
        }
    }

    fn open_parent(&mut self) {
        self.save_current_cursor();
        if self.path.pop().is_some() {
            self.filter.clear();
            self.restore_current_cursor();
        }
    }

    fn stage_delete(&mut self) {
        if self.active_delete.is_some() {
            self.status = Some("A trash operation is already running".to_string());
            return;
        }
        if self.active_refresh.is_some() {
            self.status = Some("A refresh is already running".to_string());
            return;
        }

        let targets = self.operation_targets();

        if targets.is_empty() {
            self.status = Some("Nothing selected".to_string());
        } else {
            self.pending_delete = Some(targets);
        }
    }

    fn open_targets_in_file_manager(&mut self) {
        let targets = self.operation_targets();
        if targets.is_empty() {
            self.status = Some("Nothing selected".to_string());
            return;
        }

        let mut opened = 0;
        let mut first_error = None;
        for target in &targets {
            match ops::open_in_file_manager(&target.path) {
                Ok(()) => opened += 1,
                Err(err) if first_error.is_none() => {
                    first_error = Some(format!("{}: {err}", target.path.display()));
                }
                Err(_) => {}
            }
        }

        self.status = if let Some(error) = first_error {
            Some(format!(
                "Opened {opened}/{}; first error: {error}",
                targets.len()
            ))
        } else {
            Some(format!("Opened {opened} item(s) in file manager"))
        };
    }

    fn confirm_delete(&mut self) {
        let Some(targets) = self.pending_delete.take() else {
            return;
        };
        let total = targets.len();
        let (sender, receiver) = mpsc::channel();

        thread::spawn(move || {
            let mut removed = 0;
            let mut removed_targets = Vec::new();
            let mut errors = Vec::new();

            for target in targets {
                match ops::trash_path(&target.path) {
                    Ok(()) => {
                        removed += 1;
                        removed_targets.push(target);
                    }
                    Err(err) => {
                        errors.push(format!("{}: {err}", target.path.display()));
                    }
                }
            }

            let _ = sender.send(DeleteOutcome {
                total,
                removed,
                removed_targets,
                errors,
            });
        });

        self.active_delete = Some(receiver);
        self.status = Some(format!("Moving {total} item(s) to trash..."));
    }

    fn start_refresh(&mut self) {
        if self.active_refresh.is_some() {
            self.status = Some("A refresh is already running".to_string());
            return;
        }
        if self.active_delete.is_some() {
            self.status = Some("A trash operation is already running".to_string());
            return;
        }
        if self.active_search.is_some() {
            self.status = Some("A search is already running".to_string());
            return;
        }

        let indices = self.path.clone();
        let path = self.current_node().path.clone();
        let control = ScanControl::new();
        let worker_control = control.clone();
        let (sender, receiver) = mpsc::channel();

        thread::spawn(move || {
            let result = crate::scan::scan_controlled(path, SizeMode::Physical, &worker_control);
            let _ = sender.send(RefreshOutcome { indices, result });
        });

        self.active_refresh = Some(receiver);
        self.refresh_control = Some(control);
        self.status = Some("Refreshing current directory...".to_string());
    }

    fn save_current_cursor(&mut self) {
        self.cursor_by_path
            .insert(self.current_node().path.clone(), self.selected);
    }

    fn restore_current_cursor(&mut self) {
        self.selected = self
            .cursor_by_path
            .get(&self.current_node().path)
            .copied()
            .unwrap_or(0)
            .min(self.visible_count().saturating_sub(1));
    }

    fn toggle_selected(&mut self) {
        let Some(target) = self.selected_target() else {
            self.status = Some("Nothing selected".to_string());
            return;
        };

        if self.selected_targets.remove(&target.path).is_some() {
            self.status = Some(format!("Unmarked {}", target.path.display()));
        } else {
            self.selected_targets
                .insert(target.path.clone(), target.clone());
            self.status = Some(format!("Marked {}", target.path.display()));
        }
    }

    fn select_visible(&mut self) {
        let targets = self.visible_targets();
        let count = targets.len();

        for target in targets {
            self.selected_targets.insert(target.path.clone(), target);
        }

        self.status = Some(format!("Marked {count} visible item(s)"));
    }

    fn poll_delete(&mut self) {
        let Some(receiver) = self.active_delete.take() else {
            return;
        };

        match receiver.try_recv() {
            Ok(outcome) => {
                self.apply_delete_outcome(outcome);
            }
            Err(TryRecvError::Empty) => {
                self.active_delete = Some(receiver);
            }
            Err(TryRecvError::Disconnected) => {
                self.status = Some("Trash operation ended unexpectedly".to_string());
            }
        }
    }

    fn poll_refresh(&mut self) {
        let Some(receiver) = self.active_refresh.take() else {
            return;
        };

        match receiver.try_recv() {
            Ok(outcome) => {
                self.refresh_control = None;
                self.apply_refresh_outcome(outcome);
            }
            Err(TryRecvError::Empty) => {
                self.active_refresh = Some(receiver);
            }
            Err(TryRecvError::Disconnected) => {
                self.refresh_control = None;
                self.status = Some("Refresh ended unexpectedly".to_string());
            }
        }
    }

    fn apply_refresh_outcome(&mut self, outcome: RefreshOutcome) {
        let refreshed = match outcome.result {
            Ok(refreshed) => refreshed,
            Err(_) => {
                self.status = Some("Refresh canceled".to_string());
                return;
            }
        };

        if !replace_node_at_indices(&mut self.root, &outcome.indices, refreshed) {
            self.status = Some("Refresh target no longer exists".to_string());
            return;
        }

        sanitize_indices(&self.root, &mut self.path);
        let root = &self.root;
        self.selected_targets
            .retain(|path, _| crate::scan::find_by_path(root, path).is_some());
        self.cursor_by_path
            .retain(|path, _| crate::scan::find_by_path(root, path).is_some());
        self.search_results = None;
        self.restore_current_cursor();
        self.status = Some("Refreshed current directory".to_string());
    }

    fn apply_delete_outcome(&mut self, outcome: DeleteOutcome) {
        let mut next_path = adjust_indices_after_removals(&self.path, &outcome.removed_targets);
        remove_targets_from_tree(&mut self.root, &outcome.removed_targets);
        sanitize_indices(&self.root, &mut next_path);
        self.selected_targets.retain(|path, _| {
            !outcome
                .removed_targets
                .iter()
                .any(|removed| path == &removed.path || path.starts_with(&removed.path))
        });
        self.cursor_by_path.retain(|path, _| {
            !outcome
                .removed_targets
                .iter()
                .any(|removed| path == &removed.path || path.starts_with(&removed.path))
        });
        self.path = next_path;
        self.search_results = None;
        self.restore_current_cursor();

        if outcome.errors.is_empty() {
            self.status = Some(format!("Moved {} item(s) to trash", outcome.removed));
        } else {
            self.status = Some(format!(
                "Moved {}/{}; first error: {}",
                outcome.removed, outcome.total, outcome.errors[0]
            ));
        }
    }

    fn selected_target(&self) -> Option<DeleteTarget> {
        self.selected_item()
            .map(|item| delete_target_for_visible_item(&item))
    }

    fn visible_targets(&self) -> Vec<DeleteTarget> {
        self.visible_items()
            .iter()
            .map(delete_target_for_visible_item)
            .collect()
    }

    fn operation_targets(&self) -> Vec<DeleteTarget> {
        let mut targets = self.selected_targets.values().cloned().collect::<Vec<_>>();
        if targets.is_empty() {
            return self.selected_target().into_iter().collect();
        }

        let root_paths = ops::root_paths(
            targets
                .iter()
                .map(|target| target.path.clone())
                .collect::<Vec<_>>(),
        )
        .into_iter()
        .collect::<HashSet<_>>();
        targets.retain(|target| root_paths.contains(&target.path));
        targets
    }

    fn visible_items(&self) -> Vec<VisibleItem> {
        if let Some(search_results) = &self.search_results {
            return search_results
                .entries
                .iter()
                .filter_map(|entry| {
                    node_at_indices(&self.root, &entry.indices)
                        .map(|node| visible_item_from_node(node, entry.indices.clone()))
                })
                .collect();
        }

        sorted_children(self.current_node(), self.sort, self.reversed, &self.filter)
            .into_iter()
            .filter_map(|node| {
                let child_index = self
                    .current_node()
                    .children
                    .iter()
                    .position(|child| child.path == node.path)?;
                let mut indices = self.path.clone();
                indices.push(child_index);
                Some(visible_item_from_node(node, indices))
            })
            .collect()
    }

    fn selected_item(&self) -> Option<VisibleItem> {
        self.visible_items().get(self.selected).cloned()
    }

    fn start_search(&mut self) {
        let query = self.search_query.trim().to_string();
        if query.is_empty() {
            self.search_results = None;
            self.searching = false;
            self.status = Some("Cleared search".to_string());
            return;
        }

        if self.active_search.is_some() {
            self.status = Some("Search is already running".to_string());
            return;
        }

        let base_path = self.current_node().path.clone();
        let base_tree = self.current_node().clone();
        let (sender, receiver) = mpsc::channel();
        let worker_query = query.clone();
        thread::spawn(move || {
            let outcome = match search::search_paths(
                &base_path,
                &base_tree,
                &worker_query,
                SEARCH_RESULT_LIMIT,
            ) {
                Ok(results) => SearchOutcome {
                    query: worker_query,
                    paths: results.hits.into_iter().map(|hit| hit.path).collect(),
                    total_matched: results.total_matched,
                    error: None,
                },
                Err(err) => SearchOutcome {
                    query: worker_query,
                    paths: Vec::new(),
                    total_matched: 0,
                    error: Some(format!("{err:#}")),
                },
            };
            let _ = sender.send(outcome);
        });

        self.active_search = Some(receiver);
        self.searching = false;
        self.search_results = None;
        self.selected = 0;
        self.status = Some(format!("Searching descendants for {query}..."));
    }

    fn poll_search(&mut self) {
        let Some(receiver) = self.active_search.take() else {
            return;
        };

        match receiver.try_recv() {
            Ok(outcome) => self.apply_search_outcome(outcome),
            Err(TryRecvError::Empty) => {
                self.active_search = Some(receiver);
            }
            Err(TryRecvError::Disconnected) => {
                self.status = Some("Search ended unexpectedly".to_string());
            }
        }
    }

    fn apply_search_outcome(&mut self, outcome: SearchOutcome) {
        if let Some(error) = outcome.error {
            self.status = Some(format!("Search failed: {error}"));
            return;
        }

        let entries = outcome
            .paths
            .iter()
            .filter_map(|path| {
                find_indices_by_path(&self.root, path).map(|indices| SearchEntry { indices })
            })
            .collect::<Vec<_>>();

        let shown = entries.len();
        self.search_results = Some(SearchView {
            query: outcome.query.clone(),
            entries,
            total_matched: outcome.total_matched,
        });
        self.selected = 0;
        let shown_label = if shown < outcome.total_matched {
            format!("showing first {shown}")
        } else {
            format!("showing {shown}")
        };
        self.status = Some(format!(
            "Search matched {} item(s), {shown_label}",
            outcome.total_matched
        ));
    }
}

fn visible_item_from_node(node: &EntryNode, indices: Vec<usize>) -> VisibleItem {
    VisibleItem {
        path: node.path.clone(),
        indices,
        name: node.name.clone(),
        kind: node.kind.clone(),
        size: node.size,
        file_count: node.file_count,
        dir_count: node.dir_count,
        error_count: node.error_count,
    }
}

fn delete_target_for_visible_item(item: &VisibleItem) -> DeleteTarget {
    DeleteTarget {
        path: item.path.clone(),
        indices: item.indices.clone(),
        size: item.size,
        file_count: item.file_count,
        dir_count: item.dir_count,
        error_count: item.error_count,
    }
}

fn node_at_indices<'a>(root: &'a EntryNode, indices: &[usize]) -> Option<&'a EntryNode> {
    let mut node = root;
    for index in indices {
        node = node.children.get(*index)?;
    }
    Some(node)
}

fn find_indices_by_path(root: &EntryNode, path: &std::path::Path) -> Option<Vec<usize>> {
    if root.path == path {
        return Some(Vec::new());
    }

    for (index, child) in root.children.iter().enumerate() {
        if let Some(mut indices) = find_indices_by_path(child, path) {
            indices.insert(0, index);
            return Some(indices);
        }
    }

    None
}

fn adjust_indices_after_removals(current: &[usize], removed: &[DeleteTarget]) -> Vec<usize> {
    let mut next = current.to_vec();

    for target in removed {
        if next.starts_with(&target.indices) {
            next.truncate(target.indices.len().saturating_sub(1));
        } else if let Some((removed_index, removed_parent)) = target.indices.split_last()
            && removed_parent.len() < next.len()
            && next.starts_with(removed_parent)
            && *removed_index < next[removed_parent.len()]
        {
            next[removed_parent.len()] = next[removed_parent.len()].saturating_sub(1);
        }
    }

    next
}

fn sanitize_indices(root: &EntryNode, indices: &mut Vec<usize>) {
    let mut node = root;
    let mut valid = 0;

    for index in indices.iter() {
        let Some(child) = node.children.get(*index) else {
            break;
        };
        node = child;
        valid += 1;
    }

    indices.truncate(valid);
}

fn replace_node_at_indices(
    root: &mut EntryNode,
    indices: &[usize],
    replacement: EntryNode,
) -> bool {
    if indices.is_empty() {
        *root = replacement;
        return true;
    }

    let Some((&last, parents)) = indices.split_last() else {
        return false;
    };
    let mut node = root;

    for index in parents {
        let Some(child) = node.children.get_mut(*index) else {
            return false;
        };
        node = child;
    }

    let Some(slot) = node.children.get_mut(last) else {
        return false;
    };
    *slot = replacement;
    true
}

fn remove_targets_from_tree(root: &mut EntryNode, targets: &[DeleteTarget]) {
    let mut targets = targets.to_vec();
    targets.sort_by(|a, b| b.indices.cmp(&a.indices));

    for target in &targets {
        remove_target(root, target, &target.indices);
    }
}

fn remove_target(node: &mut EntryNode, target: &DeleteTarget, indices: &[usize]) -> bool {
    let Some((&index, rest)) = indices.split_first() else {
        return false;
    };

    let removed = if rest.is_empty() {
        if node
            .children
            .get(index)
            .is_some_and(|child| child.path == target.path)
        {
            node.children.remove(index);
            true
        } else {
            false
        }
    } else if let Some(child) = node.children.get_mut(index) {
        remove_target(child, target, rest)
    } else {
        false
    };

    if removed {
        subtract_target_totals(node, target);
    }

    removed
}

fn subtract_target_totals(node: &mut EntryNode, target: &DeleteTarget) {
    node.size = node.size.saturating_sub(target.size);
    node.file_count = node.file_count.saturating_sub(target.file_count);
    node.dir_count = node.dir_count.saturating_sub(target.dir_count);
    node.error_count = node.error_count.saturating_sub(target.error_count);
}

fn kind_label(kind: &EntryKind) -> &'static str {
    match kind {
        EntryKind::Directory => "dir",
        EntryKind::File => "file",
        EntryKind::Symlink => "link",
        EntryKind::Other => "other",
        EntryKind::Error => "error",
    }
}

fn display_path(base_path: &Path, path: &Path) -> String {
    path.strip_prefix(base_path)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .unwrap_or(path)
        .display()
        .to_string()
}

fn table_text_widths(table_width: u16) -> (u16, u16) {
    let fixed_width = 1 + 9 + 5;
    let spacing_width = TABLE_COLUMN_SPACING * 4;
    let highlight_width = 2;
    let variable_width = table_width.saturating_sub(fixed_width + spacing_width + highlight_width);
    if variable_width <= 16 {
        let name_width = variable_width / 2;
        return (name_width, variable_width.saturating_sub(name_width));
    }

    let name_width = (variable_width / 3).clamp(8, 28);
    let path_width = variable_width.saturating_sub(name_width);

    (name_width, path_width)
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;

    Rect {
        x,
        y,
        width,
        height,
    }
}

fn shorten_end(value: &str, width: usize) -> String {
    if width <= 3 {
        return ".".repeat(width);
    }

    let char_count = value.chars().count();
    if char_count <= width {
        return value.to_string();
    }

    let keep = width.saturating_sub(3);
    let mut shortened = value.chars().take(keep).collect::<String>();
    shortened.push_str("...");
    shortened
}

fn shorten_middle(value: &str, width: usize) -> String {
    if width <= 3 {
        return ".".repeat(width);
    }

    let char_count = value.chars().count();
    if char_count <= width {
        return value.to_string();
    }

    let left = width.saturating_sub(3) / 2;
    let right = width.saturating_sub(3).saturating_sub(left);
    let prefix = value.chars().take(left).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(right)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}...{suffix}")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::scan;

    use super::*;

    #[test]
    fn restores_parent_cursor_after_opening_child() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("alpha")).unwrap();
        fs::create_dir(dir.path().join("beta")).unwrap();

        let root = scan::scan(dir.path(), scan::SizeMode::Logical);
        let mut app = App::new(root);
        app.sort = SortKey::Name;
        app.selected = 1;

        app.open_selected();
        assert_eq!(app.current_node().name, "beta");

        app.open_parent();
        assert_eq!(app.current_node().path, dir.path());
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn toggle_selected_marks_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), b"one").unwrap();

        let root = scan::scan(dir.path(), scan::SizeMode::Logical);
        let mut app = App::new(root);

        app.toggle_selected();
        assert_eq!(app.selected_targets.len(), 1);

        app.toggle_selected();
        assert!(app.selected_targets.is_empty());
    }

    #[test]
    fn removing_paths_updates_tree_without_rescan() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), vec![0; 10]).unwrap();
        fs::write(dir.path().join("two.txt"), vec![0; 20]).unwrap();

        let mut root = scan::scan(dir.path(), scan::SizeMode::Logical);
        let original_size = root.size;
        let removed_path = dir.path().join("one.txt");
        let removed = root
            .children
            .iter()
            .enumerate()
            .find(|(_, child)| child.path == removed_path)
            .unwrap()
            .to_owned();
        let removed_target = DeleteTarget {
            path: removed.1.path.clone(),
            indices: vec![removed.0],
            size: removed.1.size,
            file_count: removed.1.file_count,
            dir_count: removed.1.dir_count,
            error_count: removed.1.error_count,
        };

        remove_targets_from_tree(&mut root, std::slice::from_ref(&removed_target));

        assert_eq!(root.file_count, 1);
        assert_eq!(root.size, original_size - removed_target.size);
        assert!(!root.children.iter().any(|child| child.path == removed_path));
    }

    #[test]
    fn deleting_from_search_results_clears_search_view() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), vec![0; 10]).unwrap();

        let root = scan::scan(dir.path(), scan::SizeMode::Logical);
        let child = root.children.first().unwrap().clone();
        let mut app = App::new(root);
        app.search_results = Some(SearchView {
            query: "one".to_string(),
            entries: vec![SearchEntry { indices: vec![0] }],
            total_matched: 1,
        });

        app.apply_delete_outcome(DeleteOutcome {
            total: 1,
            removed: 1,
            removed_targets: vec![DeleteTarget {
                path: child.path.clone(),
                indices: vec![0],
                size: child.size,
                file_count: child.file_count,
                dir_count: child.dir_count,
                error_count: child.error_count,
            }],
            errors: Vec::new(),
        });

        assert!(app.search_results.is_none());
        assert!(app.root.children.is_empty());
    }

    #[test]
    fn refresh_replaces_scanned_tree() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), b"one").unwrap();

        let root = scan::scan(dir.path(), scan::SizeMode::Logical);
        let mut app = App::new(root);
        fs::write(dir.path().join("two.txt"), b"two").unwrap();
        let refreshed = scan::scan(dir.path(), scan::SizeMode::Logical);

        app.apply_refresh_outcome(RefreshOutcome {
            indices: Vec::new(),
            result: Ok(refreshed),
        });

        assert_eq!(app.root.file_count, 2);
        assert!(
            app.root
                .children
                .iter()
                .any(|child| child.name == "two.txt")
        );
        assert_eq!(app.status.as_deref(), Some("Refreshed current directory"));
    }

    #[test]
    fn canceled_refresh_keeps_existing_tree() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), b"one").unwrap();

        let root = scan::scan(dir.path(), scan::SizeMode::Logical);
        let mut app = App::new(root);

        app.apply_refresh_outcome(RefreshOutcome {
            indices: Vec::new(),
            result: Err(crate::scan::ScanCanceled),
        });

        assert_eq!(app.root.file_count, 1);
        assert_eq!(app.status.as_deref(), Some("Refresh canceled"));
    }

    #[test]
    fn table_paths_are_relative_and_middle_truncated() {
        let base = std::path::Path::new("/Users/tommy");
        let path = std::path::Path::new("/Users/tommy/projects/example/node_modules/pkg/file.txt");

        assert_eq!(
            display_path(base, path),
            "projects/example/node_modules/pkg/file.txt"
        );
        assert_eq!(
            shorten_middle("projects/example/node_modules", 15),
            "projec...odules"
        );
    }

    #[test]
    fn table_widths_fit_available_space() {
        let table_width = 80;
        let (name_width, path_width) = table_text_widths(table_width);
        let used = 1 + 9 + 5 + (TABLE_COLUMN_SPACING * 4) + 2 + name_width + path_width;

        assert!(used <= table_width);
        assert!(path_width > name_width);
    }

    #[test]
    fn path_key_toggles_full_path_overlay() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), b"one").unwrap();

        let root = scan::scan(dir.path(), scan::SizeMode::Logical);
        let mut app = App::new(root);

        assert!(!app.showing_full_path);
        app.handle_key(KeyEvent::from(KeyCode::Char('p')));
        assert!(app.showing_full_path);
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.showing_full_path);
    }

    #[test]
    fn mouse_scroll_moves_selection() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), b"one").unwrap();
        fs::write(dir.path().join("two.txt"), b"two").unwrap();

        let root = scan::scan(dir.path(), scan::SizeMode::Logical);
        let mut app = App::new(root);
        app.sort = SortKey::Name;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(app.selected, 1);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn clear_key_clears_filter_and_search() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), b"one").unwrap();

        let root = scan::scan(dir.path(), scan::SizeMode::Logical);
        let mut app = App::new(root);
        app.filter = "one".to_string();
        app.filtering = true;
        app.search_query = "two".to_string();
        app.searching = true;
        app.search_results = Some(SearchView {
            query: "two".to_string(),
            entries: Vec::new(),
            total_matched: 0,
        });
        app.selected = 1;

        app.clear_constraints();

        assert!(app.filter.is_empty());
        assert!(!app.filtering);
        assert!(app.search_query.is_empty());
        assert!(!app.searching);
        assert!(app.search_results.is_none());
        assert_eq!(app.selected, 0);
        assert_eq!(app.status.as_deref(), Some("Cleared filter and search"));
    }
}
