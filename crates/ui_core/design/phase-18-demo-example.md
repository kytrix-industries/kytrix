# P18：demo 并入 `ui_core` 的 example

> 背景：P09–P17 的演示程序是一个独立 workspace crate `crates/ui_core_demo`。它只被
> `ui_core` 使用，独立 crate 带来的 workspace 条目、feature 转发、测试 dev-dependency 和
> 文档命令都属于额外结构。本阶段把它删除，改成 `ui_core` 自己的 Cargo example。
>
> 用户已确认：目标名为 `demo`；`ui_core` 的 `hud` feature 仍默认关闭；另建
> `snapshot_demo` example，与 `demo` 共用基础 scene。

> **窗口/布局更新**：`snapshot_demo` 的尺寸改为 `scene::window_size()`（内容 `960×740` + 平台顶部
> 安全区）；截图尺寸因此随平台变化（macOS `960×778`、Linux `960×776`、Windows `960×740`）。
> 下文的 `960×740` 是 P18 当时的记录。

---

## 0. 结论摘要

| 项           | 决定                                                                                                                                                             |
|--------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| crate 结构   | 删除 `crates/ui_core_demo` 整个目录、workspace member 和 workspace dependency；不再有独立 demo crate                                                             |
| 代码位置     | `..`；`main.rs` 是窗口 example，`snapshot.rs` 是离屏 example，二者共用同目录的 `scene` 模块                                                                      |
| example 目标 | `[[example]] name = "demo"` → `examples/demo/main.rs`；`[[example]] name = "snapshot_demo"` → `examples/demo/snapshot.rs`                                        |
| HUD          | `ui_core` 的 `hud` 仍默认关闭；`cargo run -p ui_core --example demo --features hud` 才显示 HUD / 响应 F12                                                        |
| 截图         | `snapshot_demo` 以 `test-support` 为 `required-features`；命令 `cargo run -p ui_core --example snapshot_demo --features test-support -- --snapshot out\demo.png` |
| 测试         | 原 `crates/ui_core_demo/tests/scene.rs` 的内容按场景子模块拆成 example 内的 unit test；共享的 `TestApp` 搭建放在 `scene/test_support.rs`（仅 test）              |
| 无兼容层     | 不保留 `ui_core_demo` 包、`snapshot` feature、`--no-default-features` 语义或旧命令                                                                               |

## 1. 目标与非目标

### 1.1 目标

- `cargo run -p ui_core --example demo` 能启动原来的真实窗口演示。
- `snapshot_demo` 能生成与 P15/P16/P17 相同的离屏展示图，且不安装 HUD。
- demo 的全部场景代码、单元测试、截图种子状态都留在 example 内，不污染 `ui_core` 的公开 API。
- `cargo test -p ui_core`、clippy、fmt、doc 和交叉检查仍可执行；example 的测试有明确命令。
- 删除独立 crate 后，workspace 依赖图、CI 路径、README 与当前设计文档不再引用它。

### 1.2 非目标

- 不改变 demo 的六张卡片、交互、尺寸、文案和像素布局。
- 不改变引擎 HUD、渲染、IME、a11y 的任何行为。
- 不为 example 新增 crate、feature、依赖或共享库。
- 不保留 `ui_core_demo` 的向后兼容命令或别名。

## 2. 文件布局

```text
crates/ui_core/
├── Cargo.toml
└── examples/
    └── demo/
        ├── main.rs             # example "demo"：真实窗口
        ├── snapshot.rs         # example "snapshot_demo"：离屏 PNG
        ├── scene.rs            # 场景模块根：Scene / 常量 / 事件钩子
        └── scene/
            ├── a11y.rs
            ├── card.rs
            ├── cjk.rs
            ├── focus.rs
            ├── ime.rs
            ├── resources.rs
            ├── swatch.rs
            └── test_support.rs # #[cfg(all(test, feature = "test-support"))]
```

- `main.rs` 和 `snapshot.rs` 都使用 `pub mod scene;`。因为两个源文件在同一目录，Rust 的模块解析
  自然找到同一份 `scene.rs` / `scene/`，不需要 `#[path]`、`include!` 或额外 lib target。
  `pub mod` 让 `Scene` 的公开方法（尤其只被 `snapshot_demo` 调用的 `preview`）在 example 二进制里
  仍属于可达 API，避免 `--all-features` 构建 `demo` 时产生测试专用 `dead_code`。
