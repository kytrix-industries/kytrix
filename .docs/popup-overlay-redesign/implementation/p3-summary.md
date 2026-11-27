# P3 实施摘要（Dialog 收口 / FadeScale / 真实像素三端验收 / 性能基线）

> 范围：设计 §6 P3-1…P3-7；基线 P2 提交 `f24a3441`；P3 最终 HEAD `35c9381d`（clean；提交链 d9949aca→152616b1→f24a3441→fa41987c→5df859b8→5374c4c8→00927ff9→35c9381d），另有文档提交由 lead `git add -f` 纳入，设计 §14 与本摘要随最终提交 `git add -f` 入库。
> 结论：**P3-1…P3-7 全部落地**；引擎新增 `PopupAnimation::FadeScale` 与 paint-lane transform，Dialog/ColorPicker 用 FadeScale、Menu/ContextMenu/Dropdown/Select 用 FadeSlide；`--real-pixel-check` 在 HEAD `35c9381d` **三端各 106 checks / 0 failures PASS**（Windows 45.9 s/frame 280、macOS 72.2 s/frame 283、Ubuntu 20.3 s/frame 311）；测试/门禁 **3882 passed**（workspace 9337 passed/1 env flake/30 ignored）。

## 1. 规模与提交内容

| 项 | 结果 |
|---|---|
| P3 提交 diff（`f24a3441..35c9381d`） | **45 files changed，+7839/−558**（`crates/` 38 文件 +7432/−556；`tools/` 5 个脚本 +377；`kit-gallery.md`/.gitignore）；最终 HEAD `35c9381d` clean |
| 新增脚本 | `tools/real-pixel-check-{windows.ps1,macos.sh,linux.sh}`、`tools/macos-gates-runner.sh`、`tools/record-source-id.ps1`（5 个，随 `fa41987c`/`5df859b8` 入库） |
| 引擎 | `popup.rs`（`FadeScale`/`POPUP_SCALE_FROM=0.98`/`scale_from,scale_to`/`paint_center`/`paint_scale` 快照）、`popup/animation.rs`、`popup/dismiss.rs`、`popup/settle.rs`、`paint/pass.rs`（`PopupScale`）、`node/tree*.rs`、`app/{frame,popup}.rs`、`test_support/record.rs` |
| kit | `menu.rs`（FadeSlide、`close_with_reason`、动画期 anchor watch）、`context_menu.rs`/`dialog.rs`/`dropdown.rs`/`select.rs`/`color_picker.rs`（AnchorLost/显式 reason；Dialog 删 `recenter`）、`popup_panel.rs`（`close_with_reason`） |
| gallery | `--real-pixel-check` 真实窗口截图验收（`gallery.rs` **+3963/−7**；P3A-03/04 scrim 检查/报告字段 + 交互验收/self-check） |
| 任务 | P3-1…P3-7 completed；本轮为文档回写 |

## 2. P3-1…P3-7 结果

| # | 结果 | 关键证据 |
|---|---|---|
| P3-1 Dialog/Alert 收口 | **done** | 删除 `Dialog::recenter`（resize 不再居中，`resizing_the_window_keeps_the_anchor_opened_with`）；`close_with_reason` 统一；FocusPolicy::Trap 初焦/Tab/outside/escape/restore 沿用 P1（D16）；Alert 是 Dialog 内容面板（D9/D11） |
| P3-2 `FadeScale`（D10） | **done** | 引擎 `FadeScale` + lane transform：非模态 0.98→1、120 ms、EaseOutCubic、paint-center pivot；关闭 1→0.98、90 ms；modal fade-only；settle 同步 pivot 不 mark_paint；damage 用 layout rect 平移超集；dpr1/2 几何一致 |
| P3-3 ColorPicker 复核 | **done** | 非模态 `FadeScale`（首帧 0.98→1、settle rect 不变、局部 damage、关闭帧覆盖旧 rect）；锚点丢失上报 `AnchorLost`；内容闭包/局部坐标/tab measure/P1 复核项保持 |
| P3-4 gallery/真实像素 | **done** | `--real-pixel-check`（960×720 + 6 px `#FF00FF` 标记环 + 全屏截图 + 三阶段序列 + 负向自检 + exit 0/3 + `report.txt`/PNG）；`--pixel-check` 仍 94 checks（damage_full_frames=0）；`--debug-stack` PASS |
| P3-5 性能基线 | **done** | `FrameStats.frame_cpu/popup_opening/popup_closing`；**idle p95≈0.6–1 µs；open 首帧冷启动 p95≈1.0–1.13 ms（含 atlas/text 冷启动，10 帧样本 p95≈max），其余帧中位≈29 µs；close 中位≈26 µs、p95≈64–138 µs（尾部）**；damage_full=false；设计目标以中位帧守卫（idle p95+2 ms）满足；空闲帧 0 popup 工作（P3E-03） |
| P3-6 三端真实窗口验收 | **done** | **三端 HEAD `35c9381d` clean 各 106 checks / 0 failures PASS**：Windows 45.9 s/frame 280（隔离 43.3/277）、macOS 72.2 s/frame 283、Ubuntu 20.3 s/frame 311；identity 三端 `diff_sha=e69de29…`/source-id-file；证据见 §14.4 |
| P3-7 文档回写 | **done** | 设计 §14、本文件、`kit-gallery.md` §14；§5.2/§7.4/§10 已同步 |

