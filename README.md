# hider

Spread a file across many ordinary files using Reed-Solomon erasure coding, then
get it back later even if some of the hosts are deleted, corrupted, or the whole
folder has been moved.

```
hide     spread a file across the .dll/.exe files in a directory tree
restore  rebuild the original file from a manifest
clean    strip the hidden shards back off the hosts
```

The point of the erasure coding is that no single host is special. Losing up to
`m` of `k + m` hosts and still recovering the original is the expected case, not
a best-effort edge.

## Quick start

```bash
cargo build --release

# Hide a file. Hosts are discovered recursively; the manifest goes wherever you say.
./target/release/hider hide /path/to/game_folder secret.bin /safe/place/manifest.json

# Recover it. The manifest alone is enough as long as the tree hasn't moved.
./target/release/hider restore /safe/place/manifest.json ./out

# The tree moved? Point at its new location.
./target/release/hider restore /safe/place/manifest.json ./out --root /new/location/game_folder

# Remove the hidden data from the hosts.
./target/release/hider clean /safe/place/manifest.json --root /new/location/game_folder
```

`hide` prints the root it recorded and where it wrote the manifest, so you always
know what `restore` will need.

## Commands

### `hide <target_folder> <data_file> [manifest_path]`

Walks `target_folder` recursively, collects every `.dll` and `.exe`, and appends
one erasure-coded shard to each of the first `k + m` of them.

The shard split is half data, half parity: `k = ceil(hosts / 2)`, `m = hosts - k`.
With 252 hosts you get `k=126, m=125`, so you can lose 125 hosts and still
recover.

`manifest_path` defaults to `./manifest.json`. Point it somewhere outside the host
tree — the manifest is the only key to the data, so keep it apart from the hosts.
Missing parent directories are created.

### `restore [manifest] [output_dir] [--root <dir>]`

Rebuilds the original file into `output_dir` under its original name.
`output_dir` defaults to `./restored` and is created if missing.

`[manifest]` defaults to `./manifest.json`. `--root` is only needed once the host
tree moves; see below.

### `clean [manifest] [--root <dir>]`

Truncates each host back to the byte offset recorded at hide time, leaving the
host otherwise byte-identical. Shards whose CRC no longer matches are skipped and
reported rather than truncated, so re-running `clean` on a dirty tree cannot
corrupt a host it cannot verify.

## How relocation works

This is the part worth understanding before you rely on the tool.

The manifest records two things: `root`, the absolute path of the folder the data
was hidden into at the time, and, per segment, a `path` **relative** to that
root.

```json
{
  "orig_len": 50009,
  "k": 3,
  "m": 2,
  "root": "/tmp/demo/games",
  "source_name": "secret.bin",
  "segments": [
    { "index": 0, "path": "a3.dll",          "offset": 10, "length": 16670, "crc32": 35615559 },
    { "index": 4, "path": "sub/nested.exe",  "offset": 1,  "length": 16670, "crc32": 19835402 }
  ]
}
```

Because the segment paths are relative, the tree can be moved, renamed, or
restored from a backup without rewriting anything. `restore` and `clean` resolve
the root in this order:

1. `--root <dir>` if you pass it
2. the recorded `root`, if it still exists
3. the directory containing the manifest

So the common case needs no arguments at all. When the recorded root has gone you
get a warning naming the old path and a count of what was found under the
fallback, then either an explicit `--root` or a clear error:

```
[Warning] Recorded root "/tmp/demo/games" no longer exists (tree moved?). Re-run with --root ...
[Info] Located 0/5 segments under "/safe/place" (k=3, m=2, 0 verified)
Error: "Only 0 of 5 segments found under \"/safe/place\", but at least k=3 are required. ..."
```

That error deliberately does not surface as an abstract shard-count failure from
the erasure decoder. A moved folder and genuine data loss are different problems
and you should not have to guess which one you have.

