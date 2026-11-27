# P3 对抗性审查（真实窗口验收 / 三端 / 文档 / 规范）

> 审查人：p3-review-acceptance（独立子 agent，只读审查）
> 时间：2026-11-27 16:30–17:00（Asia/Shanghai；16:57 末次复核：Windows 补跑 idle/theme PASS，仓库源码未再变）
> 对象：P3 未提交工作树；基线 `f24a3441`（P2）；`git diff f24a3441` + 未跟踪 `tools/real-pixel-check-{macos,linux}.sh`
> 结论：**0 个 P0、0 个 P1；2 个 P2、4 个 P3**。代码与门禁本身绿、真实窗口像素证据强（本轮独立复算全部成立），但 P3-6 验收证据链与 P3 文档仍有需在提交前补齐/登记的项（详见 Findings）。**按“零 finding 才可提交”标准：当前不可直接提交；先处置 P3A-01/02（或由用户/lead 明确豁免并登记），并修正 P3A-05 的文档数字。**

## 1. 范围与方法

- 读设计 §6 P3、§7.4/§7.5、§10 DoD、§14（审查期间 16:50 刚写入），`implementation/p3-summary.md`、`.docs/gui-migration/spec-deltas/kit-gallery.md` §14，以及 `AGENTS.md`。
- 逐行审查 `crates/ui_kit/examples/gallery.rs` 的 `--real-pixel-check` 全部实现（`git diff f24a3441`：gallery +2037/−4；P3 代码总 diff 23 文件 +3416/−125，含 kit-gallery 文档 +23）、两个新脚本。
- 三端证据（`E:\kytrix\target\real-pixels\{windows,macos-remote,linux-remote}`）：读 `report.txt`/PNG，并用 Pillow 独立复算（不依赖 gallery 自身判定）；用 SSH BatchMode 只读核对远端原始证据哈希、源码同步状态、sway/TCC 运行痕迹。
- 本机复跑门禁与 gallery 命令（未改仓库；证据写到独立 `target/p3-review-windows/`，未覆盖 lead 的 `target/real-pixels/windows/`）。
- 状态：`git status` 中只有 P3 代码 22 文件 + `tools/`（untracked）+ 文档 agent 回写的 `.docs/gui-migration/spec-deltas/kit-gallery.md`；本审查文件位于 gitignored 的 `.docs` 下，未产生额外仓库改动。

## 2. Findings

### P3A-01 [P2] 三端 `--idle-check` / `--theme-check` 无 P3 HEAD 证据（§7.4 每端命令缺 2/5）

- 设计 §7.4（`popup-overlay-redesign-design.md:864`）仍写“每端：`gallery --idle-check`、`--theme-check`、`--debug-stack`、`--pixel-check`、`--real-pixel-check`、3 级菜单、残影像素采样”；§14 只交付 `--real-pixel-check` 三端 + `--debug-stack`（日志在 `/tmp`、`/private/tmp`）证据。
- 时间是旧的：Windows `E:\kytrix\target\gallery-p3r3-final-idle-check.log` / `gallery-p3r3-final-theme-check.log` 为 **2026-11-25 21:07**（gui-migration 批次，不是弹层 P3）；Ubuntu 最新 `/tmp/*idle*/*theme*` 为 **11-25/11-26**；macOS 最新 `/private/tmp/r2rev2-gallery-{idle,theme}.log` 为 **11-26**。**macOS / Ubuntu 在 P3 HEAD 均无 idle/theme 运行记录。**
- Windows 已由本审补跑（2026-11-27 16:57，P3 HEAD，`CARGO_TARGET_DIR=E:\kytrix\target`）：`--idle-check` **PASS（3 s，0 frames rendered while idle）**、`--theme-check` **PASS（0 failure across Dark/Light/Dark）**，exit 0；日志 `E:\kytrix\target\review-p3-idle.log`、`review-p3-theme.log`。Windows 这项不再缺。
- 影响：P3 改了动画（FadeScale、animation tick）、Dialog/ColorPicker 与 `FrameStats`；空闲帧/主题检查正是这些改动的回归面，P2 旧证据不能替代 macOS/Linux 的 P3 验收。§10 DoD 6 的“三端…通过并归档”仍缺 2/3。
- 建议：macOS、Ubuntu 各补跑 `--idle-check`、`--theme-check`（macOS 走 Terminal；Ubuntu 走 nested sway），日志归档；或由用户/lead 在 §14 增加显式豁免（说明 P3 不触碰主题/空闲路径、沿用 P2 证据）并改 DoD 6 措辞。**这是当前剩余的最主要验收缺口。**

