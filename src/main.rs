use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Duration,
};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

mod ops;
mod scan;
mod search;
mod tui;

use scan::{
    ScanControl, ScanProgressSnapshot, ScanReport, SizeMode, SortKey, flatten, human_size, scan,
    sorted_children,
};

#[derive(Parser, Debug)]
#[command(
    name = "dux",
    about = "Fast disk usage explorer for understanding what is taking space"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to scan.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Print a table instead of opening the interactive TUI.
    #[arg(long, short)]
    list: bool,

    /// Emit the scanned tree as JSON.
    #[arg(long)]
    json: bool,

    /// Sort entries by size, name, or kind.
    #[arg(long, value_enum, default_value_t = SortArg::Size)]
    sort: SortArg,

    /// Reverse the selected sort order.
    #[arg(long, short)]
    reverse: bool,

    /// Only show paths matching this case-insensitive text.
    #[arg(long, short)]
    filter: Option<String>,

    /// Fuzzy-search every descendant with fff-search.
    #[arg(long)]
    search: Option<String>,

    /// Maximum number of entries to print in list mode.
    #[arg(long, default_value_t = 40)]
    limit: usize,

    /// Include every descendant in list mode instead of only direct children.
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Delete a file or directory.
    Delete(DeleteArgs),

    /// Open files or directories in the system file manager.
    Open(OpenArgs),
}

#[derive(Debug, Parser)]
struct DeleteArgs {
    /// Files or directories to delete.
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Delete permanently instead of moving to the OS trash.
    #[arg(long)]
    permanent: bool,

    /// Confirm deletion without an interactive prompt.
    #[arg(long, short)]
    yes: bool,
}

