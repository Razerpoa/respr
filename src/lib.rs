use reed_solomon_erasure::galois_8::ReedSolomon;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

// ============================================================================
// 0. DATA TRANSFORMATION / OBFUSCATION HOOKS
// ============================================================================

pub fn pack(data: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let compressed = zstd::encode_all(data, 0)?;
    Ok(compressed)
}

pub fn unpack(data: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let compressed = zstd::decode_all(data)?;
    Ok(compressed)
}

// ============================================================================
// 1. MANIFEST & METADATA STRUCTURES
// ============================================================================

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SegmentMeta {
    pub index: usize,
    pub path: PathBuf,
    pub offset: u64,   // Offset where payload begins (0 for dedicated files)
    pub length: usize, // Payload size in bytes
    pub crc32: u32,    // Checksum for payload validation
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub orig_len: usize, // Size of packed payload before RS padding
    pub k: usize,
    pub m: usize,
    pub segments: Vec<SegmentMeta>,
}

impl Manifest {
    pub fn save_to_file(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string(self)?;
        let packed = pack(json.as_bytes())?;
        let mut file = File::create(path)?;
        file.write_all(&packed)?;
        file.flush()?;
        Ok(())
    }

    pub fn load_from_file(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read(path)?;
        let unpacked = unpack(&content)?;
        let json = String::from_utf8_lossy_owned(unpacked);
        let manifest: Manifest = serde_json::from_str(&json)?;
        Ok(manifest)
    }
}

// ============================================================================
// 2. CORE ENCODING / DECODING ENGINE (PURE MATH)
// ============================================================================

pub struct RedundancyEngine;

impl RedundancyEngine {
    /// Packs the input data, then splits and encodes it using Reed-Solomon.
    /// Returns the encoded shards and the packed data length.
    pub fn encode(
        data: &[u8],
        k: usize,
        m: usize,
    ) -> Result<(Vec<Vec<u8>>, usize), Box<dyn std::error::Error>> {
        // Apply packing (obfuscation/compression/encryption hook)
        let packed_data = pack(data)?;
        let packed_len = packed_data.len();

        let total_shards = k + m;
        let encoder = ReedSolomon::new(k, m)?;

        let shard_size = if packed_len == 0 {
            1
        } else {
            (packed_len + k - 1) / k
        };

        let mut shards: Vec<Vec<u8>> = vec![vec![0u8; shard_size]; total_shards];
        for (i, chunk) in packed_data.chunks(shard_size).enumerate() {
            shards[i][..chunk.len()].copy_from_slice(chunk);
        }

        encoder.encode(&mut shards)?;
        Ok((shards, packed_len))
    }

    /// Reconstructs the packed data from shards, then unpacks it back to original data.
    pub fn decode(
        shards_map: &HashMap<usize, Vec<u8>>,
        k: usize,
        m: usize,
        packed_len: usize,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let total_shards = k + m;
        let mut shards: Vec<Option<Vec<u8>>> = vec![None; total_shards];

        for (&idx, payload) in shards_map {
            if idx < total_shards {
                shards[idx] = Some(payload.clone());
            }
        }

        let encoder = ReedSolomon::new(k, m)?;
        encoder.reconstruct(&mut shards)?;

        let mut reconstructed_packed = Vec::with_capacity(packed_len);
        let mut written = 0;

        for i in 0..k {
            if let Some(shard) = &shards[i] {
                let to_write = std::cmp::min(shard.len(), packed_len - written);
                reconstructed_packed.extend_from_slice(&shard[..to_write]);
                written += to_write;
            }
        }

        // Apply unpacking (de-obfuscation/decompression/decryption hook)
        let original_data = unpack(&reconstructed_packed)?;
        Ok(original_data)
    }
}

// ============================================================================
// 3. MANIFEST-BASED SPLITTER
// ============================================================================

pub struct ManifestSplitter;

impl ManifestSplitter {
    /// MODE 1: Write standalone raw segment files into a directory and produce manifest.json
    pub fn spread_to_directory(
        data: &[u8],
        output_dir: &Path,
        k: usize,
        m: usize,
    ) -> Result<Manifest, Box<dyn std::error::Error>> {
        fs::create_dir_all(output_dir)?;
        let (shards, packed_len) = RedundancyEngine::encode(data, k, m)?;
        let mut segment_metas = Vec::new();

        for (idx, shard) in shards.into_iter().enumerate() {
            let file_name = format!("segment_{}.bin", idx);
            let file_path = output_dir.join(&file_name);

            let mut file = File::create(&file_path)?;
            file.write_all(&shard)?;
            file.flush()?;

            segment_metas.push(SegmentMeta {
                index: idx,
                path: fs::canonicalize(file_path)?,
                offset: 0,
                length: shard.len(),
                crc32: crc32fast::hash(&shard),
            });
        }

        let manifest = Manifest {
            orig_len: packed_len,
            k,
            m,
            segments: segment_metas,
        };

        manifest.save_to_file(&output_dir.join("manifest.json"))?;
        Ok(manifest)
    }