### P3A-02 [P2] Linux 真实像素验收的命令/环境不可复现；脚本 nested-sway 分支未被验证且会泄漏 compositor

- 证据（16:29 的 Linux 50 checks PASS）无可追溯启动命令/日志：
  - 远端 `/tmp` 没有脚本日志 `/tmp/kytrix-nested-sway.log`（脚本 line 29 固定写该文件）；
  - 16:29 前后没有新 `sway` 进程；唯一 nested sway 是 **PID 943500，15:33 启动**，socket `wayland-3`，日志 `/tmp/sway-nested.log`（手工启动，非脚本路径）；证据运行实际使用的是这个既存 nested 会话（或以 `WAYLAND_DISPLAY=wayland-3` 预置方式运行了脚本，从而跳过了 nested 分支）。
  - `/tmp/p3-real-ubu.log`（16:18）是**失败**运行（`MenuOpening` stage-timeout，exit 3），最终成功运行的 stdout 未落盘。
- 文档（设计 §14.4、p3-summary §5、kit-gallery §14）把命令记为 `tools/real-pixel-check-linux.sh`（并描述“以父 socket 启 nested sway”），但无法证明记录证据的运行走过该分支；Linux “≈7.8 s”也无日志可核。
- 脚本本身：成功后不 kill nested sway、无 `trap`；Ubuntu 上现存 3 个长期 `sway`（11-16 两个、11-27 15:33 一个）。重复运行可能把上一个 nested sway 的 socket 当作 parent（脚本按 glob 取第一个非锁 socket），幂等性/清理不达标。
- 建议：文档补“实际启动命令/env（WAYLAND_DISPLAY / grim / sway 版本）”；在干净环境实跑一次脚本并保留日志；脚本加 `trap 'kill $SWAY_PID' EXIT` 与退出清理；或在 §14 登记“Linux 证据以既存 nested sway 运行，脚本仅提供辅助”并补脚本自测。

### P3A-03 [P3] §7.4 的“帧号 / `damage_full` / acceptance 归档”未落地

- 设计 §7.4（line 866-867）要求“记录 OS/DPI/窗口/截图/帧号/`damage_full`/耗时到 `popup-overlay-redesign/acceptance/`”。
- 实际 `report.txt` 只记录 platform / capture_command / screen / window_logical / dpi_scale / checks / failures / result / evidence_dir / 每阶段耗时（无帧号、无 `damage_full`）；证据在 gitignored 的 `target/real-pixels/*`，没有 `.docs/popup-overlay-redesign/acceptance/` 归档副本。
- 这是可解释的（`FrameStats` 只在 `test-support` feature 下存在，gallery 真实窗口进程拿不到 `damage_full`；headless `--pixel-check` 已断言 `damage_full_frames=0`），但 §14 目前没有登记该口径差异，反而仍写“记录…帧号/`damage_full`”，形成文档-实现不一致。
- 建议：在 §14.6 增加 D-P3-x（真实窗口报告不含帧号/damage_full，damage_full 由 headless 门禁承担；截图/耗时/OS/DPI 字段已记录），并把三端 `report.txt`（很小，约 7 KB/端）复制到 `.docs/popup-overlay-redesign/acceptance/` 作为归档，或明确 target 路径即归档位置。

### P3A-04 [P3] Dialog 假绿风险：面板缺失/`scrim` 配色错误可通过当前断言；负向自检强度有限

代码路径（`crates/ui_kit/examples/gallery.rs`）：

