# P3 验收审查 r2（P3A 修复后收口复核）

> 审查人：p3-review-acceptance（独立子 agent，只读）
> 时间：2026-11-27 17:44–18:06（Asia/Shanghai）；对象 = P3A-01/02/03/04/06 修复后的未提交工作树（基线 `f24a3441`）
> 方法：重读 gallery/引擎/脚本 diff；独立复跑门禁；用 Pillow 复算三端 PNG（不信任 gallery 自报）；SSH 只读核对远端源码哈希、最终证据哈希、gate 日志与清理；核对设计 §14/`p3-summary.md`/`kit-gallery.md` §14。
> 计数：**0 P0、0 P1、1 P2、2 P3**（另有已登记的 P4 残余）。**结论：代码/门禁/三端证据实质通过；P2 为“证据快照 vs 最终树”的文档口径，修文档（或按其建议补一句说明）后 P3 可提交；无需改代码。**

## 1. P3A 关闭情况（逐项）

| 原 finding | 状态 | 证据/说明 |
|---|---|---|
| **P3A-01** macOS/Ubuntu 无 P3 HEAD idle/theme 证据 | **CLOSED** | macOS、Ubuntu 在 17:55 用最终脚本分别完整跑完 `pixel-check(94)/real-pixel(55)/idle/theme/debug-stack`，`run.log` 记录 `gates_done fail=0`，`gates_exit=0`；Windows 17:48–17:54 脚本跑完同样 5 门（debug-stack attempt 1 超时、attempt 2 exit=0、`gates_done fail=0`）。三端 idle/theme 日志均 PASS；本机副本已按远端哈希逐字节核对。 |
| **P3A-02** Linux 命令/脚本可复现性与清理 | **CLOSED（脚本行为）** | 新 Linux 脚本 `run.log` 记录 host/parent socket、fresh nested sway（17:55 run：`parent=wayland-0`、`nested_socket=wayland-1`、`sway_pid=992925`），结束 `cleanup: nested sway pid 992925 stopped`；复查 Ubuntu 已无 sway 进程。macOS 脚本记录 TCC probe、Terminal 内执行、关闭新建 Terminal 窗口（`cleanup: closed ...`），`gates_exit=0`。脚本加 `trap`、逐 gate exit 记录与 300 s 超时。唯一残余是证据源码快照问题（见 R2-01）与远端 `source_* unknown`（见 R2-02/P4）。 |
| **P3A-03** report 字段/归档 | **大部分 CLOSED** | 三端 `report.txt` 现含 `source_revision/source_status/source_diff_sha/build_profile/scrim_color_linear/scrim_alpha/last_frame/elapsed_ms/damage_full=unavailable(...)/residue_rule/evidence_layout`；report 写失败现在计入 failures 并 exit 3（`real_pixel_write_report` 返回 Result；`final_report` 检查 `report_write_failed`）。Windows report 有 `source_revision=f24a3441 / dirty(29 files) / diff_sha=1f17e4f0...`。残余：mac/Linux 远端无 `.git`，report 的 `source_revision/status/diff_sha` 为 `unknown`；`.docs/popup-overlay-redesign/acceptance/` 目录仍未创建（三端 report/run.log/PNG 在 `target/real-pixels/*`）。两项均已在设计 §14.7 登记为 P4（但措辞仍写“报告写失败仅 WARN”，与实现不符，见 R2-02）。 |
| **P3A-04** Dialog 假绿 | **CLOSED（目标场景）** | 新增 `dialog-scrim-theme/composite/alpha`、`dialog-panel-above-scrim`、`self-check-scrim-fault`、`self-check-panel-fault`；三端实测：Windows `predicted-from-baseline compared=723744 outliers>4=0‰ outliers>12=0 max_delta=1`、`derived alpha=0.5492`、`panel changed_vs_scrim=75988 (1000‰)`；macOS `compared=1846890 ... derived alpha=0.5507`；Linux `compared=603048 ... derived alpha=0.5492`。独立复算三端 scrim alpha≈0.55（dark palette），关闭图与 baseline 整图 `differing=0`。剩余“popup 内容正确性不校验/触发器排除区”已在 §14.7 登记 P4；`report` 写失败已修。 |
| **P3A-05** P3 文档数字/范围 | **基本 CLOSED** | `p3-summary` 已改 `gallery +2439/−4`（实测 `git numstat` 2439/4）；设计/总结三端均改 **55 checks PASS**（Windows 15.5–15.9 s、macOS 31.2 s、Ubuntu 8.1 s），测试总数 **3872 passed / 2 ignored**（本机复跑一致）；Linux run.log 已归档、耗时来自 `last_frame/elapsed_ms=8115`。剩余是 R2-02 列出的 3 行 P3A 残留状态文字与 kit-gallery §14 末尾 P4 行。 |
| **P3A-06** 脚本提交形态/exec 位 | **CLOSED** | `tools/` 4 个文件均已 staged：`macos-gates-runner.sh`、`real-pixel-check-{linux,macos}.sh` 为 **100755**；`real-pixel-check-windows.ps1` 100644（Windows 脚本无需 exec 位）。Linux cleanup/trap 已加；macOS runner/TCC probe/窗口清理已加。设计 §14.7 的 P3A-06 行仍写“exec bit/提交纳入”（stale，见 R2-02）。 |

