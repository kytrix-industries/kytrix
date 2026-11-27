# ColorPicker 关闭残影：实机复现与根因（task-93）

HEAD：`d9584e3ff9df55b2ea8f6f97dd3b170671f69f9f`（P0–P3 已提交，工作树干净）
环境：Windows，NVIDIA RTX 5080，D3D12 合成，DPI 125%（client 1500×1000 物理 = 1200×800 逻辑）
结论：**残影真实稳定复现；不是 damage 漏旧矩形，而是“部分重绘帧（persist + scissor）在窗口 clear-color 背景上没有可重绘的场景图元”**。仓库生产 shell 通过显式 `WINDOW_BACKDROP` 节点规避了该问题，gallery example 漏了同款不透明全窗背景节点。

## 1. 结论摘要（先看这里）

1. 真实窗口复现成功：滚动到 section ⑭ → 点 ColorPicker trigger 打开 → 关闭（OK / Esc / 外部点击三条路径均复现）。关闭后 **原浮层覆盖过的 section 卡片“间隙”（12 logical px = 15 physical px 高、浮层宽度约 377 physical px 的横条）残留色板/OK 按钮像素**；卡片本身（不透明矩形）正常重绘，所以只在间隙里看见残影——与用户两张截图一致。
2. 残影是**静态、永久**的：关闭后隔 2 s 再截图逐像素 0 变化；只有“整帧重绘”能清除（本机滚轮滚动会使 viewport damage ≈ 92% 窗口面积 > 50% 阈值 → 升级 Full 帧，残影立刻消失）。
3. **damage 没有漏旧矩形**（用户最初猜测不成立）。headless 持久增量探针显示关闭帧 `damage_full=false`，damage 为 `(530,312)-(850,708)`，完整覆盖旧浮层 `(540,321)-(840,696)`。真正的缺陷是：damage 内、没有场景图元覆盖的像素（页面背景只由 `WindowOptions.background` 的 clear color 提供）在增量帧里保持上一帧 persist 内容。
4. 触发链路：`gallery.rs` 页面根/header/viewport/content 都没有背景绘制（只有 window clear color）→ 关闭浮层时 `record.rs` 的 `FrameDraw::Incremental` 不 clear，只在 damage scissor 内重画与 damage 相交的 op → 卡片（opaque quad）被重画，间隙没有 op → 残留浮层像素。关闭动画（FadeScale 90 ms）逐帧把半透明浮层叠在残留像素上，进一步把残影“叠”到全饱和（实测最大通道差 255）。
5. 现有门禁全绿的原因：`--pixel-check` 与 `--real-pixel-check` 的 fixture **都自己画了不透明全窗 backdrop**（`gallery.rs:3633`、`gallery.rs:5166`），生产 shell 也画了 `WINDOW_BACKDROP`（`crates/kytrix/src/shell/root.rs:61` → `crates/kytrix/src/shell/ui.rs:161-195`）；只有真实 gallery 页面没画，所以只有实机用户能看到。
6. 临时修复验证：给 gallery 页面加一个全窗不透明 backdrop 节点（与 kytrix shell 同款）后，同一实机操作 before/after 差异只剩 trigger focus ring、交互日志文本、窗口边缘，**所有间隙残影消失**（见证据图）。此临时改动已还原（见 §7）。

## 2. 复现步骤与观测（真实窗口）