Nested subdirectories are fine. A host at `games/sub/nested.exe` is recorded as
`sub/nested.exe`.

## Constraints

Verified against the implementation, not inferred:

- **Host count is capped at 256.** `galois_8::ReedSolomon` has a hard `F::ORDER`
  of 256. 256 hosts works (`k=128, m=128`); 257 fails with `TooManyShards`.
- **You need at least 2 hosts.** With one host, `k=1, m=0`, and `ReedSolomon::new`
  rejects it with `TooFewParityShards`. There is no redundancy to build from a
  single host, so this is arithmetic rather than a policy choice.
- **Extension matching is case-sensitive.** Only lowercase `.dll` and `.exe` are
  collected. `b.DLL` and `c.Dll` are skipped; `d.exe` is used.
- **Empty input is rejected** with `Target file is empty`.
- **A host must live inside the root** passed to `spread_to_targets`, otherwise it
  cannot be recorded as a relative path and the operation errors rather than
  writing an unrecoverable manifest.

## Failure modes

A host that is missing, truncated, or fails its CRC32 check is treated as lost.
Each one consumes the redundancy budget, so losing `m` hosts *and* corrupting one
more will fail. Corrupt and truncated hosts are reported:

```
[Warning] Corrupt checksum for segment 7 at "/games/a3.dll"
```

Check stderr when a restore does not produce what you expected.

`restore` reports both `located` (files present) and `verified` (CRC-passing) in
one line. When they diverge sharply, the files are present but damaged.

## Manifest format and safety

The manifest is zstd-compressed JSON. To inspect one:

```bash
zstd -dc manifest.json | python3 -m json.tool
```

Two properties matter:

**Old manifests are rejected.** Any manifest written before `root` existed is
refused at load time with a message telling you to re-hide. There is no legacy
inference path. This is deliberate: a manifest drives destructive truncation, so
it is better to fail loudly than to guess at a format.

**Segment paths must be plain relative paths.** Absolute paths and `..` components
are refused:

```
Error: "segment 0 has a non-relative path \"../../etc/passwd\": only plain relative
paths are accepted. Manifests written before the `root` field existed must be re-hidden."
```

`restore_destination` likewise uses only the file-name component of the recorded
`source_name`, so a hostile manifest cannot steer a write outside `output_dir`.

## Idempotency

`hide` is **append-only**. Each run grows every host by `shard_size` bytes and
overwrites the manifest, so the newest run wins and older runs' data is stranded
in the middle of the hosts. `clean` can only remove what the current manifest
describes.

If you hide the same file repeatedly, use `clean` between runs, or accept the
growth. Verified: two consecutive hides of the same 7-byte file grew each host from
5 to 13 to 21 bytes, and `restore` returned the correct original both times.

## Development

```bash
cargo build
cargo test                 # 9 tests
cargo clippy --all-targets # 5 pre-existing warnings, none in new code
cargo fmt --check          # clean
```

Offline builds work: `cargo build --offline`.

Tests live at the bottom of `src/lib.rs` and cover relocation with an override,
fallback to the manifest directory, wrong-root diagnostics, traversal rejection,
`clean` restoring host sizes, and output-directory semantics. There is no CI.

`src/bin/create_user_dir.rs` is a dev helper that scaffolds a `simulated_user/`
tree. It is unrelated to the hiding logic.

## Threat model

Worth being blunt about: **this is obfuscation, not encryption.** The payload is
zstd-compressed and nothing else. `pack`/`unpack` in `src/lib.rs` are the intended
hook for real encryption, and the code comments mark them as such. As it stands,
anyone who can read the hosts and the manifest can recover the file without a key,
because there is no key.

The CRC32 in each segment is an integrity check against accidental corruption. It
is not authentication, and it is not a MAC.

If you need the data to be unreadable without a secret, add encryption at the
`pack`/`unpack` boundary before relying on this for anything sensitive.
