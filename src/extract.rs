use crate::cli::ExtractArgs;
use crate::crypto::resolve_archive_password;
use crate::crypto::EncryptionKey;
use crate::format::{ChunkDesc, FileEntry};
use crate::util::{
    dictionary_for_chunk, load_archive, progress_style, read_and_decode_chunk, read_header,
    safe_output_path,
};
use anyhow::{ensure, Context, Result};
use indicatif::ProgressBar;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct ChunkJob {
    chunk: ChunkDesc,
    entries: Vec<FileEntry>,
}

pub fn run(args: ExtractArgs) -> Result<()> {
    if let Some(num_threads) = args.threads {
        ensure!(num_threads > 0, "thread count must be greater than zero");
    }

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

    let mut chunks_by_id: HashMap<u32, ChunkDesc> = archive
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

    let mut chunk_jobs = Vec::with_capacity(files_by_chunk.len());
    for (chunk_id, mut entries) in files_by_chunk {
        let chunk = chunks_by_id
            .remove(&chunk_id)
            .with_context(|| format!("index references missing chunk {chunk_id}"))?;
        entries.sort_by_key(|entry| entry.offset_in_chunk);
        chunk_jobs.push(ChunkJob { chunk, entries });
    }

    let progress = ProgressBar::new(chunk_jobs.len() as u64);
    progress.set_style(progress_style(
        "[{elapsed_precise}] {bar:40.magenta/blue} {pos}/{len} chunks",
    ));

    let result = match args.threads {
        Some(num_threads) => extract_parallel(
            chunk_jobs,
            &args.archive,
            &args.out_dir,
            dictionary,
            archive.encryption_key.as_ref(),
            &progress,
            num_threads,
        ),
        None => extract_serial(
            chunk_jobs,
            &args.archive,
            &args.out_dir,
            dictionary,
            archive.encryption_key.as_ref(),
            &progress,
        ),
    };
    progress.finish_and_clear();
    let extracted_files = result?;

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

fn extract_serial(
    chunk_jobs: Vec<ChunkJob>,
    archive_path: &Path,
    out_dir: &Path,
    dictionary: Option<&[u8]>,
    encryption_key: Option<&EncryptionKey>,
    progress: &ProgressBar,
) -> Result<usize> {
    let mut archive_file = File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let mut extracted_files = 0usize;

    for job in chunk_jobs {
        extracted_files += extract_chunk(
            &mut archive_file,
            &job.chunk,
            job.entries,
            out_dir,
            dictionary,
            encryption_key,
        )?;
        progress.inc(1);
    }

    Ok(extracted_files)
}

fn extract_parallel(
    chunk_jobs: Vec<ChunkJob>,
    archive_path: &Path,
    out_dir: &Path,
    dictionary: Option<&[u8]>,
    encryption_key: Option<&EncryptionKey>,
    progress: &ProgressBar,
    num_threads: usize,
) -> Result<usize> {
    let extracted_files = AtomicUsize::new(0);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()
        .context("failed to build extract thread pool")?;

    pool.install(|| {
        chunk_jobs
            .into_par_iter()
            .try_for_each(|job| -> Result<()> {
                let mut archive_file = File::open(archive_path)
                    .with_context(|| format!("failed to open {}", archive_path.display()))?;
                let count = extract_chunk(
                    &mut archive_file,
                    &job.chunk,
                    job.entries,
                    out_dir,
                    dictionary,
                    encryption_key,
                )?;
                extracted_files.fetch_add(count, Ordering::Relaxed);
                progress.inc(1);
                Ok(())
            })
    })?;

    Ok(extracted_files.load(Ordering::Relaxed))
}

fn extract_chunk(
    archive_file: &mut File,
    chunk: &ChunkDesc,
    entries: Vec<FileEntry>,
    out_dir: &Path,
    dictionary: Option<&[u8]>,
    encryption_key: Option<&EncryptionKey>,
) -> Result<usize> {
    let dict = dictionary_for_chunk(chunk, dictionary)?;
    let raw = read_and_decode_chunk(archive_file, chunk, dict, encryption_key)?;
    let mut extracted_files = 0usize;

    for entry in entries {
        let start = usize::try_from(entry.offset_in_chunk)
            .context("file offset exceeds addressable memory")?;
        let size =
            usize::try_from(entry.original_size).context("file size exceeds addressable memory")?;
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

        let out_path = safe_output_path(out_dir, &entry.path)?;
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

    Ok(extracted_files)
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::cli::{ExtractArgs, PackArgs, PasswordSourceArgs};
    use crate::pack;
    use anyhow::Result;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Result<Self> {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "zfold-extract-test-{}-{}-{}",
                name,
                std::process::id(),
                unique
            ));
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn extract_supports_threaded_chunk_processing() -> Result<()> {
        let temp = TestDir::new("threaded")?;
        let input = temp.path.join("input");
        let archive = temp.path.join("archive.zpk");
        let output = temp.path.join("output");
        fs::create_dir_all(&input)?;

        let expected: HashMap<&str, Vec<u8>> = HashMap::from([
            ("docs/alpha.txt", b"alpha-alpha-alpha".to_vec()),
            ("docs/beta.txt", b"beta-beta-beta-beta".to_vec()),
            ("assets/gamma.bin", vec![7u8; 32]),
            ("assets/delta.bin", vec![9u8; 48]),
        ]);

        for (path, contents) in &expected {
            let full_path = input.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(full_path, contents)?;
        }

        pack::run(PackArgs {
            input: input.clone(),
            output: archive.clone(),
            chunk_size: 16,
            level: 3,
            threads: None,
            dict: None,
            password_prompt: false,
            password: PasswordSourceArgs::default(),
            exclude: Vec::new(),
        })?;

        run(ExtractArgs {
            archive: archive.clone(),
            out_dir: output.clone(),
            threads: Some(2),
            only: None,
            prefix: None,
            password: PasswordSourceArgs::default(),
        })?;

        for (path, contents) in &expected {
            let extracted = fs::read(output.join(path))?;
            assert_eq!(&extracted, contents);
        }

        Ok(())
    }

    #[test]
    fn extract_rejects_zero_threads() {
        let err = run(ExtractArgs {
            archive: PathBuf::from("archive.zpk"),
            out_dir: PathBuf::from("out"),
            threads: Some(0),
            only: None,
            prefix: None,
            password: PasswordSourceArgs::default(),
        })
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("thread count must be greater than zero"));
    }
}