1. `dialog-panel-visible`（~5833）用“当前图 vs baseline”在 panel rect 上要求 ≥200‰ 变化且 `max_delta ≥ 40`。dialog 打开时 scrim 覆盖整个 viewport，**即使 panel 没有绘制，scrim 也足以让该区域 1000‰ 变化**——断言无法区分“只有 scrim”与“panel 正常绘制”。
2. `dialog-scrim-visible`（~5841）只要求全窗 ≥500‰ 变化 + `max_delta ≥ 40`，**不校验 scrim 颜色/透明度**；scrim 配色回归（例如误用浅色/错误 alpha）仍可能 PASS。
3. `dialog-marker-under-scrim`（~5758）只在 tone==Scrim 时记录；Full tone 可检测时该检查被跳过（三端本次均走 Scrim tone；断言数 50 依赖于此，见 P3A-05）。
4. `self-check-identical`（~5398）是 `content` 与自身比较，恒真，不是检测器验证；`self-check-offset/recolor` 只证明内存图像比较函数能判红，**没有把故障注入截图/标记检测/popup 映射管线**。
5. `trigger_exclude`（~5466）在整窗 dialog 残影比较中排除物理 70×70（`REAL_PIXEL_TRIGGER_MARGIN=10`，含触发器聚焦环）；该区域内的残影不会被该断言发现（本次独立整图 diff=0，实际无残影，但门禁本身留了口子）。
6. `report` 写失败仅 WARN（不失败），理论存在“PASS 但无 report”的 exit 0 路径。

- 影响：本次三端证据我已独立验证 scrim 为黑 × dark palette 0.55（见 §4），panel 也确实可见；但上述缺口意味着未来“dialog 内容不绘制 / scrim 配色错误”这类回归不会被真实窗口门禁拦住。
- 建议（可部分登记 P4）：用“scrim-only 参考图”（baseline 线性乘 1-alpha）或已知 panel surface 角色色对 panel 区域做差分，以区分 panel 与 scrim；对 scrim 区域断言颜色/alpha（如暗色主题 alpha≈0.55）；在捕获图上注入一个已知色块跑 `real_pixel_compare`（而不是只在内存 baseline 上）；断言画面稳定时 `paint_scale==1`；report 写失败改为失败退出或至少计入 failures。

### P3A-05 [P3] P3 文档与实测数字不一致（刚回写的 §14 / p3-summary / kit-gallery §14）

审查时点（16:50–16:54）文档仍在写入，以下是当前版本的硬错误/风险，提交前需修：

1. **“gallery 约 1200 行”错误**：`git diff --numstat f24a3441 -- crates/ui_kit/examples/gallery.rs` = **+2037/−4**（仅 `--real-pixel-check` 段就约 2000 行）；设计 §14.1 与 `p3-summary.md:14` 都写“约 1200 行”。
2. **“三端均以相同 50 条命名断言运行”不严谨**：`dialog-marker-under-scrim` 仅在 Scrim tone 检测成功时记录；若 Full tone 可检测，三端均为 49 条。建议改为“本次三端各 50 条；断言语义按阶段固定，个别标记检测分支可能少一条”。
3. **Linux ≈7.8 s 无日志可核**（见 P3A-02）；Windows ≈15.9 s 与 `E:\kytrix\target\p3-real-win-final.log` 的 `elapsed=15923 ms` 一致；macOS ≈28.8 s 与脚本日志 `summary ... elapsed=28780 ms` 一致。
4. **性能数字与本次复跑有差异但仍满足目标**：文档写 `idle p95≈0.9–1 µs / open≈1.0 ms / close≈63–65 µs`；我复跑 `popup_open_and_close_frames_stay_within_the_frame_budget` 得 `idle_p95=900ns / open_p95=1.2472ms / close_p95=76.3µs`（open 1.2472 ms 仍 ≤ idle p95+2 ms；close 76.3 µs 同样满足）。建议记录为区间/附日志，或注明单次采样。
5. **P2 文档的旧 menu 度量未在 §14 更正**：设计 §13.6 / kit-gallery §13.6 记 `menu=Some((6, 180.0, 127.5125))`；当前 HEAD 三端 `--debug-stack` 一致输出 `(9, 270.0, 180.075)`（Windows 本地、macOS `/private/tmp/p3-dbg3.log`、Linux `/tmp/p3-ubu-dbg.log`）；P3 未改 gallery 菜单项，说明 P2 记录值本身已不可复现，应在 §14 更正/标注 superseded。
6. **P3-4 追溯不完整**：§14.2 P3-4 只写 `--real-pixel-check`；P3-4 要求的 ⑫⑬⑭ 新 API / 3 级菜单 / `--dump-overlay`→`--debug-stack`/`--probe` 在前序已完成、P3 无代码变更。建议在 §14.2 或 p3-summary 中明确“P3-4 前序已满足，本轮只补 `--real-pixel-check`，`--pixel-check`/`--debug-stack` 不回归（本审已复跑）”。

### P3A-06 [P3] 新脚本的提交形态/打包问题