- `scene.rs` 保持为场景模块根；子模块布局沿用 P15，不拆成新的 crate 或库。
- `snapshot.rs` 不是自动发现的 `examples/*.rs` / `examples/*/main.rs`，因此必须在
  `Cargo.toml` 显式声明 `[[example]]`。同时显式声明 `demo`，让两个目标并列可读。
- `snapshot_demo` 设 `required-features = ["test-support"]`，并设 `test = false`：默认
  `cargo test` 只跑 `demo` 目标里的共享测试副本，`snapshot_demo` 只负责截图。Cargo 的
  `--all-targets` 仍会把两个 example 都当测试目标，因此会各跑 36 项；这是 Cargo 语义，不是
  两套测试。`demo` 必须显式写 `test = true`：Cargo 显式 `[[example]]` 的 `test` 默认是
  `false`，否则默认 `cargo test` 不会跑它的测试。

`Cargo.toml` 目标片段：

```toml
[[example]]
name = "demo"
path = "examples/demo/main.rs"
# 显式 example 的 test 默认是 false；不写 true 时 `cargo test --example demo` 会跑 0 项。
test = true

[[example]]
name = "snapshot_demo"
path = "examples/demo/snapshot.rs"
required-features = ["test-support"]
test = false
```

## 3. 两个入口

### 3.1 `demo`

`examples/demo/main.rs` 只保留：

1. `pub mod scene;`
2. 创建 tokio runtime；
3. `App::new` + `Scene::open` + `Scene::install`；
4. `Application::new().run(app)`。

不再解析 `--snapshot`：截图是另一个 example target，不是窗口程序的隐藏分支。

### 3.2 `snapshot_demo`

`examples/demo/snapshot.rs` 复用 `Scene::build_into` / `Scene::preview`：

1. 解析旧参数形式 `--snapshot [path]`，默认路径仍为 `kytrix-demo.png`；
2. `TestApp::with_options` 建同尺寸窗口；
3. 建树、跑首帧、`Scene::preview` 播种、再跑一帧；
4. `TestApp::snapshot` 读回、`image::RgbaImage` 编码 PNG。

该 example 的 `required-features = ["test-support"]` 保证 `TestApp` 与离屏渲染存在；没有
feature 时 Cargo 在编译前报告缺少 target feature，不会静默失败。

`Scene::preview`、`ImeField::sample`、`A11yState::sample` 保留在公开的 scene 模块里，供
`snapshot_demo` 与测试共用；测试专用的 `dispatch_event` / `tick` / `ime_text` 则只在
`all(test, feature = "test-support")` 下编译（见 §5.2），避免正式 `demo` 构建留下测试入口。

## 4. feature 与命令

### 4.1 HUD

原 demo crate 的 `default = ["hud"]` 无法保留：Cargo example 不能拥有独立默认 feature。用户选择
保持引擎的 feature 契约：

- `ui_core` 的 `hud` 仍默认关闭；
- 不带 feature 的 `demo` 是完整六卡片场景，但没有 HUD / F12；
- 带 `--features hud` 的 `demo` 安装 HUD，副标题出现「F12 调试 HUD」，F12 切换；
- `snapshot_demo` 无论是否带 `hud` 都不安装 HUD（走 `build_into`，不调用 `Scene::install`）。

```powershell
# 无 HUD
cargo run -p ui_core --example demo

# 带 HUD
cargo run -p ui_core --example demo --features hud
```

`--no-default-features` 不再是 demo 的开关；引擎本来就没有默认 feature。

### 4.2 截图

```powershell
cargo run -p ui_core --example snapshot_demo --features test-support -- --snapshot out\demo-showcase.png
```

- 不再有 demo 的 `snapshot` feature；`test-support` 是唯一开关。
- `image` 已是 `ui_core` 的正式依赖（P13 资源解码），不需要 optional dependency。
- 该命令的 release 版本同样可用；`test-support` 只影响 example 构建，不进入引擎发布 API。

## 5. 测试迁移

### 5.1 原测试面

原 `crates/ui_core_demo/tests/scene.rs` 是独立 crate 的 integration test，通过
`ui_core_demo::scene` 访问场景 API。example 是 binary target，没有可供 integration test
导入的 library target；为了测试再建 lib 会把 demo 变回 crate，违背本阶段目标。

### 5.2 新测试组织

原 13 项测试按职责搬进场景模块的 `#[cfg(all(test, feature = "test-support"))]` unit test：

