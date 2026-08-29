use crc32fast::Hasher as Crc32Hasher;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum SplitError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Checksum mismatch for chunk {chunk}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        chunk: u32,
        expected: String,
        actual: String,
    },
    #[error("Max retries ({0}) exceeded for chunk {1}")]
    MaxRetriesExceeded(u32, u32),
    #[error("Invalid split parameters: {0}")]
    InvalidParams(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChecksumAlgo {
    Crc32,
    Xxhash,
    Sha256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitConfig {
    pub chunk_size: u64,
    pub buffer_size: usize,
    pub checksum_algo: ChecksumAlgo,
    pub max_retries: u32,
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            // USBExtreme chunk size: 4GB - 1 byte (0xFFFF_FFFF)
            // But we use a practical size for FAT32 compatibility
            chunk_size: 4_294_967_295 - 1, // 0xFFFFFFFE
            buffer_size: 8 * 1024 * 1024,  // 8MB buffer
            checksum_algo: ChecksumAlgo::Crc32,
            max_retries: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkResult {
    pub index: u32,
    pub path: String,
    pub size: u64,
    pub checksum: String,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitResult {
    pub success: bool,
    pub chunks: Vec<ChunkResult>,
    pub total_size: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressInfo {
    pub phase: String,
    pub chunk: u32,
    pub total_chunks: u32,
    pub pct: u8,
    pub speed: String,
}

/// Split an ISO file into USBExtreme format chunks.
///
/// USBExtreme naming convention: ul.<game_id>
/// For multi-part: ul.<game_id>.00, ul.<game_id>.01, ...
///
/// Returns chunk info with checksums for verification.
pub fn split_iso<F>(
    source: &Path,
    dest_dir: &Path,
    game_id: &str,
    config: &SplitConfig,
    mut on_progress: F,
) -> Result<SplitResult, SplitError>
where
    F: FnMut(ProgressInfo),
{
    let file = File::open(source)?;
    let file_size = file.metadata()?.len();

    if file_size == 0 {
        return Err(SplitError::InvalidParams("Source file is empty".into()));
    }

    let total_chunks = ((file_size + config.chunk_size - 1) / config.chunk_size) as u32;
    let mut reader = BufReader::with_capacity(config.buffer_size, file);
    let mut chunks = Vec::new();

    for chunk_idx in 0..total_chunks {
        let chunk_file_name = if total_chunks == 1 {
            format!("ul.{}", game_id)
        } else {
            format!("ul.{:02}", chunk_idx)
        };
        let chunk_path = dest_dir.join(&chunk_file_name);

        let chunk_start = (chunk_idx as u64) * config.chunk_size;
        let chunk_end = (chunk_start + config.chunk_size).min(file_size);
        let chunk_size = chunk_end - chunk_start;

        // Write chunk with retry
        let checksum = write_chunk_with_retry(
            &mut reader,
            &chunk_path,
            chunk_idx,
            chunk_size,
            config,
            &mut on_progress,
            total_chunks,
        )?;

        let verified = verify_chunk(&chunk_path, &checksum, config)?;

        chunks.push(ChunkResult {
            index: chunk_idx,
            path: chunk_path.to_string_lossy().to_string(),
            size: chunk_size,
            checksum,
            verified,
        });

        on_progress(ProgressInfo {
            phase: "copy".into(),
            chunk: chunk_idx + 1,
            total_chunks,
            pct: ((chunk_idx + 1) * 100 / total_chunks) as u8,
            speed: String::new(), // caller calculates
        });
    }

    Ok(SplitResult {
        success: true,
        chunks,
        total_size: file_size,
        error: None,
    })
}

/// Write a single chunk with retry logic and checksum computation.
fn write_chunk_with_retry<F>(
    reader: &mut BufReader<File>,
    dest: &Path,
    chunk_idx: u32,
    chunk_size: u64,
    config: &SplitConfig,
    on_progress: &mut F,
    total_chunks: u32,
) -> Result<String, SplitError>
where
    F: FnMut(ProgressInfo),
{
    let mut attempt = 0;

    loop {
        attempt += 1;

        // Compute checksum while writing
        let checksum = write_chunk(reader, dest, chunk_size, config)?;

        // Verify the written chunk
        let verified = verify_chunk(dest, &checksum, config)?;

        if verified {
            return Ok(checksum);
        }

        if attempt >= config.max_retries {
            return Err(SplitError::MaxRetriesExceeded(
                config.max_retries,
                chunk_idx,
            ));
        }

        on_progress(ProgressInfo {
            phase: "retry".into(),
            chunk: chunk_idx,
            total_chunks,
            pct: 0,
            speed: format!("Retry {}/{}", attempt, config.max_retries),
        });

        // Seek reader back to start of this chunk for retry
        let chunk_start = (chunk_idx as u64) * config.chunk_size;
        reader.seek(SeekFrom::Start(chunk_start))?;
    }
}

/// Write a chunk from reader to file, computing checksum as we go.
fn write_chunk(
    reader: &mut BufReader<File>,
    dest: &Path,
    chunk_size: u64,
    config: &SplitConfig,
) -> Result<String, SplitError> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dest)?;
    let mut writer = BufWriter::with_capacity(config.buffer_size, file);

    let mut hasher = match config.checksum_algo {
        ChecksumAlgo::Crc32 => ChunkHasher::Crc32(Crc32Hasher::new()),
        ChecksumAlgo::Xxhash => ChunkHasher::Xxhash(0),
        ChecksumAlgo::Sha256 => ChunkHasher::Sha256(Sha256::new()),
    };

    let mut remaining = chunk_size;
    let mut buffer = vec![0u8; config.buffer_size];

    while remaining > 0 {
        let to_read = (remaining as usize).min(buffer.len());
        let bytes_read = reader.read(&mut buffer[..to_read])?;
        if bytes_read == 0 {
            break;
        }

        let data = &buffer[..bytes_read];
        writer.write_all(data)?;

        match &mut hasher {
            ChunkHasher::Crc32(h) => h.update(data),
            ChunkHasher::Xxhash(state) => {
                *state = xxhash_rust::xxh3::xxh3_64(data);
            }
            ChunkHasher::Sha256(h) => h.update(data),
        }

        remaining -= bytes_read as u64;
    }

    writer.flush()?;

    Ok(match hasher {
        ChunkHasher::Crc32(h) => format!("{:08x}", h.finalize()),
        ChunkHasher::Xxhash(state) => format!("{:016x}", state),
        ChunkHasher::Sha256(h) => hex::encode(h.finalize()),
    })
}

enum ChunkHasher {
    Crc32(Crc32Hasher),
    Xxhash(u64),
    Sha256(Sha256),
}

/// Verify a written chunk by re-reading and comparing checksum.
fn verify_chunk(path: &Path, expected_checksum: &str, config: &SplitConfig) -> Result<bool, SplitError> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(config.buffer_size, file);

    let mut hasher = match config.checksum_algo {
        ChecksumAlgo::Crc32 => ChunkHasher::Crc32(Crc32Hasher::new()),
        ChecksumAlgo::Xxhash => ChunkHasher::Xxhash(0),
        ChecksumAlgo::Sha256 => ChunkHasher::Sha256(Sha256::new()),
    };

    let mut buffer = vec![0u8; config.buffer_size];
    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        let data = &buffer[..bytes_read];
        match &mut hasher {
            ChunkHasher::Crc32(h) => h.update(data),
            ChunkHasher::Xxhash(state) => {
                *state = xxhash_rust::xxh3::xxh3_64(data);
            }
            ChunkHasher::Sha256(h) => h.update(data),
        }
    }

    let computed = match hasher {
        ChunkHasher::Crc32(h) => format!("{:08x}", h.finalize()),
        ChunkHasher::Xxhash(state) => format!("{:016x}", state),
        ChunkHasher::Sha256(h) => hex::encode(h.finalize()),
    };

    Ok(computed == expected_checksum)
}

/// Copy ISO as-is (no-split mode for NTFS/exFAT).
pub fn copy_iso_nosplit<F>(
    source: &Path,
    dest_dir: &Path,
    game_id: &str,
    config: &SplitConfig,
    mut on_progress: F,
) -> Result<SplitResult, SplitError>
where
    F: FnMut(ProgressInfo),
{
    let dest_path = dest_dir.join(format!("ul.{}", game_id));

    let file = File::open(source)?;
    let file_size = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(config.buffer_size, file);

    let checksum = write_chunk(&mut reader, &dest_path, file_size, config)?;

    on_progress(ProgressInfo {
        phase: "copy".into(),
        chunk: 1,
        total_chunks: 1,
        pct: 100,
        speed: String::new(),
    });

    let verified = verify_chunk(&dest_path, &checksum, config)?;

    Ok(SplitResult {
        success: true,
        chunks: vec![ChunkResult {
            index: 0,
            path: dest_path.to_string_lossy().to_string(),
            size: file_size,
            checksum,
            verified,
        }],
        total_size: file_size,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_config_default() {
        let config = SplitConfig::default();
        assert_eq!(config.chunk_size, 4_294_967_294);
        assert_eq!(config.buffer_size, 8 * 1024 * 1024);
        assert_eq!(config.max_retries, 3);
    }
}