- `tools/` 为 untracked；仓库当前没有已跟踪的 `.sh`，`core.filemode=false`。从 Windows `git add` 后脚本将是 **100644**，文档里的 `tools/real-pixel-check-macos.sh` / `tools/real-pixel-check-linux.sh` 直接执行在 Unix clone 上会 `Permission denied`（远端当前是 755，是同步 tar 保留的，不是 git 语义）。
- 建议：`git update-index --add --chmod=+x tools/*.sh`（计入提交）或文档统一写 `bash tools/...`；顺带补 Linux 脚本 cleanup（P3A-02）。Windows 没有独立脚本（命令在文档中），可接受但建议在 §14.4 注明“Windows 无需脚本”。

## 3. Harness 假绿风险评估（重点）

| 攻击/失败向量 | 结果 | 依据 |
|---|---|---|
| 窗口未找到/被遮挡/最小化 | **红**（marker 检测失败 exit 3） | Windows 16:01 实测 `FAIL marker ... expected=1200x900`；macOS 16:07/16:09 实测 TCC 下拍壁纸同样 `FAIL marker` |
| 截图全黑/桌面壁纸 | **红** | 同上（macOS TCC 未授权时） |
| popup 打开但未绘制（picker/menu） | **红**（visibility 区域无变化） | `real_pixel_visibility_check` 对 baseline 比较，要求 ≥50‰ 变化；scrim 之外的场景无其他变化源 |
| popup 打开但位置错（>5 px） | 通常红（所报 rect 区域未变化）；≤5 px 小偏移可能漏 | 门禁定位语义是残影/可见性，不校验位置精度；位置由 `--debug-stack` 的 rect=stable 断言覆盖 |
| Dialog 面板缺失但 scrim 在 | **可能假绿** | P3A-04-1：`dialog-panel-visible` 对 baseline 比较，scrim 单独即可满足 200‰/delta 40 |
| Dialog scrim 配色错误 | **可能假绿** | P3A-04-2：只断言覆盖/变化，不校验颜色/alpha；本次独立推算三端 alpha≈0.55 与 dark palette 一致（§4），但门禁未内置 |
| 截图缓存/旧图 | **红**（每次截图前删临时文件、命令失败即失败；无复用） | `screenshot()` 删除临时文件 → 外部命令 → 解码；`real_pixel_run_process` 检查子进程退出码 |
| 关闭残影 | **红**（残影区 `differing==0` + 990‰ coverage；本次三端整图复算也 0 差异） | `real_pixel_residue_check` / `real_pixel_content_residue_check`；本轮独立 Pillow 复算 |
| DPI/尺寸错（dpr 映射错） | **红**（content-physical-size、marker-thickness、marker-on-screen） | Windows 1.25→1200×900/thickness 8；macOS 2.0→1914×1437/thickness 10（容差 8）；Linux 1.0→1276×670/thickness 6 |
| sway tiling/resize | 可运行（实际尺寸 1276×670 记录；`sync_layout` 更新 marker/backdrop） | Linux report；文档已登记“compositor 尺寸非 960×720” |
| 对话框 scrim 遮标记 | 有回退（baseline origin），关闭后重新检测并全图残影比较；回退时少一条 `dialog-marker-under-scrim` 断言 | 代码 ~5240-5293/5758；本次三端均 Scrim tone 检测成功 |
| 超时/挂死 | 红（阶段 20 s、看门狗 180 s、截图子进程 45 s、exit 3） | 代码常量；Linux 16:18 实测 stage-timeout exit 3 |
| 报告写失败 | 仅 WARN，可能 PASS 无证据 | `real_pixel_write_report` 只打印 WARN（P3A-04-6） |
| 并发运行 | 未防护（同一 evidence 目录会被 `remove_dir_all`，两窗口可互相遮挡） | `build()` 开头删目录；无锁 |

结论：**主要假绿面在 Dialog（panel/scrim）与证据落盘**；其他残影/定位/DPI 路径在本轮三端真实证据与历史失败记录下可信度高。建议把 P3A-04 的 1/2/6 作为 P4 强化项登记，至少让“dialog 面板由 scrim 单独通过”这一点不再无记录。

## 4. 三端证据独立核对（不依赖 gallery 自身 PASS 行）

原始目录：`E:\kytrix\target\real-pixels\windows`、`macos-remote`、`linux-remote`；PNG 均为内容区裁剪（marker 环内）。用 Pillow 逐像素复算：