    /// MODE 2: Append raw segment payloads to existing host files and produce manifest.json
    pub fn spread_to_targets<P: AsRef<Path>>(
        data: &[u8],
        target_files: &[P],
        manifest_output_path: &Path,
        k: usize,
        m: usize,
    ) -> Result<Manifest, Box<dyn std::error::Error>> {
        let target_files = target_files;
        let total_shards = k + m;
        if target_files.len() < total_shards {
            return Err(format!(
                "Insufficient target files: required {}, provided {}",
                total_shards,
                target_files.len()
            )
            .into());
        }

        let (shards, packed_len) = RedundancyEngine::encode(data, k, m)?;
        let mut segment_metas = Vec::new();

        for (idx, shard) in shards.into_iter().enumerate() {
            let target_path = &target_files[idx];

            // Open target file and determine current size (offset)
            let mut file = OpenOptions::new().append(true).open(target_path)?;
            let offset = file.metadata()?.len();

            file.write_all(&shard)?;
            file.flush()?;

            segment_metas.push(SegmentMeta {
                index: idx,
                path: fs::canonicalize(target_path)?,
                offset,
                length: shard.len(),
                crc32: crc32fast::hash(&shard),
            });
        }

        let manifest = Manifest {
            orig_len: packed_len,
            k,
            m,
            segments: segment_metas,
        };

        if manifest_output_path.is_dir() {
            let new_output = manifest_output_path.join("manifest.json");
            File::create(&new_output)?;
            manifest.save_to_file(&new_output)?;
        }
        if manifest_output_path.is_file() {
            if !manifest_output_path.exists() {
                File::create(&manifest_output_path)?;
            }
            manifest.save_to_file(manifest_output_path)?;
        }
        Ok(manifest)
    }

    /// Wrapper for construct_from_manifest using path
    pub fn construct_from_path(
        manifest_path: &Path,
        dest: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let manifest = Manifest::load_from_file(manifest_path)?;
        ManifestSplitter::construct_from_manifest(&manifest, dest)
    }

    /// Reconstructs original data using a Manifest data.
    pub fn construct_from_manifest(
        manifest: &Manifest,
        dest: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut valid_shards: HashMap<usize, Vec<u8>> = HashMap::new();

        for meta in &manifest.segments {
            let mut file = match File::open(&meta.path) {
                Ok(f) => f,
                Err(_) => continue, // Missing file or target host
            };

            // Seek to offset
            if file.seek(SeekFrom::Start(meta.offset)).is_err() {
                continue;
            }

            let mut payload = vec![0u8; meta.length];
            if file.read_exact(&mut payload).is_err() {
                continue; // Truncated payload
            }

            // Integrity Check
            if crc32fast::hash(&payload) != meta.crc32 {
                eprintln!(
                    "[Warning] Corrupt checksum for segment {} at {:?}",
                    meta.index, meta.path
                );
                continue;
            }

            valid_shards.insert(meta.index, payload);
        }

        let reconstructed = match RedundancyEngine::decode(
            &valid_shards,
            manifest.k,
            manifest.m,
            manifest.orig_len,
        ) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Valid Shards: {}", valid_shards.len());
                return Err(e);
            }
        };

        let mut dest_file = File::create(dest)?;
        dest_file.write_all(&reconstructed)?;
        dest_file.flush()?;

        Ok(())
    }

    /// Delete the segments from a given manifest
    pub fn delete_from_manifest(manifest: &Manifest) {
        for meta in &manifest.segments {
            let mut file = match OpenOptions::new().write(true).read(true).open(&meta.path) {
                Ok(f) => f,
                Err(_) => continue, // Missing file or target host
            };

            // Seek to offset
            if file.seek(SeekFrom::Start(meta.offset)).is_err() {
                continue;
            }

            let mut payload = vec![0u8; meta.length];
            if file.read_exact(&mut payload).is_err() {
                continue; // Truncated payload
            }

            // Integrity Check
            if crc32fast::hash(&payload) != meta.crc32 {
                eprintln!(
                    "[Warning] Corrupt checksum for segment {} at {:?}",
                    meta.index, meta.path
                );
                continue;
            }

            match file.set_len(meta.offset) {
                Ok(()) => (),
                Err(e) => {
                    eprintln!("[ERROR] Failed to truncate file: {}", e);
                }
            };
        }
    }
}

// ============================================================================
// 4. AUTOMATED TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_directory_manifest_flow() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let seg_dir = dir.path().join("segments");
        let dest_file = dir.path().join("reconstructed.txt");
        let payload = b"Manifest-based directory mode testing.";

        // K=3, M=2 (5 total segments)
        let manifest = ManifestSplitter::spread_to_directory(payload, &seg_dir, 3, 2)?;
        assert_eq!(manifest.segments.len(), 5);

        // Delete 2 segments to test fault-tolerance (3 left >= K)
        fs::remove_file(&manifest.segments[0].path)?;
        fs::remove_file(&manifest.segments[4].path)?;

        ManifestSplitter::construct_from_path(&seg_dir.join("manifest.json"), &dest_file)?;
        assert_eq!(fs::read(&dest_file)?, payload);

        Ok(())
    }

    #[test]
    fn test_target_append_manifest_flow() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let manifest_path = dir.path().join("manifest.json");
        let dest_file = dir.path().join("reconstructed.txt");
        let payload = b"Manifest-based target append mode testing.";

        // Create 4 dummy host files
        let mut target_paths = Vec::new();
        for i in 0..4 {
            let path = dir.path().join(format!("host_{}.mp3", i));
            fs::write(&path, b"ORIGINAL_MP3_AUDIO_HEADER_DATA")?;
            target_paths.push(path);
        }

        // K=2, M=2 (Appends to 4 target files)
        ManifestSplitter::spread_to_targets(payload, &target_paths, &manifest_path, 2, 2)?;

        // Simulate deleting 1 host file entirely
        fs::remove_file(&target_paths[0])?;

        ManifestSplitter::construct_from_path(&manifest_path, &dest_file)?;
        assert_eq!(fs::read(&dest_file)?, payload);

        Ok(())
    }
}
