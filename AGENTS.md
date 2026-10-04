# AGENTS.md

Guidance for agents working in this repository. This file is **stale-proofing
documentation**: it describes the current state, and the failure modes and
constraints listed here were verified by running the code, not inferred from
reading it. Re-verify before trusting any line.

## What this is

Single-crate Rust binary + library that hides data inside the tails of other
files using Reed-Solomon erasure coding.

- `src/lib.rs` (~800 lines) — all logic. `Manifest`/`SegmentMeta` schema,
  `RedundancyEngine` (zstd pack + RS encode/decode), `ManifestSplitter`
  (spread/construct/delete), plus 9 tests.
- `src/main.rs` (~245 lines) — `hide`/`restore`/`clean` CLI.
- `src/bin/create_user_dir.rs` — dev helper that scaffolds a `simulated_user/`
  tree. Unrelated to hiding logic; do not wire it into anything.

The earlier design (`TailSplitter`, 24-byte header + `RS_SPLIT` magic footer,
`construct_from_targets` scanning candidates) **no longer exists**. Manifests
carry all metadata; there is no magic footer and no candidate scanning. If you
find yourself reasoning about tail formats, you are reading stale docs — the old
`AGENTS.md` described that design and it was fully replaced.

## Commands

```bash
cargo build --offline
cargo test                  # 9 tests, all passing
cargo clippy --all-targets  # 5 pre-existing warnings, see below
cargo fmt --check           # clean
```

Pre-existing clippy warnings — do **not** assume a clean baseline, and do not
treat these as regressions you introduced:

- `src/lib.rs:152` — `very complex type` on `RedundancyEngine::encode`'s
  `Result<(Vec<Vec<u8>>, usize), Box<dyn Error>>`
- `src/lib.rs:163` — `manual_div_ceil` on `(packed_len + k - 1) / k`
- `src/lib.rs:197` — `needless_range_loop` in `decode`
- `src/main.rs:10` — `useless use of vec!` on `whitelisted_extensions`
- `src/main.rs:17` — `collapsible_if` on the extension check

## CLI surface

```
hide <target_folder> <data_file> [manifest_path]
restore [manifest] [output_dir] [--root <dir>]
clean [manifest] [--root <dir>]
```

`--root <dir>` and `--root=<dir>` are both accepted and the flag may appear
anywhere; `take_root_flag` strips it before positional parsing. Defaults:
manifest `./manifest.json`, output dir `./restored` (a real directory — the
restored file keeps its original name inside it).

`hide` derives `k = ceil(hosts / 2)`, `m = hosts - k` from the number of `.dll`/
`.exe` files found recursively. `root` is the canonicalized `target_folder`.
`find_files_recursive` takes `&Path` specifically so the canonicalized folder can
be reused as the manifest root without cloning.

## Manifest schema

```rust
Manifest { orig_len, k, m, root: PathBuf, source_name: Option<String>, segments }
SegmentMeta { index, path: PathBuf, /* relative to root */ offset, length, crc32 }
```

`offset` is where the shard begins within the host (`0` for dedicated segment
files from `spread_to_directory`). `orig_len` is the length of the **zstd-packed**
payload, before RS padding — not the original file size.

Changing any field name, order, or type breaks every manifest already written.
There is no version field; that is a deliberate consequence of having no legacy
support.

`Manifest::save_to_file`/`load_from_file` wrap zstd around serde_json. The file is
not human-readable; inspect with `zstd -dc manifest.json | python3 -m json.tool`.

### Path resolution

`SegmentMeta::resolve(root)` joins a relative `path` onto `root`, and returns an
absolute `path` unchanged. That pass-through keeps `join` semantics well-defined
for manifests of any age even though legacy manifests are rejected at load.

`Manifest::pick_root(override, manifest_dir)` tries override → recorded root if it
still exists → manifest's own directory. It does **not** guarantee the result
exists; callers report how many segments resolved.

Two load-time invariants, both deliberate:

- Segment paths must be `Normal`/`CurDir` components only. Absolute paths and `..`
  are refused. A manifest drives destructive truncation in `delete_from_manifest`,
  so it is validated rather than trusted.
- Manifests lacking `root` (i.e. all pre-relocation manifests) are therefore
  rejected with a "must be re-hidden" message. **There is no legacy inference
  path.** If a task appears to require one, that is a scope decision, not a
  missing feature to quietly add.

`restore_destination` uses only the **file-name component** of `source_name`, so a
hostile manifest cannot write outside `output_dir`. It errors rather than clobber
when `dest` exists and is not a directory.