| 新位置            | 测试                                                                              |
|-------------------|-----------------------------------------------------------------------------------|
| `scene.rs`        | 首帧提交/图元/无豆腐块、Tab 序与焦点面板、preview 种子、HUD 默认不安装 + F12 开关 |
| `scene/a11y.rs`   | a11y 角色/名称/值、方向键更新滑块与 a11y value                                    |
| `scene/swatch.rs` | 点击色块更新选中态与 a11y                                                         |
| `scene/ime.rs`    | IME 只在输入框聚焦时生效、caret hint 跟随文本、组合中 Esc、失焦/窗口失焦取消组合  |
| `scene/cjk.rs`    | CJK 段落折行                                                                      |

共享搭建（`setup` / `setup_installed` / `frame` / `press`）放入
`examples/demo/scene/test_support.rs`；它只在 `all(test, feature = "test-support")` 下声明，
符合 AGENTS §5.1 的「同一模块下 test_support」约定。每个测试仍使用 `TestApp`，不访问引擎私有实现。

不建立 `examples/demo/tests/`：integration test 无法导入 binary example；unit test 保持在场景
模块边界内。也不使用 `tests.rs` 集中存放测试。

### 5.3 命令

```powershell
# 无 HUD：23 个已有单元测试 + 12 个场景测试（35）
cargo test -p ui_core --features test-support --example demo

# 有 HUD：再加 F12 场景测试（13 个场景测试，共 36）
cargo test -p ui_core --features "hud,test-support" --example demo

# 引擎默认测试：668 lib + 23 demo 单元测试（场景测试需要 test-support）
cargo test -p ui_core

# 全量（无 HUD）：723 lib + 集成测试 + 35 demo 场景测试
cargo test -p ui_core --features test-support

# 全量（带 HUD）：729 lib + 集成测试 + 36 demo 场景测试
cargo test -p ui_core --features "hud,test-support"
```

显式 `[[example]]` 的 `test = true` 让 `cargo test` 也会运行 example 的单元测试；但没有
`test-support` 时，13 项场景测试被 cfg 排除，只跑 23 项场景单元测试。因此：

- 只跑引擎默认测试不能证明 demo 场景完整；
- `--features test-support` 才会运行全部场景测试；
- `--example demo` 是聚焦调试命令，不是唯一入口。

## 6. 删除与文档同步

- workspace 根：删除 `crates/ui_core_demo` member 和 `ui_core_demo` workspace dependency。
- `crates/ui_core_demo/`：整个删除；不保留空目录或兼容包。
- `Cargo.lock`：由 Cargo 删除 `ui_core_demo` 包及其依赖边。
- `.github/workflows/gui-ci.yml`：
    - 触发路径删除 `crates/ui_core_demo/**`；
    - clippy / fmt 只保留 `-p ui_core`；
    - Windows job 的 test-support 步骤改为 `--features "hud,test-support"`，因此同时运行 36 项
      demo 场景测试与 HUD 集成测试；
    - Linux/macOS job 的 test-support 步骤会运行 35 项 demo 场景测试；Linux 增装
      `fonts-noto-cjk` 以满足 V-105 的 CJK 断言，macOS 使用系统字体。
- `..`：更新运行/截图/测试/门禁命令，删除 demo crate 的依赖状态。
- `phase-17-debug-hud.md`：把 demo 默认 feature 的描述改为 P18 后的显式 `--features hud`。
- `phase-15-demo-showcase.md` / `phase-16-ime-caret.md`：更新当前路径和命令；历史阶段结论保留。
- `design/DEVIATIONS.md`：追加 P18 节，登记「demo 不再是 crate」「example 无法有独立默认 feature」
  「snapshot_demo 与 test-support」和新的验收记录。

## 7. 验收

| 项                       | 命令 / 判据                                                                                                                     |
|--------------------------|---------------------------------------------------------------------------------------------------------------------------------|
| 结构                     | `cargo metadata` 无 `ui_core_demo` package；`crates/ui_core_demo` 不存在                                                        |
| 编译                     | `cargo check -p ui_core --example demo`；`cargo check -p ui_core --example demo --features hud`                                 |
| 截图编译                 | `cargo check -p ui_core --example snapshot_demo --features test-support`                                                        |
| 场景测试                 | `cargo test -p ui_core --features test-support --example demo`（35）                                                            |
| HUD 测试                 | `cargo test -p ui_core --features "hud,test-support" --example demo`（36）                                                      |
| 全量测试                 | `cargo test -p ui_core --features "hud,test-support"`（729 lib + 36 demo + 集成）；`--all-targets` 会因两个 example 各跑一次 36 |
| 真实运行                 | `cargo run -p ui_core --example demo --features hud`，窗口出现、F12 切换；                                                      
 `snapshot_demo` 输出 PNG |