## 2. r2 Findings

### R2-01 [P2] 三端证据快照与“P3 最终树”不完全一致（文档口径）

- 最终代码在 **17:56:07** 修改了 `crates/ui_core/src/paint/pass.rs`（P3K-06：`Primitive::Text` 改为恒等直通，不再缩放 CanvasText）；此前的 P3 证据运行：
  - macOS/Ubuntu 的 55-check 证据于 **17:55** 生成；远端 `pass.rs` 仍为修改前版本 `sha256=954c4ac3...`（本地最终为 `cc7605e4...`），差异仅 `PopupScale::primitive` 的 `Primitive::Text` 分支（远端会缩放 CanvasText origin，本地恒等直通）。远端 `gallery.rs` 已同步为最终 `8a7017...`，macOS wrapper 也已同步最终 `7f9045...`；因此**唯一源码差异是 pass.rs 的 CanvasText 分支**。
  - Windows 目录中 `report.txt`/PNG 为 **17:59**（最终树，`dirty(29)`、diff_sha 1f17e4f0…），而 `pixel-check/idle-check/theme-check/debug-stack` 日志为 **17:48–17:54**（前一次脚本运行，report 当时为 dirty(26)）；证据目录是两次运行混合，不能视作同一次“最终树五门”快照。
- 影响：真实像素门禁的验收场景（ColorPicker/menu/dialog）**不含 CanvasText**，因此 CanvasText 分支差异对本次截图/检查结果无实际影响；但文档（设计 §14.4/§14.5、`kit-gallery` §14）写“P3 最终树全部复跑”与证据元数据不符，审计严格性不成立。Windows 混合目录也让“同一 run 的 5 门”不可直接证明。
- 建议（二选一，均不需改产品代码）：
  1. **补一句口径**：在 §14.4 记录“macOS/Ubuntu 55 checks 证据来自 P3K-06 pass.rs CanvasText 恒等直通之前的源（唯一差异、验收场景不含 CanvasText）；Windows report/PNG 为最终树，gate 日志为同目录前一次脚本 run（dirty 26→29）”，并把 source 哈希清单列入证据说明；或
  2. 在最终树上重跑三端脚本（Windows 脚本 + 远端 rsync 后 mac/linux），用单次 `run.log` + 单 report 的快照替换现有混合证据。
- 严重度：**P2（仅文档/证据口径，不是功能缺陷）**；若不接受“57 行源码差异不影响”的论证，则按第 2 条重跑。

### R2-02 [P3] 设计/kit-gallery 的 P3A 残留状态文字仍写旧行为

- 设计 §14.7（18:04 版本）第 1660–1662 行、未提交的 `kit-gallery.md` §14 line 772 仍写：
  - P3A-03 “报告写失败仍仅 WARN”（实际 `real_pixel_write_report` 失败会计入 failures 并 exit 3）；
  - P3A-04 “报告写失败仍仅 WARN”（同上）；
  - P3A-06 “exec bit/提交纳入…需归档”（实际 4 个脚本已 staged，3 个 `.sh` 为 100755）；
  - kit-gallery 的 Linux “run.log 未归档”在 18:03 版已改为 55 checks，但 §14 末尾 P4 行仍保留旧句。
