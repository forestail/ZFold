use crate::cli::VerifyArgs;
use crate::crypto::resolve_archive_password;
use crate::format::FileEntry;
use crate::util::{
    dictionary_for_chunk, load_archive, progress_style, read_and_decode_chunk, read_header,
};
use anyhow::{ensure, Context, Result};
use indicatif::ProgressBar;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;

pub fn run(args: VerifyArgs) -> Result<()> {
    let header = read_header(&args.archive)?;
    let password = resolve_archive_password(&args.password, header.is_encrypted())?;
    let archive = load_archive(&args.archive, password.as_deref())?;
    let dictionary = archive.dictionary.as_deref();

    let mut files_by_chunk: BTreeMap<u32, Vec<&FileEntry>> = BTreeMap::new();
    let mut seen_paths = HashSet::new();
    for entry in &archive.index.files {
        ensure!(
            seen_paths.insert(entry.path.clone()),
            "duplicate file path in index: {}",
            entry.path
        );
        files_by_chunk
            .entry(entry.chunk_id)
            .or_default()
            .push(entry);
    }

    let mut archive_file = File::open(&args.archive)
        .with_context(|| format!("failed to open {}", args.archive.display()))?;
    let progress = ProgressBar::new(archive.index.chunks.len() as u64);
    progress.set_style(progress_style(
        "[{elapsed_precise}] {bar:40.green/blue} {pos}/{len} chunks",
    ));

    for chunk in &archive.index.chunks {
        ensure!(
            chunk.file_offset + chunk.compressed_size <= archive.footer.metadata_offset(),
            "chunk {} extends beyond the chunk data region",
            chunk.chunk_id
        );

        let dict = dictionary_for_chunk(chunk, dictionary)?;
        let raw = read_and_decode_chunk(
            &mut archive_file,
            chunk,
            dict,
            archive.encryption_key.as_ref(),
        )?;

        let mut entries = files_by_chunk.remove(&chunk.chunk_id).unwrap_or_default();
        ensure!(
            entries.len() == chunk.file_count as usize,
            "chunk {} file count mismatch: index has {}, chunk descriptor says {}",
            chunk.chunk_id,
            entries.len(),
            chunk.file_count
        );

        entries.sort_by_key(|entry| entry.offset_in_chunk);
        let mut previous_end = 0usize;
        for entry in entries {
            let start = usize::try_from(entry.offset_in_chunk)
                .context("file offset exceeds addressable memory")?;
            let size = usize::try_from(entry.original_size)
                .context("file size exceeds addressable memory")?;
            let end = start
                .checked_add(size)
                .context("file range overflowed while verifying")?;

            ensure!(
                start >= previous_end,
                "chunk {} has overlapping file spans",
                chunk.chunk_id
            );
            ensure!(end <= raw.len(), "file {} exceeds chunk bounds", entry.path);
            ensure!(
                crc32fast::hash(&raw[start..end]) == entry.crc32,
                "crc32 mismatch for {}",
                entry.path
            );

            previous_end = end;
        }

        progress.inc(1);
    }

    progress.finish_and_clear();
    ensure!(
        files_by_chunk.is_empty(),
        "some files reference chunk ids that are not present in the chunk table"
    );

    println!(
        "verified {} files across {} chunks",
        archive.index.files.len(),
        archive.index.chunks.len()
    );

    Ok(())
}
