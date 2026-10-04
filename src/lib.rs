use reed_solomon_erasure::galois_8::ReedSolomon;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
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
    /// Location of the host, stored relative to `Manifest::root`.
    pub path: PathBuf,
    pub offset: u64,   // Offset where payload begins (0 for dedicated files)
    pub length: usize, // Payload size in bytes
    pub crc32: u32,    // Checksum for payload validation
}

impl SegmentMeta {
    /// Absolute location of this segment's payload.
    ///
    /// A relative `path` is joined onto `root`; an absolute `path` is used
    /// as-is, which keeps the join well-defined regardless of manifest age.
    pub fn resolve(&self, root: &Path) -> PathBuf {
        if self.path.is_absolute() {
            self.path.clone()
        } else {
            root.join(&self.path)
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub orig_len: usize, // Size of packed payload before RS padding
    pub k: usize,
    pub m: usize,
    /// Absolute directory the hosts were hidden into, recorded at hide time.
    ///
    /// Segment paths are relative to this. When the tree is relocated the
    /// recorded value goes stale, so `restore`/`clean` accept a `--root`
    /// override and fall back to the manifest's own directory.
    #[serde(default)]
    pub root: PathBuf,
    /// File name of the original hidden file, used to name the restored
    /// output. Recorded but never trusted for path resolution.
    #[serde(default)]
    pub source_name: Option<String>,
    pub segments: Vec<SegmentMeta>,
}

/// Reject segment paths that are not plain relative paths.
///
/// A manifest drives destructive truncation in `delete_from_manifest`, so
/// `..` and absolute paths must never be honoured. Manifests written by
/// `hide` always store paths relative to `Manifest::root`.
fn validate_segment_path(index: usize, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(format!(
                    "segment {} has a non-relative path {:?}: only plain relative paths are \
                     accepted. Manifests written before the `root` field existed must be re-hidden.",
                    index, path
                )
                .into());
            }
        }
    }
    Ok(())
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
        for meta in &manifest.segments {
            validate_segment_path(meta.index, &meta.path)?;
        }
        Ok(manifest)
    }

    /// Pick the directory that segment paths are resolved against.
    ///
    /// Priority: explicit override, then the recorded root while it still
    /// exists, then the directory holding the manifest. The result is not
    /// guaranteed to exist — callers report how many segments resolved.
    pub fn pick_root(&self, override_root: Option<&Path>, manifest_dir: &Path) -> PathBuf {
        if let Some(root) = override_root {
            return root.to_path_buf();
        }

        if !self.root.as_os_str().is_empty() {
            if self.root.is_dir() {
                return self.root.clone();
            }
            eprintln!(
                "[Warning] Recorded root {:?} no longer exists (tree moved?). \
                 Re-run with --root <folder containing the hosts>.",
                self.root
            );
        }

        manifest_dir.to_path_buf()
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
        source_name: Option<&str>,
        k: usize,
        m: usize,
    ) -> Result<Manifest, Box<dyn std::error::Error>> {
        fs::create_dir_all(output_dir)?;
        let root = fs::canonicalize(output_dir)?;
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
                path: PathBuf::from(&file_name),
                offset: 0,
                length: shard.len(),
                crc32: crc32fast::hash(&shard),
            });
        }

        let manifest = Manifest {
            orig_len: packed_len,
            k,
            m,
            root: root.clone(),
            source_name: source_name.map(str::to_owned),
            segments: segment_metas,
        };

        manifest.save_to_file(&output_dir.join("manifest.json"))?;
        Ok(manifest)
    }

    /// MODE 2: Append raw segment payloads to existing host files and produce manifest.json
    ///
    /// `root_dir` is recorded in the manifest as the anchor for the relative
    /// host paths, so that whole tree can later be relocated and recovered
    /// with a `--root` override.
    pub fn spread_to_targets<P: AsRef<Path>>(
        data: &[u8],
        target_files: &[P],
        root_dir: &Path,
        manifest_output_path: &Path,
        source_name: Option<&str>,
        k: usize,
        m: usize,
    ) -> Result<Manifest, Box<dyn std::error::Error>> {
        let total_shards = k + m;
        if target_files.len() < total_shards {
            return Err(format!(
                "Insufficient target files: required {}, provided {}",
                total_shards,
                target_files.len()
            )
            .into());
        }

        let root = fs::canonicalize(root_dir).unwrap_or_else(|_| root_dir.to_path_buf());
        let (shards, packed_len) = RedundancyEngine::encode(data, k, m)?;
        let mut segment_metas = Vec::new();

        for (idx, shard) in shards.into_iter().enumerate() {
            let target_path = target_files[idx].as_ref();

            // Open target file and determine current size (offset)
            let mut file = OpenOptions::new().append(true).open(target_path)?;
            let offset = file.metadata()?.len();

            file.write_all(&shard)?;
            file.flush()?;

            let absolute = fs::canonicalize(target_path)?;
            let relative = absolute.strip_prefix(&root).map_err(|_| {
                format!(
                    "target {:?} is not inside root {:?}, so it cannot be recorded as a \
                     relative path; pick a root that contains every host",
                    absolute, root
                )
            })?;

            segment_metas.push(SegmentMeta {
                index: idx,
                path: relative.to_path_buf(),
                offset,
                length: shard.len(),
                crc32: crc32fast::hash(&shard),
            });
        }

        let manifest = Manifest {
            orig_len: packed_len,
            k,
            m,
            root: root.clone(),
            source_name: source_name.map(str::to_owned),
            segments: segment_metas,
        };

        // A directory receives `manifest.json` inside it; anything else is
        // treated as the file path itself, created along with any missing
        // parent directories.
        let manifest_path = if manifest_output_path.is_dir() {
            manifest_output_path.join("manifest.json")
        } else {
            manifest_output_path.to_path_buf()
        };
        if let Some(parent) = manifest_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        manifest.save_to_file(&manifest_path)?;
        Ok(manifest)
    }

    /// Wrapper for construct_from_manifest that resolves the root itself.
    pub fn construct_from_path(
        manifest_path: &Path,
        root_override: Option<&Path>,
        dest: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let manifest = Manifest::load_from_file(manifest_path)?;
        let manifest_dir = manifest_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let root = manifest.pick_root(root_override, &manifest_dir);
        ManifestSplitter::construct_from_manifest(&manifest, &root, dest)
    }

    /// Resolve the destination file for a restored payload.
    ///
    /// `dest` is a directory: it is created if missing, and the file is
    /// written inside it under the name recorded at hide time. Only the file
    /// name component of that record is used, never any directory part.
    pub fn restore_destination(
        manifest: &Manifest,
        dest: &Path,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        if !dest.exists() {
            fs::create_dir_all(dest)?;
        } else if !dest.is_dir() {
            return Err(format!(
                "restore destination {:?} exists and is not a directory",
                dest
            )
            .into());
        }

        let name = manifest
            .source_name
            .as_deref()
            .and_then(|n| Path::new(n).file_name())
            .filter(|n| !n.is_empty())
            .ok_or("manifest has no recorded source file name")?;

        Ok(dest.join(name))
    }

    /// Reconstructs original data using a Manifest and the root its segment
    /// paths are relative to.
    pub fn construct_from_manifest(
        manifest: &Manifest,
        root: &Path,
        dest: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut valid_shards: HashMap<usize, Vec<u8>> = HashMap::new();
        let mut located = 0usize;

        for meta in &manifest.segments {
            let path = meta.resolve(root);
            if path.exists() {
                located += 1;
            }

            let mut file = match File::open(&path) {
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
                    meta.index, path
                );
                continue;
            }

            valid_shards.insert(meta.index, payload);
        }

        eprintln!(
            "[Info] Located {}/{} segments under {:?} (k={}, m={}, {} verified)",
            located,
            manifest.segments.len(),
            root,
            manifest.k,
            manifest.m,
            valid_shards.len()
        );

        // Surface a wrong/missing root as itself rather than letting the
        // erasure decoder report it as an abstract shard shortage.
        if located < manifest.k {
            return Err(format!(
                "Only {} of {} segments found under {:?}, but at least k={} are required. \
                 Pass --root <folder containing the hosts> if the tree has moved.",
                located,
                manifest.segments.len(),
                root,
                manifest.k
            )
            .into());
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

        let out_path = ManifestSplitter::restore_destination(manifest, dest)?;
        let mut dest_file = File::create(&out_path)?;
        dest_file.write_all(&reconstructed)?;
        dest_file.flush()?;

        Ok(())
    }

    /// Delete the segments from a given manifest
    pub fn delete_from_manifest(manifest: &Manifest, root: &Path) {
        let mut cleaned = 0usize;

        for meta in &manifest.segments {
            let path = meta.resolve(root);
            let mut file = match OpenOptions::new().write(true).read(true).open(&path) {
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
                    meta.index, path
                );
                continue;
            }

            match file.set_len(meta.offset) {
                Ok(()) => cleaned += 1,
                Err(e) => {
                    eprintln!("[ERROR] Failed to truncate file: {}", e);
                }
            };
        }

        eprintln!(
            "[Info] Cleaned {}/{} segments under {:?}",
            cleaned,
            manifest.segments.len(),
            root
        );
    }
}