## 3. 引擎实现口径

- **动画**：`PopupAnimation { None, Fade, FadeSlide, FadeScale }`；`POPUP_OPEN_DURATION=120 ms`、`POPUP_CLOSE_DURATION=90 ms`、`POPUP_SLIDE_DISTANCE=4 px`、`POPUP_SCALE_FROM=0.98`；`PopupAnimationState.scale_from/scale_to`；进度 `EaseOutCubic`。
- **`PopupTransform`（`pub(crate)`）**：`offset/opacity/scale/pivot/blocked`；`paint_scale()` clamp `[0,1]`（非法值 1.0）；pivot=`PopupEntry::paint_center()`（paint rect 中心，空则 hit rect 中心）。
- **paint lane transform**：`PopupScale { factor, pivot }` 对 Quad/Shadow/Image/GlyphRun/Path/Geometry/Clip/Canvas 递归缩放（Shadow.offset 按向量缩放，P3E-01 修复；**P3K-06 closed：`Primitive::Text` 恒等直通，CanvasText 在 Canvas push 前解析为 glyph run，不再有 Text 只缩放 origin 的缺口**）；`paint_scale` 侧表写入，`PopupSnapshotEntry.paint_scale` 暴露。**popup content 全部平铺在 popup root 下，`PopupScale::compose` 已删除**（`inherited_scale` 仅传递最近层 scale，不做嵌套 compose；P3E-06/P3K-04）。
- **damage 契约**：动画 tick 加入 `rect.translate(previous.offset)` 与 `rect.translate(offset)`（layout rect，不是缩放后 paint rect）；`scale ∈ [0.98,1]` 向内 ⇒ layout rect 是 paint rect 超集，无漏 damage。`settle` rect 同步后把 scaled popup pivot 更新为新中心，**不 mark_paint**：pivot 是纯绘制派生值，但安全前提是同帧 tick 已 mark_paint；缓存键含 pivot 保证不误用旧 pivot；settle 同步 pivot 不额外 mark_paint 以免 `place_subtree` 迁移 damage 并入同帧导致 `damage_full`。
- **modal fade-only**：`is_fade_only()`（modal/Modal/Scrim）时 FadeScale scale 固定 1.0；FadeSlide 模态 offset 0；scrim 关闭不再被缩放（P3E-02/P3K-01）。
- **关闭 reason**：`PopupPanel::close_with_reason` + `begin_close_popup(id, reason)`；锚点丢失/滚出视口统一 `DismissReason::AnchorLost`（Menu/ContextMenu/Dropdown/Select/ColorPicker；Dialog 为 modal 无锚点关闭路径），程序化默认 `Programmatic`，`on_close` 透传。
- **FrameStats**：`frame_cpu: Option<Duration>`（**覆盖 `App::frame` 全程，含 popup tick/paint；非单 popup tick 计时，与 idle 帧区间不完全对等，P3E-03**）、`popup_opening/popup_closing: bool`（动画工作标志），配合 `popup_settle_rounds/popup_unsettled` 断言空闲帧 0 工作。

## 4. 性能实测（本机 Windows 11/D3D12/125% DPI）

| 指标 | 实测 | 目标/守卫 |
|---|---|---|
| idle p95 | **0.6–1 µs**（本机复跑 700ns） | — |
| open 帧 | **首帧冷启动 p95≈1.0–1.13 ms**（含 atlas/text 冷启动；10 帧样本时 p95≈max）；**其余帧中位≈29 µs**（本机复跑 29.1 µs） | 守卫用**中位帧** ≤ idle p95+2 ms（设计与测试一致，P3E-03）；p95 仅记录 |
| close 帧 | 中位≈26 µs；p95≈64–138 µs（尾部；本机复跑 137.8 µs） | 同上（中位守卫） |
| damage_full | false（pixel-check `damage_full_frames=0`） | I6/§4 |
| 空闲帧 | 0 settle / 0 damage / 0 popup 工作 | `idle_frames_do_no_popup_settle_or_damage_work` |

