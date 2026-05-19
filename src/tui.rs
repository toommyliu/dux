use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use crate::{
    ops,
    scan::{EntryKind, EntryNode, SortKey, human_size, sorted_children},
};

pub fn run(root: EntryNode) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = App::new(root).run(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
    selected_targets: HashMap<PathBuf, DeleteTarget>,
    cursor_by_path: HashMap<PathBuf, usize>,
    pending_delete: Option<Vec<DeleteTarget>>,
    active_delete: Option<Receiver<DeleteOutcome>>,
    status: Option<String>,
    table_state: TableState,
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
            selected_targets: HashMap::new(),
            cursor_by_path: HashMap::new(),
            pending_delete: None,
            active_delete: None,
            status: None,
            table_state,
        }
    }

    fn run(mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        loop {
            self.poll_delete();
            terminal.draw(|frame| self.draw(frame))?;

            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press && self.handle_key(key) {
                        break;
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
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let node = self.current_node();
        let direction = if self.reversed { "reversed" } else { "normal" };
        let title = Line::from(vec![
            Span::styled(
                "dux",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                node.path.display().to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
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
        ]);

        frame.render_widget(
            Paragraph::new(vec![title, stats]).block(Block::new().borders(Borders::BOTTOM)),
            area,
        );
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let node = self.current_node();
        let rows = sorted_children(node, self.sort, self.reversed, &self.filter)
            .into_iter()
            .map(|child| {
                let mark = if self.selected_targets.contains_key(&child.path) {
                    "*"
                } else {
                    ""
                };
                let style = match child.kind {
                    EntryKind::Directory => Style::default().fg(Color::Blue),
                    EntryKind::Error => Style::default().fg(Color::Red),
                    EntryKind::Symlink => Style::default().fg(Color::Magenta),
                    _ => Style::default(),
                };

                Row::new([
                    Cell::from(mark),
                    Cell::from(human_size(child.size)),
                    Cell::from(kind_label(&child.kind)),
                    Cell::from(child.name.clone()),
                    Cell::from(child.path.display().to_string()),
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
                Constraint::Length(2),
                Constraint::Length(10),
                Constraint::Length(7),
                Constraint::Percentage(35),
                Constraint::Percentage(65),
            ],
        )
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
        } else if self.filtering {
            format!("/{}", self.filter)
        } else {
            "space mark  a mark visible  u unmark all  d trash marked/current  enter open  h parent  / filter  q quit"
                .to_string()
        };

        let mut lines = vec![Line::from(prompt)];
        if let Some(status) = &self.status {
            lines.push(Line::from(status.clone()));
        }

        let style = if self.pending_delete.is_some() {
            Style::default().fg(Color::Red)
        } else if self.active_delete.is_some() {
            Style::default().fg(Color::Yellow)
        } else if self.filtering {
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

    fn handle_key(&mut self, key: KeyEvent) -> bool {
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
                let count = self.visible_count();
                if count > 0 {
                    self.selected = (self.selected + 1).min(count - 1);
                    self.save_current_cursor();
                }
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.save_current_cursor();
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
            KeyCode::Char('d') => {
                self.stage_delete();
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
        sorted_children(self.current_node(), self.sort, self.reversed, &self.filter).len()
    }

    fn open_selected(&mut self) {
        let Some(target_path) =
            sorted_children(self.current_node(), self.sort, self.reversed, &self.filter)
                .get(self.selected)
                .map(|node| node.path.clone())
        else {
            return;
        };

        self.save_current_cursor();
        if let Some(index) = self
            .current_node()
            .children
            .iter()
            .position(|child| child.path == target_path && child.kind == EntryKind::Directory)
        {
            self.path.push(index);
            self.filter.clear();
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

        let mut targets = self.selected_targets.values().cloned().collect::<Vec<_>>();
        if targets.is_empty() {
            targets = self.selected_target().into_iter().collect();
        } else {
            let root_paths = ops::root_paths(
                targets
                    .iter()
                    .map(|target| target.path.clone())
                    .collect::<Vec<_>>(),
            )
            .into_iter()
            .collect::<HashSet<_>>();
            targets.retain(|target| root_paths.contains(&target.path));
        }

        if targets.is_empty() {
            self.status = Some("Nothing selected".to_string());
        } else {
            self.pending_delete = Some(targets);
        }
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
        sorted_children(self.current_node(), self.sort, self.reversed, &self.filter)
            .get(self.selected)
            .and_then(|node| self.delete_target_for_child(node))
    }

    fn visible_targets(&self) -> Vec<DeleteTarget> {
        sorted_children(self.current_node(), self.sort, self.reversed, &self.filter)
            .into_iter()
            .filter_map(|node| self.delete_target_for_child(node))
            .collect()
    }

    fn delete_target_for_child(&self, node: &EntryNode) -> Option<DeleteTarget> {
        let child_index = self
            .current_node()
            .children
            .iter()
            .position(|child| child.path == node.path)?;
        let mut indices = self.path.clone();
        indices.push(child_index);

        Some(DeleteTarget {
            path: node.path.clone(),
            indices,
            size: node.size,
            file_count: node.file_count,
            dir_count: node.dir_count,
            error_count: node.error_count,
        })
    }
}

fn adjust_indices_after_removals(current: &[usize], removed: &[DeleteTarget]) -> Vec<usize> {
    let mut next = current.to_vec();

    for target in removed {
        if next.starts_with(&target.indices) {
            next.truncate(target.indices.len().saturating_sub(1));
        } else if let Some((removed_index, removed_parent)) = target.indices.split_last() {
            if removed_parent.len() < next.len()
                && next.starts_with(removed_parent)
                && *removed_index < next[removed_parent.len()]
            {
                next[removed_parent.len()] = next[removed_parent.len()].saturating_sub(1);
            }
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

        remove_targets_from_tree(&mut root, &[removed_target.clone()]);

        assert_eq!(root.file_count, 1);
        assert_eq!(root.size, original_size - removed_target.size);
        assert!(!root.children.iter().any(|child| child.path == removed_path));
    }
}
