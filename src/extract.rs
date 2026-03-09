use crate::cli::ExtractArgs;
use crate::crypto::resolve_archive_password;
use crate::format::{ChunkDesc, FileEntry};
use crate::util::{
    dictionary_for_chunk, load_archive, progress_style, read_and_decode_chunk, read_header,
    safe_output_path,
};
use anyhow::{ensure, Context, Result};
use indicatif::ProgressBar;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};

pub fn run(args: ExtractArgs) -> Result<()> {
    let header = read_header(&args.archive)?;
    let password = resolve_archive_password(&args.password, header.is_encrypted())?;
    let archive = load_archive(&args.archive, password.as_deref())?;
    let dictionary = archive.dictionary.as_deref();

    let selected: Vec<FileEntry> = archive
        .index
        .files
        .iter()
        .filter(|entry| matches_filters(entry, &args))
        .cloned()
        .collect();
    ensure!(
        !selected.is_empty(),
        "no files matched the extraction filters"
    );

    fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("failed to create {}", args.out_dir.display()))?;

    let chunks_by_id: HashMap<u32, ChunkDesc> = archive
        .index
        .chunks
        .iter()
        .cloned()
        .map(|chunk| (chunk.chunk_id, chunk))
        .collect();

    let mut files_by_chunk: BTreeMap<u32, Vec<FileEntry>> = BTreeMap::new();
    for entry in selected {
        files_by_chunk
            .entry(entry.chunk_id)
            .or_default()
            .push(entry);
    }

    let progress = ProgressBar::new(files_by_chunk.len() as u64);
    progress.set_style(progress_style(
        "[{elapsed_precise}] {bar:40.magenta/blue} {pos}/{len} chunks",
    ));

    let mut archive_file = File::open(&args.archive)
        .with_context(|| format!("failed to open {}", args.archive.display()))?;
    let mut extracted_files = 0usize;

    for (chunk_id, mut entries) in files_by_chunk {
        let chunk = chunks_by_id
            .get(&chunk_id)
            .with_context(|| format!("index references missing chunk {chunk_id}"))?;
        entries.sort_by_key(|entry| entry.offset_in_chunk);

        let dict = dictionary_for_chunk(chunk, dictionary)?;
        let raw = read_and_decode_chunk(
            &mut archive_file,
            chunk,
            dict,
            archive.encryption_key.as_ref(),
        )?;

        for entry in entries {
            let start = usize::try_from(entry.offset_in_chunk)
                .context("file offset exceeds addressable memory")?;
            let size = usize::try_from(entry.original_size)
                .context("file size exceeds addressable memory")?;
            let end = start
                .checked_add(size)
                .context("file range overflowed while extracting")?;
            ensure!(end <= raw.len(), "file {} exceeds chunk bounds", entry.path);

            let bytes = &raw[start..end];
            ensure!(
                crc32fast::hash(bytes) == entry.crc32,
                "crc32 mismatch while extracting {}",
                entry.path
            );

            let out_path = safe_output_path(&args.out_dir, &entry.path)?;
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&out_path, bytes)
                .with_context(|| format!("failed to write {}", out_path.display()))?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&out_path, fs::Permissions::from_mode(entry.mode))?;
            }

            extracted_files += 1;
        }

        progress.inc(1);
    }

    progress.finish_and_clear();
    println!(
        "extracted {} files into {}",
        extracted_files,
        args.out_dir.display()
    );

    Ok(())
}

fn matches_filters(entry: &FileEntry, args: &ExtractArgs) -> bool {
    let only_match = args.only.as_ref().map_or(true, |only| entry.path == *only);
    let prefix_match = args
        .prefix
        .as_ref()
        .map_or(true, |prefix| entry.path.starts_with(prefix));
    only_match && prefix_match
}
