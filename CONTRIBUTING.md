# Contributing to turboGP

## Getting started

1. Clone the repo
2. Install Rust **1.89** (`rustup install 1.89.0` — this is the MSRV
   declared in `Cargo.toml` and verified by the `MSRV` CI workflow)
3. `cargo build` — must compile clean (with `-D warnings`)
4. `cargo test --lib --tests` — the full test suite must pass (lib + integration)
5. `cargo fmt --check` — must pass
6. `cargo clippy --all-targets -- -D warnings` — must pass (no warnings tolerated)
7. `bash scripts/check_no_panics.sh` — must report zero panic paths
8. `bash scripts/check_dead_code.sh` — must report zero dead modules
9. `bash scripts/check_file_size.sh` — every `src/**/*.rs` file must be ≤ 2,000 LOC

The first build installs `mimalloc` as the global allocator and pulls in the
Parquet/Arrow readers; subsequent builds are incremental. Cross-OS testing
runs on `ubuntu-latest` and `macos-latest` (see `.github/workflows/cross-os.yml`).

## Branch naming

- `feat/<short-description>` — new features
- `fix/<short-description>` — bug fixes
- `docs/<short-description>` — documentation only
- `kernel/<short-description>` — kernel table changes
- `adr/<number>-<short-description>` — ADR implementation
- `wave-<nn>/<short-description>` — v3 wave-scoped work

## Commit format

```
<type>: <description>

<body>
```

Types: `feat`, `fix`, `docs`, `refactor`, `test`, `kernel`, `adr`, `wave`

## Pull request process

1. Create a branch from `main`
2. Write code + tests
3. Run the local pre-flight:
   ```bash
   cargo fmt
   cargo clippy --all-targets -- -D warnings
   cargo test --lib --tests
   bash scripts/check_no_panics.sh
   bash scripts/check_dead_code.sh
   bash scripts/check_file_size.sh
   ```
4. Open a PR using `.github/PULL_REQUEST_TEMPLATE.md`
5. All CI checks (see below) must pass
6. One review required for merge

## CI checks

Every push and pull request triggers the workflows in `.github/workflows/`:

| Workflow | File | What it gates |
|----------|------|---------------|
| **CI** | `ci.yml` | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, release build, benchmark compile |
| **Cross-OS** | `cross-os.yml` | `cargo build` + `cargo test` on `ubuntu-latest` and `macos-latest` (single-threaded) |
| **MSRV** | `msrv.yml` | Verifies `Cargo.toml` declares `rust-version = "1.89"` and `cargo check` passes on Rust 1.89 |
| **Coverage** | `coverage.yml` | `cargo llvm-cov` with a **60 % threshold** — fails the build below 60 % |
| **Fuzz** | `fuzz.yml` | 10,000-iteration SQL fuzz test (`tests/fuzz_test.rs -- --ignored`), daily cron + on push |
| **Dead Code** | `deadcode.yml` | Runs `check_no_panics.sh`, `check_dead_code.sh`, `check_file_size.sh` |
| **Security** | `security.yml` | `cargo audit` + `cargo deny check`, weekly cron |
| **Release** | `release.yml` | Binary release packaging |

The CI environment sets `RUSTFLAGS="-D warnings"`, so any compiler warning
fails the build — there is no "warnings are tolerated" escape hatch.

## Coding standards

- **Follow `rustfmt.toml`** — run `cargo fmt` before committing
- **No `unwrap()` / `expect()` / `panic!()` / `unreachable!()` / `todo!()`
  / `unimplemented!()` in production code.** This is enforced by
  `scripts/check_no_panics.sh` in CI. Use the `?` operator and `Result<T,
  Error>` everywhere. Panics inside `#[cfg(test)]` modules are permitted.
- **`Result` everywhere.** Public functions return `crate::Result` (the
  `thiserror`-derived `Error` enum in `src/lib.rs`). Internal helpers
  should propagate errors with `?`, not convert them to `Option` or panic.
- **All `unsafe` blocks must have a `// SAFETY:` comment** explaining the
  invariant that makes the deref / pointer arithmetic sound
- **All public functions must have doc comments** (`#![warn(missing_docs)]`
  is set in `src/lib.rs`)
- **All new kernels must be benchmarked** — add to `benches/` and run with
  `cargo bench --features bench-external`
- **Files must stay under 2,000 LOC.** `check_file_size.sh` fails CI if any
  `src/**/*.rs` file exceeds 2,000 lines. Decompose large files into
  focused sub-modules (see `engine/query_interpreter/` for the pattern).

## Architecture

Before contributing, read:

1. [ARCHITECTURE.md](ARCHITECTURE.md) — the dispatch-based architecture (1 page)
2. [CHANGELOG.md](CHANGELOG.md) — what has changed per wave
3. [docs/adr/](docs/adr/) — the 25 accepted design decisions
4. [docs/adr/OPEN_QUESTIONS.md](docs/adr/OPEN_QUESTIONS.md) — decisions below 80 % confidence

New work should trace to an ADR in [docs/adr/](docs/adr/). If no ADR exists,
write one first (use the ADR format documented in
[docs/adr/README.md](docs/adr/README.md)).

## Testing

- **Unit tests**: `#[test]` in each module (typically inside a
  `mod_tests.rs` sibling file or `#[cfg(test)] mod tests` block)
- **Integration tests**: `tests/` directory (end-to-end, pgwire, SQL
  semantics, concurrency, etc.)
- **Benchmarks**: `benches/` directory (criterion), gated behind the
  `bench-external` feature for external baselines (DuckDB, ClickHouse)
- **Fuzz tests**: `tests/fuzz_test.rs` — 10,000-iteration SQL fuzz,
  runs in CI on every push and daily via cron

Every new kernel MUST have:

1. A **correctness test** (does it produce the right answer?)
2. A **parity test** (does AVX-512 match scalar?)
3. A **benchmark** (what's the throughput?)

Every new SQL feature MUST have:

1. A **parser test** (does the SQL parse to the expected AST?)
2. An **executor test** (does `QueryEngine::execute()` produce the right
   `QueryResult`?)
3. A **pgwire round-trip test** if it touches the wire protocol

## Deployment

The `deploy/` directory ships deployment artifacts (Wave 10):

- `deploy/helm/` — Helm chart (`Chart.yaml`, `values.yaml`,
  `templates/{statefulset,service,pdb,configmap,secret}.yaml`)
- `deploy/k8s/turbogp.yaml` — bare K8s StatefulSet manifest

The server binary (`src/bin/turbogp.rs`) supports graceful shutdown on
SIGTERM/SIGINT — pod termination drains in-flight queries before exiting.
See `README.md` Quick Start for the binary CLI flags.

## License

By contributing, you agree your contributions are licensed under the
[CCL-X License](LICENSE.md) (Civil Common License X, Version 1.2).
