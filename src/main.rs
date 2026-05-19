use std::{
    io::{self, IsTerminal},
    path::PathBuf,
};

use anyhow::Result;
use clap::{Parser, ValueEnum};

mod scan;
mod tui;

use scan::{ScanReport, SizeMode, SortKey, flatten, human_size, scan, sorted_children};

#[derive(Parser, Debug)]
#[command(
    name = "dux",
    about = "Fast disk usage explorer for understanding what is taking space"
)]
struct Cli {
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

    /// Maximum number of entries to print in list mode.
    #[arg(long, default_value_t = 40)]
    limit: usize,

    /// Include every descendant in list mode instead of only direct children.
    #[arg(long)]
    all: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SortArg {
    Size,
    Name,
    Kind,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = scan(&cli.path, SizeMode::Physical);

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
    println!("{:>10}  {:<7}  {}", "Size", "Kind", "Path");

    if cli.all {
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
