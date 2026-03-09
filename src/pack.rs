use crate::cli::PackArgs;
use crate::crypto::{
    derive_key, encrypt_section, generate_salt, resolve_pack_password, SectionKind,
};
use crate::format::{ChunkDesc, FileEntry, Footer, Header, Index, CODEC_ZSTD, FOOTER_MAGIC};
use crate::scan::{scan_files, FileMeta};
use crate::util::{compress_buffer, configure_threads, progress_style};
use anyhow::{ensure, Context, Result};
use indicatif::ProgressBar;
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};

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

    let compressed_chunks: Result<Vec<_>> = groups
        .par_iter()
        .enumerate()
        .map(|(chunk_id, group)| {
            let chunk = compress_chunk(
                chunk_id as u32,
                group,
                args.level,
                dictionary.as_deref(),
                encryption.as_ref().map(|(_, key)| key),
            )?;
            progress.inc(1);
            Ok(chunk)
        })
        .collect();

    progress.finish_and_clear();
    let mut compressed_chunks = compressed_chunks?;
    compressed_chunks.sort_by_key(|chunk| chunk.chunk_id);

    let output = File::create(&args.output)
        .with_context(|| format!("failed to create {}", args.output.display()))?;
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

    for chunk in compressed_chunks {
        let file_offset = current_offset;
        writer.write_all(&chunk.compressed)?;
        current_offset += chunk.compressed.len() as u64;

        index.chunks.push(ChunkDesc {
            chunk_id: chunk.chunk_id,
            file_offset,
            compressed_size: chunk.compressed.len() as u64,
            uncompressed_size: chunk.uncompressed_size,
            file_count: chunk.entries.len() as u32,
            codec: CODEC_ZSTD,
            dict_id: if dictionary.is_some() { 1 } else { 0 },
        });
        index.files.extend(chunk.entries);
    }

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

    println!(
        "packed {} files into {} chunks ({} bytes) -> {}",
        index.files.len(),
        index.chunks.len(),
        current_offset,
        args.output.display()
    );

    Ok(())
}

#[derive(Debug)]
struct CompressedChunk {
    chunk_id: u32,
    compressed: Vec<u8>,
    entries: Vec<FileEntry>,
    uncompressed_size: u64,
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
