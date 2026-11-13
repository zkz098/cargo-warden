# cargo-warden

A unified Cargo plugin and code quality toolkit for Rust and Slint projects.

`cargo-warden` consolidates architecture drift guards, static quality checks, Slint asset/i18n synchronization, Git hook management, and local CI parity gates into a single configurable Cargo subcommand and standalone CLI.

---

## Features

| Module | Command | Description |
| :--- | :--- | :--- |
| **Module & Workspace DAG Guard** | `cargo warden dag` | Parses Rust module imports via `syn` (Tarjan SCC cycles + `[[dag.layers]]` / `[[dag.rules]]`) and enforces workspace crate dependency boundaries via `cargo metadata` (`[dag.workspace]`). |
| **Workspace & CI Parity Check** | `cargo warden ci-parity` | Compares `cargo metadata` workspace members against CI workflow test lines to catch untested or stale crates. |
| **Slint Theme Color Guard** | `cargo warden theme [check\|apply\|generate]` | Detects raw `#hex` color literals in `.slint` files, rewrites them to `Theme.c<hex>` with imports, and generates `theme.slint` from TOML. |
| **Slint & Rust i18n Catalog Sync** | `cargo warden i18n [check\|generate]` | Parses gettext `.po` catalogs, generates a sorted Rust static lookup table, and verifies catalog synchronization. |
| **Cognitive Complexity & Metrics** | `cargo warden complexity` | Measures function cognitive complexity, function length, and nesting depth via `syn` with path-scoped `[[complexity.overrides]]`. |
| **File Length Guard** | `cargo warden file-length` | Enforces physical or effective code line (SLOC) thresholds across configured paths with `.gitignore`, glob ignores, and `[[file_length.overrides]]`. |
| **Project Init & Baseline** | `cargo warden init` / `baseline` | Auto-detects project layout to generate `warden.toml`, manages & prunes baseline snapshots (`baseline prune`), and audits `// warden:allow(...)` suppressions. |
| **Local CI Verification Gate** | `cargo warden gate` | Mirrors CI pipelines locally (`fmt`, `clippy`, `nextest`/`test`, `doc`, plus optional `machete`, `deny`, and `audit`) with Cargo env scrubbing. |
| **Unified Static Check** | `cargo warden check` | Runs all configured static checks in milliseconds—supports `--staged`, `--diff <base>`, and `--format text\|json\|sarif\|github-actions`. |
| **Git Hook Management** | `cargo warden hook [install\|uninstall\|status]` | Installs, updates, inspects, and safely uninstalls `pre-commit` / `pre-push` hooks (including `--staged` mode) while preserving existing custom scripts. |
| **Diagnostics & Completions** | `cargo warden doctor` / `completions` | Checks configuration, toolchain, optional gate tools, and Git hook health (`doctor`), and generates shell completion scripts (`completions`). |

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

### 3. Slint Design Token & UI Specification Guard (`theme`)

```bash
# Check for unmapped hardcoded colors (#rgb, #rgba, #rrggbb, #rrggbbaa, rgb(), rgba())
# Comments and string literals are automatically ignored.
cargo warden theme check --ui-dir ui/

# Enforce all UI token rules (color, spacing, radius, typography, duration) + token validation
cargo warden theme check --all-rules --unknown-tokens --unused-tokens --verify-generated

# Rewrite hardcoded hex colors to Theme tokens and inject path-relative imports
cargo warden theme apply --ui-dir ui/

# Generate theme.slint from TOML or W3C DTCG `.tokens.json` definition
cargo warden theme generate --out-file ui/theme.slint
```

### 4. Gettext `.po` Catalog Sync, Quality & Source Coverage (`i18n`)

```bash
# Verify catalog sync, placeholder consistency ({}, {0}, {name}, %s), and source -> .po coverage
cargo warden i18n check --lang-dir lang/ --out-file src/i18n/catalog.rs

# Enable strict mode (reject empty/fuzzy translations + cross-locale gaps) and source scanning
cargo warden i18n check --source-dirs crates/my-app/ui --source-dirs crates/my-app/src --strict --check-unused

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

# Count effective code lines (SLOC), excluding blank lines and comments
cargo warden file-length --effective --max-lines 500 --paths src/
```

### 7. Project Init, Baseline & Incremental / Structured Checks

```bash
# Auto-detect project layout and generate warden.toml (use -i for interactive prompts)
cargo warden init

# Capture current violations into warden-baseline.json (regressions still fail)
cargo warden baseline generate

# Incremental checks on staged changes or diff against a Git ref
cargo warden check --staged
cargo warden check --diff origin/main

# Emit JSON or SARIF 2.1.0 diagnostics
cargo warden check --format json
cargo warden check --format sarif
```

Inline suppressions are also supported via comments: `// warden:allow(complexity)`, `// warden:allow(dag)`, `// warden:allow(slint-theme)`, and `// warden:allow-file(file-length)`.

### 8. Unified Static Check & Local Gate (`check` / `gate`)

```bash
# Run all static checks enabled in warden.toml / Cargo.toml metadata (parallel by default)
cargo warden check
cargo warden check --no-parallel

# Auto-fix fixable issues (Slint theme tokens/files, i18n catalog sync, clippy --fix, cargo fmt)
cargo warden check --fix

# Also run fmt, clippy, and doc checks (without running the test suite)
cargo warden check --full

# Run unified verification pipeline (static checks + fmt + clippy + test/nextest + doc + custom steps)
cargo warden gate

# Auto-fix before running remaining gate checks (runs clippy --fix, cargo fmt, custom fix overrides)
cargo warden gate --fix

# Control parallel execution of consecutive read-only steps (fmt, machete, deny, audit, read_only custom steps)
cargo warden gate --parallel
cargo warden gate --no-parallel

# Filter or skip specific gate steps (including custom step names)
cargo warden gate --only fmt,clippy,doc
cargo warden gate --skip test

# Include cargo-machete, cargo-deny, and cargo-audit (plus full_only custom steps)
cargo warden gate --full
```

### 9. Git Hook Management (`hook`)

```bash
# Install a pre-commit hook running `cargo warden check` (or `--staged` for incremental checks)
cargo warden hook install
cargo warden hook install --staged

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
source_dirs = ["crates/my-app/ui", "crates/my-app/src"]
tr_macros = ["tr", "t", "gettext"]
strict = false
check_placeholders = true
check_unused = false

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
parallel = true

[gate]
include_checks = true
parallel = true
# rustfmt_toolchain = "1.85.0"

[[gate.test_groups]]
name = "core"
packages = ["my-types", "my-core"]
features = []

# Optional custom gate steps
# [[gate.custom_steps]]
# name = "license-header"
# command = "cargo about check"
# cargo_tool = "about"
# read_only = true
# fix_command = "cargo about generate"
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