> 测试输出（P3 修复后）：`popup frame budget: idle_p95=700ns open_median=29.1µs open_p95=1.1283ms close_median=26µs close_p95=137.8µs ...（open 首帧含 atlas/text 冷启动，故用中位数守卫设计目标 idle p95 + 2ms，p95 仅记录）`（`cargo test -p ui_core --features test-support --test popup_behavior popup_open_and_close_frames_stay_within_the_frame_budget -- --nocapture`）。

## 5. 三端真实窗口像素验收

| 平台 | 命令 | 结果 | 内容/DPR | 耗时 | 截图 API |
|---|---|---|---|---|---|
| Windows 11 | `cargo run -p ui_kit --example gallery -- --real-pixel-check` | **PASS 106/0** | 1200×900 / 1.25（screen 4800×3072） | **45.9 s**（elapsed_ms=45865、frame 280；隔离 43.3/277） | PowerShell CopyFromScreen（虚拟屏幕） |
| macOS | `tools/real-pixel-check-macos.sh` | **PASS 106/0** | 1914×1437 / 2.0（screen 3360×2100；容差 8） | **72.2 s**（elapsed_ms=72198、frame 283；TCC 需 Terminal） |
| Ubuntu nested sway | `tools/real-pixel-check-linux.sh` | **PASS 106/0** | 1276×670 / 1.0（screen 1280×720；fresh sway+trap） | **20.3 s**（elapsed_ms=20297、frame 311） | `grim`（wl_output 布局） |

- 证据：本机 `target/real-pixels/windows/`、`target/real-pixels/macos-remote/`、`target/real-pixels/linux-remote/`（三端 per-run：`target/real-pixels/{windows/20261127T143605Z-e69de29b,macos/20261127T143706Z-e69de29b,linux/20261127T143842Z-e69de29b}/`，本地副本 `macos-head35c9`/`linux-head35c9`；report/run.log/00–17 PNG）；远端 `/Users/chenbaiyu/kytrix-p3-accept/target/real-pixels/macos/`、`/home/chenbaiyu/kytrix-p3-accept/target/real-pixels/linux/`。
- 语义：标记环反解内容几何；打开必须变化（覆盖量/分散像素），关闭逐像素恢复（容差 2/255、coverage floor、触发器环排除）；偏移/改色负向自检判红；exit 0/3，看门狗 180 s。
- 平台差异：macOS TCC/壁纸问题；Linux 需 Wayland+grim，缺前置脚本 exit 2，nested sway 窗口被 compositor 调整为输出尺寸；Dialog scrim 覆盖标记环时先重试 scrim-dimmed tone、失败回退 baseline origin；标记厚度 6 px×DPR（Windows 8/macOS 10/Linux 6），尺寸容差 max(4,4×DPR)。
- **独占桌面**：并发桌面下 `--real-pixel-check` 假红（exit 3）、`--debug-stack` 可能 300 s timeout；验收脚本对 debug-stack 自动重试 3 次（macOS/Linux 脚本），单次 timeout 只记录该次日志、不改变 gates 总结果。
- **证据快照口径**：以 `report.txt` 的 `source_revision`/`source_status`/`source_diff_sha`/`source_identity` 为准——**三端最终证据 HEAD `35c9381d` + `clean`，`diff_sha=e69de29bb2d1d6434b8b29ae775ad8c2e48c5391`，`source_identity=source-id-file`**；`tools/record-source-id.ps1` 生成 `tools/source-id.txt`（未跟踪生成物，提交前重生成；无则回退 git/unknown）。P3K-06 Text 恒等直通不影响验收场景。
- **假绿修复**：`baseline-stable`/`close-residue` 比较按标记框内缩 `REAL_PIXEL_CORNER_INSET=4` 物理 px，排除 OS 圆角 AA；三端 106/106 稳定；`gallery.rs` 当前 HEAD SHA256=`abf26d49176b613be5b3012ca4137a36e70bd25ebd39fb08c784f34aa250d02f`。

