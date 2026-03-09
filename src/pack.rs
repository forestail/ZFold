use crate::cli::PackArgs;
use crate::crypto::{
    derive_key, encrypt_section, generate_salt, resolve_pack_password, SectionKind,
};
use crate::format::{ChunkDesc, FileEntry, Footer, Header, Index, CODEC_ZSTD, FOOTER_MAGIC};
use crate::scan::{scan_files, FileMeta};
use crate::util::{compress_buffer, configure_threads, progress_style};
use anyhow::{ensure, Context, Result};
use indicatif::ProgressBar;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run(args: PackArgs) -> Result<()> {
    configure_threads(args.threads)?;
    ensure!(args.chunk_size > 0, "chunk size must be greater than zero");

    let input_root = args
        .input
        .canonicalize()
        .with_context(|| format!("failed to resolve input directory {}", args.input.display()))?;
    let files = scan_files(&input_root, &args.exclude)?;
    ensure!(
        !files.is_empty(),
        "no files found under {}",
        input_root.display()
    );

    let dictionary = match &args.dict {
        Some(path) => Some(
            std::fs::read(path)
                .with_context(|| format!("failed to read dictionary {}", path.display()))?,
        ),
        None => None,
    };

    let chunk_size = u32::try_from(args.chunk_size).context("chunk size must fit into u32")?;
    let password = resolve_pack_password(&args.password, args.password_prompt)?;
    let encryption = match password.as_deref() {
        Some(password) => {
            let salt = generate_salt();
            let key = derive_key(password, &salt)?;
            Some((salt, key))
        }
        None => None,
    };
    let groups = group_into_chunks(&files, args.chunk_size);
    let progress = ProgressBar::new(groups.len() as u64);
    progress.set_style(progress_style(
        "[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} chunks",
    ));

    let temp_output = temporary_output_path(&args.output);
    let result = (|| -> Result<(usize, usize, u64)> {
        let output = File::create(&temp_output)
            .with_context(|| format!("failed to create {}", temp_output.display()))?;
        let mut writer = BufWriter::new(output);
        let mut current_offset = 0u64;

        let header = Header::new(
            chunk_size,
            dictionary.is_some(),
            encryption.as_ref().map(|(salt, _)| *salt),
        );
        header.write_to(&mut writer)?;
        current_offset += Header::SIZE;

        let mut index = Index::default();
        stream_chunks_to_writer(
            &mut writer,
            &groups,
            args.level,
            dictionary.as_deref(),
            encryption.as_ref().map(|(_, key)| key),
            dictionary.is_some(),
            args.threads,
            &mut index,
            &mut current_offset,
            &progress,
        )?;

        let (dict_offset, dict_size) = match dictionary.as_deref() {
            Some(bytes) => {
                let bytes = if let Some((_, key)) = encryption.as_ref() {
                    encrypt_section(bytes, key, SectionKind::Dictionary, 0)
                        .context("failed to encrypt dictionary blob")?
                } else {
                    bytes.to_vec()
                };
                let offset = current_offset;
                writer.write_all(&bytes)?;
                current_offset += bytes.len() as u64;
                (offset, bytes.len() as u64)
            }
            None => (0, 0),
        };

        let index_offset = current_offset;
        let mut index_blob = index
            .encode_binary()
            .context("failed to serialize archive index")?;
        if let Some((_, key)) = encryption.as_ref() {
            index_blob = encrypt_section(&index_blob, key, SectionKind::Index, 0)
                .context("failed to encrypt archive index")?;
        }
        writer.write_all(&index_blob)?;
        current_offset += index_blob.len() as u64;

        let footer = Footer {
            magic: FOOTER_MAGIC,
            index_offset,
            index_size: index_blob.len() as u64,
            dict_offset,
            dict_size,
            file_count: index.files.len() as u64,
            chunk_count: index.chunks.len() as u32,
            index_crc32: crc32fast::hash(&index_blob),
        };
        footer.write_to(&mut writer)?;
        current_offset += Footer::SIZE;

        writer.flush()?;
        drop(writer);

        replace_output(&temp_output, &args.output)?;

        Ok((index.files.len(), index.chunks.len(), current_offset))
    })();

    progress.finish_and_clear();

    match result {
        Ok((file_count, chunk_count, archive_size)) => {
            println!(
                "packed {} files into {} chunks ({} bytes) -> {}",
                file_count,
                chunk_count,
                archive_size,
                args.output.display()
            );
            Ok(())
        }
        Err(err) => {
            let _ = std::fs::remove_file(&temp_output);
            Err(err)
        }
    }
}

