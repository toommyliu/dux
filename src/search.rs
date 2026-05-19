use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use fff_search::{
    FFFMode, FilePicker, FilePickerOptions, FuzzySearchOptions, MixedItemRef, PaginationArgs,
    QueryParser,
};

use crate::scan::EntryNode;

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct SearchHits {
    pub hits: Vec<SearchHit>,
    pub total_matched: usize,
}

pub fn search_paths(
    base_path: &Path,
    tree: &EntryNode,
    query: &str,
    limit: usize,
) -> Result<SearchHits> {
    let mut seen = HashSet::new();
    let mut hits = Vec::new();
    let tree_query = TreeQuery::new(query);
    let tree_matched = append_tree_matches(tree, &tree_query, limit, &mut seen, &mut hits);
    if tree_matched > 0 {
        return Ok(SearchHits {
            hits,
            total_matched: tree_matched,
        });
    }

    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: base_path.display().to_string(),
        enable_mmap_cache: false,
        enable_content_indexing: false,
        mode: FFFMode::Ai,
        cache_budget: None,
        watch: false,
    })
    .with_context(|| format!("failed to create search index for {}", base_path.display()))?;

    picker
        .collect_files()
        .with_context(|| format!("failed to index {}", base_path.display()))?;

    let parser = QueryParser::default();
    let parsed = parser.parse(query);
    let results = picker.fuzzy_search_mixed(
        &parsed,
        None,
        FuzzySearchOptions {
            max_threads: 0,
            current_file: None,
            project_path: Some(base_path),
            pagination: PaginationArgs { offset: 0, limit },
            ..Default::default()
        },
    );

    let fff_total_matched = results.total_matched;
    hits.extend(
        results
            .items
            .into_iter()
            .map(|item| match item {
                MixedItemRef::File(file) => file.absolute_path(&picker, picker.base_path()),
                MixedItemRef::Dir(dir) => dir.absolute_path(&picker, picker.base_path()),
            })
            .map(|path| SearchHit { path })
            .filter(|hit| seen.insert(hit.path.clone()))
            .take(limit.saturating_sub(hits.len())),
    );

    Ok(SearchHits {
        hits,
        total_matched: fff_total_matched,
    })
}

struct TreeQuery {
    tokens: Vec<String>,
    path_like: bool,
}

impl TreeQuery {
    fn new(query: &str) -> Self {
        Self {
            tokens: search_tokens(query),
            path_like: query.contains('/') || query.contains('\\'),
        }
    }
}

fn append_tree_matches(
    tree: &EntryNode,
    query: &TreeQuery,
    limit: usize,
    seen: &mut HashSet<PathBuf>,
    hits: &mut Vec<SearchHit>,
) -> usize {
    let mut matched = 0;

    for child in &tree.children {
        append_tree_match(child, query, limit, seen, hits, &mut matched);
    }

    matched
}

fn append_tree_match(
    node: &EntryNode,
    query: &TreeQuery,
    limit: usize,
    seen: &mut HashSet<PathBuf>,
    hits: &mut Vec<SearchHit>,
    matched: &mut usize,
) {
    if tree_entry_matches(node, query) && seen.insert(node.path.clone()) {
        *matched += 1;
        if hits.len() < limit {
            hits.push(SearchHit {
                path: node.path.clone(),
            });
        }
    }

    for child in &node.children {
        append_tree_match(child, query, limit, seen, hits, matched);
    }
}

fn tree_entry_matches(node: &EntryNode, query: &TreeQuery) -> bool {
    if query.tokens.is_empty() {
        return false;
    }

    let haystack = if query.path_like {
        node.path.to_string_lossy().to_lowercase()
    } else {
        node.name.to_lowercase()
    };

    query.tokens.iter().all(|token| haystack.contains(token))
}

fn search_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|token| !token.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::scan;

    use super::*;

    #[test]
    fn searches_files_under_base_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested").join("video-cache.txt"), b"cache").unwrap();
        fs::write(dir.path().join("notes.txt"), b"notes").unwrap();

        let tree = scan::scan(dir.path(), scan::SizeMode::Logical);
        let hits = search_paths(dir.path(), &tree, "video", 10).unwrap();

        assert!(
            hits.hits
                .iter()
                .any(|hit| hit.path.ends_with("video-cache.txt"))
        );
    }

    #[test]
    fn includes_disk_files_fff_skips_as_binary() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("movie.iso"), b"image").unwrap();

        let tree = scan::scan(dir.path(), scan::SizeMode::Logical);
        let hits = search_paths(dir.path(), &tree, "movie", 10).unwrap();

        assert!(hits.hits.iter().any(|hit| hit.path.ends_with("movie.iso")));
    }

    #[test]
    fn prioritizes_scanned_tree_matches_before_fuzzy_results() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("project").join("node_modules")).unwrap();
        fs::write(dir.path().join("node_mdx.txt"), b"fuzzy").unwrap();

        let tree = scan::scan(dir.path(), scan::SizeMode::Logical);
        let hits = search_paths(dir.path(), &tree, "node_modules", 1).unwrap();

        assert_eq!(hits.hits.len(), 1);
        assert!(hits.hits[0].path.ends_with("project/node_modules"));
    }

    #[test]
    fn plain_search_matches_names_not_every_descendant_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("project").join("node_modules")).unwrap();
        fs::write(
            dir.path()
                .join("project")
                .join("node_modules")
                .join("package.json"),
            b"{}",
        )
        .unwrap();

        let tree = scan::scan(dir.path(), scan::SizeMode::Logical);
        let hits = search_paths(dir.path(), &tree, "node_modules", 10).unwrap();

        assert_eq!(hits.total_matched, 1);
        assert_eq!(hits.hits.len(), 1);
        assert!(hits.hits[0].path.ends_with("project/node_modules"));
    }

    #[test]
    fn path_like_search_can_match_descendant_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("project").join("node_modules")).unwrap();
        fs::write(
            dir.path()
                .join("project")
                .join("node_modules")
                .join("package.json"),
            b"{}",
        )
        .unwrap();

        let tree = scan::scan(dir.path(), scan::SizeMode::Logical);
        let hits = search_paths(dir.path(), &tree, "node_modules/package", 10).unwrap();

        assert_eq!(hits.total_matched, 1);
        assert!(hits.hits[0].path.ends_with("node_modules/package.json"));
    }
}
