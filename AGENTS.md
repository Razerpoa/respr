# AGENTS.md

## What this is

Single-crate Rust binary that hides data in the tails of other files using
Reed-Solomon erasure coding. **All logic lives in `src/main.rs`** — one file, one
`impl TailSplitter` with two methods:

- `spread_to_targets(data, target_files, k, m)` — RS-encodes `data` into `k + m`
  equal-size shards and **appends** one shard to each of the first `k + m` target
  files. Target files must already exist.
- `construct_from_targets(candidate_files, dest)` — scans candidates, keeps the
  files whose tail carries the magic footer + a CRC32-valid payload, then
  RS-reconstructs and writes `orig_len` bytes to `dest`.

No lib target, no tests, no README, no CI. `cargo test` runs 0 tests.

## Commands

```bash
cargo build
cargo clippy --all-targets    # 3 warnings already exist (see below)
cargo fmt --check             # currently FAILS: src/main.rs is not rustfmt-clean
```

Pre-existing lint state — do not assume a clean baseline:
- `manual_div_ceil` at `src/main.rs:40` and `:137` (the `(len + k - 1) / k` shard-size calc)
- `needless_range_loop` at `src/main.rs:190`
- `cargo fmt` wants to rewrap the `println!` in `main()` and add a trailing newline

Offline builds work: `cargo build --offline` (deps already in the local registry).

`rand` is declared in `Cargo.toml` but not used anywhere in `src/` — don't go
looking for seeded/randomized logic, and don't assume removing it is a behavior change.

## Running it

`cargo run` **fails immediately** out of the box: `main()` hardcodes six host
files that do not exist in this repo (`img1.jpg`, `img2.jpg`, `img3.jpg`,
`song1.mp3`, `song2.mp3`, `document.pdf`). `spread_to_targets` opens them
append-only and propagates the `NotFound`, so you get a bare
`Error: Os { code: 2, kind: NotFound }` with no hint about which file.

To exercise it, create the hosts first and run from that directory:

```bash
mkdir -p /tmp/hider-demo && cd /tmp/hider-demo
touch img1.jpg img2.jpg img3.jpg song1.mp3 song2.mp3 document.pdf
cargo run --manifest-path /path/to/hider/Cargo.toml
```

Gotchas when experimenting:
- Host paths are relative to the **CWD**, and `dest` (`extracted_data.txt`) is
  also written to the CWD — not next to the host files.
- `spread_to_targets` is **append-only and not idempotent**: every run grows each
  host by `shard_size + 32` bytes (58 → 116 after a second run). Re-running
  reconstructs the *newest* tail correctly, but host files never shrink. Start
  from a clean directory each time.
- There are no tests, so any behavior change is verified by running the binary
  and inspecting output — do not assume `cargo test` covers anything.

## On-disk tail format (positional, backward-sensitive)

Each tail is: `payload` + 24-byte header + 8-byte magic `RS_SPLIT`.
Header fields are little-endian and written in a fixed order:
`orig_len: u64`, `k: u32`, `m: u32`, `idx: u32`, `crc32: u32`.

- `shard_size = ceil(orig_len / k)`, forced to 1 when `orig_len == 0`.
- Reordering or resizing these fields silently breaks reading of tails already
  written by `spread_to_targets`.
- Magic is checked at the *last* 8 bytes, so only one tail per file is ever seen.

## RS constraints and failure modes (verified by experiment)

- `k >= 1`, `m >= 0`, and `k + m <= 256` — `galois_8::ReedSolomon::new` has a
  hard `F::ORDER = 256` cap and returns "The number of provided shards is greater
  than the one in codec" above it. `m == 0` errors ("number of provided parity
  shards is smaller than the one in codec").
- Reconstruction tolerates losing up to `m` hosts. `main()` uses `k=2, m=4`:
  losing 4 of 6 recovers; losing 5 fails with
  `TooFewShardsPresent`; losing all 6 fails with "No valid tail segments found".
- A corrupt or truncated tail payload is **silently skipped** with a stderr line
  `Corrupted segment detected at <path>, ignoring.` — and it consumes the same
  budget as a lost file. Losing 4 *and* corrupting a 5th with `k=2, m=4` fails.
  Warnings on stderr are the only signal; check them when a run looks wrong.
- `construct_from_targets` takes `k`/`m`/`orig_len` from the **last** valid tail
  it scanned (last write wins), so mixing tails from two different spreads can
  produce a plausible-looking but wrong reconstruction.

## Loose artifacts at the repo root

`constructed_data`, `example_data`, `recovered_data.txt`, and
`redundant_segments/*.bin` are leftovers from earlier manual experiments — the
current `main()` does not produce any of them. They are not referenced by the
code. Note `.gitignore` ignores `/target` and `/segments`, but the directory is
actually named `redundant_segments`, so it is *not* ignored.

The repo has no commits yet on `master`; every file is currently untracked.