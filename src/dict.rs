use crate::cli::{TrainDictArgs, TrainDictMode};
use crate::scan::{scan_files, FileMeta};
use anyhow::{ensure, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::BTreeSet;
use std::path::Path;

pub fn run(args: TrainDictArgs) -> Result<()> {
    ensure!(
        args.max_samples > 0,
        "max-samples must be greater than zero"
    );
    ensure!(args.dict_size > 0, "dict-size must be greater than zero");
    ensure!(
        args.max_sample_bytes > 0,
        "max-sample-bytes must be greater than zero"
    );

    let include_globset = build_optional_globset(&args.include)?;
    let extensions = normalize_extensions(&args.extensions);
    let mut candidates = Vec::new();

    for input in &args.inputs {
        let input_root = input
            .canonicalize()
            .with_context(|| format!("failed to resolve input directory {}", input.display()))?;
        let files = scan_files(&input_root, &args.exclude)?;
        candidates.extend(files.into_iter().filter(|meta| {
            matches_training_filters(meta, include_globset.as_ref(), &extensions, args.mode)
        }));
    }

    ensure!(
        !candidates.is_empty(),
        "no files matched the dictionary filters"
    );
    candidates.sort_by(|left, right| {
        left.rel_path
            .cmp(&right.rel_path)
            .then_with(|| left.size.cmp(&right.size))
    });

    let sampled_files = pick_sample_files(&candidates, args.max_samples);
    let mut sample_buffers = Vec::with_capacity(sampled_files.len());

    for meta in sampled_files {
        let mut bytes = std::fs::read(&meta.abs_path)
            .with_context(|| format!("failed to read {}", meta.abs_path.display()))?;
        if bytes.len() > args.max_sample_bytes {
            bytes.truncate(args.max_sample_bytes);
        }
        if !bytes.is_empty() {
            sample_buffers.push(bytes);
        }
    }

    ensure!(
        !sample_buffers.is_empty(),
        "no non-empty samples were collected for dictionary training"
    );

    let sample_refs: Vec<&[u8]> = sample_buffers.iter().map(Vec::as_slice).collect();
    let dictionary = zstd::dict::from_samples(&sample_refs, args.dict_size)
        .context("failed to train zstd dictionary")?;

    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&args.output, &dictionary)
        .with_context(|| format!("failed to write {}", args.output.display()))?;

    println!(
        "trained dictionary from {} samples across {} input roots -> {}",
        sample_refs.len(),
        args.inputs.len(),
        args.output.display()
    );

    Ok(())
}

fn build_optional_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }

    builder
        .build()
        .map(Some)
        .context("failed to build include globset")
}

fn normalize_extensions(extensions: &[String]) -> BTreeSet<String> {
    extensions
        .iter()
        .map(|extension| {
            extension
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
        })
        .filter(|extension| !extension.is_empty())
        .collect()
}

fn matches_training_filters(
    meta: &FileMeta,
    include_globset: Option<&GlobSet>,
    extensions: &BTreeSet<String>,
    mode: TrainDictMode,
) -> bool {
    if let Some(globset) = include_globset {
        if !globset.is_match(&meta.rel_path) {
            return false;
        }
    }

    if !extensions.is_empty()
        && !path_extension(&meta.rel_path).is_some_and(|extension| extensions.contains(extension))
    {
        return false;
    }

    match mode {
        TrainDictMode::All => true,
        TrainDictMode::Html => is_html_like(&meta.rel_path),
        TrainDictMode::Text => is_text_like(&meta.rel_path),
    }
}

fn path_extension(rel_path: &str) -> Option<&str> {
    Path::new(rel_path)
        .extension()
        .and_then(|value| value.to_str())
}

fn is_html_like(rel_path: &str) -> bool {
    matches!(
        path_extension(rel_path)
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("html") | Some("htm")
    )
}

fn is_text_like(rel_path: &str) -> bool {
    matches!(
        path_extension(rel_path)
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("html")
            | Some("htm")
            | Some("css")
            | Some("js")
            | Some("mjs")
            | Some("cjs")
            | Some("jsx")
            | Some("ts")
            | Some("tsx")
            | Some("json")
            | Some("xml")
            | Some("txt")
            | Some("md")
            | Some("markdown")
            | Some("csv")
            | Some("tsv")
            | Some("ini")
            | Some("cfg")
            | Some("conf")
            | Some("toml")
            | Some("yaml")
            | Some("yml")
            | Some("svg")
            | Some("sql")
            | Some("properties")
            | Some("gradle")
            | Some("log")
    )
}

fn pick_sample_files(files: &[FileMeta], max_samples: usize) -> Vec<FileMeta> {
    if files.len() <= max_samples {
        return files.to_vec();
    }

    let step = files.len() as f64 / max_samples as f64;
    let mut out = Vec::with_capacity(max_samples);
    for index in 0..max_samples {
        let candidate = (index as f64 * step).floor() as usize;
        out.push(files[candidate].clone());
    }
    out
}
