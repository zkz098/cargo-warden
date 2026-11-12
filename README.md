# cargo-warden 🛡️

A unified Cargo plugin and code quality toolkit for Rust and Slint projects.

`cargo-warden` 将原本分散在各个项目私有脚本、门禁与测试文件中的**架构防漂移守卫**与**代码质量工具**统一提炼为一个可插拔的 Cargo 插件与可执行 CLI。

---

## ✨ 核心特性

| 功能模块 | 命令 | 说明 |
| :--- | :--- | :--- |
| **模块依赖 DAG 与循环守卫** | `cargo warden dag` | 零外部 AST 依赖解析模块树，Tarjan 强连通分量算法检测模块循环依赖；校验组装模块（如 `bootstrap`）的反向依赖违规 |
| **Workspace 与 CI 漂移校验** | `cargo warden ci-parity` | 提取 `cargo metadata` 工作区成员，比对 CI 脚本（`.github/workflows/ci.yml`），防止新 crate 漏测或废弃 crate 残留 |
| **Slint 主题色值检查与替换** | `cargo warden theme [check\|apply\|generate]` | 扫描 `.slint` 裸 `#hex` 颜色字面量，自动重写为 `Theme.c<hex>` 并注入 import，从 TOML 生成 `theme.slint` |
| **Slint & Rust 双端 i18n 目录同步** | `cargo warden i18n [check\|generate]` | 解析标准 Gettext `.po` 翻译文件，自动生成 Rust 端二分查找静态查找表并校验双端一致性 |
| **认知复杂度检测** | `cargo warden complexity` | 基于 `syn` 的函数认知复杂度检测（SonarQube 标准：控制流嵌套层级加权、逻辑运算符跳转加权），精准定位坏味道函数 |
| **文件最大长度守卫** | `cargo warden file-length` | 检测文件物理行数，支持扩展名白名单与路径排除，防止产生难以维护的上帝文件（God Files） |
| **本地门禁流水线** | `cargo warden gate` | 本地复现 CI 门禁（fmt, clippy, doc, tests, audit），自动清洗子进程 Cargo 环境变量与工具链版本匹配 |
| **一键综合静态检查** | `cargo warden check` | 毫秒级一键运行当前项目配置的所有静态质量检查，极适合作为 Git pre-commit hook |
| **Git Hook 自动管理** | `cargo warden hook [install\|uninstall\|status]` | 一键安装、更新、卸载 `pre-commit` / `pre-push` 钩子，安全保留已有自定义脚本 |

---

## 🚀 安装与运行

### 本地安装
```bash
cargo install --path ~/projects/cargo-warden
```

安装后即可直接作为 Cargo 插件调用：
```bash
cargo warden --help
```
也可以作为独立 CLI 直接运行：
```bash
cargo-warden --help
```

---

## 🛠️ CLI 命令详解

### 1. 模块依赖 DAG 守卫 (`dag`)
```bash
cargo warden dag --root crates/my-core/src --forbid bootstrap
```
- 分析指定源码目录下的顶层 module 依赖关系图。
- 当出现循环引用（如 `a <-> b`）时抛出错误并输出环回路。
- 当有其他模块引入了 `--forbid` 指定的顶层组装模块（如 `bootstrap`）时阻止提交。

### 2. CI 漂移校验 (`ci-parity`)
```bash
cargo warden ci-parity --workflow .github/workflows/ci.yml
```
- 读取 Cargo Workspace 所有成员列表。
- 扫描 CI 工作流文件中的测试步骤（如 `nextest` / `llvm-cov` / `cargo test`）。
- 若存在未被 CI 覆盖的 workspace member，立即报错。

### 3. Slint UI 主题与硬编码色值守卫 (`theme`)
```bash
# 检查是否存在未映射的裸色值
cargo warden theme check --ui-dir ui/

# 自动将硬编码颜色批量替换为 Theme.c<hex>
cargo warden theme apply --ui-dir ui/

# 根据配置生成 theme.slint
cargo warden theme generate --out-file ui/theme.slint
```