| 平台 | report | PASS 行 = checks | baseline vs closed（02/04/06）整图 | open 变化（01/03/05，>2） | scrim 独立推算 | marker 恢复 |
|---|---|---|---|---|---|---|
| Windows 11 | 50 / 0 failures / PASS，screen 4800×3072，dpr 1.25，内容 1200×900 | 50 | **differing=0, max_delta=0** ×3 | 178351 / 73458 / 1079976 px changed | 暗色 scrim alpha≈0.55（按线性混合反解：42→26、50→32、62→40） | `tone=Full origin=(1878,858) size=(1200,900)` 与 baseline 相同 |
| macOS 15.8.3 | 50 / 0 / PASS，screen 3360×2100，dpr 2.0，内容 1914×1437（期望 1920×1440，容差 8） | 50 | **differing=0, max_delta=0** ×3 | 457146 / 188288 / 2749008 px | 同左，alpha≈0.566（量化误差，目标 0.55） | `origin=(243,182) size=(1914,1437)` 与 baseline 相同 |
| Ubuntu Wayland（nested sway） | 50 / 0 / PASS，screen 1280×720，dpr 1.0，内容 1276×670 | 50 | **differing=0, max_delta=0** ×3 | 114388 / 47382 / 854914 px | 同左，alpha≈0.566 | `origin=(2,48) size=(1276,670)` 与 baseline 相同 |

- **checks=50 构成**（代码核对）：4 warmup + 6 baseline + 1 picker-open-call + 6 picker-opening + 1 picker-close-call + 2 picker-closing（stack-empty/residue）+ 1 menu-open-call + 2 menu-level2/3 + 9 menu-opening（marker/stack/state/z/parent/in-window/3 visibility）+ 1 menu-close-call + 4 menu-closing（stack-empty/3 residue）+ 1 dialog-open-call + 8 dialog-opening（marker-under-scrim/stack/state/scrim-state/in-window/scrim-covers/panel-visible/scrim-visible）+ 1 dialog-close-call + 3 dialog-closing（marker-restored/stack-empty/content-residue）= **50**。三端报告均与之一致。
- **Windows 复跑**：用当前源码构建的二进制，独立 `CARGO_TARGET_DIR=E:\kytrix\target\p3-review-windows` 运行 `--real-pixel-check` → **50 checks / 0 failures / exit 0**；与 lead 16:27 运行的报告（`p3-real-win-final.log`，elapsed 15923 ms）在去掉时间/路径后**逐行一致**；两次运行的 8 张 PNG 仅窗口圆角/边缘 52 个像素不同（baseline 与 closed 各自内部仍 0 差异），属窗口边框合成差异，不影响断言。另观察到共享目录 `target/real-pixels/windows` 在 **16:43–16:44** 还有一次 50 PASS 运行（report `failures=0`，与我方 16:41 运行的 PASS 行仅 `self-check-offset max_delta` 239 vs 242 不同，系 baseline 边框像素差异）。**Windows 口径 3 次运行均 50 PASS，可重复。**
- **源码/证据一致性**：远端 `~/kytrix-p3-accept` 无 `.git`（同步树）；`gallery.rs` SHA256 `d12407c2…`、`popup/animation.rs c79291d1…`、`paint/pass.rs fde13c69…`、`dialog.rs ee679ee0…`、`color_picker.rs 7db25094…` 与本地工作树一致；macOS 全部 9 个证据文件（含 report）与本地 `macos-remote/` 哈希逐一相等；Linux 全部 9 个同样相等。`target/p3-final-src.tar.gz`（16:28:04，12.6 MB）可作为源码包同步证据（无 SHA manifest，建议归档）。
- **运行痕迹**：macOS 脚本确认经 Terminal 跑通（`/var/folders/.../kytrix-real-pixels-macos.log` + `.exit`=0，16:29）；macOS `--debug-stack` 16:30 PASS（`/private/tmp/p3-dbg3.exit`=0）；Linux `--debug-stack` 16:31 PASS（`/tmp/p3-ubu-dbg.log`）；Windows `--debug-stack` 16:27 PASS（`E:\kytrix\target\p3-debugstack-final.log`）。
- **历史失败记录（证明门禁不是恒绿）**：Windows 16:01 marker FAIL exit 3；macOS 16:07/16:09 TCC 壁纸 marker FAIL；Linux 16:18 MenuOpening stage-timeout checks=22 exit 3；macOS 16:17/16:20 `--debug-stack` environment-dismiss FAIL exit 3。这些是**好迹象**。
- **Dialog scrim 配色**：门禁只断言变化；我用 baseline/05-dialog-open 内容区像素反解线性混合系数，三端均 ≈0.55（dark palette `scrim_alpha=0.55`、黑），与代码 `Role::Scrim(black×alpha)` 一致。证据可信，但门禁本身未内置该校验（P3A-04-2）。

