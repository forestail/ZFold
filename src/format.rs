use crate::crypto::{FLAG_ENCRYPTED, SALT_LEN};
use anyhow::{ensure, Context, Result};
use std::io::{Cursor, Read, Write};

pub const HEADER_MAGIC: [u8; 4] = *b"HPK1";
pub const FOOTER_MAGIC: [u8; 4] = *b"HPKF";
pub const INDEX_MAGIC: [u8; 4] = *b"HPKI";
pub const ARCHIVE_VERSION: u16 = 1;
pub const FLAG_HAS_DICT: u16 = 0x0001;
pub const CODEC_ZSTD: u8 = 1;

const INDEX_VERSION: u16 = 1;
const INDEX_HEADER_SIZE: usize = 28;
const CHUNK_RECORD_SIZE: usize = 40;
const FILE_RECORD_SIZE: usize = 52;

#[derive(Debug, Clone)]
pub struct Header {
    pub magic: [u8; 4],
    pub version: u16,
    pub flags: u16,
    pub chunk_size: u32,
    pub reserved: [u8; 16],
}

impl Header {
    pub const SIZE: u64 = 28;

    pub fn new(
        chunk_size: u32,
        has_dictionary: bool,
        encryption_salt: Option<[u8; SALT_LEN]>,
    ) -> Self {
        let mut flags = if has_dictionary { FLAG_HAS_DICT } else { 0 };
        let reserved = match encryption_salt {
            Some(salt) => {
                flags |= FLAG_ENCRYPTED;
                salt
            }
            None => [0; SALT_LEN],
        };

        Self {
            magic: HEADER_MAGIC,
            version: ARCHIVE_VERSION,
            flags,
            chunk_size,
            reserved,
        }
    }

    pub fn has_dictionary(&self) -> bool {
        self.flags & FLAG_HAS_DICT != 0
    }

    pub fn is_encrypted(&self) -> bool {
        self.flags & FLAG_ENCRYPTED != 0
    }

    pub fn encryption_salt(&self) -> Option<[u8; SALT_LEN]> {
        self.is_encrypted().then_some(self.reserved)
    }