- 影响：`kit-gallery.md` 是 **tracked 文件**（本轮会提交），会把错误状态写进提交；设计文档虽 gitignored，但为交付文档。任务要求“文档与实测一致”。
- 建议：把上述行改为“P3A-03 仅剩 acceptance 归档/远端 source unknown（P4）；P3A-04 仅剩 popup 内容正确性/触发器排除；报告写失败已 fail-fast；P3A-06 已 staged/755”。这是 3 处文字修正，无代码改动。

### R2-03 [P3] Windows wrapper 的 `--debug-stack` 在并发桌面下 attempt 1 超时（300 s），靠重试通过

- 证据：`E:\kytrix\target\real-pixels\windows\run.log`（17:48–17:54）记录 `debug-stack attempt 1 exit=124`（300 s timeout；其 `debug-stack-1.log` 已在 17:49:13 打印 `overlay-check ... PASS` 与 `complete -> closing window`，但进程未退出，被脚本 kill），attempt 2 exit=0、`gates_done fail=0`。当时 lead 自身有另一条 gallery 命令并发（PID 67928）在跑；非独占桌面。
- 影响：脚本有 3 次重试，最终 PASS；但一次 run 会因此耗时 ~5 min，且 `run.log` 会留下 124，容易被误读为失败。功能/证据不受影响（attempt 2 干净 PASS）。设计 §14.4 已写“需独占桌面/前台窗口，并发会假红”；可再注明“`--debug-stack` 偶发不退出时脚本会 3 次重试，单次 timeout 不改变 gates 结果”。
- 严重度：**P3（环境/运行性残余，非阻塞）**；建议验收时不要与其它 gallery/截图命令并行。

## 3. 三端最终证据核对（本审独立复算）

| 平台 | report | 独立整图复算（02/04/06 vs baseline） | 打开图变化 | scrim 独立反解 | 证据/源码 |
|---|---|---|---|---|---|
| Windows（17:59 report/PNG） | **55 / 0 / PASS**，dpr1.25，1200×900，frame 97，elapsed 15508 ms，`source=f24a3441 dirty(29)`、diff_sha 1f17e4f0… | 3 张全部 `differing(>2)=0, max_delta=0` | 01/03/05 changed(>8)=178312 / 72969 / 1079926 | 采样点 alpha≈0.538（量化）≈dark 0.55 | 独立再跑本机 `--real-pixel-check`（隔离 target）：**55/0/PASS**，`elapsed 15637 ms`、frame 95、同 revision/diff_sha；证据与 lead 报告一致 |
| macOS（17:55） | **55 / 0 / PASS**，dpr2.0，1914×1437，frame 97，elapsed 31211 ms | 3 张全部 `differing(>2)=0` | 455718 / 186448 / 2748632 | report derived alpha=0.5507；独立采样暗化一致 | 远端 run.log 5 门 exit 0，cleanup 关闭 2 个 Terminal 窗口；本地 `macos-remote/` 的 report/run.log/pixel/idle/theme/debug 日志与远端逐字节哈希一致 |
| Ubuntu（17:55，fresh nested sway） | **55 / 0 / PASS**，dpr1.0，1276×670，frame 91，elapsed 8115 ms | 3 张全部 `differing(>2)=0` | 114055 / 46612 / 854911 | report derived alpha=0.5492；独立采样 alpha≈0.575（量化） | 远端 run.log：fresh `wayland-1`、pid 992925、`cleanup: nested sway pid 992925 stopped`；`gates_done fail=0`；本地 `linux-remote/` 与远端全部哈希一致 |