## 5. P3-4 gallery / 命令核对

- gallery 的 P3 diff 只有：imports、`Flags.real_pixel_check`、`parse_flags` 分支、real-pixel 段、`main` 分支、usage/banner 文案；**未改 ⑫⑬⑭、3 级链、`--pixel-check`、`--debug-stack`、`--probe`**。⑫⑬⑭ 与 3 级链为 P1/P2 已交付，P3 以“不回归”验收。
- 本机复跑（`CARGO_TARGET_DIR=E:\kytrix\target`）：
  - `gallery --pixel-check` → **PASS，checks=94，sampled_frames=32，open_animation_frames=8，close_animation_frames=6+6，damage_full_frames=0**，`first_visible==stable=(496,91,300×375)`，exit 0。
  - `gallery --debug-stack` → **PASS**（menu=Some((9,270.0,180.075))、menu_frames=466、picker_open=true、picker_frames=363、entries=0），exit 0。
  - `--help` exit 0；`--dump-overlay` exit 2（改名提示）；未知参数 exit 2；`--scroll-to=abc` exit 2；`--auto-close=abc` exit 2；usage 含 `--real-pixel-check`。
  - `--probe` 未复跑：代码零改动且需真实窗口长跑；P2 已有 probe 日志，风险低。
  - Windows 补跑 `--idle-check` / `--theme-check`（P3 HEAD）：`idle-check PASS (3 s, 0 frames rendered while idle)`、`theme-check PASS (0 failure(s) across Dark/Light/Dark)`，exit 0；日志见 `target/review-p3-{idle,theme}.log`。
- 真实窗口 3 级链/Picker/Dialog：`--debug-stack` 覆盖 3 级链与 picker；`--real-pixel-check` 用独立验收场景覆盖 ColorPicker/3-level MenuChain/Dialog+scrim。**注意**：真实像素门禁的验收场景是 gallery 内的专用 `RealPixelAcceptance`，不是 ⑫⑬⑭ 的页面实例；⑫⑬⑭ 的视觉内容仍只由 `--debug-stack`/单测/人工证据覆盖（P3A-04 覆盖范围限制，P4 可扩展）。

## 6. 门禁与规范（AGENTS）核对

| 项 | 结果 |
|---|---|
| `cargo +nightly fmt --all -- --check` | PASS |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS |
| `cargo check --workspace --all-targets` | PASS |
| `cargo test -p ui_core -p ui_kit` | **3865 passed / 0 failed / 2 ignored**（ui_core lib 2805+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 120、snapshot 14、demo 39、ui_kit 875） |
| P3 新增测试 | 引擎 +9、kit +9 = **+18**，与设计 §14.5/p3-summary 一致；逐文件 `#[test]` 计数（menu 79/context_menu 21/dialog 55/color_picker 81 等）抽查一致 |
| P3-5 性能 | 复跑 `popup_open_and_close_frames_stay_within_the_frame_budget`：`idle_p95=900ns open_p95=1.2472ms close_p95=76.3µs`；设计目标“≤ idle p95+2 ms”满足；测试守卫是 +16.7 ms（D-P3-8 已登记） |
| AGENTS：注释/allow/依赖 | 新增行 0 注释、0 `#[allow]`/`#[expect]`、无 `serde_json`/动态 JSON；`image` 为既有依赖；无新 crate。`rg` 旧 API 无新引用 |
| 证据哈希/源码同步 | 见 §4；远端源码关键文件 SHA256 与本地一致，三端证据副本哈希一致 |
| 文档 | 设计 §14、p3-summary、kit-gallery §14 已生成；P3 口径与 API（`FadeScale`/`paint_scale`/`close_with_reason`/`FrameStats`）与实现一致；仍需修正 P3A-05 的数字/范围、并补 P3A-01/02/03 的证据或偏差登记 |