    pub fn write_to<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.magic)?;
        write_u16(writer, self.version)?;
        write_u16(writer, self.flags)?;
        write_u32(writer, self.chunk_size)?;
        writer.write_all(&self.reserved)?;
        Ok(())
    }

    pub fn read_from<R: Read>(reader: &mut R) -> Result<Self> {
        let mut magic = [0u8; 4];
        let mut reserved = [0u8; 16];

        reader.read_exact(&mut magic)?;
        let version = read_u16(reader)?;
        let flags = read_u16(reader)?;
        let chunk_size = read_u32(reader)?;
        reader.read_exact(&mut reserved)?;

        ensure!(magic == HEADER_MAGIC, "invalid archive header magic");
        ensure!(
            version == ARCHIVE_VERSION,
            "unsupported archive version {version}"
        );

        Ok(Self {
            magic,
            version,
            flags,
            chunk_size,
            reserved,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ChunkDesc {
    pub chunk_id: u32,
    pub file_offset: u64,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub file_count: u32,
    pub codec: u8,
    pub dict_id: u32,
}

impl ChunkDesc {
    fn write_binary<W: Write>(&self, writer: &mut W) -> Result<()> {
        write_u32(writer, self.chunk_id)?;
        write_u64(writer, self.file_offset)?;
        write_u64(writer, self.compressed_size)?;
        write_u64(writer, self.uncompressed_size)?;
        write_u32(writer, self.file_count)?;
        writer.write_all(&[self.codec])?;
        writer.write_all(&[0u8; 3])?;
        write_u32(writer, self.dict_id)?;
        Ok(())
    }

    fn read_binary<R: Read>(reader: &mut R) -> Result<Self> {
        let chunk_id = read_u32(reader)?;
        let file_offset = read_u64(reader)?;
        let compressed_size = read_u64(reader)?;
        let uncompressed_size = read_u64(reader)?;
        let file_count = read_u32(reader)?;
        let mut codec = [0u8; 1];
        let mut reserved = [0u8; 3];
        reader.read_exact(&mut codec)?;
        reader.read_exact(&mut reserved)?;
        let dict_id = read_u32(reader)?;

        Ok(Self {
            chunk_id,
            file_offset,
            compressed_size,
            uncompressed_size,
            file_count,
            codec: codec[0],
            dict_id,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub chunk_id: u32,
    pub offset_in_chunk: u64,
    pub original_size: u64,
    pub mtime_unix_ns: u64,
    pub mode: u32,
    pub crc32: u32,
    pub flags: u16,
}

#[derive(Debug, Clone, Default)]
pub struct Index {
    pub chunks: Vec<ChunkDesc>,
    pub files: Vec<FileEntry>,
}

impl Index {
    pub fn encode_binary(&self) -> Result<Vec<u8>> {
        let chunk_count =
            u32::try_from(self.chunks.len()).context("too many chunks for binary index")?;
        let file_count =
            u64::try_from(self.files.len()).context("too many files for binary index")?;

        let mut string_table = Vec::new();
        let mut file_records = Vec::with_capacity(self.files.len());

        for file in &self.files {
            let path_offset =
                u64::try_from(string_table.len()).context("string table exceeded u64")?;
            let path_len = u32::try_from(file.path.len()).context("file path is too long")?;
            string_table.extend_from_slice(file.path.as_bytes());
            file_records.push(FileRecord {
                path_offset,
                path_len,
                chunk_id: file.chunk_id,
                offset_in_chunk: file.offset_in_chunk,
                original_size: file.original_size,
                mtime_unix_ns: file.mtime_unix_ns,
                mode: file.mode,
                crc32: file.crc32,
                flags: file.flags,
            });
        }

        let string_table_size =
            u64::try_from(string_table.len()).context("string table exceeded u64")?;
        let mut out = Vec::with_capacity(
            INDEX_HEADER_SIZE
                + self.chunks.len() * CHUNK_RECORD_SIZE
                + self.files.len() * FILE_RECORD_SIZE
                + string_table.len(),
        );

        out.extend_from_slice(&INDEX_MAGIC);
        write_u16(&mut out, INDEX_VERSION)?;
        write_u16(&mut out, 0)?;
        write_u32(&mut out, chunk_count)?;
        write_u64(&mut out, file_count)?;
        write_u64(&mut out, string_table_size)?;

        for chunk in &self.chunks {
            chunk.write_binary(&mut out)?;
        }
        for record in &file_records {
            record.write_binary(&mut out)?;
        }
        out.extend_from_slice(&string_table);

        Ok(out)
    }

    pub fn decode_binary(blob: &[u8]) -> Result<Self> {
        let mut reader = Cursor::new(blob);
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        ensure!(magic == INDEX_MAGIC, "invalid binary index magic");

        let version = read_u16(&mut reader)?;
        ensure!(
            version == INDEX_VERSION,
            "unsupported binary index version {version}"
        );
        let _reserved = read_u16(&mut reader)?;
        let chunk_count = read_u32(&mut reader)?;
        let file_count = read_u64(&mut reader)?;
        let string_table_size = read_u64(&mut reader)?;

        let chunk_count_usize = usize::try_from(chunk_count).context("chunk count is too large")?;
        let file_count_usize = usize::try_from(file_count).context("file count is too large")?;

        let mut chunks = Vec::with_capacity(chunk_count_usize);
        for _ in 0..chunk_count_usize {
            chunks.push(ChunkDesc::read_binary(&mut reader)?);
        }

        let mut file_records = Vec::with_capacity(file_count_usize);
        for _ in 0..file_count_usize {
            file_records.push(FileRecord::read_binary(&mut reader)?);
        }

        let strings_start =
            usize::try_from(reader.position()).context("index record section exceeded usize")?;
        let string_table_size_usize =
            usize::try_from(string_table_size).context("string table size is too large")?;
        let strings_end = strings_start
            .checked_add(string_table_size_usize)
            .context("string table size overflowed")?;
        ensure!(
            strings_end == blob.len(),
            "binary index string table size mismatch"
        );
        let string_table = &blob[strings_start..strings_end];

        let mut files = Vec::with_capacity(file_count_usize);
        for record in file_records {
            let path_start =
                usize::try_from(record.path_offset).context("path offset is too large")?;
            let path_end = path_start
                .checked_add(record.path_len as usize)
                .context("path length overflowed")?;
            ensure!(
                path_end <= string_table.len(),
                "path points outside the string table"
            );

            let path = std::str::from_utf8(&string_table[path_start..path_end])
                .context("path in binary index is not valid UTF-8")?
                .to_owned();

            files.push(FileEntry {
                path,
                chunk_id: record.chunk_id,
                offset_in_chunk: record.offset_in_chunk,
                original_size: record.original_size,
                mtime_unix_ns: record.mtime_unix_ns,
                mode: record.mode,
                crc32: record.crc32,
                flags: record.flags,
            });
        }

        Ok(Self { chunks, files })
    }
}

#[derive(Debug, Clone)]
pub struct Footer {
    pub magic: [u8; 4],
    pub index_offset: u64,
    pub index_size: u64,
    pub dict_offset: u64,
    pub dict_size: u64,
    pub file_count: u64,
    pub chunk_count: u32,
    pub index_crc32: u32,
}

impl Footer {
    pub const SIZE: u64 = 52;

    pub fn metadata_offset(&self) -> u64 {
        if self.dict_size > 0 {
            self.dict_offset
        } else {
            self.index_offset
        }
    }

    pub fn write_to<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.magic)?;
        write_u64(writer, self.index_offset)?;
        write_u64(writer, self.index_size)?;
        write_u64(writer, self.dict_offset)?;
        write_u64(writer, self.dict_size)?;
        write_u64(writer, self.file_count)?;
        write_u32(writer, self.chunk_count)?;
        write_u32(writer, self.index_crc32)?;
        Ok(())
    }

    pub fn read_from<R: Read>(reader: &mut R) -> Result<Self> {
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        ensure!(magic == FOOTER_MAGIC, "invalid archive footer magic");

        Ok(Self {
            magic,
            index_offset: read_u64(reader)?,
            index_size: read_u64(reader)?,
            dict_offset: read_u64(reader)?,
            dict_size: read_u64(reader)?,
            file_count: read_u64(reader)?,
            chunk_count: read_u32(reader)?,
            index_crc32: read_u32(reader)?,
        })
    }
}

#[derive(Debug, Clone)]
struct FileRecord {
    path_offset: u64,
    path_len: u32,
    chunk_id: u32,
    offset_in_chunk: u64,
    original_size: u64,
    mtime_unix_ns: u64,
    mode: u32,
    crc32: u32,
    flags: u16,
}

impl FileRecord {
    fn write_binary<W: Write>(&self, writer: &mut W) -> Result<()> {
        write_u64(writer, self.path_offset)?;
        write_u32(writer, self.path_len)?;
        write_u32(writer, self.chunk_id)?;
        write_u64(writer, self.offset_in_chunk)?;
        write_u64(writer, self.original_size)?;
        write_u64(writer, self.mtime_unix_ns)?;
        write_u32(writer, self.mode)?;
        write_u32(writer, self.crc32)?;
        write_u16(writer, self.flags)?;
        write_u16(writer, 0)?;
        Ok(())
    }

    fn read_binary<R: Read>(reader: &mut R) -> Result<Self> {
        let path_offset = read_u64(reader)?;
        let path_len = read_u32(reader)?;
        let chunk_id = read_u32(reader)?;
        let offset_in_chunk = read_u64(reader)?;
        let original_size = read_u64(reader)?;
        let mtime_unix_ns = read_u64(reader)?;
        let mode = read_u32(reader)?;
        let crc32 = read_u32(reader)?;
        let flags = read_u16(reader)?;
        let _reserved = read_u16(reader)?;

        Ok(Self {
            path_offset,
            path_len,
            chunk_id,
            offset_in_chunk,
            original_size,
            mtime_unix_ns,
            mode,
            crc32,
            flags,
        })
    }
}

fn write_u16<W: Write>(writer: &mut W, value: u16) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u32<W: Write>(writer: &mut W, value: u32) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u64<W: Write>(writer: &mut W, value: u64) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u16<R: Read>(reader: &mut R) -> Result<u16> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32<R: Read>(reader: &mut R) -> Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64<R: Read>(reader: &mut R) -> Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::{ChunkDesc, FileEntry, Footer, Header, Index, ARCHIVE_VERSION, FOOTER_MAGIC};
    use anyhow::Result;

    #[test]
    fn binary_index_roundtrip_preserves_entries() -> Result<()> {
        let index = Index {
            chunks: vec![ChunkDesc {
                chunk_id: 7,
                file_offset: 128,
                compressed_size: 64,
                uncompressed_size: 100,
                file_count: 2,
                codec: 1,
                dict_id: 1,
            }],
            files: vec![
                FileEntry {
                    path: "docs/index.html".to_owned(),
                    chunk_id: 7,
                    offset_in_chunk: 0,
                    original_size: 40,
                    mtime_unix_ns: 11,
                    mode: 0,
                    crc32: 123,
                    flags: 0,
                },
                FileEntry {
                    path: "docs/about.html".to_owned(),
                    chunk_id: 7,
                    offset_in_chunk: 40,
                    original_size: 60,
                    mtime_unix_ns: 22,
                    mode: 0,
                    crc32: 456,
                    flags: 0,
                },
            ],
        };

        let blob = index.encode_binary()?;
        let decoded = Index::decode_binary(&blob)?;

        assert_eq!(decoded.chunks.len(), 1);
        assert_eq!(decoded.files.len(), 2);
        assert_eq!(decoded.files[0].path, "docs/index.html");
        assert_eq!(decoded.files[1].offset_in_chunk, 40);
        Ok(())
    }

    #[test]
    fn header_and_footer_are_v1() -> Result<()> {
        let header = Header::new(1024, true, Some([3u8; 16]));
        assert_eq!(header.version, ARCHIVE_VERSION);

        let footer = Footer {
            magic: FOOTER_MAGIC,
            index_offset: 100,
            index_size: 20,
            dict_offset: 80,
            dict_size: 20,
            file_count: 2,
            chunk_count: 1,
            index_crc32: 1234,
        };

        let mut bytes = Vec::new();
        footer.write_to(&mut bytes)?;
        assert_eq!(bytes.len() as u64, Footer::SIZE);
        Ok(())
    }
}