### 4. Slint & Rust i18n `.po` 同步校验 (`i18n`)
```bash
# 检查当前 Rust 查找表是否与 .po 保持最新同步
cargo warden i18n check --lang-dir lang/ --out-file src/i18n/catalog.rs

# 根据各语言 .po 文件重新生成 Rust 查找表
cargo warden i18n generate --lang-dir lang/ --out-file src/i18n/catalog.rs
```

### 5. 认知复杂度检测 (`complexity`)
```bash
# 检测认知复杂度超过 20 的函数，忽略测试文件
cargo warden complexity --max 20 --paths src/ crates/
```
示例输出：
```text
[complexity] ❌ 1 function(s) exceed maximum cognitive complexity (threshold: 20):
  ❌ crates/engine/src/scheduler.rs:142 - `schedule_next()` score: 27 (> 20)
```

### 6. 文件最大长度检测 (`file-length`)
```bash
# 检查超过 800 行的 .rs 和 .slint 文件
cargo warden file-length --max-lines 800 --ext rs --ext slint --paths src/
```

### 7. 一键全项静态门禁 (`check`)
```bash
cargo warden check
```
读取 `warden.toml` 或 `Cargo.toml` 中的元数据配置，毫秒级快速运行所有静态守卫。

### 8. Git Hook 管理 (`hook`)
```bash
# 安装 pre-commit 钩子（默认运行 cargo warden check）
cargo warden hook install

# 安装 pre-push 钩子运行完整门禁
cargo warden hook install --hook pre-push --gate

# 查看钩子安装状态
cargo warden hook status

# 卸载钩子（仅移除 cargo-warden 托管区块，保留原有自定义脚本）
cargo warden hook uninstall
```
- 若仓库中已存在非 `cargo-warden` 管理的 hook，默认拒绝覆盖，可通过 `--append` 追加或 `--force` 替换。

---

## ⚙️ 配置文件 `warden.toml`

在项目根目录下创建 `warden.toml`（或在 `Cargo.toml` 中配置 `[workspace.metadata.warden]`）：

```toml
[dag]
roots = ["crates/limedl-core/src"]
forbid_imports = ["bootstrap"]

[ci_parity]
workflow = ".github/workflows/ci.yml"
test_patterns = ["nextest", "llvm-cov"]
ignore_crates = ["fuzz"]
fanout_crates = ["disk", "engine", "http", "store", "types"]

[slint_theme]
ui_dir = "crates/limedl-native/ui"
out_file = "crates/limedl-native/ui/theme.slint"
unchanged_colors = [
    "ffffff", "dc2626", "b91c1c", "fecaca", "fef2f2",
    "0b1104", "6b7280", "ffffff55"
]

[i18n]
lang_dir = "crates/limedl-native/lang"
out_file = "crates/limedl-native/src/i18n/catalog.rs"
domain = "limedl-native"
locales = ["zh_CN", "zh_TW", "en"]

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

[gate]
rustfmt_toolchain = "1.99.0"

[[gate.test_groups]]
name = "engine"
packages = ["limedl-types", "limedl-disk", "limedl-store", "limedl-http", "limedl-engine", "limedl-core"]
features = ["limedl-core/test-utils"]

[[gate.test_groups]]
name = "native"
packages = ["limedl-native"]
features = []
```

---

## 🔍 日志与排错 (`tracing`)

项目内置基于 `tracing` 的结构化诊断日志支持，输出自动定向至 `stderr`，不干扰标准命令结果输出：

- **详细度控制**：
  - `cargo warden check`：默认输出关键步骤与检查结果
  - `cargo warden check -v`：输出 INFO 级别诊断事件
  - `cargo warden check -vv`：输出 DEBUG 级别详细日志（扫描路径、配置加载、子进程派生等）
  - `cargo warden check -vvv`：输出 TRACE 级别超详日志（逐文件行数/AST 解析追踪、忽略规则匹配等）
  - `cargo warden check -q`：静默模式，仅输出错误信息
- **环境变量控制**：支持通过 `RUST_LOG` 自定义模块过滤（如 `RUST_LOG=cargo_warden::checks::complexity=trace cargo warden check`）。

---

## 📄 License
Licensed under Apache-2.0 or MIT at your option.
