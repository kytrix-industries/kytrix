# 剩余工作交接（Round 69 快照）

> 分支 `ui-core-p0-p2-remediation`，HEAD `f65d1cf`，**88 个 commit**（基线 `a9a4220`）。
> 工作树干净；三平台门禁全绿（Win 893 / Linux 822 / macOS 812 lib 测试 + 54）。
> 详细台账：`review/remediation-progress.md`；验收报告：`review/FINAL-ACCEPTANCE.md`。

## 已完成

P0 2/2（+ 本会话新发现 1 条已修）、P1 13/13、两轮对抗式复审新增 P1 15/15、
复审 P2 26/26（2 条经证实非缺陷）、复审 P3 20+/20+、原始 P2 46/50。

## 剩余 4 条原始 P2（按建议顺序）

### 1. win-1：Windows IME 禁用路径
- **证据**：`review/findings-windows-platform.md` 的 IME 条目。
- **现状**：`platform/windows/ime.rs` 只有启用态处理（`WM_IME_STARTCOMPOSITION` 等）；
  没有 `ImmAssociateContext(hwnd, None/prev)` 的禁用/恢复路径。
- **做法**：`App` 加按窗口的 `ime_enabled` 映射（仿 `cursor_pending`/`capture_request`
  的单槽模式或 `a11y_delivered` 的 HashMap）；泵每轮取变更并调
  `ImmAssociateContext`，保存/恢复旧 HIMC；禁用时 `wndproc` 忽略 `WM_IME_*` 且不派发
  `ImeEvent`。补 `wndproc_tests` 用例（禁用后 `WM_IME_COMPOSITION` 不产生事件）。

### 2. mac-2：macOS IME `replacementRange` 被忽略
- **证据**：`review/findings-macos.md` 的 IME 条目。
- **现状**：`platform/macos/window.rs` 的 `GuiView` 实现 `NSTextInputClient`，
  `setMarkedText:replacementRange:` 忽略 replacementRange（总是追加到末尾/替换全部）。
- **做法**：把 `replacementRange` 转成「字节区间」随 `ImeEvent::Preedit` 一起上报（新增
  字段或 `PreeditRange`），App/文本输入层按区间替换；无选区时 `{NSNotFound, 0}` 表示插入。
  补单元测试（纯范围换算逻辑抽成函数可测）。

### 3. win-2：系统菜单 / 标题栏拖动进入 Win32 模态循环阻塞泵（**需用户裁决**）
- 方案 A（推荐）：接受阻塞并文档化到 `WindowOptions` 与 `review/FINAL-ACCEPTANCE.md` §5。
- 方案 B：自绘非客户区（WM_NCCALCSIZE + WM_NCHITTEST + 自绘标题栏/按钮），2–3 轮。

### 4. txt-1：亚像素定位 + hint 策略（**需用户裁决**）
- 方案 A（推荐）：按 Skia 默认（开亚像素定位、关 hinting），改 `text/raster.rs` 的
  `Render` 选项 + 字形键跟随 `subpixel_x` 量化；标注可回退常量。
- 方案 B：保持现状，在 `text.rs` 文档写明取舍。

## 操作须知（踩坑记录）

- **只用 `edit` 工具改 Rust 源码**。PowerShell 多行字符串替换多次损坏文件（CRLF、
  反引号、控制字符），事故记录见提交历史里的 `chore` 修复。
- 三平台验证流程：`tools/remote/rsync.ps1 -Target linux|macos` → `tools/remote/cargo.ps1 -CargoArgs "..."`；
  macOS 需 `+nightly-2026-11-09`（见 `tools/remote/cargo.ps1`）。
- 不要对同一远端 target 并发跑两个 cargo。
- 每次提交前跑：`cargo test -p ui_core --features test-support`、
  `cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings`、
  `cargo +nightly fmt -p ui_core -- --check`、`cargo doc -p ui_core --no-deps`。

## 完成后的收尾

1. 三平台全量验证（含集成测试与示例 target）。
2. 更新 `review/FINAL-ACCEPTANCE.md` 的 §5（待决项表清零）与 §1 数量表。
3. 台账追加最终快照。
4. 提交后将本目标标记为完成。
