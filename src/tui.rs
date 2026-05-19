use std::{io, time::Duration};

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

use crate::scan::{EntryKind, EntryNode, SortKey, human_size, sorted_children};

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
    table_state: TableState,
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
            table_state,
        }
    }

    fn run(mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        loop {
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
            Constraint::Length(2),
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
                let style = match child.kind {
                    EntryKind::Directory => Style::default().fg(Color::Blue),
                    EntryKind::Error => Style::default().fg(Color::Red),
                    EntryKind::Symlink => Style::default().fg(Color::Magenta),
                    _ => Style::default(),
                };

                Row::new([
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
                Constraint::Length(10),
                Constraint::Length(7),
                Constraint::Percentage(35),
                Constraint::Percentage(65),
            ],
        )
        .header(
            Row::new(["Size", "Kind", "Name", "Path"]).style(
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
        let prompt = if self.filtering {
            format!("/{}", self.filter)
        } else {
            "enter open  h/left parent  j/k move  s size  n name  t kind  r reverse  / filter  q quit"
                .to_string()
        };

        frame.render_widget(
            Paragraph::new(prompt)
                .style(Style::default().fg(if self.filtering {
                    Color::Yellow
                } else {
                    Color::Gray
                }))
                .block(Block::new().borders(Borders::TOP)),
            area,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
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
                }
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
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
            KeyCode::Char('s') => {
                self.sort = SortKey::Size;
                self.selected = 0;
                false
            }
            KeyCode::Char('n') => {
                self.sort = SortKey::Name;
                self.selected = 0;
                false
            }
            KeyCode::Char('t') => {
                self.sort = SortKey::Kind;
                self.selected = 0;
                false
            }
            KeyCode::Char('r') => {
                self.reversed = !self.reversed;
                self.selected = 0;
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

        if let Some(index) = self
            .current_node()
            .children
            .iter()
            .position(|child| child.path == target_path && child.kind == EntryKind::Directory)
        {
            self.path.push(index);
            self.selected = 0;
            self.filter.clear();
        }
    }

    fn open_parent(&mut self) {
        if self.path.pop().is_some() {
            self.selected = 0;
            self.filter.clear();
        }
    }
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