## 6. 测试与门禁基线

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **3882 passed / 0 failed / 2 ignored**（ui_core lib 2802+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 124、snapshot 14、demo 39、ui_kit 891） |
| `cargo test --workspace --no-fail-fast` | **103 binaries：9337 passed / 1 failed / 30 ignored**；唯一失败为 `download_provider::fetch::tests::a_download_shorter_than_expected_fails_and_keeps_the_part` 的 WSAENOBUFS 10055 socket 资源耗尽环境 flake，单测重跑 PASS，与本 feature 无关 |
| clippy（`-p ui_core -p ui_kit --all-targets -D warnings`） | PASS |
| fmt（`cargo +nightly fmt --all -- --check`） | PASS |
| `cargo check --workspace --all-targets` | PASS |
| `gallery --pixel-check` | PASS 94 checks（32 sampled/8 open animation/6+6 close/damage_full_frames=0） |
| `gallery --debug-stack` | PASS（3 级链/取色器栈/rect/Z/parent；menu 度量 `(9, 270.0, 180.075)`） |
| `gallery --real-pixel-check` | **三端 HEAD `35c9381d` clean 各 106/0 checks PASS**：Windows 45.9 s/frame 280、macOS 72.2 s/frame 283、Ubuntu 20.3 s/frame 311；含 scrim theme/composite/alpha + scrim/panel fault + 交互/self-check；需独占桌面，并发会假红 |

逐文件 `#[test]`：引擎 `popup/tests` 41、`popup/placement` 32、`popup_behavior` 124、`a11y/popup_tests` 22；kit `menu` 79、`menu_chain` 13、`context_menu` 21、`popup_panel` 20、`dropdown` 35、`select` 39、`dialog` 55、`tooltip` 28、`alert` 11、`color_picker` 83。

**P3 新增测试（相对 P2 3847，净 +35）**：
- 引擎 `popup_behavior` 111→124（+13）：P3 FadeScale/shadow offset/scrim/glyph-image 等 + 最终交互/焦点/API 清理新增。
- kit 866→891（+25）：Dropdown/Select slide、popup_panel scrim、color_picker 边界 Tab、reason 不泄漏等；`ui_core` lib 2805→2802（−3，API 死代码删除抵消新增）。

## 7. 行为偏差登记（D-P3-1…D-P3-7）

1. **D-P3-1 Dialog 删除 `recenter`**（关闭 D-P1-7）：只保留打开时 anchor 居中，resize 不再居中/不关闭。
2. **D-P3-2 FadeScale modal fade-only**：modal/scrim 不缩放，只淡入淡出。
3. **D-P3-3 damage 用 layout rect 平移超集**：scale≤1 向内；动画 tick 同时加旧/新 offset，关闭帧覆盖旧 rect。
4. **D-P3-4 settle 同步 pivot 不 mark_paint**：pivot 是纯绘制派生值，但安全前提是同帧 tick 已 mark_paint；缓存键含 pivot 保证不误用旧 pivot；settle 同步 pivot 不额外 mark_paint 以免 `place_subtree` 迁移 damage 并入同帧导致 `damage_full`。
5. **D-P3-5 真实窗口像素用截图+标记环**（不做 GPU readback，延续 P0 用户裁决）。
6. **D-P3-6 三端截图 API/前置不同**：Windows CopyFromScreen、macOS screencapture（TCC/Terminal）、Linux grim+nested sway；无 Wayland/grim 时 exit 2；Dialog scrim 覆盖标记时回退 baseline origin。
7. **D-P3-7 性能守卫/口径**：守卫用**中位帧** ≤ idle p95+2 ms（与设计目标一致）；p95 仅记录（10 帧样本 p95≈max，open 首帧含 atlas/text 冷启动），`frame_cpu` 覆盖 `App::frame` 全程（P3E-03）。
8. **D-P3-8（FQ-02）Tab 边界/Closing 重开**：ColorPicker 内部控件 Tab 导航、末控件 Tab 关闭；profile 自定义 Take 面板 `tabbed_out` 防关闭动画期重开；补完整 Tab/Shift+Tab 序列测试。
9. **D-P3-9（FQ-08）`leave_owner_scope` 主树查找**：Shift+Tab 逃逸只在主树前后找可聚焦，不绕回 popup 内容；`a_non_modal_take_popup_shift_tab_escapes_backwards` 断言实际落点。

## 8. 剩余 P4/未闭环

P3K-07 scale 不参与 hit/a11y/IME 的契约未登记、P3K-08 debug-stack 无 paint_scale/像素门禁不采样动画帧、R2-e WindowRect 1×1/0-size 口径、R2-f submenu 句柄回调 root-only、外部滚动语义（window_scroll_epoch 含无关容器）、reconcile Closing churn、ContextMenu 右键自动绑定（D-P2-4）、typeahead IME（D-P2-5）、macOS TCC 一键化、acceptance 归档目录（命令日志/帧号）、真实像素扩展到更多组件/主题。以上均无用户可见验收阻塞，R2-01/02/03/04、FQ-01/08 等已关闭。

## 9. 交接

- 设计 §14 为最终口径；`kit-gallery.md` §14 同步 gallery 侧语义/命令/结果；`p2-summary.md` 保留 P2 历史。
- 证据在 `target/real-pixels/`（gitignored）；提交时 `tools/` 两个脚本需纳入（P3 验收脚本）。