| 全门禁                   | `cargo test -p ui_core`；`cargo test -p ui_core --features test-support`；                                                      

`cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings`；
`cargo +nightly fmt -p ui_core -- --check`；`cargo doc -p ui_core --no-deps` |
| 交叉 | macOS `cargo check --features hud --target aarch64-apple-darwin`；Linux 按宿主环境记录限制 |

## 8. 风险与边界

- **Cargo example 不能有独立默认 feature**：因此 HUD 必须显式 `--features hud`；这不是回退，
  而是合并后的唯一正确语义。
- **example 的 23 项单元测试会被 plain `cargo test` 执行，但 13 项场景测试需要 `test-support`**：
  只跑 `cargo test -p ui_core` 不能证明场景测试全跑；CI/README 明示 `test-support` 与
  `--example demo` 两种入口。
- **两个 example 重复编译共享 scene/测试**：Cargo 没有「example 公共库」；共享源码、独立二进制是
  example 的标准能力，避免为此新增 crate 或把 demo 代码放进引擎 API。`--all-targets` 会把共享
  测试在两个 example 里各跑一次；CI 使用非 all-targets 命令，只跑一次。
- **snapshot_demo 依赖 test-support**：离屏截图本来就使用 `TestApp` / WARP readback；该 feature
  不进发布 API。Cargo 的 `required-features` 在编译期明确报错。
- **Linux CI 运行 demo 场景测试依赖 CJK 字体**：V-105 的宿主依赖仍在；CI 通过
  `fonts-noto-cjk` 提供字体，不能把该包缺失的本地容器失败当成代码回归。

## 9. 实施记录（2026-11-13，Windows）

| 项                 | 实测                                                                                                                                                                                                                                                                                               |
|--------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 结构               | `cargo metadata --no-deps` 只有 `ui_core`；`crates/ui_core_demo/` 不存在；根 `Cargo.toml` / `Cargo.lock` / CI 无旧 member、dependency、path                                                                                                                                                        |
| 默认测试           | `cargo test -p ui_core`：668 lib passed / 1 ignored + **23** demo 单元测试                                                                                                                                                                                                                         |
| test-support       | `cargo test -p ui_core --features test-support`：723 lib + **35** demo 场景测试；集成 `frame_record 6` / `layout 4` / `snapshot 8`                                                                                                                                                                 |
| HUD 全量           | `cargo test -p ui_core --features "hud,test-support"`：729 lib + **36** demo 场景测试；集成 `hud 7` / `frame_record 6` / `layout 4` / `snapshot 8`                                                                                                                                                 |
| 聚焦               | `cargo test -p ui_core --features test-support --example demo` → 35；加 `hud` → 36                                                                                                                                                                                                                 |
| lint / 格式 / 文档 | `clippy --all-targets`（默认与全 feature）exit 0；`fmt --check` exit 0；`doc --features hud --no-deps` 零 warning；`git diff --check` exit 0                                                                                                                                                       |
| 截图               | `snapshot_demo --features test-support`：960×740，181451 B；加 `hud`：183065 B，SHA256 `713AEF84…`，与迁移前 `ui_core_demo --features snapshot` 的输出**逐字节相同**。只差 `test-support` 时仅副标题少「F12 调试 HUD」，HUD 均不安装                                                               |
| 实机               | `cargo run -p ui_core --example demo --features hud`：窗口标题正确、HUD 显示 `FPS 60.0 / Frame 16.66 ms / CPU 1.08 ms / GPU 0.01 ms / Draw 1 / Prims 250 / Inst 761 / 1200x925 @ 1.25x D3D12`；F12 由场景测试覆盖                                                                                  |
| 交叉               | macOS `cargo check -p ui_core --features hud --target aarch64-apple-darwin` 通过；`--examples` 因 dev-dependency `criterion → alloca` 需要宿主 `cc` 而失败（本机无 C 编译器），不是 example 代码错误。Linux 仍因宿主缺 `pkg-config` / Wayland / fontconfig sysroot 失败（既有环境边界 V-04/V-108） |