## 7. 文档核对清单（提交前）

- [ ] 设计 §14.1 / p3-summary “约 1200 行” → 实际 gallery +2037/−4；总代码 22 文件（不含 kit-gallery 文档 +23）→ 修正。
- [ ] §14.4 Linux 命令/耗时（7.8 s）与实际运行环境对齐（见 P3A-02）。
- [ ] §14.4/§7.4 “相同 50 条” → 说明条件分支（Scrim tone）。
- [ ] §14.6 增加“真实窗口 report 不含帧号/damage_full，由 headless 门禁承担”的偏差登记，并把三端 report.txt 归档到 `.docs/popup-overlay-redesign/acceptance/`。
- [ ] §13.6 / kit-gallery §13.6 的 P2 menu 度量 `(6,180,127.5)` 标注 superseded（当前 `(9,270,180.075)`）。
- [ ] P3-4 追溯补一句：⑫⑬⑭/3 级链/`--probe` 前序已满足，P3 只补 `--real-pixel-check`，`--pixel-check`/`--debug-stack` 已复跑不回归。
- [ ] 性能数字注明复跑区间/日志；三端 idle/theme 结果补进 §14.5 或登记豁免。
- [ ] 提交 `tools/*.sh` 时处理可执行位，或文档改 `bash tools/...`。

## 8. 残余风险（不阻塞提交，建议登记）

1. 三端中 macOS/Linux 的真实像素证据只有 1 次成功运行（Windows 2 次）；macOS 另有两个 `--debug-stack` environment-dismiss 失败记录（16:17/16:20，focus 被 Terminal/环境抢走），说明 macOS 真实窗口运行对“窗口保持 active/不切焦点”敏感；文档尚未记录该前提。
2. 真实像素门禁只验证 popup 区域“可见变化/关闭恢复”，不验证 popup 内容是否正确（空白面板也会通过 picker/menu 可见性；Dialog 问题见 P3A-04）。
3. 触发器排除区（全窗残影检查）与 Dialog panel/scrim 区分缺口（P3A-04）。
4. 性能数据来自 headless TestApp CPU 计时，不等于真实窗口帧间隔/GPU 帧预算；`frame_cpu` 在动画 tick 之后开始计时，popup 动画 tick 不计入 open 帧 CPU。
5. 无 CI 接入：三端真实窗口验收依赖人工桌面/TCC/nested sway；证据在 gitignored target，长期不可复现。
6. 并发 `--real-pixel-check` 无锁，两个窗口会互相遮挡并清空同一 evidence 目录。

## 9. 复跑命令（供 lead 复现本审查）

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'
cargo +nightly fmt --all -- --check
cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings
cargo check --workspace --all-targets
cargo test -p ui_core -p ui_kit
cargo run -q -p ui_kit --example gallery -- --pixel-check
cargo run -q -p ui_kit --example gallery -- --debug-stack
E:\kytrix\target\debug\examples\gallery.exe --help          # 0
E:\kytrix\target\debug\examples\gallery.exe --dump-overlay  # 2
# 隔离再跑真实窗口验收，不覆盖 lead 证据：
$env:CARGO_TARGET_DIR='E:\kytrix\target\p3-review-windows'
E:\kytrix\target\debug\examples\gallery.exe --real-pixel-check
# 性能单测输出：
$env:CARGO_TARGET_DIR='E:\kytrix\target'
cargo test -p ui_core --features test-support --test popup_behavior popup_open_and_close_frames_stay_within_the_frame_budget -- --exact --nocapture
```

## 10. 提交结论

- **代码质量/门禁/真实窗口像素证据：通过**（0 P0/0 P1；三端 50 checks 与 PNG 独立复算全部成立；Windows 可重复；源码/证据哈希可追溯）。
- **提交前必须处理/豁免**：P3A-01（macOS/Linux 的 idle/theme 证据或豁免；Windows 已由本审补跑 PASS）、P3A-02（Linux 命令/环境可复现性与脚本清理）、P3A-05（文档数字/范围修正）。
- **建议登记/后续**：P3A-03（report 字段/归档偏差）、P3A-04（Dialog 假绿与负向自检强化）、P3A-06（脚本 exec bit/打包）。
- 在上述 P2 项处置并确认文档最终版本后，**P3 可以提交**；若 lead 选择以显式偏差登记替代补跑/修文档，也可提交，但需在 §14 写清豁免依据。
