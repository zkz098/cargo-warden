# cargo-warden

A unified Cargo plugin and code quality toolkit for Rust and Slint projects.

`cargo-warden` consolidates architecture drift guards, static quality checks, Slint asset/i18n synchronization, Git hook management, and local CI parity gates into a single configurable Cargo subcommand and standalone CLI.

---

## Features

| Module | Command | Description |
| :--- | :--- | :--- |
| **Module Dependency DAG Guard** | `cargo warden dag` | Parses Rust module imports via `syn`, detects cycles using Tarjan's SCC algorithm, and enforces forbidden module boundaries (e.g., `bootstrap`). |
| **Workspace & CI Parity Check** | `cargo warden ci-parity` | Compares `cargo metadata` workspace members against CI workflow test lines to catch untested or stale crates. |
| **Slint Theme Color Guard** | `cargo warden theme [check\|apply\|generate]` | Detects raw `#hex` color literals in `.slint` files, rewrites them to `Theme.c<hex>` with imports, and generates `theme.slint` from TOML. |
| **Slint & Rust i18n Catalog Sync** | `cargo warden i18n [check\|generate]` | Parses gettext `.po` catalogs, generates a sorted Rust static lookup table, and verifies catalog synchronization. |
| **Cognitive Complexity Checker** | `cargo warden complexity` | Measures function cognitive complexity via `syn` (control-flow nesting and boolean operator transitions) and reports threshold violations. |
| **File Length Guard** | `cargo warden file-length` | Enforces maximum physical line counts across configured file extensions and paths to prevent god files. |
| **Local CI Verification Gate** | `cargo warden gate` | Mirrors CI pipelines locally (`fmt`, `clippy`, `nextest`/`test`, `doc`, plus optional `machete`, `deny`, and `audit`) with Cargo env scrubbing. |
| **Unified Static Check** | `cargo warden check` | Runs all configured static checks in milliseconds—ideal for pre-commit hooks and CI. |
| **Git Hook Management** | `cargo warden hook [install\|uninstall\|status]` | Installs, updates, inspects, and safely uninstalls `pre-commit` / `pre-push` hooks while preserving existing custom scripts. |

---

## Installation

### From crates.io

```bash
cargo install cargo-warden
```

### From Git or local source

```bash
cargo install --git https://github.com/zkz098/cargo-warden.git
# or from a local checkout:
cargo install --path .
```

Invoke either as a Cargo subcommand or standalone binary:

```bash
cargo warden --help
cargo-warden --help
```

---

## CLI Reference

### 1. Module Dependency DAG Guard (`dag`)

```bash
cargo warden dag --root crates/my-core/src --forbid bootstrap
```

- Inspects top-level modules in the given source root.
- Fails and prints any circular dependency components (`a <-> b`).
- Fails if sibling modules import any module listed with `--forbid`.

### 2. Workspace & CI Parity (`ci-parity`)

```bash
cargo warden ci-parity --workflow .github/workflows/ci.yml
```

- Queries workspace members via `cargo metadata --no-deps`.
- Scans matching test lines in the CI workflow (e.g., `nextest`, `llvm-cov`, `cargo test`).
- Reports active workspace crates missing from CI test runs.

### 3. Slint Theme Color Guard (`theme`)

```bash
# Check for unmapped hardcoded hex colors
cargo warden theme check --ui-dir ui/

# Rewrite hardcoded hex colors to Theme.c<hex> and inject imports
cargo warden theme apply --ui-dir ui/

# Generate theme.slint from the configured theme definition
cargo warden theme generate --out-file ui/theme.slint
```

### 4. Gettext `.po` Catalog Sync (`i18n`)

```bash
# Verify the generated Rust catalog is up to date with .po files
cargo warden i18n check --lang-dir lang/ --out-file src/i18n/catalog.rs

# Regenerate the Rust lookup table from .po files
cargo warden i18n generate --lang-dir lang/ --out-file src/i18n/catalog.rs
```

### 5. Cognitive Complexity (`complexity`)

```bash
cargo warden complexity --max 25 --paths src/ crates/
```

Example output:

```text
[complexity] analyzing cognitive complexity (max allowed: 25)...
[complexity] ❌ 1 function(s) exceed maximum cognitive complexity (threshold: 25):
  ❌ crates/engine/src/scheduler.rs:142 - `schedule_next()` score: 27 (> 25)
```

### 6. File Length Guard (`file-length`)

```bash
cargo warden file-length --max-lines 800 --ext rs --ext slint --paths src/
```

### 7. Unified Static Check & Local Gate (`check` / `gate`)

```bash
# Run all static checks enabled in warden.toml / Cargo.toml metadata
cargo warden check

# Also run fmt, clippy, and doc checks (without running the test suite)
cargo warden check --full

# Run unified verification pipeline (static checks + fmt + clippy + test/nextest + doc)
cargo warden gate

# Filter or skip specific gate steps
cargo warden gate --only fmt,clippy,doc
cargo warden gate --skip test

# Include cargo-machete, cargo-deny, and cargo-audit
cargo warden gate --full
```

### 8. Git Hook Management (`hook`)

```bash
# Install a pre-commit hook running `cargo warden check`
cargo warden hook install

# Install a pre-push hook running `cargo warden gate`
cargo warden hook install --hook pre-push --gate

# Append to an existing custom hook instead of failing
cargo warden hook install --append

# Check installed hook status
cargo warden hook status

# Remove cargo-warden hook (or strip only its managed block)
cargo warden hook uninstall
```

---

## Configuration (`warden.toml`)

Create `warden.toml` (or `.warden.toml`) in the repository root, or place the configuration under `[workspace.metadata.warden]` in `Cargo.toml` (see also [`warden.example.toml`](warden.example.toml)):

```toml
[dag]
roots = ["crates/my-core/src"]
forbid_imports = ["bootstrap"]

[ci_parity]
workflow = ".github/workflows/ci.yml"
test_patterns = ["nextest", "llvm-cov"]
ignore_crates = ["fuzz"]
fanout_crates = []

[slint_theme]
ui_dir = "crates/my-app/ui"
out_file = "crates/my-app/ui/theme.slint"
unchanged_colors = ["ffffff", "000000", "6b7280", "ffffff55"]

[i18n]
lang_dir = "crates/my-app/lang"
out_file = "crates/my-app/src/i18n/catalog.rs"
domain = "my-app"
locales = ["en", "zh_CN"]

[complexity]
max = 25
paths = ["crates", "src"]
ignore_tests = true
ignore = ["target"]

[file_length]
max_lines = 800
extensions = ["rs", "slint"]
paths = ["crates", "src"]
ignore = ["target", "tests"]

[check]
fmt = true
clippy = false
doc = false

[gate]
include_checks = true
# rustfmt_toolchain = "1.85.0"

[[gate.test_groups]]
name = "core"
packages = ["my-types", "my-core"]
features = []
```

---

## Logging & Diagnostics

`cargo-warden` emits structured diagnostics via `tracing` to `stderr` so standard command output stays clean:

- `cargo warden check`: warnings and check results (default)
- `cargo warden check -v`: `INFO` diagnostic events
- `cargo warden check -vv`: `DEBUG` details (paths scanned, config loading, child processes)
- `cargo warden check -vvv`: `TRACE` details (per-file line counts, AST inspection, scrubbed env vars)
- `cargo warden check -q`: errors only
- Custom filtering via `RUST_LOG`, e.g. `RUST_LOG=cargo_warden::checks::complexity=trace cargo warden check`

---

## License

Licensed under the GNU General Public License v3.0 or later ([`LICENSE`](LICENSE)).