- **本机门禁（在最终源码上复跑）**：`cargo +nightly fmt --all -- --check` PASS；clippy PASS；`cargo check --workspace --all-targets` PASS；`cargo test -p ui_core -p ui_kit` **3872 passed / 0 failed / 2 ignored**（ui_core lib 2806+1、frame_record 8、gui_smoke 0+1、layout 4、popup_behavior 123、snapshot 14、demo 39、ui_kit 878）；`gallery --pixel-check` **94 checks PASS, damage_full_frames=0**；`--debug-stack` PASS（3 级链/picker/entries=0）。
- **harness 假绿复评**（相对 r1）：
  - 已消除：scrim 缺失/错误 alpha/错误颜色由 `dialog-scrim-composite/alpha` 判红；panel 未绘制由 `dialog-panel-above-scrim` 判红；`report.txt` 写失败现在 exit 3；证据目录保留 `run.log`/gate 日志（`remove_file` 仅清旧 PNG/report）。
  - 仍存在（已登记 P4）：不验证 popup **内容**（空白面板 surface 仍可能通过视觉检查）；`trigger_exclude` 与 scrim composite 的 panel/edge/macOS 顶栏排除区可掩盖局部错误；负向自检仍是内存图注入，非截图管线注入。
- **源码/证据一致性**：远端 gallery/frame/popup_behavior/dialog/dropdown/select 等与本地最终哈希一致；唯一不一致是上述 `pass.rs` CanvasText 分支（R2-01）。Windows 独立 run 与 lead report 的 revision/diff_sha 相同；mac/linux report 的 `source_*` 为 unknown（R2-02/P4）。

## 4. 文档核对

- 设计 §14：三端结果已统一为 **55×3**，测试 3872/2 ignored、pixel-check 94、debug-stack PASS、性能 median 守卫 +2 ms（P3E-03 已登记）、P3A 修复说明与脚本/GUI 前置均写入；`§10 DoD 6` 已改为三端 55 PASS。
- `p3-summary.md`：+2439/−4、3872、三端 55、P3K-06/`Primitive::Text` 恒等直通、run.log/TCC/独占桌面均已写入。
- `kit-gallery.md` §14：已更新为 55×3、各端 elapsed/frame、fresh sway/trap、独占桌面说明；**line 772 的 4 项 P4 文案仍含旧“报告写失败 WARN / exec bit”句**（R2-02）。
- 设计 §14.7 的 P3A 行同样残留旧句（R2-02）；`acceptance/` 归档目录仍不存在（P4，需在 §14 明确“target 即归档”或补目录）。
- 结论：文档主体已与实测一致；只剩 R2-02 的 3 行状态文字 + R2-01 的证据快照说明。

## 5. 残余风险（不阻塞）

1. 验收场景是专用 `RealPixelAcceptance` 窗口，不是 gallery ⑫⑬⑭ 页面实例；P3-4 的真实像素扩展到更多组件/主题为 P4。
2. `--pixel-check`/`--real-pixel-check` 不采样动画帧（P3K-08），scale 动画仅由单测 + 最终稳定帧门禁覆盖。
3. 真实窗口 `damage_full` 不可得（report 明确 `unavailable`，由 headless gate 证明 0）。
4. 远端 report 的 `source_revision/status/diff_sha=unknown`（rsync 无 .git）；建议以后在 sync 脚本写 `source.txt` 或让脚本注入 `KYTRIX_SOURCE_*`。
5. Windows `--debug-stack` 在非独占桌面下可能 300 s 不退出（R2-03）；验收时串行运行。

## 6. 提交结论

- **代码与门禁：通过**；**三端真实窗口证据：均 55 checks PASS，独立 PNG 复算无残影、scrim/panel 检查有实值，证据副本与远端哈希一致**。P3A-01/02/04/06 实质关闭；P3A-03 仅剩已登记 P4 的归档/远端 source unknown。
- **提交前建议（不改代码）**：
  1. 按 R2-01 在 §14.4 补“mac/Linux 55-check 证据的运行源码快照（P3K-06 pass.rs 差异；验收场景无 CanvasText）”或重跑三端脚本；
  2. 按 R2-02 修正设计 §14.7 三行与 `kit-gallery.md` §14 line 772 的 P3A-03/04/06 旧状态；
  3. （可选）在 §14.4 注明 Windows `debug-stack` attempt1 timeout/retry 与独占桌面前提。
- 完成上述文档 2 处修正后：**P3 可提交**。若坚持“零 finding”字面标准，则当前尚有 R2-01（P2）未关闭，不宜提交；但不存在需要改代码或重做证据的功能性阻塞。
