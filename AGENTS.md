# AGENTS.md

nm-dedupe is a Rust CLI for macOS that replaces duplicate files across `node_modules` folders
with APFS clones. It touches users' files, so data safety comes before everything else.

## Layout

| path | purpose |
|---|---|
| `src/apfs.rs` | thin syscall wrappers: clonefile, renamex_np, getattrlist, xattrs, statfs |
| `src/replace.rs` | the replace procedure, end-of-run `finalize`, leftover recovery, test hooks |
| `src/scan.rs` | parallel `getattrlistbulk` walk, compact in-memory layout |
| `src/index.rs` | SQLite index (`~/.cache/nm-dedupe/index-v2.db`): hash cache and canonicals |
| `src/plan.rs` | grouping by content, canonical choice, work list, forecast |
| `src/main.rs` | CLI, phases, output |
| `tests/safety.rs` | integration tests on real APFS; core invariant: no file loses content or metadata |
| `tests/e2e-npm.sh` | end-to-end run on real npm packages |
| `bench/sample-projects.sh` | reproducible benchmark projects |
| `docs/` | GitHub Pages site |

## Checks (all must pass before a commit)

```sh
cargo fmt --check
cargo clippy --release --all-targets -- -D warnings
npx --yes jscpd@4 --min-tokens 30 --threshold 0 --format rust src tests
cargo test --release
tests/e2e-npm.sh
```

Tests need macOS with APFS. CI runs the same steps.

## Rules

- **Never weaken the replace invariant**: a path holds either the original or a byte-identical
  clone, and an original is deleted only after its content is re-verified. Every safety
  mechanism needs a test that fails when the mechanism is disabled (check by mutating it).
- Never run nm-dedupe for real on a user's folders without explicit permission; use `-n` or
  a sandbox (`tests/`, `bench/`).
- Measure instead of assuming. Benchmark on an isolated APFS volume (sparse disk image), since
  system-volume free space moves with other processes and APFS frees blocks asynchronously.
- Look up current versions (`npm view <pkg> version`, `gh api repos/<owner>/<repo>/releases/latest`)
  instead of writing versions from memory.
- No data from a developer's machine (paths, file counts, disk sizes) in README or `docs/`.
- No duplicate code (jscpd 0 clones). Comments only where the code isn't self-explanatory.

## Commits and releases

- Conventional Commits with a short English subject, e.g. `fix: keep xattrs on skipped clones`.
  No co-author trailers.
- Release: bump `version` in `Cargo.toml`, tag without `v` prefix (`git tag 0.2.0`), push the tag.
  The release workflow builds a universal binary and writes notes from all commits since the
  previous tag (`.github/scripts/changelog.sh`).