## Verified constraints and failure modes

All confirmed by running the binary.

- **256 hosts max.** `galois_8::ReedSolomon` has a hard `F::ORDER = 256`. 256 works
  (`k=128, m=128`), 257 fails `TooManyShards`.
- **Minimum 2 hosts.** One host gives `k=1, m=0` → `TooFewParityShards`. Arithmetic,
  not policy.
- **Extension match is case-sensitive**, lowercase only. `b.DLL`/`c.Dll` are
  skipped; `d.exe` is used. A real tree may silently yield fewer hosts than
  expected because of this.
- **Empty input rejected** (`Target file is empty`).
- **Hosts must be inside `root`** passed to `spread_to_targets`, else it errors
  rather than writing an unrecoverable manifest.
- **Recovery tolerates losing exactly `m` hosts.** Verified on the real 252-host
  Stardew Valley tree: `k=126, m=125`, deleted 125, recovered byte-identical.
- **Corrupt or truncated hosts are skipped** with a stderr warning and consume the
  same budget as a missing file. Losing `m` *and* corrupting one more fails.
- **Restore after `clean` fails loudly**, printing `0 verified` then a decode
  error. It does not emit a bogus file. The `located` vs `verified` counts in the
  `[Info]` line diverge sharply when files are present but damaged — that gap is
  the diagnostic.
- **`hide` is append-only, not idempotent.** Two consecutive hides of a 7-byte
  file grew hosts 5 → 13 → 21 bytes. The manifest is overwritten so the newest run
  wins; older runs' data is stranded mid-host and `clean` cannot remove it. Call
  `clean` between runs if that matters.
- **Garbage/truncated manifests fail at decompression**, not silently:
  `Unknown frame descriptor` / `incomplete frame`.

## Testing notes

9 tests at the bottom of `src/lib.rs`. There is no CI. `cargo test` is the only
regression net, so run it before claiming a behavior change works.

When adding tests, note the helper `hide_into_root(payload, root, manifest_path,
k, m)` builds dummy hosts inside `root` and spreads into them.

`tempdir()` paths can be symlinked on some platforms; canonicalize both `root` and
any relocated path in tests that depend on `pick_root` seeing an existing
directory.

## Running it manually

Use a scratch directory, never the repo:

```bash
mkdir -p /tmp/hider-demo/games/sub && cd /tmp/hider-demo
for i in 0 1 2 3; do printf 'ORIGINAL_%s' $i > games/a$i.dll; done
printf 'X' > games/sub/nested.exe
head -c 50000 /dev/urandom > secret.bin
BIN=/path/to/hider/target/debug/hider
$BIN hide games secret.bin keys/manifest.json   # manifest outside the host tree
$BIN restore keys/manifest.json out
$BIN restore keys/manifest.json out2 --root archive/games
$BIN clean keys/manifest.json --root archive/games
```

`hide` needs the host tree to contain `.dll`/`.exe` files or it fails with
`No DLL files found in the specified directory`.

## Destructive-gotcha: never test `hide`/`clean` destructively against the only copy

`hide` mutates every host in the target tree, and testing the fault-tolerance
limit requires deleting hosts.

During development on this repo, 125 host files were deleted from
`simulated_user/Stardew Valley/` to test the `k=126, m=125` floor. They were not
restored. That tree is now missing 125 of its original 252 hosts and only a
game reinstall brings them back.

Before any test that deletes or appends to real files:

- copy the tree first, or
- work entirely inside a `mktemp -d` scratch dir, or
- use `cargo test`, which already uses `tempfile::tempdir`

`simulated_user/` is gitignored, so **git cannot recover deletions there**. Treat
it as the only copy of whatever is in it, or as disposable — decide which, and
say so, before touching it.

## Repo state

`.gitignore` covers `/target`, `/manifest.json`, `/redundant_segments/`,
`/segments/`, `/restored/`, `/simulated_user/`. All are generated artifacts, not
source. `/manifest.json` is ignored because it is a hide-time output, not an
input to the build.

Note `simulated_user/` is ignored while `redundant_segments/` was the old name in
a previous `.gitignore` — if you see artifacts named `redundant_segments`, that
is from an older layout, not current code.

## Security posture

`pack`/`unpack` in `src/lib.rs` are zstd compression only, marked in-code as the
obfuscation/encryption hook. **This is not encryption.** There is no key, so
anyone with hosts + manifest recovers the file. CRC32 is integrity, not
authentication. If a task needs confidentiality, it must add encryption at that
boundary deliberately — do not describe the current state as secure.