### 2.1 命令与窗口

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'
cargo build -p ui_kit --example gallery
E:\kytrix\target\debug\examples\gallery.exe --scroll-to=3150
```

- 窗口创建：`1200×800` logical（`gallery.rs:966-971`，`background: canvas`）；实机 client `(109,138) 1500×1000` 物理、DPI 120（125%）。
- `--scroll-to=3150` 在 8 s 后触发（`gallery.rs:2340-2349`），实际被 clamp 到最大滚动位置（日志 `gallery: scroll-to offset=3150`），页面停在 section ⑫–⑮。
- 自动化脚本：`E:\kytrix\target\ghost-real\ghost-real.ps1`（Win32 `FindWindow/GetClientRect/ClientToScreen` + `SetCursorPos/mouse_event/keybd_event` + `CopyFromScreen`；先 `SetProcessDpiAwarenessContext(-4)`，截图按 virtual-screen 原点偏移裁剪）。
- 关键点击坐标（物理屏幕）：ColorPicker trigger `(172,728)`；弹层 OK `(498,677)`；外部点击 `(1309,438)`；Esc 通过 `keybd_event(VK_ESCAPE)`。

### 2.2 现象

| 关闭方式 | 截图 | 结果 |
| --- | --- | --- |
| OK 按钮 | `03-closed.png` / `closed-client.png` | 3 条间隙残影（灰阶色板行、彩色色板行、OK 蓝色块） |
| Esc | `08-esc-closed.png` | 同样 3 条 |
| 外部点击（点击 section ⑫ 卡片空白） | `10-outside-closed.png` | 同样 3 条 |

像素量化（`closed-client.png` vs `before-client.png`，同一滚动位置）：
- 残影横条 y = 140–154、343–357、508–522（physical；每条 15 px = 12 logical px 卡片间隙），x = 44–420（≈ 浮层宽 377 px）。
- 每条横条 changed 像素 ≈ 5655（377×15）+ 卡片圆角 AA 约 42；整帧 diff 23681 中还包含交互日志新行、trigger focus ring、scrollbar/窗口边缘等已知动态区域。
- 残影颜色：灰阶色板行（saturation 0）、彩色色板行（saturation 167–229）、OK 按钮 brand blue `(76,154,255)`。
- 关闭后 2 s 再截图（`04-closed-later.png` vs `05-stable.png`）：**changed = 0**，残影不自行消失。
- 滚轮向上 1 格（`06-wheel.png`）：页面滚动，viewport damage 约等于窗口面积的 92%（> `FULL_FRAME_AREA_RATIO=0.5`）→ 走 Full 帧 `ClearRenderTargetView`，**残影全部消失**。

### 2.3 与用户截图对照

用户截图（`user-02-picker-closed-ghost.png`）中残影只出现在 section 卡片之间的白/暗背景带，卡片上干净；本机复现的三条横条完全同构（见 `evidence-figure.png` 中栏红框）。

## 3. 根因链路（file:line）

### 3.1 触发链

1. **gallery 页面只有窗口 clear color，没有全窗背景图元**
   - `crates/ui_kit/examples/gallery.rs:966-971`：`create_window(WindowOptions { size, background: canvas })`。
   - 根样式 `:981-984` 只有 `flex_col + size_full`；header `:985-998`、viewport `:1054-1066`、content `:1088-1106` 均无 background paint。
   - 对照：生产 shell 显式画不透明全窗节点 `WINDOW_BACKDROP`：`crates/kytrix/src/shell/root.rs:60-61` → `crates/kytrix/src/shell/ui.rs:161-195`。
2. **ColorPicker 关闭 = 非模态 Auto popup，走 90 ms FadeScale 动画，最后 remove 节点**
   - `crates/ui_kit/src/kit/color_picker.rs:1469-1471`：`spec.tier=Auto`、`spec.animation=PopupAnimation::FadeScale`；关闭策略 `:85-90`。
   - `crates/ui_core/src/popup.rs:591-597`：`POPUP_OPEN_DURATION=120ms`、`POPUP_CLOSE_DURATION=90ms`、`POPUP_SCALE_FROM=0.98`。
   - `crates/ui_core/src/popup/dismiss.rs:196-235` 入 Closing 动画；`:238-254` 动画结束 `finalize_close` → `close_popup_now`；`crates/ui_core/src/app/popup.rs:517-639` 移除节点并产生旧绘制外延 damage；仅 modal/scrim 走 `force_full_paint`（`app/popup.rs:592-594`、`:416-418`），Auto popup 不走。
3. **Windows 持久化合成：Full 帧 clear，增量帧只在 damage scissor 内重画 op，不 clear**
   - `crates/ui_core/src/render/windows/compositor/record.rs:410-457`：`FrameDraw::Full` 对 persist 整块 `ClearRenderTargetView`（`:418-423`），`FrameDraw::Direct` 对 swapchain clear（`:376`）。
   - `:458-515`：`FrameDraw::Incremental` 绑定 persist RTV，按每条 damage 设置 scissor（`:481-502`），只绘制与 damage 相交的 op；**没有 clear**；之后只把 damage 区域 copy 到 back buffer（`:504-514`）。
   - 这是既定实现（`.docs/popup-overlay-redesign/evidence/paint-damage-rootcause.md:155,348` 明确“scissor 外的 persist 纹理原样保留”）。
4. **关闭帧的 damage 是正确的（关键判别）**
   - headless 持久增量探针（同一条 `record.rs` 增量路径，WARP）实测：弹层 `(540,321)-(840,696)`；关闭动画帧 damage = `(530,312)-(850,708)`（含阴影/AA pad）与 `(539,699)-(661,731)`（trigger rect），`damage_full=false`，每一帧都 `covers_popup=true`；unmount 帧同样覆盖。
   - 因此“旧矩形没进 damage”被证伪；问题发生在 damage 区域内部：背景像素没有 op 可重画。

### 3.2 为什么只在卡片间隙残留

- 浮层覆盖区内：section 卡片自身有 opaque background quad（`gallery.rs:794-819` 的 `section()` + theme `SurfaceRaised`），增量帧会把它们重画，所以卡片上干净。
- 卡片之间 12 logical px 的间隙、页面左右留白、header 背后等区域**没有任何 op**，只有 window clear color；增量帧不会 clear，persist 里仍是关闭前最后一帧浮层像素 → 残留。
- 因为浮层是“横向大、纵向覆盖多个卡片”，所以视觉上只在若干条 15 px 高的横带里看到色板——正好是用户截图的形态。

### 3.3 关闭动画放大残影（同一根因的叠加效应）

- 关闭动画逐帧把 opacity 递减的浮层图元 emit 到同一批像素（`popup/animation.rs:149-180` 计算 closing opacity；`paint/pass.rs:513-560,1802-1806` 把 opacity 应用到 lane/装饰/内容）。
- 在卡片上：浮层图元叠在已重画的不透明卡片上，无问题；在间隙里：因为背景没有被重画，半透明浮层叠在上一帧的浮层像素上 → **alpha 累积、颜色越来越饱和**，最后一帧 unmount 时留下近乎全色的残留（headless 最大通道差 255，实机 227）。
- 观察到的旁证：同一机制下，半透明/AA 内容在增量帧重复叠画会改变观感；本次还观察到 header 文本在浮层交互后变亮、整帧重绘后恢复（见 §8 未决观察）。

### 3.4 为什么现有门禁全绿

| 门禁 | 检查内容 | 为什么没抓到 |
| --- | --- | --- |
| `--pixel-check`（headless 持久增量） | 旧浮层区域逐像素回到打开前基线 + 覆盖率下限 + 负向自检 | fixture 自己画了不透明全窗 backdrop：`gallery.rs:3629-3643`（`gallery.pixel-check:backdrop`）；背景永远有 op 可重画 |
| `--real-pixel-check`（真实窗口截图） | ColorPicker/MenuChain/Dialog 的合成像素 | fixture 同样画 `gallery.real-pixel:backdrop`：`gallery.rs:5163-5174`；而且它跑的是 synthetic scene，不是真实 gallery 页面 |
| `--debug-stack` | popup 栈/rect/state（程序化 open/close，确在真实 gallery 页面） | 只校验栈/矩形，不读像素、不截图；关闭后直接关窗退出 |
| `cargo test -p ui_core -p ui_kit` | damage/painted_extent 不变量、kit 行为 | 所有像素用例同样都带 backdrop（`test_support/pixels.rs:463` 的 `backdrop()` helper）；没有“无背景节点”场景 |

生产 kytrix shell 画 backdrop 所以真实产品窗口不中招；gallery example（用户启动的那个）是唯一漏画 backdrop 的宿主。

## 4. 判别实验（headless，可重复）

探针源码：`E:\kytrix\target\ghost-real\ghost_probe.rs`（仓库外；临时拷入 `crates/ui_kit/examples/ghost_probe.rs` 运行，已删除、工作树干净）。场景：1200×800 窗口、窗口背景 = `SurfaceCanvas`（不画 backdrop）、3 张不透明卡片 + 2 条 12 px 间隙、ColorPicker 锚定在第 3 张卡片的 trigger；用 `test_support::pixels::PersistentPixels` 逐帧重放（与真实窗口完全相同 `record.rs` 增量通路）。

```powershell
cp E:\kytrix\target\ghost-real\ghost_probe.rs crates/ui_kit/examples/ghost_probe.rs
cargo run -p ui_kit --example ghost_probe              # 无 backdrop
cargo run -p ui_kit --example ghost_probe -- --light  # 无 backdrop，浅色
cargo run -p ui_kit --example ghost_probe -- --backdrop  # 加全窗不透明 backdrop
```

结果（保存于 `probe-*.txt`）：

| 场景 | 残影像素 | 位置 | 结论 |
| --- | --- | --- | --- |
| 无 backdrop（dark） | **3696** | bands `(552,564,536,844)` = 12 行 × 308 px，`inside_cards=0` | `GHOST_ON_BACKGROUND_ONLY` |
| 无 backdrop（light） | **3696** | 同上 | `GHOST_ON_BACKGROUND_ONLY` |
| 加全窗不透明 backdrop | **0** | — | `CLEAN` |

- 3696 = 12（间隙行）× 308（浮层宽 + AA pad），**残影像素恰好等于浮层在“背景-only 区域”的面积**，没有多一个卡片像素；实机 15 px × 377 px ≈ 5655 与之一致。
- 关闭帧 `damage [530,312]-[850,708]` ⊇ 浮层 `[540,321]-[840,696]`，`damage_full=false` → damage 覆盖旧外延但没有背景 op，故残留。

## 5. 候选修复方案（需要用户决策）

> 用户已明确：修复方案多于 1 个时停下来由用户决策；以下方案均未修改生产代码。

### 方案 A（推荐先用，已验证）：给 gallery（及其他依赖 window background 的宿主）补不透明全窗 backdrop 节点

- 改动面：`crates/ui_kit/examples/gallery.rs`（约 20 行，与 `crates/kytrix/src/shell/ui.rs:161-195` 同款）：
  - 根样式加 `.relative()`；
  - 在 root 下加第一个子节点：absolute inset 0、`background` 绑定 `Role::SurfaceCanvas`（初始 `canvas`）；
  - 不改任何引擎/kit 行为。
- 已验证：临时打上该 patch 后，同一实机操作 before/after diff 只剩已知动态区域（trigger focus ring 413 px、交互日志文本 4059 px、窗口边缘 38 px），**三个间隙横条全部消失**；headless 加 backdrop 残影 0。
- 风险：低。新增 1 个全窗 quad：每帧仅在 damage scissor 内绘制，开销很小；主题切换由 binding 保持同步；浮层阴影/AA 会正确叠在 backdrop 上。需顺带审计所有 `App::create_window` 宿主是否都画了不透明背景（至少 gallery/其他 examples；kytrix 已有）。
- 测试：真实窗口 gallery 页面 ghost gate（见 §6.1）+ headless 断言 gallery 根下有覆盖窗口的 opaque backdrop（§6.2）。
- 注意：这是“让宿主遵守既有的隐式契约”，不是修掉契约本身的脆弱性。

### 方案 B（引擎加固，根治契约脆弱）：增量帧先把 damage 区域重绘成窗口背景

- 语义：`FrameDraw::Incremental` 在绘制 op 之前，把每条 damage 矩形清成 `clear_color`（或先 emit 一个全窗背景 op），使“damage 内像素从零重画”，与 Full 帧语义一致；damage 之外仍保留 persist。
- 改动面（三端逐端评估）：
  - Windows：`record.rs` Incremental 分支在 `bind_frame` 前一次 `ClearRenderTargetView(persist.rtv, clear_color, Some(&damage_rects))`（D3D12 支持 rect 数组），`FrameDraw::Full/Direct` 不变。
  - Linux Vulkan：`vkCmdClearAttachments` 支持按 rect clear，改动小。
  - macOS Metal：`MTLLoadAction` 只能整附件 Clear，需用 scissor + 一个纯色 draw pass（或 blit/fill 方案）实现按 damage 矩形填背景，改动最大。
  - 备选（跨端统一）：在 frame payload 里注入“窗口背景”op（全窗纯色 quad，最先绘制），由 instance/op 路径自然裁剪到 damage；需把窗口背景色接入 paint/payload，并保证全帧/增量一致。
- 收益：不再要求每个宿主画 backdrop；修复同类问题（任何 popup/tooltip/menu 关闭在背景区残留、半透明内容在增量帧重复混合导致 alpha 累积）。
- 风险/成本：中高。触及三端最底层合成路径与性能门禁；需验证所有像素/真实窗口门禁与 `damage_full` 断言不回归；需确认没有内容依赖“不 clear 的 persist 背景”（按场景语义应没有）。
- 测试：见 §6.3（无 backdrop 的持久增量像素测试在修复前必红、修复后必绿）；三端 `--real-pixel-check` 加无 backdrop fixture。

### 方案 C（不推荐，仅作临时止血）：popup 关闭强制整帧/整窗 damage

- 改动面：`app/popup.rs` 对所有 popup close 调 `force_full_paint`（现在只有 modal/scrim），或在 popup 的旧 extent damage 之外加 `push_full`。
- 风险：直接违背 P0–P3 的核心目标与现有测试 `opening_and_closing_the_panel_never_forces_a_full_repaint`（`color_picker.rs:3862`）；菜单/取色器每次关闭整窗重绘，性能和闪烁代价大。仅在需要临时止血时考虑，不应作为最终修复。

### 方案 D（可选配套，降低再次踩坑）：把 `window_backdrop` 提取为可复用 helper

- 生产 shell 的实现（`crates/kytrix/src/shell/ui.rs:161-195`）可以提取到 ui_kit（如 `kit::backdrop` 或引擎 API），gallery/其它宿主统一调用；降低“新 example 忘记画背景”的概率。
- 风险：新增公开 API 面；按 AGENTS 需要确认真实复用价值（当前至少 kytrix shell + gallery + 可能的其它 example 两个以上消费者）。若采用 A，建议同时做。

**推荐组合**：先用 A（低风险、已实测）消除用户可见 BUG；是否再做 B 由用户决策（B 是根治“背景契约”的引擎级方案，但跨三端、风险/工作量更高）。C 不建议；D 可选。

## 6. 回归测试建议

1. **真实窗口 gallery 页面 ghost gate（最高价值，覆盖用户路径）**
   - 在现有 `--real-pixel-check` / `tools/real-pixel-check-windows.ps1` 体系里新增一个“真实 gallery 页面”阶段（不是 synthetic fixture）：`--scroll-to=3150` → 打开 ColorPicker（真实点击或程序化均可）→ 分别用 OK/Esc/外部点击关闭 → 在关闭后截图，与打开前同位置截图对比；
   - 断言：旧浮层 rect 内、页面背景区域（卡片 rect 之外）不得有像素差异（容差 ≤2）；已知动态区域（交互日志矩形、trigger/focus ring、scrollbar、窗口边缘）显式排除，且排除面积设上限防假绿；对“打开前/关闭后”两张图都要求同滚动位置。
   - 该 gate 在当前 HEAD 会 FAIL（三处横条），方案 A/B 修复后 PASS。
2. **headless 不变量：gallery 页面必须有覆盖窗口的不透明背景图元**
   - 若采用 A：在 gallery 构建完成后断言存在一个布局覆盖整个 window、background role=SurfaceCanvas 的节点（比像素测试便宜、稳定），防止未来 example 又漏画。
3. **引擎级像素测试（方案 B 的必要回归）**
   - `test_support::pixels` 增加：**不带 backdrop 节点**的窗口，放一个不透明/半透明节点，remove/隐藏后重放增量 damage，断言重放像素 == 整帧基线（当前必红：背景区保留旧像素）。同时覆盖“关闭动画多帧后回到基线”（防止 alpha 累积）。
   - 三端各自跑；Windows 的 WARP 离线路径可直接覆盖。
4. **damage 覆盖不变量（防用户最初猜测的回归）**
   - 扩展 `color_picker.rs::opening_and_closing_the_panel_never_forces_a_full_repaint`（`:3862`）：除 `damage_full==false` 外，断言关闭每个渲染帧的 damage 覆盖旧 `paint_rect`（含阴影 pad）；现有实现已通过（探针证据），但可防未来 damage 收窄。
5. **可选**：`--debug-stack` 在完成 picker-close 检查后加一次“关闭后像素/截图断言”（它已经在真实 gallery 页面上跑完完整 open/close 流程，是接入 gate 成本最低的位置）。

## 7. 证据路径与临时改动还原

证据目录：`E:\kytrix\target\ghost-real\`

- 实机复现：`01-before-open.png`、`02-open.png`、`03-closed.png`（OK）、`08-esc-closed.png`（Esc）、`10-outside-closed.png`（外部点击）；对应 client 裁剪 `*-client.png`；小图 `*-small.png`。
- 对照图：`evidence-figure.png`（前/后/修复后三栏），`ghost-annotated-small.png`（三条残影带标注），`diff-highlight.png`。
- 稳定性：`04-closed-later.png`、`05-stable.png`（2 s 后 0 diff）、`06-wheel.png`（滚动 Full 帧后残影消失）。
- 方案 A 验证：`11-backdrop-before.png`、`12-backdrop-open.png`、`13-backdrop-closed.png`（临时 backdrop build），diff 结果见 §5。
- headless 探针：`ghost_probe.rs`、`probe-dark-no-backdrop.txt`、`probe-light-no-backdrop.txt`、`probe-dark-with-backdrop.txt`。
- 分析脚本：`ghost-real.ps1`（真实窗口点击/截图）、`compare.py`、`analyze.py`、`pair.py`、`rows.py`、`figure.py`。
- 用户原图副本：`user-01-picker-open.png`、`user-02-picker-closed-ghost.png`。
- 临时改动 diff：`temp-gallery-backdrop-probe.diff`（gallery.rs +20/−1，已 `git checkout` 还原）；`ghost_probe.rs` 已移出仓库。当前 `git status` 干净（只剩本文件待由 lead 决定是否入库）。

临时改动明细（已有 diff 存档，未提交）：
1. `gallery.rs`：root 加 `.relative()` + 全窗 `gallery.backdrop.probe` 节点（`SurfaceCanvas`，absolute inset 0），用于验证方案 A；已还原。
2. 仓库外 `ghost_probe.rs`：headless 判别探针；临时拷入 examples 运行后删除。

## 8. 未决观察（未纳入本 bug，建议单独 P3 跟踪）

- header 标题文本亮度在“打开浮层”后变亮、持久到关闭之后，直到滚动触发 Full 帧才恢复（`01-before-open.png` vs `02-open.png`/`03-closed.png` vs `06-wheel.png` 的 y=31–53 区域）。现象与“半透明/AA 内容在增量帧被重复叠画到 persist”同类，但标题区域不在浮层 damage 内，机制未定位；不影响本次残影判定，建议单列观察项。
- 卡片圆角/边框 AA 在残影横条上下有 1–4 px 的轻微变化（残影带边缘），同属增量重绘未 clear 的边界效应。

## 9. 明确未做的事

- 未修改任何生产代码/测试；临时 gallery patch 与探针已全部还原（`git status` clean）。
- 未实现任何候选修复（等用户对 §5 方案决策）。
- 未提交 git。
