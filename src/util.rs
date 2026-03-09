use crate::crypto::{decrypt_section, derive_key, EncryptionKey, SectionKind};
use crate::format::{ChunkDesc, Footer, Header, Index, CODEC_ZSTD};
use anyhow::{bail, ensure, Context, Result};
use indicatif::ProgressStyle;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ArchiveMetadata {
    pub header: Header,
    pub footer: Footer,
    pub index: Index,
    pub dictionary: Option<Vec<u8>>,
    pub encryption_key: Option<EncryptionKey>,
    pub archive_size: u64,
}

pub fn configure_threads(threads: Option<usize>) -> Result<()> {
    if let Some(num_threads) = threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build_global()
            .context("failed to configure rayon thread pool")?;
    }
    Ok(())
}

pub fn read_header(path: &Path) -> Result<Header> {
    let file =
        File::open(path).with_context(|| format!("failed to open archive {}", path.display()))?;
    let mut reader = BufReader::new(file);
    Header::read_from(&mut reader)
}

pub fn load_archive(path: &Path, password: Option<&str>) -> Result<ArchiveMetadata> {
    let file =
        File::open(path).with_context(|| format!("failed to open archive {}", path.display()))?;
    let archive_size = file.metadata()?.len();
    let mut reader = BufReader::new(file);

    ensure!(
        archive_size >= Header::SIZE + Footer::SIZE,
        "archive is too small to contain a valid header and footer"
    );

    let header = Header::read_from(&mut reader)?;
    let encryption_key = match header.encryption_salt() {
        Some(salt) => {
            let password = password.context("archive is encrypted; provide a password source")?;
            Some(derive_key(password, &salt)?)
        }
        None => None,
    };

    reader.seek(SeekFrom::End(-(Footer::SIZE as i64)))?;
    let footer = Footer::read_from(&mut reader)?;

    let index_end = footer
        .index_offset
        .checked_add(footer.index_size)
        .context("archive index offset overflowed")?;
    ensure!(
        index_end + Footer::SIZE == archive_size,
        "archive footer does not match index location"
    );

    if footer.dict_size > 0 {
        let dict_end = footer
            .dict_offset
            .checked_add(footer.dict_size)
            .context("dictionary blob offset overflowed")?;
        ensure!(
            dict_end == footer.index_offset,
            "dictionary blob must sit immediately before the index"
        );
        ensure!(
            footer.dict_offset >= Header::SIZE,
            "dictionary blob overlaps the archive header"
        );
    } else {
        ensure!(
            footer.dict_offset == 0,
            "dictionary offset must be zero when no dictionary is present"
        );
    }

    ensure!(
        footer.metadata_offset() >= Header::SIZE,
        "archive metadata overlaps the header"
    );

    let index_len = usize::try_from(footer.index_size).context("archive index is too large")?;
    let mut index_blob = vec![0u8; index_len];
    reader.seek(SeekFrom::Start(footer.index_offset))?;
    reader.read_exact(&mut index_blob)?;
    ensure!(
        crc32fast::hash(&index_blob) == footer.index_crc32,
        "archive index crc32 mismatch"
    );

    if let Some(key) = encryption_key.as_ref() {
        index_blob = decrypt_section(&index_blob, key, SectionKind::Index, 0)
            .context("failed to decrypt archive index")?;
    }

    let index = Index::decode_binary(&index_blob)?;
    let dictionary = if footer.dict_size > 0 {
        let dict_len = usize::try_from(footer.dict_size).context("dictionary blob is too large")?;
        let mut bytes = vec![0u8; dict_len];
        reader.seek(SeekFrom::Start(footer.dict_offset))?;
        reader.read_exact(&mut bytes)?;
        if let Some(key) = encryption_key.as_ref() {
            Some(
                decrypt_section(&bytes, key, SectionKind::Dictionary, 0)
                    .context("failed to decrypt archive dictionary blob")?,
            )
        } else {
            Some(bytes)
        }
    } else {
        None
    };

    if header.has_dictionary() {
        ensure!(
            dictionary.is_some(),
            "archive header says a dictionary is present, but no dictionary blob was found"
        );
    } else {
        ensure!(
            dictionary.is_none(),
            "archive contains a dictionary blob, but the header flag is not set"
        );
    }

    ensure!(
        footer.file_count == index.files.len() as u64,
        "archive footer file count does not match index"
    );
    ensure!(
        footer.chunk_count == index.chunks.len() as u32,
        "archive footer chunk count does not match index"
    );

    Ok(ArchiveMetadata {
        header,
        footer,
        index,
        dictionary,
        encryption_key,
        archive_size,
    })
}