#[derive(Debug, Parser)]
struct OpenArgs {
    /// Files or directories to open in the system file manager.
    #[arg(required = true)]
    paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SortArg {
    Size,
    Name,
    Kind,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(command) = &cli.command {
        return match command {
            Command::Delete(args) => delete_path(args),
            Command::Open(args) => open_paths(args),
        };
    }

    let interactive_tui =
        !cli.json && !cli.list && io::stdout().is_terminal() && io::stdin().is_terminal();
    let root = if interactive_tui {
        let Some(root) = scan_with_progress(&cli.path)? else {
            return Ok(());
        };
        root
    } else {
        scan(&cli.path, SizeMode::Physical)
    };

    if cli.json {
        let report = ScanReport {
            root,
            size_mode: "physical".to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if cli.list || !io::stdout().is_terminal() {
        print_list(&root, &cli);
    } else {
        tui::run(root)?;
    }

    Ok(())
}

fn scan_with_progress(path: &Path) -> Result<Option<scan::EntryNode>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_scan_progress_loop(path, &mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_scan_progress_loop(
    path: &Path,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<Option<scan::EntryNode>> {
    let (mut control, mut receiver) = spawn_startup_scan(path);
    let mut cancel_requested = false;
    let mut canceled_snapshot = None;

    loop {
        let snapshot = canceled_snapshot
            .clone()
            .unwrap_or_else(|| control.snapshot());
        terminal.draw(|frame| {
            draw_scan_progress(
                frame,
                path,
                &snapshot,
                cancel_requested,
                canceled_snapshot.is_some(),
            )
        })?;

        if canceled_snapshot.is_none() {
            match receiver.try_recv() {
                Ok(outcome) => {
                    if let Some(root) = outcome.root {
                        return Ok(Some(root));
                    }
                    if outcome.canceled {
                        canceled_snapshot = Some(control.snapshot());
                        cancel_requested = false;
                    } else {
                        bail!("scan ended without producing a root");
                    }
                }
                Err(TryRecvError::Disconnected) => bail!("scan worker ended unexpectedly"),
                Err(TryRecvError::Empty) => {}
            }
        }

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            let quit = matches!(key.code, KeyCode::Char('q'))
                || matches!(key.code, KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL));
            if quit {
                control.cancel();
                return Ok(None);
            }

            if canceled_snapshot.is_some() {
                if matches!(key.code, KeyCode::Char('r')) {
                    let scan = spawn_startup_scan(path);
                    control = scan.0;
                    receiver = scan.1;
                    cancel_requested = false;
                    canceled_snapshot = None;
                }
            } else if matches!(key.code, KeyCode::Esc) {
                cancel_requested = true;
                control.cancel();
            }
        }
    }
}

fn spawn_startup_scan(path: &Path) -> (ScanControl, Receiver<scan::ControlledScan>) {
    let control = ScanControl::new();
    let worker_control = control.clone();
    let worker_path = path.to_path_buf();
    let (sender, receiver) = mpsc::channel();

    thread::spawn(move || {
        let result =
            scan::scan_controlled_partial(worker_path, SizeMode::Physical, &worker_control);
        let _ = sender.send(result);
    });

    (control, receiver)
}

fn draw_scan_progress(
    frame: &mut Frame,
    root_path: &Path,
    snapshot: &ScanProgressSnapshot,
    cancel_requested: bool,
    canceled: bool,
) {
    let area = frame.area();
    let [top, body, _bottom] = Layout::vertical([
        Constraint::Percentage(35),
        Constraint::Length(9),
        Constraint::Min(0),
    ])
    .areas(area);
    let panel_width = area.width.saturating_sub(8).clamp(32, 96);
    let panel_x = area.x + area.width.saturating_sub(panel_width) / 2;
    let panel = ratatui::layout::Rect {
        x: panel_x,
        y: top.y + top.height,
        width: panel_width,
        height: body.height.min(area.height),
    };
    let current_path = if snapshot.current_path.as_os_str().is_empty() {
        root_path.display().to_string()
    } else {
        snapshot.current_path.display().to_string()
    };
    let width = panel.width.saturating_sub(4) as usize;
    let status = if canceled {
        Span::styled(
            "Scan canceled  r retry  q quit",
            Style::default().fg(Color::Yellow),
        )
    } else if cancel_requested {
        Span::styled("Canceling...", Style::default().fg(Color::Yellow))
    } else {
        Span::styled("Esc cancel  q quit", Style::default().fg(Color::Gray))
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                "dux",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  scanning"),
        ]),
        Line::from(shorten_middle(&root_path.display().to_string(), width)),
        Line::from(""),
        Line::from(format!(
            "{} scanned  {} files  {} dirs  {} errors",
            snapshot.scanned, snapshot.files, snapshot.dirs, snapshot.errors
        )),
        Line::from(shorten_middle(&current_path, width)),
        Line::from(""),
        Line::from(status),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(Block::new().borders(Borders::ALL).title(" Scan progress ")),
        panel,
    );
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

fn open_paths(args: &OpenArgs) -> Result<()> {
    for path in &args.paths {
        ops::open_in_file_manager(path)?;
    }

    println!("Opened {} item(s) in file manager", args.paths.len());

    Ok(())
}

fn delete_path(args: &DeleteArgs) -> Result<()> {
    if !args.yes {
        bail!("refusing to delete without --yes");
    }

    let paths = ops::root_paths(args.paths.clone());

    for path in &paths {
        if args.permanent {
            ops::delete_permanently(path)?;
        } else {
            ops::trash_path(path)?;
        }
    }

    let action = if args.permanent {
        "Deleted permanently"
    } else {
        "Moved to trash"
    };
    println!("{action} {} item(s)", paths.len());

    Ok(())
}

fn print_list(root: &scan::EntryNode, cli: &Cli) {
    let sort = match cli.sort {
        SortArg::Size => SortKey::Size,
        SortArg::Name => SortKey::Name,
        SortArg::Kind => SortKey::Kind,
    };
    let filter = cli.filter.as_deref().unwrap_or_default();

    println!(
        "{}  {} files  {} dirs  {} errors",
        human_size(root.size),
        root.file_count,
        root.dir_count.saturating_sub(1),
        root.error_count
    );
    println!("{}", root.path.display());
    println!();
    println!("{:>10}  {:<7}  Path", "Size", "Kind");

    if let Some(query) = &cli.search {
        match search::search_paths(&root.path, root, query, cli.limit) {
            Ok(results) => {
                for hit in results.hits {
                    let Some(entry) = scan::find_by_path(root, &hit.path) else {
                        continue;
                    };

                    println!(
                        "{:>10}  {:<7}  {}",
                        human_size(entry.size),
                        format!("{:?}", entry.kind).to_lowercase(),
                        entry.path.display()
                    );
                }
            }
            Err(err) => eprintln!("search failed: {err:#}"),
        }
    } else if cli.all {
        let mut entries = Vec::new();
        flatten(root, &mut entries);
        entries.remove(0);

        let filter = filter.to_lowercase();
        entries.retain(|entry| {
            filter.is_empty()
                || entry.name.to_lowercase().contains(&filter)
                || entry
                    .path
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&filter)
        });

        entries.sort_by(|a, b| match sort {
            SortKey::Size => b.size.cmp(&a.size).then_with(|| {
                a.path
                    .display()
                    .to_string()
                    .cmp(&b.path.display().to_string())
            }),
            SortKey::Name => a
                .name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| b.size.cmp(&a.size)),
            SortKey::Kind => format!("{:?}", a.kind)
                .cmp(&format!("{:?}", b.kind))
                .then_with(|| b.size.cmp(&a.size)),
        });

        if cli.reverse {
            entries.reverse();
        }

        for entry in entries.into_iter().take(cli.limit) {
            println!(
                "{:>10}  {:<7}  {}",
                human_size(entry.size),
                format!("{:?}", entry.kind).to_lowercase(),
                entry.path.display()
            );
        }
    } else {
        for entry in sorted_children(root, sort, cli.reverse, filter)
            .into_iter()
            .take(cli.limit)
        {
            println!(
                "{:>10}  {:<7}  {}",
                human_size(entry.size),
                format!("{:?}", entry.kind).to_lowercase(),
                entry.path.display()
            );
        }
    }
}
