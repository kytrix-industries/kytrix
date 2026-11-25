# 贡献指南（仓库级约定）

本文件只写「`AGENTS.md` 之外、且必须按**机器 / 平台差异**执行」的约定。代码风格、依赖、序列化、
错误处理、注释与验收命令以根 `AGENTS.md` 为准（§8：`cargo test`、`cargo +nightly clippy -- -D warnings`、
`cargo +nightly fmt`）。**设计与实现的偏差登记**只有一个权威表：`.docs/text-input-redesign-design.md` §11.2。

---

## 1. 本机配置：`.gitattributes`（**不入库**，需自行创建）

**结论（P4-12）**：仓库**不跟踪** `.gitattributes`。根 `.gitignore` 第 3 行的 `.*` 规则会忽略所有点文件
（只豁免 `!.github` / `!.gitignore` / `!.cargo`），仓库决策是**保持不跟踪**、也**不**添加 `!.gitattributes` 豁免。

| 项 | 说明 |
|---|---|
| 为什么需要它 | 统一 EOL：源码 / 脚本以 LF 入库。`core.autocrlf=true` 的 Windows 上若缺该文件，`git archive` / 检出会把 `.sh` 转成 CRLF，远端 macOS/Linux 直接 `$'\r': command not found` |
| 为什么不在库 | 提交策略已定：不把本机 EOL 策略强加给远端 / CI（历史轮次反复出现 CRLF 工件与审查口径差异，P4-12 裁决为「文档记录」而非跟踪） |
| 谁要建 | 每台开发机（尤其 Windows），放在**仓库根**：`<repo>/.gitattributes` |
| 不要做 | 不要 `git add .gitattributes`；不要在 `.gitignore` 里加 `!.gitattributes`（那等于变相跟踪） |

**请按以下内容配置**（与维护者本机一致；注释可保留，也可删除）：

```gitattributes
# 统一跨平台换行符：脚本与源码必须以 LF 入库，
# 否则 `core.autocrlf=true` 的 Windows 上 `git archive` 会把脚本转成 CRLF，
# 导致远端 macOS/Linux 无法执行（"$'\r': command not found"）。
* text=auto

*.sh        text eol=lf
*.ps1       text eol=crlf
*.rs        text eol=lf
*.toml      text eol=lf
*.md        text eol=lf
*.glsl      text eol=lf
*.hlsl      text eol=lf
*.metal     text eol=lf
*.json      text eol=lf
*.rc        text eol=lf
Cargo.lock  text eol=lf

*.png       binary
*.ico       binary
*.ttf       binary
*.otf       binary
```

**验证**（三条都要过）：

```powershell
git check-ignore -v .gitattributes                       # 预期：.gitignore:3:.*  → 说明不会被误提交
git ls-files .gitattributes                              # 预期：空（未跟踪）
git check-attr text eol -- crates/ui_core/src/app.rs     # 预期：text: set / eol: lf
```

若将来要改为跟踪（需维护者裁决）：必须同时加 `!.gitattributes`、`git add -f` 全仓重新归一，
并重跑一次 CRLF 检查（`git grep -lI $'\r'`）——这是一次独立批次，不在普通 PR 里做。

---

## 2. Linux 尺寸基线差异的机理（P4-75）

`ui_core` 有几个 `size_of` 基线在 Linux 上比 Windows/macOS **小 8 B**，断言按 `cfg(target_os = "linux")`
分平台绑定。**不要把它们「统一」成一个常量**，也不要按 `cfg` 猜测新数值。

| 结构 | Windows / macOS | Linux | 断言位置 |
|---|---|---|---|
| `Node` | 336 B | 328 B | `node/tree/tests.rs`（`NODE_SIZE_BASELINE`） |
| `Shadows` | 88 B | 80 B | `paint/style.rs`（`SHADOWS_SIZE_BASELINE`） |
| `PaintSpec` | 192 B | 184 B | `paint/style.rs`（`PAINT_SPEC_SIZE_BASELINE`） |
| `TextContent` | 112 B | 112 B | `node/tree/tests.rs`（无差异） |

