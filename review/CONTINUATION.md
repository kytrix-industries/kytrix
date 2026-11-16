# 剩余工作交接（已完成，2026-11-17）

> 本文件记录 2026-11-16 的剩余工作，**现已全部完成**。
> 分支 `ui-core-p0-p2-remediation`，累计 91 个 commit（基线 `a9a4220`）。
> 最终台账与验收结论见 `review/remediation-progress.md`（Round 75 快照）与
> `review/FINAL-ACCEPTANCE.md`。

## 完成情况

| 项 | 状态 |
|---|---|
| win-1 Windows IME 禁用路径 | ✅ commit `f863426` |
| mac-2 macOS IME `replacementRange` | ✅ commit `f863426`（P1 常量修复 `3bd8846`） |
| win-2 自绘标题栏交互（用户选 B） | ✅ commit `f863426`（消除 Win32 模态循环） |
| txt-1 亚像素定位（用户选 A） | ✅ commit `f863426` |
| a11y-1 `SetTextSelection` 死动作 | ✅ 范围收敛（已移除默认声明） |
| 第三轮对抗式复审 | ✅ 1 P1 + 2 P2 修复（`3bd8846`），P3 记入 backlog |

三平台门禁：Windows 921 / Linux 835 / macOS 825 lib 测试（+6/4/8/39），
clippy `-D warnings` / fmt / doc 全干净。

## P3 backlog（已完成，commit `4489d28`；仅剩人工/真机项）

- win-1：禁用态下系统可能在焦点/输入法切换后重新关联 IME，需 `WM_INPUTLANGCHANGE`/
  `WM_IME_SETCONTEXT` 幂等再摘除（真机验证丢字风险）。
- win-2：无 Esc 取消拖动；拖动期间右键/滚轮以客户区形式到达应用；UI 卡顿时快速双击可能
  退化；`WM_CAPTURECHANGED`/`WM_CANCELMODE`/定时器 tick 无测试；`MIN_VISIBLE_EDGE`
  是物理常量；最大化还原只按水平比例。
- txt-1：ppem 仍整数化；端到端亚像素测试只在 1x；无文本像素 golden。
- mac-2：doc 坐标依赖 `markedRange.location == 0`；surrounding-text 通路待补。
- 真机目视：高 DPI 下关 hinting 的字形观感、win-2 跨 DPI 拖动、macOS 真实中文/日文 IME。

## 操作须知（踩坑记录）

- **只用 `edit` 工具改 Rust 源码**。PowerShell 多行字符串替换多次损坏文件（CRLF、
  反引号、控制字符），事故记录见提交历史里的修复提交。
- 三平台验证：`tools/remote/rsync.ps1 -Target linux|macos` → `tools/remote/cargo.ps1`；
  macOS 需 `+nightly-2026-11-09`。
- 不要对同一远端 target 并发跑两个 cargo。