// ============================================================================
// 4. AUTOMATED TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Spread `payload` across `count` dummy hosts inside `root`, recording
    /// `root` as the manifest anchor.
    fn hide_into_root(
        payload: &[u8],
        root: &Path,
        manifest_path: &Path,
        k: usize,
        m: usize,
    ) -> Result<Manifest, Box<dyn std::error::Error>> {
        fs::create_dir_all(root)?;
        let mut targets = Vec::new();
        for i in 0..(k + m) {
            let path = root.join(format!("host_{}.dll", i));
            fs::write(&path, b"ORIGINAL_HOST_CONTENT")?;
            targets.push(path);
        }
        ManifestSplitter::spread_to_targets(
            payload,
            &targets,
            root,
            manifest_path,
            Some("payload.bin"),
            k,
            m,
        )
    }

    #[test]
    fn test_directory_manifest_flow() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let seg_dir = dir.path().join("segments");
        let out_dir = dir.path().join("restored");
        let payload = b"Manifest-based directory mode testing.";

        // K=3, M=2 (5 total segments)
        let manifest =
            ManifestSplitter::spread_to_directory(payload, &seg_dir, Some("payload.bin"), 3, 2)?;
        assert_eq!(manifest.segments.len(), 5);

        // Segment paths are stored relative to the recorded root.
        assert_eq!(manifest.root, fs::canonicalize(&seg_dir)?);
        for meta in &manifest.segments {
            assert!(!meta.path.is_absolute());
            assert!(meta.resolve(&manifest.root).is_file());
        }

        // Delete 2 segments to test fault-tolerance (3 left >= K)
        fs::remove_file(manifest.segments[0].resolve(&manifest.root))?;
        fs::remove_file(manifest.segments[4].resolve(&manifest.root))?;

        ManifestSplitter::construct_from_path(&seg_dir.join("manifest.json"), None, &out_dir)?;
        assert_eq!(fs::read(out_dir.join("payload.bin"))?, payload);

        Ok(())
    }

    #[test]
    fn test_target_append_manifest_flow() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let manifest_path = dir.path().join("keys").join("manifest.json");
        let out_dir = dir.path().join("restored");
        let payload = b"Manifest-based target append mode testing.";
        let root = dir.path().join("hosts");

        // Manifest goes to a directory that does not exist yet.
        let manifest = hide_into_root(payload, &root, &manifest_path, 2, 2)?;
        assert!(
            manifest_path.is_file(),
            "manifest must be written to a new path"
        );
        assert_eq!(manifest.root, fs::canonicalize(&root)?);

        // Simulate deleting 1 host file entirely
        fs::remove_file(root.join("host_0.dll"))?;

        ManifestSplitter::construct_from_path(&manifest_path, None, &out_dir)?;
        assert_eq!(fs::read(out_dir.join("payload.bin"))?, payload);

        Ok(())
    }

    #[test]
    fn test_relocated_root_flow() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let manifest_path = dir.path().join("keys").join("manifest.json");
        let out_dir = dir.path().join("restored");
        let payload =
            b"Payload large enough to span several shards, so relocation is really exercised.";

        let root = dir.path().join("stardew");
        hide_into_root(payload, &root, &manifest_path, 2, 2)?;

        // Relocate the whole host tree, leaving the manifest behind.
        let moved = dir.path().join("moved").join("stardew");
        fs::create_dir_all(dir.path().join("moved"))?;
        fs::rename(&root, &moved)?;
        let moved = fs::canonicalize(&moved)?;

        // Recorded root is stale, so an explicit override is required.
        ManifestSplitter::construct_from_path(&manifest_path, Some(&moved), &out_dir)?;
        assert_eq!(fs::read(out_dir.join("payload.bin"))?, payload);

        Ok(())
    }

    #[test]
    fn test_manifest_dir_fallback() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let manifest_path = dir.path().join("keys").join("manifest.json");
        let out_dir = dir.path().join("restored");
        let payload = b"Fallback to the manifest's own directory when the root moved.";

        let root = dir.path().join("stardew");
        hide_into_root(payload, &root, &manifest_path, 2, 2)?;

        // Relocate the tree and move the manifest inside it, so that the
        // recorded root is stale but the manifest dir resolves the paths.
        let moved = dir.path().join("moved");
        fs::create_dir_all(&moved)?;
        fs::rename(&root, &moved)?;
        let relocated_manifest = moved.join("manifest.json");
        fs::rename(&manifest_path, &relocated_manifest)?;

        // No --root: recorded root is gone, so the manifest dir is used.
        ManifestSplitter::construct_from_path(&relocated_manifest, None, &out_dir)?;
        assert_eq!(fs::read(out_dir.join("payload.bin"))?, payload);

        Ok(())
    }

    #[test]
    fn test_wrong_root_reports_missing_segments() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let manifest_path = dir.path().join("manifest.json");
        let out_dir = dir.path().join("restored");
        let payload = b"Pointing at the wrong folder must not look like a shard shortage.";

        let root = dir.path().join("stardew");
        hide_into_root(payload, &root, &manifest_path, 2, 2)?;

        let wrong = dir.path().join("elsewhere");
        fs::create_dir_all(&wrong)?;
        let err = ManifestSplitter::construct_from_path(&manifest_path, Some(&wrong), &out_dir)
            .expect_err("a wrong root must fail");
        assert!(err.to_string().contains("--root"), "got: {}", err);

        Ok(())
    }

    #[test]
    fn test_restore_destination_uses_directory() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let manifest_path = dir.path().join("manifest.json");
        let out_dir = dir.path().join("nested").join("out");
        let payload = b"output lands in a directory under the recorded name.";

        let root = dir.path().join("hosts");
        let manifest = hide_into_root(payload, &root, &manifest_path, 1, 1)?;

        ManifestSplitter::construct_from_path(&manifest_path, None, &out_dir)?;
        let written = out_dir.join("payload.bin");
        assert!(written.is_file(), "must write inside the directory");
        assert_eq!(fs::read(&written)?, payload);

        // A non-directory destination is an error, not a clobbered file.
        let occupied = dir.path().join("occupied");
        fs::write(&occupied, b"keep me")?;
        let err = ManifestSplitter::construct_from_manifest(&manifest, &root, &occupied)
            .expect_err("file destination must be refused");
        assert!(err.to_string().contains("not a directory"), "got: {err}");
        assert_eq!(
            fs::read(&occupied)?,
            b"keep me",
            "existing file must survive"
        );

        Ok(())
    }

    #[test]
    fn test_source_name_cannot_escape_output_dir() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let out_dir = dir.path().join("out");
        let manifest = Manifest {
            orig_len: 0,
            k: 1,
            m: 0,
            root: dir.path().to_path_buf(),
            // A hostile name must not steer the write out of out_dir.
            source_name: Some("../../escaped.bin".into()),
            segments: vec![],
        };

        let resolved = ManifestSplitter::restore_destination(&manifest, &out_dir)?;
        assert_eq!(
            resolved,
            out_dir.join("escaped.bin"),
            "only the file name component may be used"
        );
        assert!(resolved.starts_with(&out_dir));

        Ok(())
    }

    #[test]
    fn test_rejects_path_traversal() -> Result<(), Box<dyn std::error::Error>> {
        for bad in ["../../etc/passwd", "/etc/passwd"] {
            let dir = tempdir()?;
            let manifest = Manifest {
                orig_len: 4,
                k: 1,
                m: 0,
                root: dir.path().to_path_buf(),
                source_name: Some("payload.bin".into()),
                segments: vec![SegmentMeta {
                    index: 0,
                    path: PathBuf::from(bad),
                    offset: 0,
                    length: 4,
                    crc32: 0,
                }],
            };
            let manifest_path = dir.path().join("manifest.json");
            manifest.save_to_file(&manifest_path)?;

            let err = Manifest::load_from_file(&manifest_path)
                .expect_err("manifest with an escaping path must be rejected");
            assert!(
                err.to_string().contains("non-relative"),
                "unexpected error for {bad}: {err}"
            );
        }

        Ok(())
    }

    #[test]
    fn test_clean_removes_appended_payload() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let manifest_path = dir.path().join("manifest.json");
        let payload = b"clean should shrink hosts back to their original size.";
        let root = dir.path().join("hosts");
        let host = root.join("host_0.dll");
        let original = b"ORIGINAL_HOST_CONTENT";

        fs::create_dir_all(&root)?;
        fs::write(&host, original)?;
        let targets = vec![host.clone(), {
            let p = root.join("host_1.dll");
            fs::write(&p, original)?;
            p
        }];
        let manifest = ManifestSplitter::spread_to_targets(
            payload,
            &targets,
            &root,
            &manifest_path,
            Some("payload.bin"),
            1,
            1,
        )?;

        let grew = fs::metadata(&host)?.len();
        assert!(grew > original.len() as u64, "payload should be appended");

        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();
        let resolved = manifest.pick_root(None, &manifest_dir);
        ManifestSplitter::delete_from_manifest(&manifest, &resolved);

        assert_eq!(
            fs::read(&host)?,
            original,
            "host must return to original size"
        );
        Ok(())
    }
}