#[derive(Debug)]
struct CompressedChunk {
    chunk_id: u32,
    compressed: Vec<u8>,
    entries: Vec<FileEntry>,
    uncompressed_size: u64,
}

struct ChunkMessage {
    result: Result<CompressedChunk>,
}

fn compress_chunk(
    chunk_id: u32,
    group: &[FileMeta],
    level: i32,
    dictionary: Option<&[u8]>,
    encryption_key: Option<&[u8; 32]>,
) -> Result<CompressedChunk> {
    let total_size = group.iter().map(|meta| meta.size as usize).sum();
    let mut raw = Vec::with_capacity(total_size);
    let mut entries = Vec::with_capacity(group.len());
    let mut offset = 0u64;

    for meta in group {
        let data = std::fs::read(&meta.abs_path)
            .with_context(|| format!("failed to read {}", meta.abs_path.display()))?;

        entries.push(FileEntry {
            path: meta.rel_path.clone(),
            chunk_id,
            offset_in_chunk: offset,
            original_size: data.len() as u64,
            mtime_unix_ns: meta.mtime_unix_ns,
            mode: meta.mode,
            crc32: crc32fast::hash(&data),
            flags: 0,
        });

        raw.extend_from_slice(&data);
        offset += data.len() as u64;
    }

    let compressed = compress_buffer(&raw, level, dictionary)?;
    let compressed = match encryption_key {
        Some(key) => encrypt_section(&compressed, key, SectionKind::Chunk, chunk_id as u64)
            .with_context(|| format!("failed to encrypt chunk {chunk_id}"))?,
        None => compressed,
    };

    Ok(CompressedChunk {
        chunk_id,
        compressed,
        entries,
        uncompressed_size: raw.len() as u64,
    })
}

fn stream_chunks_to_writer<W: Write + Send>(
    writer: &mut W,
    groups: &[Vec<FileMeta>],
    level: i32,
    dictionary: Option<&[u8]>,
    encryption_key: Option<&[u8; 32]>,
    has_dictionary: bool,
    threads: Option<usize>,
    index: &mut Index,
    current_offset: &mut u64,
    progress: &ProgressBar,
) -> Result<()> {
    let channel_capacity = channel_capacity(threads);
    let (tx, rx) = sync_channel::<ChunkMessage>(channel_capacity);

    rayon::scope(move |scope| -> Result<()> {
        for (chunk_id, group) in groups.iter().enumerate() {
            let tx = tx.clone();
            scope.spawn(move |_| {
                let result =
                    compress_chunk(chunk_id as u32, group, level, dictionary, encryption_key)
                        .with_context(|| format!("failed to compress chunk {}", chunk_id));
                let _ = tx.send(ChunkMessage { result });
            });
        }
        drop(tx);

        let mut pending = BTreeMap::new();
        let mut next_chunk_id = 0u32;
        let mut first_error = None;

        for _ in 0..groups.len() {
            let message = rx
                .recv()
                .context("chunk worker stopped before all chunks were processed")?;
            progress.inc(1);

            match message.result {
                Ok(chunk) if first_error.is_none() => {
                    pending.insert(chunk.chunk_id, chunk);
                    while let Some(chunk) = pending.remove(&next_chunk_id) {
                        write_chunk(writer, chunk, has_dictionary, index, current_offset)?;
                        next_chunk_id += 1;
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    pending.clear();
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }

        if let Some(err) = first_error {
            return Err(err);
        }

        Ok(())
    })
}

fn write_chunk<W: Write>(
    writer: &mut W,
    chunk: CompressedChunk,
    has_dictionary: bool,
    index: &mut Index,
    current_offset: &mut u64,
) -> Result<()> {
    let file_offset = *current_offset;
    writer.write_all(&chunk.compressed)?;
    *current_offset += chunk.compressed.len() as u64;

    index.chunks.push(ChunkDesc {
        chunk_id: chunk.chunk_id,
        file_offset,
        compressed_size: chunk.compressed.len() as u64,
        uncompressed_size: chunk.uncompressed_size,
        file_count: chunk.entries.len() as u32,
        codec: CODEC_ZSTD,
        dict_id: if has_dictionary { 1 } else { 0 },
    });
    index.files.extend(chunk.entries);

    Ok(())
}

fn channel_capacity(threads: Option<usize>) -> usize {
    threads.unwrap_or_else(default_thread_count).max(1)
}

fn default_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
}

fn temporary_output_path(output: &Path) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let suffix = format!("{}.{}.part", std::process::id(), unique);
    output.with_extension(match output.extension() {
        Some(ext) => format!("{}.{}", ext.to_string_lossy(), suffix),
        None => suffix,
    })
}

fn replace_output(temp_output: &Path, output: &Path) -> Result<()> {
    match std::fs::remove_file(output) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to replace {}", output.display()))
        }
    }

    std::fs::rename(temp_output, output)
        .with_context(|| format!("failed to move archive into place at {}", output.display()))
}