**机理**：不是目标平台 ABI 差异，而是**依赖图 feature 差异**——Linux 依赖链上的 `wayland-backend` 打开了
`smallvec` 的 `union` feature，`SmallVec` 的内联头从 24 B 缩到 16 B（差 8 B），凡内联含 `SmallVec` 的结构
在 Linux 上整体少 8 B。`cfg(target_os = "linux")` 只是「这套 feature 只在该目标启用」的工程代理。

**纪律**：

1. 新增字段若让 `Node` 超过 336 B，**先装箱搬走冷字段**（该预算已无余量），不要改基线凑数。
2. 依赖 feature 形状变了（升级 `wayland-backend`、换 `smallvec` 特性），按**实跑结果**更新基线，并在提交说明里写清机理。
3. 这段背景**写提交说明 / 本文**，**不写代码注释**（`AGENTS.md` §1.6）。

---

## 3. 三端 `readback` 的 `cfg` 口径（P4-74，已统一）

三端 `readback` 模块的编译条件**已统一为** `#[cfg(any(test, feature = "test-support"))]`：

| 文件 | `readback` 的 cfg |
|---|---|
| `crates/ui_core/src/render/windows.rs` | `#[cfg(any(test, feature = "test-support"))]` |
| `crates/ui_core/src/render/macos.rs` | `#[cfg(any(test, feature = "test-support"))]` |
| `crates/ui_core/src/render/linux.rs` | `#[cfg(any(test, feature = "test-support"))]` |

统一后 Linux 的纯 `cfg(test)` 也能编译读回路径，于是 `cargo test -p ui_core`（默认 features）在
Linux 上也会编译 `render/linux/readback.rs`；该模块的 `ReadbackImage` 尺寸字段因此必须在锁内测试里被
读取（`compositor.rs` 的 `pixel()` 用 `image.width/height` 做边界断言），否则 Linux 默认 clippy 会报
`dead_code`。

**门禁矩阵（三端一致）**：
- `cargo test -p ui_core`（默认 features）：必须 0 failed，且 Linux 默认 clippy 必须通过（Linux 的 GPU 并发用例需要可用 Vulkan 设备；无 ICD 时该用例硬失败，且**不受** `KYTRIX_TEST_SKIP_GPU_PIXELS` 影响——该开关只被 `tests/snapshot.rs` 与 COLRv1 夹具读取）；
- `cargo test -p ui_core --features test-support`：必须 0 failed；
- `cargo +nightly clippy -p ui_core --all-targets -- -D warnings`：默认与 `--features test-support` 都要过。

**纪律**：再改这行 cfg 必须同时复核上述矩阵在**三端**的编译结果，并同步
`crates/ui_core/tests/golden/README.md` 的「平台」节。

---

## 4. 像素 golden 与 GPU 环境（P4-02 / P4-03）

| 约定 | 内容 |
|---|---|
| 基线归属 | 每个平台只跟 `crates/ui_core/tests/golden/<device>/` 里**自己那份**比；跨设备比同一份 golden 会永久红 |
| 彩色 emoji | **只有 Windows（`warp/`）建基线**；`lavapipe/` `metal-real/` 按设计不建（字体不同，跨设备无意义） |
| 缺字体 / 缺后端 | 像素用例**默认硬失败**；只有 `KYTRIX_TEST_SKIP_GPU_PIXELS=1` 显式豁免（用途与四条限制见 golden README） |
| 更新基线 | bless 前核对「生成环境与字体版本」表、确认未置位豁免开关；变更逐张记录原因（见 golden README） |

完整流程、容差与失败信息格式：`crates/ui_core/tests/golden/README.md`。

---

## 5. 文档登记惯例

| 内容 | 登记位置 |
|---|---|
| 设计与实现的偏差（编号连续） | `.docs/text-input-redesign-design.md` §11.2 |
| WONTFIX（明确不修） | 同文件 §11.11.2（含理由分类：环境限制 / 模型固有 / 测试读面 / 微优化；工件类单列） |
| 引擎公开面增量 | `crates/ui_core/design/custom-gui-engine-design.md` §1.3（白名单表 + 增量子表） |
| 数字口径 | 文档中的门禁数字必须写实测值（禁止写旧值或占位；换工具链后重跑并同步 §11.7 表格） |

**不要**在代码里留注释解释历史原因（`AGENTS.md` §1.6）；背景进提交说明或上述文档。
