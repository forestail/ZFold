use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct FileMeta {
    pub abs_path: PathBuf,
    pub rel_path: String,
    pub mtime_unix_ns: u64,
    pub mode: u32,
    pub size: u64,
}

pub fn scan_files(root: &Path, excludes: &[String]) -> Result<Vec<FileMeta>> {
    let globset = build_globset(excludes)?;
    let mut out = Vec::new();

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        let abs_path = entry.path().to_path_buf();
        let rel_path = abs_path
            .strip_prefix(root)
            .with_context(|| format!("failed to strip archive root from {}", abs_path.display()))?
            .to_string_lossy()
            .replace('\\', "/");

        if globset.is_match(&rel_path) {
            continue;
        }

        let metadata = entry.metadata()?;
        let size = metadata.len();
        let mtime_unix_ns = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
            .unwrap_or(0);

        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            metadata.mode() & 0o777
        };

        #[cfg(not(unix))]
        let mode = 0u32;

        out.push(FileMeta {
            abs_path,
            rel_path,
            mtime_unix_ns,
            mode,
            size,
        });
    }

    out.sort_by(file_order);
    Ok(out)
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    builder.build().context("failed to build exclusion globset")
}

fn file_order(left: &FileMeta, right: &FileMeta) -> Ordering {
    content_group(&left.rel_path)
        .cmp(&content_group(&right.rel_path))
        .then_with(|| left.rel_path.cmp(&right.rel_path))
        .then_with(|| left.size.cmp(&right.size))
}

fn content_group(rel_path: &str) -> u8 {
    let extension = Path::new(rel_path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());

    match extension.as_deref() {
        Some("html") | Some("htm") => 0,
        Some("css") | Some("js") | Some("json") | Some("xml") | Some("txt") | Some("svg") => 1,
        _ => 2,
    }
}