fn group_into_chunks(files: &[FileMeta], chunk_size: usize) -> Vec<Vec<FileMeta>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut current_size = 0usize;

    for file in files {
        let file_size = file.size as usize;
        let exceeds = !current.is_empty() && current_size + file_size > chunk_size;
        if exceeds {
            groups.push(std::mem::take(&mut current));
            current_size = 0;
        }

        current.push(file.clone());
        current_size += file_size;

        if current_size >= chunk_size {
            groups.push(std::mem::take(&mut current));
            current_size = 0;
        }
    }

    if !current.is_empty() {
        groups.push(current);
    }

    groups
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::cli::{PackArgs, PasswordSourceArgs};
    use crate::util::{dictionary_for_chunk, load_archive, read_and_decode_chunk};
    use anyhow::Result;
    use std::collections::HashMap;
    use std::fs::{self, File};
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
                "zfold-pack-test-{}-{}-{}",
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
    fn pack_writes_a_valid_archive_without_buffering_all_chunks() -> Result<()> {
        let temp = TestDir::new("streaming")?;
        let input = temp.path.join("input");
        let output = temp.path.join("archive.zpk");
        fs::create_dir_all(&input)?;

        let expected: HashMap<&str, Vec<u8>> = HashMap::from([
            ("alpha.txt", b"alpha-alpha".to_vec()),
            ("beta.txt", b"beta-beta-beta".to_vec()),
            ("gamma.bin", vec![7u8; 32]),
        ]);

        for (path, contents) in &expected {
            fs::write(input.join(path), contents)?;
        }

        run(PackArgs {
            input: input.clone(),
            output: output.clone(),
            chunk_size: 8,
            level: 3,
            threads: None,
            dict: None,
            password_prompt: false,
            password: PasswordSourceArgs::default(),
            exclude: Vec::new(),
        })?;

        let archive = load_archive(&output, None)?;
        assert_eq!(archive.index.files.len(), expected.len());
        assert!(archive.index.chunks.len() >= 2);

        let dictionary = archive.dictionary.as_deref();
        let mut archive_file = File::open(&output)?;
        let mut raw_chunks = HashMap::new();

        for chunk in &archive.index.chunks {
            let dict = dictionary_for_chunk(chunk, dictionary)?;
            let raw = read_and_decode_chunk(
                &mut archive_file,
                chunk,
                dict,
                archive.encryption_key.as_ref(),
            )?;
            raw_chunks.insert(chunk.chunk_id, raw);
        }

        for entry in &archive.index.files {
            let chunk = raw_chunks
                .get(&entry.chunk_id)
                .expect("chunk payload should be present");
            let start = entry.offset_in_chunk as usize;
            let end = start + entry.original_size as usize;
            assert_eq!(
                &chunk[start..end],
                expected
                    .get(entry.path.as_str())
                    .expect("source payload should be present")
            );
        }

        Ok(())
    }
}