pub fn compress_buffer(data: &[u8], level: i32, dictionary: Option<&[u8]>) -> Result<Vec<u8>> {
    match dictionary {
        Some(dict_bytes) => {
            let mut encoder = zstd::stream::Encoder::with_dictionary(Vec::new(), level, dict_bytes)
                .context("failed to create zstd encoder with dictionary")?;
            encoder.write_all(data)?;
            encoder.finish().context("failed to finish zstd encoding")
        }
        None => zstd::stream::encode_all(Cursor::new(data), level)
            .context("failed to encode chunk with zstd"),
    }
}

pub fn decode_buffer(data: &[u8], dictionary: Option<&[u8]>) -> Result<Vec<u8>> {
    match dictionary {
        Some(dict_bytes) => {
            let mut decoder = zstd::stream::Decoder::with_dictionary(Cursor::new(data), dict_bytes)
                .context("failed to create zstd decoder with dictionary")?;
            let mut raw = Vec::new();
            decoder.read_to_end(&mut raw)?;
            Ok(raw)
        }
        None => {
            zstd::stream::decode_all(Cursor::new(data)).context("failed to decode chunk with zstd")
        }
    }
}

pub fn read_chunk(file: &mut File, chunk: &ChunkDesc) -> Result<Vec<u8>> {
    ensure!(
        chunk.codec == CODEC_ZSTD,
        "unsupported codec {} in chunk {}",
        chunk.codec,
        chunk.chunk_id
    );

    let compressed_len =
        usize::try_from(chunk.compressed_size).context("chunk compressed size is too large")?;
    let mut buf = vec![0u8; compressed_len];
    file.seek(SeekFrom::Start(chunk.file_offset))?;
    file.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn read_and_decode_chunk(
    file: &mut File,
    chunk: &ChunkDesc,
    dictionary: Option<&[u8]>,
    encryption_key: Option<&EncryptionKey>,
) -> Result<Vec<u8>> {
    let blob = read_chunk(file, chunk)?;
    let compressed = match encryption_key {
        Some(key) => decrypt_section(&blob, key, SectionKind::Chunk, chunk.chunk_id as u64)
            .with_context(|| format!("failed to decrypt chunk {}", chunk.chunk_id))?,
        None => blob,
    };
    let raw = decode_buffer(&compressed, dictionary)?;
    ensure!(
        raw.len() as u64 == chunk.uncompressed_size,
        "chunk {} decompressed to {} bytes, expected {}",
        chunk.chunk_id,
        raw.len(),
        chunk.uncompressed_size
    );
    Ok(raw)
}

pub fn dictionary_for_chunk<'a>(
    chunk: &ChunkDesc,
    dictionary: Option<&'a [u8]>,
) -> Result<Option<&'a [u8]>> {
    if chunk.dict_id == 0 {
        return Ok(None);
    }

    match dictionary {
        Some(bytes) => Ok(Some(bytes)),
        None => bail!(
            "chunk {} requires dictionary id {}, but no embedded dictionary was found",
            chunk.chunk_id,
            chunk.dict_id
        ),
    }
}

pub fn safe_output_path(base: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    ensure!(
        !rel_path.is_absolute(),
        "archive entry must not be an absolute path: {rel}"
    );

    let mut sanitized = PathBuf::new();
    for component in rel_path.components() {
        match component {
            Component::Normal(part) => sanitized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                bail!("archive entry path escapes the output directory: {rel}");
            }
        }
    }

    ensure!(
        !sanitized.as_os_str().is_empty(),
        "archive entry resolved to an empty path"
    );

    Ok(base.join(sanitized))
}

pub fn progress_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_bar())
}

#[cfg(test)]
mod tests {
    use super::safe_output_path;
    use anyhow::Result;
    use std::path::Path;

    #[test]
    fn safe_output_path_rejects_parent_components() {
        let base = Path::new("out");
        let err = safe_output_path(base, "../escape.txt").unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn safe_output_path_keeps_normal_paths() -> Result<()> {
        let base = Path::new("out");
        let joined = safe_output_path(base, "docs/index.html")?;
        assert_eq!(joined, Path::new("out").join("docs").join("index.html"));
        Ok(())
    }
}
