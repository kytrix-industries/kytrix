# P2-5 `kit` gallery 示例与三端真实窗口验收（spec-delta）

> 批次：P2-5（base_ui kit 收尾）+ **P2-6b 收尾（G3 修复 / G7 登记 / `--theme-check`）** + **P2-6c 收尾（G2 settle 根因修复 / G5 ColorPicker 可观测 / G6 Spinner 颜色）** + **P2-6d 合并后审查收尾（F1–F8，§11.8）**
> 工作树：`E:\kytrix\.agents\worktrees\p2-5`（P2-5）、`E:\kytrix\.agents\worktrees\p2-6b`（P2-6b）、`E:\kytrix\.agents\worktrees\p2-6c`（P2-6c）
> 分支：`mig/p2-5`（基线 `gui-engine-migration@3ce9593`）→ `mig/p2-6b`（先 `git merge mig/p2-5`，head `55ee31c`）→ `mig/p2-6c`（基线 `gui-engine-migration@810d0e7`，head **`e94f389`**；P2-6c 提交 `9068a27`）
> 提交：P2-5 `cd607d0`（`feat(base_ui): P2-5 kit gallery 示例（三端真实窗口验收）`）；P2-6b `55ee31c`（`fix(base_ui): P2-6b 运行时主题响应性（G3）+ 品牌适配（G7）+ gallery 收尾`，未合并/未 push origin）；P2-6c `9068a27`（`fix(base_ui): P2-6c 列表 rebuild settle（G2）+ ColorPicker open_state/Spinner 颜色 + gallery 收尾`，未合并/未 push origin）；P2-6d `e94f389`（`fix(base_ui): P2-6d 审查收尾（set_count/push 真实泵序、文本行高残余、文档与测试守护）`，未合并/未 push origin）
> 交付：`crates/base_ui/examples/gallery.rs`（P2-5 新建，P2-6b 加 `--theme-check` / `--start-light`，P2-6c 把 `--dump-overlay` 改成 G2/G5 断言 + Spinner 角色色）、`crates/base_ui/src/kit/mod.rs`（文档）、P2-6b 的 kit 主题读取改造（见 §9）、P2-6c 的 kit 修复（见 §11）、P2-6d 的审查收尾（§11.8）
> 证据：`E:\kytrix\.review\mig-p2-5-*`、`E:\kytrix\.review\mig-p2-6b-*`、`E:\kytrix\.review\mig-p2-6c-*`、`E:\kytrix\.review\mig-p2-6d-*`（截图 / 日志 / 变异）

## 1. 示例结构

单文件、只依赖 `ui_core` + `base_ui::{kit, theme, style_builder}`；窗口 1200×800、系统标题栏、引擎 scroll 容器承载 **16 个分区**卡片（`flow flex_col`；P2-5 时为 15 个，P3 在 ⑨ 后插入 ⑩ 宽表，旧 ⑩–⑮ 顺延为 ⑪–⑯）：

| # | 分区 | 覆盖组件 / 能力 |
|---|---|---|
| ① | 主题色卡 / 图标 / 品牌 | `theme` 7 角色色卡（bind_style 跟随外观）、`icon::IconKind::ALL`（55）+ `IconSpec`、`brand::ALL`（7）+ `Brand` |
| ② | 按钮 | `button::{Button, ButtonVariant}` × 尺寸（`UnifiedSizable`）5 变体 × 3 档 + 禁用 / 加载 / 图标 + 文字 / 选中 |
| ③ | 开关 | `toggle::Toggle`：Switch / Checkbox / Radio、`Space`/`Enter`、`on_change` |
| ④ | 反馈 | `badge` 6 色、`progress`、`spinner`（静态，见 G6）、`separator`、`empty_state` |
| ⑤ | 标签栏 | `tab_bar::TabBar`（Slide 指示条 / 关闭按钮 / 方向键） |
| ⑥ | 列表 | `list::List` 等高 / 变高（`RowHeight::Variable` 实测回写）/ 双端 deque（push 按钮） |
| ⑦ | **双轴滚动演示** | `scroll_bar::ScrollBar` 垂直 + 水平两条 bar 代理同一 `App::make_scroll` 容器（20 行 × 宽内容；两轴可滚、拖拽 / track 滚轮 / 轨道点击）；**外层页面还有 hover 淡入的 overlay 滚动条**（页面可滚时出现） |
| ⑧ | 表格 | `table::{Table, Column}`：固定/弹性列（`Column::flex(header, weight, min_width)` 三参数）、跨列（`span(2)`）、可排序表头、`SelectionMode::Multi`、变高行 |
| ⑨ | 树 | `tree::{Tree, TreeItem}`：展开折叠 / 层级 / 方向键 / 单选 |
| **⑩** | **宽表（新增）** | `Table` 横向滚动 + **内部 sticky 表头**：窄容器 420（出现水平条、可横向滚动/表头跟随）与宽容器 1100（无水平条）对照；三参数列配置；G7–G9 |
| ⑪ | 输入框 | `input::{InputSpec, InputState, InputNode}`：单行 / 掩码 / 禁用 / 多行 + IME 提示 |
| ⑫ | markdown | `markdown::{parse::parse_document, view::MarkdownView}`：富文本 / 代码 / 表格 / 引用 / 列表 / 行内图片占位 |
| ⑬ | 浮层 | `popup_panel::PopupPanel` / `MenuChain`、`tooltip::Tooltip`、`dropdown`、`select`（多选 + 标签）、`menu`、`context_menu` |
| ⑭ | 对话框 | `dialog::DialogBuilder`（普通对话框 + `alert` 形状 / 确认取消 / on_close / 模态遮罩） |
| ⑮ | 颜色选择器 | `color_picker::ColorPicker`（触发器 + on_change） |
| ⑯ | 交互日志 | 最近 8 条事件（指针命中节点 debug id / 主题 / 按钮 / 开关 / 表格 / 菜单 / 对话框） |

> **编号说明**：本文件 §2–§4、§9–§11、§13.5 中 P2-5/P2-6b 时期的日志与引用沿用当时 15 分区编号（旧 ⑩ 输入框、⑪ markdown … ⑮ 日志）；P3 起现行编号以上表为准（**⑩ 宽表**，旧 ⑩–⑮ 顺延为 ⑪–⑯，共 16 分区）。

实现要点：

- **组合根义务**：`ThemeHandle` / `FrameRouter` 各一份（浮层由引擎 popup 栈持有，不再有 `Layer` / `AnimationRouter`）；`App::on_frame` 单钩子里 `drain_events → 各组件 dispatch → status 镜像 → 各组件 sync`（与 `kit/mod.rs` 的组件形态一致）；
- 页面滚动：内容列显式 `height(total)` + `flex_none()`（taffy 默认 `flex_shrink:1` 会把卡片压扁——gallery 首版曾只显示分区标题）；
- 验收钩子：`--auto-demo`（`dispatch_action(Programmatic)` 触发主按钮）、`--idle-check`（1 s 基线 + 3 s 帧增量 ≤2，PASS 后关窗）、`--wheel-check`（进程内注入滚轮/拖拽，G1–G9 **37 断言**；PASS 文案 `gallery: wheel-check PASS (G1-G5, G7-G9; 37 assertion(s))`；G6 已并入 G2/取消）、`--probe`（打印关键节点布局盒 + 底部滚动偏移 + 嵌套列表 `mounted_rows`，供外部点击换算坐标）、`--scroll-to=<y>`（程序化滚动，截图用）；退出码 0/3；
- 页面滚动条：**Hover 淡入的 overlay 滚动条**（页面可滚时出现，不占布局；⑦ 的两条条是独立容器内的常驻 bar）；
- 宽表 ⑩：窄 420 / 宽 1100 两个 `Table` 对照（sticky 表头、水平条 tail gap 4 px、三参数列配置）；G7 覆盖水平条 track 滚轮 + thumb 拖动；
- 截图/点击脚本（仅本地，`.review` gitignored）：`mig-p2-5-win-final.ps1`（DPI-aware OS 级点击 / 滚轮 / 键盘）、`mig-p2-5-mac-gallery.sh`、`mig-p2-5-linux-gallery.sh`。

## 2. 三端真实运行结果

| 项 | Windows 11（本机 RTX 5080 / D3D12） | macOS 15.8.3（Intel / launchctl asuser 501） | Linux（GNOME mutter Wayland / lavapipe） |
|---|---|---|---|
| 构建 | ✅ `cargo build -p base_ui --example gallery`（`KYTRIX_UI_CORE_MANIFEST=0` 过渡开关，见 spec-delta `build-transition.md`） | ✅ 43 s | ✅ 17 s（需用户目录补 `libxkbcommon-x11`/`libxcb` 等 `.so` 开发符号，见 `p2-5-linux-gallery.sh`） |
| 窗口 | ✅ 1200×800 / DPI 1.25（物理 1500×1000） | ✅ 1200×800 / Retina 2× | ✅ 1200×800 / Vulkan llvmpipe，Wayland swapchain extent=(1200,800)，present_mode=MAILBOX |
| 首帧 / 事件 | ✅ 首帧 frame_index=2；真实 OS 点击（主题切换、主按钮、开关）、滚轮、键盘（输入框键入 `kit gallery 123`）均生效；日志见 §3 | ✅ frame_index 2；`--auto-demo` 动作派发 fired=true；无 OS 级注入（ssh 会话无辅助功能权限），输入类交互以程序化动作演示 | ✅ frame_index 1→3；`--auto-demo` fired=true；Wayland 会话 + 软渲染 |
| I3（空闲 3 s） | ✅ 0 帧（baseline 2 → 2） | ✅ 0 帧（2 → 2） | ✅ 2 帧（1 → 3；Linux 首帧后既有启动补轮，与 E10c 登记一致；阈值 ≤2 PASS） |
| 退出码 | ✅ 0（idle-check PASS 后关窗） | ✅ 0 | ✅ 0 |
| 截图 | ✅ `.review/mig-p2-5-gallery-win*.png`（客户区 CopyFromScreen，DPI-aware；主题/日志/对话框/浮层齐全） | ⚠️ **环境限制**：TCC 未授予 Screen Recording，`screencapture` 只得到桌面壁纸（6 255 224 B 固定尺寸）、`screencapture -l <windowid>` 报 `could not create image from window`；窗口存在证据：Swift `CGWindowListCopyWindowInfo` 列出 `owner=gallery, window=229` + 运行日志 | ⚠️ **环境限制**：mutter 不实现 `wlr-screencopy-unstable-v1`（grim 报错原文）且 `org.gnome.Shell.Screenshot` 返回 `AccessDenied: Screenshot is not allowed`；真实窗口已被 xdg-toplevel 创建（Vulkan swapchain 日志） |
| 证据文件 | `.review/mig-p2-5-gallery-win.log`（idle/auto-demo）+ `-win-run.log` / `-win-theme.png` / `-win-log*.png` / `-win-dialog.png` / `-win-alert.png` / `-win-menu.png` / `-win-context.png` / `-win-picker.png` / `-win-lists.png` / `-win-markdown.png` / `-win-input.png` | `.review/mig-p2-5-gallery-mac.log`、`mig-p2-5-gallery-mac.png`（壁纸，证明截图权限受限） | `.review/mig-p2-5-gallery-linux.log`（含 Vulkan/swapchain、grim/gdbus 拒绝原文） |

重复命令（本地）：

```powershell
# Windows：idle-check + auto-demo（README 里的标准验收）
$env:KYTRIX_UI_CORE_MANIFEST='0'; cargo run -p base_ui --example gallery -- --auto-demo --idle-check
# 截图/交互证据（需要桌面会话）
pwsh -File .review/mig-p2-5-win-final.ps1
# macOS / Linux
bash .review/p2-5-mac-gallery.sh     # 通过 ssh my_macos 执行
bash .review/p2-5-linux-gallery.sh   # 通过 ssh my_ubuntu 执行
```

## 3. 交互证据摘录（Windows 真实窗口）

`mig-p2-5-gallery-win-run.log` / 截图底部交互日志（最近 8 条）：

```text
gallery: 建树完成（16 个分区；P2-5 时为 15，P3 插入 ⑩ 宽表后为 16）
window: resize 1200×800
window: focus=true
theme: 切换为 Light                      ← 真实点击顶栏按钮（截图 -theme.png 全窗变浅色）
pointer down (53,390) -> <none>
button: 主要 点击                        ← 真实点击 ② 第一行主按钮（动作 handler 生效）
pointer down (73,617) -> gallery.toggle.switch.0.interactive
toggle[通知]: false                      ← 真实点击 ③ 开关（命中节点 debug id 可见）
pointer down (95,341) -> <none>
dialog: open()                           ← 真实点击 ⑬「打开对话框」
dialog: 关闭 Dismissed                    ← Esc（引擎 Dismiss）
pointer down (170,341) -> <none>
alert: open()                            ← 真实点击 ⑬「打开 Alert」
pointer down (488,179) -> <none>
menu: 打开（↑↓ / Enter / Esc / 子菜单 →）  ← 真实点击 ⑫「打开 Menu」
pointer down (607,179) -> <none>
context-menu: 打开（按钮演示右键语义）
pointer down (51,473) -> gallery.color-picker.trigger
```

对话框（普通 + alert）在 Windows 上渲染/模态/焦点/Esc 关闭均正常（`-win-dialog.png` / `-win-alert.png`）。

## 4. 暴露的缺口清单

> P 级：P0 阻断交付 / P1 真实窗口不可用或数据错误 / P2 明显体验或一致性问题 / P3 低成本改进 / P4 记录项。
> 「建议批次」是给 Lead 排期的建议 crate/批次，不表示本批修改。

### G1（P1，引擎）外层 scroll 滚动后，内层 scroll / 虚拟列表内容整体不绘制 —— **由 P2-6a 处理（本批不重复；P2-6b 浅色截图仍可见此缺口）**

- 现象：页面（引擎 scroll 容器）滚动到 ⑥⑦⑧⑨ 后，**列表行、自绘滚动条的 20 行、表格数据行、树行全部空白**，只剩卡片标题 / 表格表头 / 自绘滚动条滑块、以及 `gallery.color-picker.trigger` 等普通节点。滚轮滚动与 `App::scroll_to` 程序化滚动都复现；改变窗口尺寸（重排）也不恢复。
- 反证：同一构建首屏 ①②③④ 全部正常；`--scroll-to=2300` 后 ⑩ 输入框 / ⑪ markdown（纯 flow 内容）渲染正常；`--probe bottom` 打印嵌套列表 `mounted_rows = [("list_fixed",8),("list_variable",6),("list_deque",5),("table",8),("tree",9)]` —— **行节点没有丢，只是不绘制/被裁掉**。
- 初判根因：内层 scroll 容器（以及其虚拟列表行 / 绝对定位内容）在布局期按窗口坐标固化，外层 scroll 偏移没有传播到这些子树的绘制/裁剪链；渲染 damage 又只覆盖外层 scroll 的移动区域，于是整块不画。属 E3（虚拟列表）/E9（集合）与 scroll 组合的引擎缺口。
- 建议：P2 引擎批（E3d 或 E9d）—— 外层 scroll 时对后代 scroll/virtual 子树做偏移传播 + 重新挂载/标脏；补真实窗口（或 WARP 离屏）嵌套滚动的像素回归。
- 证据：`.review/mig-p2-5-gallery-win-lists.png`、`-win-markdown.png`（同一次程序化滚动，flow 内容正常）、`mig-p2-5-probe2.log`（mounted_rows）、`mig-p2-5-afterresize.png`。

### G2（P1，引擎/kit）`ItemPanel` 浮层（Menu / ContextMenu / Dropdown / Select）在真实窗口不挂载行 —— **P2-6a 定位到 kit `list.rs`；P2-6c 已修复（§11.1）**

- 现象：⑫ 的「打开 Menu」「打开 ContextMenu」与 Dropdown/Select 面板只画出**空白面板壳**（尺寸正确），没有条目文本/分隔线/高亮；深色、浅色主题都空白。
- 反证（模型层正常）：真实点击 Dropdown → ↓ ↓ Enter 成功选中「选项 B」并回写触发器、触发 `dropdown: 选中 b` 日志。
- 证据（`--debug-overlay` 临时探针，已随代码清理，日志保留）：`gallery.menu:panel:list:viewport children=0`（连续 4 次 dump，frame 104/112/120/120 均 0），即虚拟列表**从未 mount 行窗口**；`GALLERY` 页面里的 List（⑥）同样用 `List::build/attach` 却正常 —— 差异是面板在 `Overlay` 里且首帧 `visible=false`（overlay 的 measure-then-place 语义）。
- 初判根因：引擎虚拟列表在「初始不可见子树」里不挂载；overlay 面板显示后没有触发 rewindow（kit 的 `ItemPanel::sync` 也未在可见后补 `set_items`）。可能同属 G1 的「不可见/滚动偏移下的虚拟列表」家族。
- 建议：P2 引擎批（E3d/E9d 一起）或 P2-4c3 kit 修补（`ItemPanel::sync` 在 `overlay.is_open && 首次可见` 时重发 `set_items` + 强制 rewindow）；补真窗口/离屏回归（现有 kit 测试用 TestApp，未覆盖 overlay 首帧不可见路径）。
- 证据：`.review/mig-p2-5-gallery-win-menu.png` / `-win-context.png` / `-win-log2.png`；`.review/mig-p2-5-overlay-gap-excerpt.log`。

### G3（P2，kit 契约）主题切换只覆盖 `bind_style` 组件；挂载期常量颜色的组件在浅色主题不可读

**P2-6b：已修复**（详见 §9）。所有 kit 组件的颜色读取改走 `ThemeHandle` 信号依赖：
input（底色/边框/文字/占位符走 `bind_style`，caret/选区在 `sync` 里按当前外观重取）、
markdown（`MarkdownNode` 在外观变化时按新主题重建子树）、icon（`IconSpec::build_themed`
的 `bind_style.foreground` = E7 调制通道）、tree/list 行（原本已动态；补测试守护）、
color_picker（原本已跟随：`appearance` 镜像 + 面板/画布重算）。gallery 增加 `--theme-check`
（深↔浅往返 + 逐组件颜色断言）与 `--start-light`（浅色启动截图）。

现象（P2-5 记录，保留作历史）：浅色主题（`-win-theme.png`）下：图标仍按深色 `FgMuted` 调制（几乎不可见）、输入框底色/文字仍是深色主题值、markdown 全部文本仍是深色主题色（标题几乎不可见）、tree/list 行文字与品牌 logo 也有同类问题；而 badge/button/toggle/tab_bar/浮层壳（bind_style）能跟随。gallery 的缓解：品牌 logo 放在固定深色底板上；输入/markdown 不做缓解（作为证据）。

- 修复后 gallery 增加端到端断言（`--theme-check`，三端实跑 PASS）：输入框底色/边框/文字/占位符/禁用底色、图标调制色、markdown 首个文本的 `TextStyle.color`、列表行前景、树行（折叠标 `FgMuted` + 标签 `FgDefault` + 选中底色 `Selection`）在深↔浅两个外观下都与 `theme_for(appearance)` 的角色色一致。
- 证据：`E:\kytrix\.review\mig-p2-6b-gallery-*.log`、`mig-p2-6b-win-light-*.png`（浅色顶栏/输入/markdown）。

### G4（P2，引擎/E10）一次性定时器回调写 Signal 不产生重绘帧 —— **由 P2-6a 处理（本批不重复）**

- 现象：`--auto-demo` 在 `App::after` 回调里 `dispatch_action`（handler 内 `log_line` 写 `Signal<Vec<SmolStr>>`），`frame_index` 前后均为 2，600 ms 后仍为 2；3 s idle 窗口 0 帧，日志区未更新。真实点击路径（同一 handler）则正常出帧。
- 初判：timer 回调里的信号失效只置脏/请求帧位，但泵在该轮不再进入应用轮/帧（E10 的唤醒路径没有把 `request_frame` 转成下一轮应用轮）；或 timer 回调在泵尾运行、唤醒位被同一轮消费。
- 建议：引擎 E10c2 批（timer 回调后的帧请求要能落到下一轮 pump）；gallery 已把 `--auto-demo` 只作为动作路径证据，不依赖其可见性。
- 证据：`.review/mig-p2-5-gallery-win.log`（`auto-demo dispatch fired=true frame_index 2 -> 2` / `probe frame_index=2`）；mac/linux 同现象（mac 2→2、linux 1→1 + 2 个既有启动轮）。

### G5（P2，kit）`ColorPicker` 触发器命中但不出现面板（待复核）—— **P2-6a 复核：面板其实正常出现；P2-6c 补可观测 API（§11.2）**

- 现象：真实点击 ⑭ 色块（日志 `pointer down (51,473) -> gallery.color-picker.trigger`）后无面板、无 `color-picker` 日志（该组件没有 open 回调）；`is_open` 未在外部暴露打印。可能与 G2 同源（overlay 首帧不可见 → 内容不绘制），但连面板壳也不可见，需 P2-4d2b 复核。
- 建议：P2-4d2b 复核（低风险先查 `ColorPicker::build` 的 trigger action 注册 + overlay `open` 返回值），并给 ColorPicker 补 `on_open/on_close` 回调或 `open_state` 文档。

### G6（P3，kit）`Spinner` 无可运行时启停 API —— **颜色部分已由 P2-6c 处理（§11.3）；运行时启停仍登记 P3**

- 只有 `Spinner::spinning(bool)` 构造器 + `is_spinning()`；无 `set_spinning`。gallery 为满足 I3（空闲 3 s 无持续帧）只能以 `spinning(false)` **静态**展示（看不到旋转）。建议 P3 补 `Spinner::set_spinning(&self, app, bool)`（含释放动画槽），gallery 再把它接到开关上。

### G7（P3，资产/主题）品牌 logo 只适配深色背景

**P2-6b：登记，不做代码修复**（详见 §9.2）。核对旧实现 `crates/base_ui/src/brand.rs`（迁移前）：
`Brand` 就是 `img(path)`（`gpui_kit::Img`，无 `image_modulation` / 无按外观选资产 / 无背景板）——
**旧实现同样没有色调或背景适配，因此不是迁移回归**。7 个 PNG 都是按深色背景设计的彩色资产
（像素统计：Longbridge 86% 深色、OKX 71% 深色 + 26% 纯白、Hyperliquid 平均亮度 229），
kit 侧现有能力（E7 `image_modulation` 是乘性调制）无法在不破坏品牌色的前提下做「浅色变体」。

- 处置：**资产批次**补浅色背景变体（或产品只在深色表面使用品牌 logo）才是正确修法；
  `brand` 的主题独立性由 `kit::brand::tests::a_brand_logo_is_theme_independent` 固定
  （外观切换不失效品牌节点、调制色恒为 `Color::WHITE`；变异「给品牌加主题调制」⇒ 用例红）。
- 示例侧：gallery 按产品用法给一块**固定深色**底板承载 7 个 logo（不跟随主题——跟随会破坏对比度），
  两种外观下都可辨认（`mig-p2-6b-win-light-top.png`）。

### G8（P4，示例约束）Esc 关窗与浮层关闭的次序在真实泵序下不稳定

- 首版 gallery 用「Esc：有浮层先关浮层、无浮层关窗」，在对话框刚好关闭的同帧会误关整个窗口（两次实测）。最终示例**取消 Esc 关窗**（Esc 只交给引擎关浮层，窗口用系统关闭按钮 / `--auto-close` 退出），并在标题状态说明。建议：若产品需要 Esc 关窗，应由组合根维护「上一帧浮层集合」快照（本批尝试过 `App::overlays` 镜像仍不稳），或引擎提供「Esc 是否已被消费」的查询面。

### G9（P4，表格）跨列后的剩余列宽分配留下大块空白

- ⑧ 的 5 列中「标签」`span(2)` 覆盖最后一列；实际表头 `描述 flex(2)` 与右侧「标签」之间出现约 500 px 空白，`标签` 被推到最右。可能与「跨列宽度 = 被跨列宽度之和」+ taffy flex 分配有关，视觉可接受但不像旧表。建议 P2-2 表格批复核（或 P3 视觉基线批）。

### G10（P4，引擎 API）缺 `Window::node_screen_rect`

- 外部驱动点击需要「节点当前屏幕矩形」，公开面只有 `Node::layout().bounds`（内容坐标），要自己减祖先 scroll 偏移（`anchor_rect` 已登记同样需求）。本批 gallery 的 `--probe` 用 layout bounds + `ScrollState.offset` 换算；建议引擎补 `Window::node_screen_rect`（与 P2-4b R? 登记合并）。

## 5. 其他记录

- **`KYTRIX_UI_CORE_MANIFEST=0` 仍是 Windows 链接 example/test 的前置**（gpui-pre 与 ui_core 双 `RT_MANIFEST ID 1`，`CVT1100`）。示例文档与脚本已写明；P5 删 `gpui-kit` 后连同开关一起删除（见 `spec-deltas/build-transition.md`）。
- **Linux 无 sudo 的构建前置**：gpui-pre 需要 `libxkbcommon-x11` / `libxcb*` 开发符号，机器只有 `.so.N`。验收脚本在 `$HOME/p2-5-libs` 建 `.so → .so.N` 符号链接并设 `LIBRARY_PATH`（不改系统）；P5 删 gpui-kit 后不再需要。
- **截图能力**：Windows 完整；macOS 受 TCC（Screen Recording 未授权，壁纸/`could not create image from window`）；Linux 受 mutter 未实现 `wlr-screencopy-unstable-v1` + GNOME Shell Screenshot `AccessDenied`。这不是示例缺陷，登记为环境限制；真实窗口存在性由平台日志（mac `CGWindowListCopyWindowInfo` owner=gallery、Linux Vulkan swapchain extent=(1200,800)）与退出码/帧计数证明。
- **I3 口径**：三端都按「1 s 基线 → 3 s 帧增量」实测；Windows/macOS 0 帧，Linux 2 帧（既有启动补轮，与 E10c 基线一致）。Spinner 静态化是 gallery 为满足该指标做的取舍（G6）。
- **test-ledger**：本批不新增自动化测试（示例 target 不进 lib 测试），旧 gpui 测试登记不变。

## 6. 变更清单（本批代码）

| 文件 | 变更 |
|---|---|
| `crates/base_ui/examples/gallery.rs` | 新增（单文件画廊 + 验收钩子 `--auto-demo` / `--idle-check` / `--probe` / `--scroll-to=`）。不引用旧顶层模块（不用 `crate::style` / `crate::icon` 等） |
| `crates/base_ui/src/kit/mod.rs` | 文档补「gallery 示例」一段（指向本 spec-delta 与三端证据目录） |
| `.docs/gui-migration/spec-deltas/kit-gallery.md` | 本文件（gitignored 台账） |

## 7. 门禁（实跑，head `cd607d0`）

| # | 门禁 | 结果 | 日志 |
|---|---|---|---|
| 1 | `cargo +nightly fmt --check -p base_ui`（含 example target） | ✅ exit 0 | `.review/mig-p2-5-g1-fmt.log` |
| 2 | `cargo +nightly clippy -p base_ui --all-targets -- -D warnings`（`KYTRIX_UI_CORE_MANIFEST=0`） | ✅ exit 0（修掉 29 条 in-example clippy：redundant locals / braces / clone、`is_multiple_of`） | `.review/mig-p2-5-g2-clippy.log` |
| 3 | `cargo test -p base_ui --lib`（env=0） | ✅ 1099 passed / 0 failed | `.review/mig-p2-5-g3-base_ui.log` |
| 4 | `cargo test -p ui_core --features hud,test-support --lib`（清除 env） | ✅ 2250 passed / 0 failed / 1 ignored | `.review/mig-p2-5-g4-ui_core.log` |
| 5 | `cargo xtask gates --platform all --step quick --scope ui_core,base_ui` | ✅ PASS（mac 2/2、linux 2/2，4 commands，32.5 s，head `cd607d0`） | `.review/mig-p2-5-g5-quick.log` |
| 6 | 三端真实窗口运行 | ✅ Windows / macOS / Linux 均 exit 0 + I3 PASS（截图限制见 §2/§5） | `mig-p2-5-gallery-{win,mac,linux}.log` + `-win-*.png` |

## 8. 遗留 / 需裁决

1. **G1/G2 是真实窗口首用暴露的引擎级 P1**：修复前 kit 的「页面内嵌列表/表格/树 + 页面滚动」「菜单/下拉/选择面板」在真实应用不可用（TestApp 路径未覆盖）。建议 Lead 立即排 P2 引擎批（E3d/E9d + E10c2 合并一批「真实窗口组合路径」），并考虑给 kit 补一个 `gallery-smoke`（三端 quick / 离屏像素）门禁。
2. **G3 主题切换**：是否要求 P2 结束前所有 kit 组件跟随主题？当前 badge/button/toggle 等已跟随，input/markdown/icon/brand 未跟随。若产品 P3 才用浅色，可登记到 P3。
3. **G4 timer→frame**：影响 tooltip 延迟显示/浮层淡出收尾的真实可见性（P2-4b3 的测试是 TestApp 路径）；建议与 G1/G2 同批。
4. **macOS/Linux 截图**：若验收必须有截图，需要 Lead 在 mac GUI 上手动授予 Screen Recording（或提供带 TCC 权限的执行通道）；Linux 需换支持 wlr-screencopy 的合成器或安装 `gnome-screenshot`（当前无 sudo）。
5. **gallery 归位**：本示例是 P2-5 验收资产；P5 删旧模块后可继续作为 kit 回归示例保留（建议同时拆出 `--snapshot` 离屏模式进三端门禁）。

## 9. P2-6b 收尾：运行时主题响应性（G3 修复）/ 品牌适配（G7 登记）/ `--theme-check`

> 工作树 `E:\kytrix\.agents\worktrees\p2-6b`，分支 `mig/p2-6b`（`git merge mig/p2-5` 后 head `55ee31c`）。
> 写作用域：`crates/base_ui/src/kit/**`、`crates/base_ui/src/theme/handle.rs`、`crates/base_ui/examples/gallery.rs`；未改 `ui_core` 产品代码。

### 9.1 G3 修法（按组件）

主题信号是 `ThemeHandle` 内部的 `Signal<Appearance>`；`ReadCtx` 里读它即登记依赖，写它只失效读过它的节点（既有语义，见 `theme/handle.rs` 的 I6/I17 用例）。三条落点：

| 组件 | 修法 | 文件 |
|---|---|---|
| `kit::input` | `build` 改为接收 `&ThemeHandle`；底色/边框/文字/占位符/焦点/错误边框全部移进 `bind_style`（首帧样式 pass 求值早于绘制）；`TextStyle` 不再写颜色（`bind_style.foreground` 覆盖）；caret/选区画布颜色在 `sync` 里按 `ThemeHandle::current(app)` 重取（`caret` 节点绑一条只读外观的空补丁，保证「写外观 → 标脏 → 出一帧 → sync 重画」） | `kit/input.rs` |
| `kit::markdown::view` | 新增 `MarkdownNode`：`mount` 按当前外观建树；根节点绑只读外观依赖；`sync(app)` 在外观变化时**重建子树**（逐 run 颜色没有动态通道，`StylePatch::foreground` 会压成单色）。`MarkdownView` 保留为静态用法 | `kit/markdown/view.rs` |
| `kit::icon` | 新增 `IconSpec::build_themed(app, window, parent, &ThemeHandle, Role)`：`bind_style.foreground` = E7 图片调制通道，外观切换只失效图标节点（`IconSpec::build` 的固定色路径不变） | `kit/icon.rs` |
| `kit::tree` / `kit::list` | 原本已是 `ThemeHandle` + `bind_style`（行文字/折叠标/选中底色），本批补主题切换测试守护 | `kit/tree.rs`、`kit/list.rs` |
| `kit::color_picker` | 原本已跟随（`appearance` 镜像 + 面板/画布重算）；`build_panel` 的 `kit_input::build` 改用 `ThemeHandle`（随本批签名） | `kit/color_picker.rs` |
| `kit::tooltip` / 其它 | 静态初值只作首帧占位，绘制由 `bind_style` 覆盖（既有形态，无需改） | — |

`ThemeHandle` 新增 `current(&self, app: &App) -> Appearance`（**不登记依赖**：帧外 `sync` 的外观判据）并补 `Debug`（`InputNode` 等句柄的 `Debug` 需要）。

gallery 收尾：
- ① 图标改 `build_themed(.., Role::FgMuted)`；
- ⑪ markdown 改 `MarkdownNode::mount` 并在帧钩子里 `sync`（重建后 `link_at` 用新的 `handles()`）；
- ⑥ 列表行 / ⑧ 表格单元格 / ⑦ 滚动演示行 / ⑮ 日志 / ⑫ Overlay 面板 / ⑬ 对话框正文的静态文字色改 `bind_style`（或复用 `themed_text` 辅助）；
- 新增验收钩子 `--theme-check`（深↔浅往返 + 逐组件颜色断言，PASS 才退出 0）与 `--start-light`（浅色启动，截图用）。

### 9.2 G7 处置（登记，不改代码）

详见 §4 G7。结论：**不是迁移回归**（旧 `crate::brand` 同为 `img(path)`、不调制），修正需要**资产批次补浅色变体**；kit 侧把该决策固定成回归用例 `a_brand_logo_is_theme_independent`（外观切换不失效品牌节点、调制色恒白）。示例用固定深色底板承载（不跟随主题）。

### 9.3 新增测试（6 条 lib 测试 + 1 条 gallery 端到端）

| 测试 | 断言 |
|---|---|
| `kit::input::tests::switching_appearance_repaints_every_input_color` | 浅/深两次：底色/边框/文字/占位符 = 对应外观配方；四个主题读者被失效、静态兄弟**不**被失效 |
| `kit::input::tests::switching_appearance_repaints_the_caret_and_selection_canvas` | 焦点 + 选区下切外观：画布首个 Quad（选区）与末个 Quad（caret）换新外观色 |
| `kit::markdown::view::tests::switching_appearance_rebuilds_the_markdown_with_the_new_theme` | 切外观后子树文本色 = 新主题 `FgDefault`；同外观 `sync` 为 no-op |
| `kit::icon::tests::a_themed_icon_follows_the_appearance_and_a_static_icon_does_not` | 主题图标调制色跟随、静态图标不失效且颜色不变 |
| `kit::tree::ui_tests::switching_appearance_repaints_tree_row_text_and_selection` | 行的折叠标/标签/选中底色跟随；静态兄弟不被失效 |
| `kit::brand::tests::a_brand_logo_is_theme_independent` | G7 决策守护：品牌节点不读主题、调制色恒白 |
| gallery `--theme-check`（真实窗口，三端） | 输入框（含禁用态）/图标/markdown/列表行/树行在深↔浅两个外观下与主题配方一致 |

变异自检（每项实跑后还原，日志 `mig-p2-6b-mut-*`）：
1. **主题切换不重绘**（`set_appearance` 不写信号）⇒ input×2 / markdown / icon / tree 用例红 + gallery `--theme-check` exit 3（12 failures）；
2. **未读主题节点被误失效**（`App::invalidate` 把全窗口所有节点标脏）⇒ input（静态兄弟）/icon（静态图标）/tree（静态兄弟）/brand 用例红；
3. **品牌适配回退**（`Brand` 调制色不再是 `Color::WHITE`）⇒ brand 回归用例红（「品牌图调制色恒为白」）。

### 9.4 门禁与证据（head `57b2e14`）

| # | 门禁 | 结果 | 日志 |
|---|---|---|---|
| 1 | `cargo +nightly fmt --check -p base_ui` | ✅ exit 0 | `mig-p2-6b-g1-fmt.log` |
| 2 | `cargo +nightly clippy -p base_ui --all-targets -- -D warnings`（env=0） | ✅ exit 0 | `mig-p2-6b-g2-clippy.log` |
| 3 | `cargo test -p base_ui --lib`（env=0） | ✅ **1105 passed** / 0 failed（P2-5 基线 1099 + 本批 6） | `mig-p2-6b-g3-base_ui.log` |
| 4 | `cargo test -p ui_core --features hud,test-support --lib`（清除 env） | ✅ **2274 passed** / 0 failed / 1 ignored | `mig-p2-6b-g4-ui_core.log` |
| 5 | `cargo xtask gates --platform all --step quick --scope ui_core,base_ui` | ✅ PASS（mac 2/2、linux 2/2，4 commands，74.5 s，head `57b2e14`） | `mig-p2-6b-g5-quick.log` |
| 6 | gallery 三端真实窗口（`--theme-check` ×2 + `--idle-check`） | ✅ Windows/macOS/Linux 全 PASS：深↔浅双向 `theme-check PASS (0 failures)`；`idle-check PASS (0 frame(s))` | `mig-p2-6b-gallery-{win,mac,linux}-*.log` |

- **Windows 本机**：`--theme-check` 深→浅→深与浅→深→浅都 PASS（exit 0）；`--idle-check` 0 帧；浅色截图 `mig-p2-6b-win-light-top.png`（图标调制/品牌底板）、`mig-p2-6b-win-light-markdown-input.png`（输入框白底深字、markdown 深字/浅灰代码块，均可读）、`mig-p2-6b-win-{light,dark}-top.png`（切换对照）。
- **macOS 15.8.3（launchctl asuser 501）**：head `57b2e14`，build 52 s，theme-check 两种起始外观各 exit 0，idle-check exit 0 / 0 帧；截图仍受 TCC 限制（6255224 B 壁纸，非示例缺陷）。
- **Linux（GNOME Wayland + llvmpipe）**：head `57b2e14`，build 37 s，theme-check 两种起始外观各 exit 0，idle-check exit 0 / 0 帧；grim 仍报 `compositor doesn't support wlr-screencopy-unstable-v1`（环境限制）。
- **冷启动竞态（Linux 首跑暴露，已修）**：`--theme-check` 首个检查点（900 ms）在软渲染冷启动时可能早于首帧（样式补丁尚未求值 → 11 项 `actual None` 误报）。修法：检查前要求 `Window::frame_index() >= 1`，未就绪则 200 ms 重试（上限 20 次）；切换外观后的复查等待从 200 ms 提到 400 ms。修复后 Linux 首跑 PASS。

**残余/说明**：① 浅色截图里 ⑥⑦⑧⑨ 的列表/表格/树行仍空白 —— G1（引擎）**已由 P2-6a 在 `mig/p2-6a` 修复**（§10.1），本分支未合并 `mig/p2-6a` 前仍可见；与本批颜色无关（`--theme-check` 直接断言这些节点的样式补丁，PASS）；② gallery 的 spinner 颜色仍是固定 `BrandBase`（G6 无运行时颜色 API，视觉影响可忽略）；③ `--start-light` 仅示例验收用，产品外观选择仍由组合根决定；④ G2 的根因被 P2-6a 定位到 **kit `list.rs`**（§10.2，一行 `compute_layout` 修复提案）——属于本批写作用域，但为避免与 P2-6a 并发改同一文件、且不扩本批交付边界，**未在 P2-6b 落地**，交 Lead 排期（建议 P2-6c 或 P2-6a 合并后由 kit 侧一行修）。

## 10. P2-6a 真实窗口组合路径（G1 修复 / G2 定位转 P2-6b / G4 验收口径修正 / G5 复核）

> 工作树 `E:\kytrix\.agents\worktrees\p2-6a`，分支 `mig/p2-6a`（`git merge mig/p2-5` 后开工）。
> 写作用域：`crates/ui_core/src/**`（滚动/绘制路径）、`crates/ui_core/src/app/interaction_tests.rs`、`crates/base_ui/examples/gallery.rs`（探针）、本文件。**未改 kit 生产代码**。
> 证据目录：`E:\kytrix\.review\mig-p2-6a-*`（Windows 截图 / 日志、macOS 日志）；探针：`--dump-overlay`。

### 10.1 G1（已修，ui_core）：纯滚动快路径把嵌套滚动子树留在旧坐标

- **根因**（代码推导 + TestApp 复现）：`window/paint.rs` 的「纯滚动帧快路径」（`refresh_scroll_offsets` → `sync_scroll_offsets`）在「只有偏移变化、没有节点脏」的帧里**只把偏移刷进槽位表、不遍历树**。但滚动容器子树里 lane 的烘焙坐标是 `bake = total − 本槽偏移`：**内层滚动槽的后代** `bake` 里含**外层**位移，槽位的屏幕裁剪区（GPU scissor）也随外层移动。两者都只有遍历才能更新 ⇒ 页面一滚，⑥⑦⑧⑨ 的内层内容按旧坐标烘焙、又被**首帧的空裁剪区**剪掉：整块空白（`mounted_rows` 一直正常）。重排也不恢复：布局后的帧是增量路径，未访问的内层容器仍不刷新槽位。
- **修法**（3 处，`paint/pass.rs` 不动）：
  1. `window/paint.rs`：`sync_scroll_offsets` 先只读找出「本帧偏移真的变了」的容器；任一变化容器的子树里还有**别的滚动槽**（`has_nested_scroll_slot`，用槽位表 `scroll_slot_nodes` + 祖先判定）就返回 `false` ⇒ 调用方退回**完整遍历**（全量路径重建 lane 与槽位表）。单层滚动仍走快路径（既有 AR4 P1-3 用例逐字节不变）。
  2. `window/paint.rs`：新增 `has_nested_scroll_slot(window, container)`（槽位表是「本窗口已登记滚动槽」的全集，比 `NodeTree::scroll_containers` 更准：虚拟列表容器不在后者里）。
  3. `node/tree.rs`：`NodeTree::is_ancestor` 由私有改 `pub(crate)`（唯一新消费者是上面的判定），带文档说明。
- **测试**（`app/interaction_tests.rs`，TestApp 复现真实窗口组合）：
  - `a_nested_scroll_container_follows_the_outer_container_scroll`：外层 scroll 内嵌内层 scroll + 行；断言内层槽位裁剪区 = `(0,0,200,80)`、行烘焙坐标 = `(0,−20,200,40)`；
  - `a_virtual_list_inside_a_page_scroll_stays_visible_after_scrolling_the_page`：外层页面 scroll 内嵌**虚拟列表**（gallery ⑥⑦⑧⑨ 的真实形态）：断言行不丢、槽位裁剪区随页面滚动重算、行烘焙坐标含页面位移。
  - **变异自检**（`mig-p2-6a-mutation.log`）：还原 `window/paint.rs` 修改后两条用例均红（`left: PhysicalRect { x0:0, y0:300, x1:200, y1:300 }` vs `(0,0,200,80)`）；恢复后绿，且 `a_one_pixel_scroll_frame_rebuilds_nothing_and_damages_only_the_viewport`（单层快路径守护）保持绿。
- **真实窗口证据**（Windows 本机，DPI 1.25，`--scroll-to=1400` 程序化滚动后截图）：`mig-p2-6a-g1-lists-after-scroll.png`（⑦ 自绘滚动条 `滚动行 #0..#5`、⑧ 表格 8 行 × 4 列、⑨ 树 `src/main.rs/kit/list.rs` 全部绘制）；`mig-p2-6a-g1b-bottom-after-scroll.png`（页底 ⑫⑬⑭⑮）。

### 10.2 G2（引擎侧无缺陷，根因在 kit ⇒ **P2-6b 修**）

- **真实窗口证据**（`--dump-overlay`，`mig-p2-6a-overlay-win.log` + `mig-p2-6a-g2-menu-shell.png`）：`menu.open -> Ok(true)`；面板壳尺寸正确（`gallery.menu:panel:surface rect=(451.2,202.4 192.0x151.2)`），但
  `gallery.menu:panel:list:viewport children=0 scroll=offset=0.0 content=192.0x0.0 viewport=192.0x151.2`，滚动条滑块 `4.0x0.0` ⇒ **内容高度 0 = 行高全部为 0**，不是「浮层未挂载/不绘制」。
- **根因链**（kit `crates/base_ui/src/kit/list.rs`）：
  1. `ItemPanel::open` 先 `list.attach`（`menu.rs:645`）再 `list.set_items`（`menu.rs:658`）；`attach` 里唯一一次布局（`populate_first_frame` → `compute_layout`，`list.rs:1382-1395`）发生在 `count == 0` 时；
  2. `set_items` → `rebuild`（`list.rs:1330-1350`）挂行容器后**同步建行内容**（`fill_row_contents`），但之后**不再跑布局**；
  3. 真实泵序是「钩子（事件派发 + 各组件 `sync`）→ 帧」（`pump.rs` ④），而 TestApp 都是「帧（含布局）→ sync」（kit 测试的 `drain` 顺序）——于是真实窗口里 `List::sync` 的首轮测量（`list.rs:1066-1102`）读到**从未布局**的内容节点 `layout().bounds.height == 0`，`set_virtual_extents` 写入**全零高度**；
  4. 引擎按契约处理：`visible_range_extents` 全零内容 ⇒ `0..0`（`interaction/virtual_list.rs:59-61`）⇒ `rewindow_virtual` 回收全部行（`app/virtual_list.rs:253-286`）；此后没有行可测、`ensure_heights` 只在 count 变化时补 ⇒ **永久空**（gallery 页面 ⑥ 的 List 只因后续 `mount` 的 `compute_layout` 补过布局而幸免）。
- **为什么 TestApp 绿**：kit 测试都先 drain（帧 → 布局）再 sync（`menu.rs:2467-2477`、`dropdown.rs:1031-1040`、`list.rs:1661-1667`），测量时布局已就绪。
- **建议补丁（P2-6b，kit 侧 1–2 行级）**：`List::rebuild` 里 `fill_row_contents` 之后**先 settle 布局**（`Window::compute_layout`，`populate_first_frame` 已有同款用法）再让 sync 测量；或把 `rebuild` 新挂/新建的行标记为「本帧已 populate，测量跳过一次」（`sync` 已有 `populated_now` 机制，`list.rs:1076`）。可选引擎侧硬化（不建议）：全零 extents 视作「未测量」自愈会破坏 F1「零高行折叠」契约（`app/virtual_list.rs:901-927`）。
- **本批未改 kit**：登记给 P2-6b；验收口径以 `--dump-overlay` 的 `content.height > 0` + `viewport children > 0` 为准。

### 10.3 G4（引擎无缺陷 → **验收探针口径错**，gallery 已修）

- **追踪事实**（临时帧追踪，已清理）：`App::after` 回调里打开浮层 → `request_frame` → `Dispatcher::wake` → 泵 ④ 钩子之后 `take_frame_wake()` 为 true → `frame_all` → **`FrameIndex` 连续提交 3→13**（含浮层淡入动画）；定时器/泵/失效链路本身完全正常。
- **原「2→2」的真正原因**：`--auto-demo` 唯一可见效果是第 ⑮ 分区的日志文本（内容坐标 y≈4500，页在顶部时**在视口外**）⇒ damage 与 surface 求交为空 ⇒ `plan() == Plan::Idle` ⇒ `frame_reaches_gpu == false` ⇒ `PublishDecision::NoGpuWork`（`window/frame.rs:215-238`）⇒ 结算但不递增 `FrameIndex`、不 present（I3 / V-122⑧ 的既定行为）。TestApp 无 renderer 时该门恒放行，所以单测绿。
- **gallery 修法**：`--auto-demo` 同时写**屏内** `status`（顶栏 `bind_text`，追加 ` · auto-demo ✓`），帧号断言才有意义；模块文档写明口径。真实窗口（`mig-p2-6a-g4-win.log`）：`auto-demo dispatch fired=true frame_index 2 -> 2`（同帧内）→ **`600 ms after dispatch frame_index=3`**，`--idle-check` 仍 `PASS`（3 s 1 帧）。
- **残余**：若产品要求「写信号必须出帧」覆盖屏外节点，需要改 `NoGpuWork` 策略（会破坏 Linux 的 present 门禁），不建议；应用侧应让可见内容变化绑定屏内节点。

### 10.4 G5（复核结论：**不可复现**，面板正常出现）

- P2-5 的同一份证据里，真实点击 `gallery.color-picker.trigger` 后的截图 `mig-p2-5-gallery-win-picker.png` **显示色板面板已打开**（Palette/HSLA 标签、12×12 色板、HEX 输入、OK 按钮齐全）；P2-5 报告「无面板」来自「没有 open 日志」的推断（该组件本就无 `on_open` 回调），与同批截图矛盾。
- ColorPicker 用的是 `Overlay` + 手工 `build_panel`（**非虚拟列表**），G2 的全零 extents 根因不适用。
- 本轮无法再取真实点击证据：Windows 会话的 OS 输入注入在本轮不可用（`SetCursorPos`/`mouse_event` 后应用收不到 pointer 事件，`--dump-overlay` 日志里 `is_open` 恒 false；同脚本在 P2-5 环境可用，属环境限制，与代码无关）。
- **遗留（P3，kit）**：给 ColorPicker 补 `on_open/on_close` 或公开 `open_state`（与其 `is_open` 已有，扩一个 `open_state() -> Signal<bool>` 即可），让验收不靠截图判读。

### 10.5 本批未修 / 登记的残余

1. **screen-baked 内容在外层滚动时仍会停在旧坐标**（代码推导，gallery 未暴露）：`Geometry`/`Canvas` 节点的 lane 恒属槽位 0、`bake = total`（`paint/pass.rs` 的 `screen_baked` 分支），祖先滚动必须重建其 lane；而纯滚动快路径不遍历 ⇒ 页面里的图表类内容会「冻结」。建议 P3：给滚动槽登记「子树含 screen-baked 节点」标记，纳入 `has_nested_scroll_slot` 同款判据（或届时实现「槽位链」合成）。
2. **嵌套滚动的页面滚动帧现在走完整遍历**（正确优先）：可后续按「变化容器 → 受影响槽位子树」精确标脏，避免整窗访问。
3. **G2 关闭依赖 P2-6b 的 kit 补丁**（§10.2），gallery 的 ⑫⑭ 验收在其落地前仍为「面板壳 + 无行」。

### 10.6 门禁（实跑）

| # | 门禁 | 结果 | 日志 |
|---|---|---|---|
| 1 | `cargo +nightly fmt -p ui_core -p base_ui -- --check` | ✅ exit 0 | `.review/mig-p2-6a-g1-fmt.log` |
| 2 | `cargo +nightly clippy -p ui_core --all-targets --features hud,test-support -- -D warnings` | ✅ exit 0（**必须 +nightly**：默认 stable 1.97 的 clippy 无 `definition_in_module_root`、却新增 `inline_modules`，全仓 154 处既有 `mod tests` 会被判错 —— 工具链漂移，与本次改动无关；`+nightly` = clippy 0.1.99 通过） | `.review/mig-p2-6a-g2-clippy.log` |
| 3 | `cargo +nightly test -p ui_core --features hud,test-support --lib` | ✅ 2276 passed / 0 failed / 1 ignored | `.review/mig-p2-6a-g3-ui_core.log` |
| 4 | `cargo +nightly test -p ui_core --features test-support --test snapshot` | ✅ 11 passed；`git status` 无基线/快照文件变化（零 diff） | `.review/mig-p2-6a-g4-snapshot.log` |
| 5 | `KYTRIX_UI_CORE_MANIFEST=0 cargo +nightly test -p base_ui --lib` | ✅ 1099 passed / 0 failed | `.review/mig-p2-6a-g5-base_ui.log` |
| 6 | `cargo +nightly xtask gates --platform all --step quick --scope ui_core,base_ui` | ✅ PASS（4 commands，67.6 s；mac/linux 各 2，`== head: 162116c`） | `.review/mig-p2-6a-g6-quick.log` + `mig-gates-1795272504-all.log` |
| 7 | gallery 真实窗口（Windows 本机 / macOS 远端） | ✅ Windows：G1 截图（⑥⑦⑧⑨ 全部绘制）+ G2 面板壳/内容高度 0 + G4 帧号 2→3 + I3 PASS；macOS：G4 2→3、G2 同指纹、probe mounted_rows、窗口 1200×800 存在、exit 0（§10.7） | `.review/mig-p2-6a-*` |
| 附 | `cargo +nightly clippy -p base_ui --all-targets -- -D warnings`（`KYTRIX_UI_CORE_MANIFEST=0`，覆盖 gallery example） | ✅ exit 0 | `.review/mig-p2-6a-g2b-clippy-base_ui.log` |

### 10.7 macOS 远端复跑（ssh `my_macos`，scratch `~/kytrix-mig-src-mig-p2-6a` = head `162116c`）

日志：`mig-p2-6a-mac-gallery.log`（构建 + 三段真实窗口运行）、`mig-p2-6a-mac-window.log`（窗口存在性）。
构建：`cargo build -p base_ui --example gallery`（`CARGO_TARGET_DIR=$HOME/kytrix-review-target`）—— 见日志 `build exit=0`。

| 场景 | macOS 结果 |
|---|---|
| `--auto-demo --idle-check`（G4） | `frame_index 2 -> 2`（同帧内）→ **`600 ms after dispatch frame_index=3`**；`idle-check PASS`（3 s 1 帧）；exit 0 —— 与 Windows 完全一致，证明「定时器回调写屏内信号 ⇒ 出帧」不是平台特例 |
| `--dump-overlay`（G2/G5） | `menu.open -> Ok(true)`；`gallery.menu:panel:list:viewport children=0 scroll=offset=0.0 content=192.0x0.0 viewport=192.0x151.5`、滚动条滑块 `4.0x0.0` ⇒ **与 Windows 同一条 kit 根因**（全零行高），跨平台确认 |
| `--probe --auto-close=8`（G1 数据面） | 页滚到底后 `mounted_rows [("list_fixed",8),("list_variable",6),("list_deque",5),("table",8),("tree",9)]`、各分区布局盒正常；exit 0 |
| 窗口存在性 | `CGWindowListCopyWindowInfo(.optionAll)`：`WINDOW owner=gallery id=246 layer=0 bounds={1200×800 @ (80,105)} onscreen=1`；程序 `exit=0` / `event loop exited cleanly` |
| 截图 | ⚠️ 同 P2-5：TCC 未授予 Screen Recording（`screencapture` 只得到壁纸/报错），本批以「窗口存在 + 帧号 + probe 数据」为证据 |

> macOS 的 G1 像素证据无法截取（环境限制）；但 G1 的修复点在 `window/paint.rs`（平台无关的帧分类与槽位表），Windows 截图为像素级证据，macOS 侧以「同一代码路径 + probe 数据 + 退出码 0」承担回归。

## 11. P2-6c kit 收尾：G2 列表 rebuild settle / G5 ColorPicker 可观测 / G6 Spinner 颜色

> 工作树 `E:\kytrix\.agents\worktrees\p2-6c`，分支 `mig/p2-6c`（基线 `gui-engine-migration@810d0e7`，head `9068a27`）。
> 写作用域：`crates/base_ui/src/kit/{list,color_picker,spinner}.rs`、`crates/base_ui/examples/gallery.rs`、相关测试。
> **未改 `ui_core`**（引擎侧契约无缺陷，§10.2 已定位根因在 kit）。
> 证据：`E:\kytrix\.review\mig-p2-6c-*`（门禁 / 变异 / 三端 gallery 日志 / 截图）。

### 11.1 G2（P1）：rebuild / attach 建完行内容后先 settle 布局

- **根因**（P2-6a §10.2 已定位，本批按该结论落地）：真实泵序是「钩子（事件派发 + 各组件
  `sync`）→ 帧（text → layout）」。`List::rebuild`（`set_items` / `set_count` / `push` 的共同
  路径）与 `attach` 的首帧建行在钩子里同步建内容子树，但**建完不跑布局**；下一轮 `sync` 的
  变高实测读到内容节点 `layout().bounds.height == 0`，把全零高度写进 `set_virtual_extents`，
  引擎按「零高条目折叠」契约把可见窗口算成 `0..0` 并回收全部行 —— 之后没有行可测，列表永久空白。
  TestApp 的 kit 测试习惯「先 drain（帧）再 sync」，掩盖了这条路径。
- **修法**（`crates/base_ui/src/kit/list.rs`）：
  1. 新增私有 `settle_layout(app, window)`：`Window::compute_layout`（与
     `populate_first_frame` 的 measure-then-place 同一入口，幂等 + taffy 缓存增量）。
  2. `List::rebuild`：`fill_row_contents` 之后、`row_height.is_variable()` 且本次真的补了行内容
     时 settle。
  3. `List::populate_first_frame`（`attach` 路径）：同样在 `fill_row_contents` 之后 settle。
  固定行高列表不测量内容，不做额外布局（避免给高频 `push` 增加无谓成本）。
- **为什么首轮就测得真实行高**：菜单 / 表格的变高行内容带**显式行高**，settle 后内容容器的
  布局盒立即可测；`sync` 写回的值非零（菜单实测 = 6 行 × 30 = 180，不再是 0），引擎不会回收
  可见窗口。文本行高的变高列表仍按既有语义在后续帧（shape 之后）修正（见 §11.7 残余 2）。
- **回归测试**（`kit::list::ui_tests`，刻意保持真实泵序：**不先 drain 直接 sync**）：
  - `the_first_sync_after_mount_measures_real_row_heights_without_a_frame_first`：`mount` 后
    不 drain 直接 `sync`，断言已挂载行高度 = 内容显式高 + `padding_y × 2`、`state.mounted > 0`、
    高度前缀和 > 0；
  - `the_first_sync_after_a_rebuild_measures_real_row_heights_without_a_frame_first`：`set_items`
    后不 drain 直接 `sync`（菜单 `ItemPanel::open → list.set_items` 的真实路径），同一断言。
- **gallery 验收钩子**：`--dump-overlay` 从「只打印」改成「打印 + 断言 + 退出码」：
  - 第 0/1 轮记录菜单面板 `(viewport children, content height, scrollbar thumb height)`，
    判据 `children > 0 && content > 0 && thumb > 0`；
  - 第 2 轮关菜单、程序化 `ColorPicker::open`，记录 `is_open`（与 `open_state` 信号一致）与
    `…:panel.content` 子节点数；
  - 末轮打印 `overlay-check PASS/FAIL`，`main` 失败时退出码 3（与 theme/idle check 同口径）。
- **三端真实窗口证据（head `9068a27`）**：判据全部 PASS（详见 §11.5）。
  - Windows 本机（DPI 1.25 / D3D12）：`menu=(6, 180.0, 126.4)`、`picker_open=true`、
    `picker_children=4`，exit 0；截图 `.review/mig-p2-6c-win-g2-menu.png`（菜单行
    `编辑 / 复制 Ctrl+C（高亮）/ 粘贴 Ctrl+V / 分隔线 / 删除` + 右侧滑块）与
    `.review/mig-p2-6c-win-g5-picker.png`（Palette/HSLA、12×12 色板、HEX `#4C9AFF`、OK）。
  - macOS 15.8.3（`launchctl asuser 501`）：`menu=(6, 180.0, 127.5)`、picker 同判据，exit 0。
  - Linux（GNOME mutter Wayland / llvmpipe）：`menu=(6, 180.0, 127.5125)`、picker 同判据，exit 0。
- **变异 M1**：把 `settle_layout` 改成 no-op 后 ——
  - 两条真实泵序测试红（第 0 行实测 `4.0` = padding-only，期望 44.0）；
  - gallery `--dump-overlay` `menu=(0, 0.0, 0.0)`、`overlay-check FAIL`、exit 3
    （与 P2-6a §10.2 的空面板指纹逐项一致）。日志：`mig-p2-6c-mut1-tests.log`、
    `mig-p2-6c-mut1-gallery.log`；还原后全绿。

### 11.2 G5（P2）：ColorPicker 打开状态可观测

- 新增（`crates/base_ui/src/kit/color_picker.rs`）：
  - `open_state() -> Signal<bool>`：overlay 的开状态信号（可 `bind_*` 读、验收直接读；与
    `is_open` 同一份真相）；
  - `on_open(callback)`：面板打开时同步回调一次（点击触发器与 `open()` 同一路径；关闭不回调；
    重复注册替换）；
  - `open(app, window) -> bool` / `close(app) -> bool`：程序化开关（`open` 已打开时 no-op，
    返回是否真的打开）——真实窗口验收不依赖 OS 输入注入（macOS/Linux 无人值守环境的确定性证据）。
  - 既有 `is_open(app)` 保留（引擎侧外部点击 / Esc 关闭后到下一次 `sync` 之前仍报 `true` 的语义不变）。
- **测试** `the_open_state_signal_and_on_open_callback_track_the_panel`：初始信号 false；
  点击触发器后 `is_open` / `open_state` 均 true 且 `on_open` 恰一次；`close` 后两者 false 且
  不回调；程序化 `open()` 再触发一次；重复 `open()` 返回 false 且不再回调。
- **gallery**：`--dump-overlay` 用 `open()` + `is_open`/`open_state` 断言面板打开且内容根
  `children=4`（不再靠截图判读）。
- **变异 M3**：`open()` 不建面板（打开状态不更新）⇒ 该测试红（`程序化打开返回 true` 失败）。
  日志 `mig-p2-6c-mut3-tests.log`；还原后绿。

### 11.3 G6（P3）：Spinner 颜色可配置（角色跟随 / 运行时改色）

- 新增（`crates/base_ui/src/kit/spinner.rs`）：
  - `SpinnerColor { Fixed(Color), Role(Role) }`（公开枚举）与 `color(app)` 读当前生效颜色；
  - `with_role(role)`：颜色跟随主题角色 —— 节点登记一条只读外观依赖（空 `StylePatch` 补丁，
    与 `kit::input` 的 caret 同一形态），深浅切换会失效节点并出一帧，`sync` 按新外观重画；
  - `with_color(color)` 保持固定色语义；`set_color(app, color)` 运行时改固定色（清画布缓存 +
    `request_node_repaint`；等值写入 no-op，不标脏不出帧）；
  - `sync` 的画布短路键从 `(bounds, angle)` 扩成 `(bounds, angle, color)`，保证外观变化即使
    角度/几何不变也会重画。
- **测试** `a_role_coloured_spinner_follows_the_appearance_and_set_color_updates_it`：
  `with_role(BrandBase)` 在浅/深两套外观下画出各自 `brand.base`（第 0 个点 alpha 0.15）；
  `set_color(WHITE)` 后画固定白；等值 `set_color` 返回 false。
- **gallery**：Spinner 改用 `with_role(Role::BrandBase)`（不再挂载期固定色），仍
  `spinning(false)` 保 I3（空闲 0 帧，见 §11.5）。
- **变异 M2**：`sync` 忽略 `self.color(app)`（恒用白色）⇒ 角色色测试红（实际白色 vs 期望
  `brand.base`）。日志 `mig-p2-6c-mut2-tests.log`；还原后绿。
- **残余**：运行时**启停** API（`set_spinning`）仍未提供 —— gallery 为满足 I3 保持静态展示，
  登记 P3（与 P2-5 G6 原条目一致）。

### 11.4 测试增减（base_ui lib：1105 → 1109）

| 新增测试 | 断言 |
|---|---|
| `kit::list::ui_tests::the_first_sync_after_mount_measures_real_row_heights_without_a_frame_first` | 真实泵序（mount 后不 drain）：首轮 sync 读到实测行高，可见行不被回收 |
| `kit::list::ui_tests::the_first_sync_after_a_rebuild_measures_real_row_heights_without_a_frame_first` | 真实泵序（set_items 后不 drain）：重建路径同一断言（G2 回归） |
| `kit::color_picker::ui_tests::the_open_state_signal_and_on_open_callback_track_the_panel` | `open_state` 信号 / `on_open` 边沿 / 程序化开关语义（G5） |
| `kit::spinner::tests::a_role_coloured_spinner_follows_the_appearance_and_set_color_updates_it` | 角色色跟随外观 + `set_color` 运行时改色 + 等值 no-op（G6） |

删除测试：无。旧 gpui 测试登记不变（本批没有迁移语义变化，只有新增可观测面/颜色来源）。
测试台账：`.docs/gui-migration/test-ledger/p2-6c.md`。

### 11.5 门禁与三端 gallery（实跑，head `9068a27`）

| # | 门禁 | 结果 | 日志 |
|---|---|---|---|
| 1 | `cargo +nightly fmt --check -p base_ui` | ✅ exit 0 | `mig-p2-6c-g1-fmt.log` |
| 2 | `cargo +nightly clippy -p base_ui --all-targets -- -D warnings`（env=0） | ✅ exit 0 | `mig-p2-6c-g2-clippy.log` |
| 3 | `cargo test -p base_ui --lib`（`KYTRIX_UI_CORE_MANIFEST=0`） | ✅ **1109 passed** / 0 failed（P2-6b 基线 1105 + 本批 4） | `mig-p2-6c-g3-base_ui.log` |
| 4 | `cargo test -p ui_core --features hud,test-support --lib`（清除 env） | ✅ 2276 passed / 0 failed / 1 ignored（未改 ui_core，与 P2-6a 同值） | `mig-p2-6c-g4-ui_core.log` |
| 5 | `cargo +nightly xtask gates --platform all --step quick --scope ui_core,base_ui` | ✅ PASS（4 commands，73.7 s；head `9068a27`；mac/linux 各 2） | `mig-p2-6c-g5-quick.log`、`mig-gates-1795273743-all.log` |

三端真实窗口（`--dump-overlay --auto-close=7` + `--theme-check` ×2 + `--idle-check`）：

| 平台 | overlay-check（G2/G5） | theme-check | idle-check | 退出码 | 日志 |
|---|---|---|---|---|---|
| Windows 11（本机，DPI 1.25 / D3D12） | ✅ `menu=(6, 180.0, 126.4)` `picker_open=true` `picker_children=4` | ✅ 深→浅→深 + 浅→深→浅各 0 failures | ✅ 0 帧（2→2） | 0 | `mig-p2-6c-gallery-win-overlay.log`、`-win-theme-{dark,light}.log`、`-win-idle.log`、`-win-shots.log` |
| macOS 15.8.3（launchctl asuser 501） | ✅ `menu=(6, 180.0, 127.5)`、picker 同判据 | ✅ 双向 0 failures | ✅ 0 帧（2→2） | 0 | `mig-p2-6c-gallery-mac.log` |
| Linux（GNOME Wayland / llvmpipe） | ✅ `menu=(6, 180.0, 127.5125)`、picker 同判据 | ✅ 双向 0 failures | ✅ 0 帧（1→1） | 0 | `mig-p2-6c-gallery-linux.log` |

- 截图：Windows 两张（G2 菜单行 / G5 色板）见 §11.1；macOS/Linux 截图仍受环境限制
  （TCC 未授予 Screen Recording / mutter 无 `wlr-screencopy`），以 `overlay-check` 日志判据 +
  退出码为证据（与 P2-5/P2-6a/P2-6b 同口径）。
- 重复命令：
  ```powershell
  $env:KYTRIX_UI_CORE_MANIFEST='0'; cargo run -p base_ui --example gallery -- --dump-overlay --auto-close=7
  ```
  ```bash
  bash .review/p2-6c-mac-gallery.sh     # 经 ssh my_macos 执行（脚本内容从本地管道传给 bash -s）
  bash .review/p2-6c-linux-gallery.sh   # 经 ssh my_ubuntu 执行
  ```

### 11.6 变异清单（每项实跑后还原）

| # | 变异 | 结果 | 日志 |
|---|---|---|---|
| M1 | `settle_layout` 改成 no-op（去 rebuild/attach settle） | 两条真实泵序测试红（height 4.0 vs 44.0）+ gallery `overlay-check FAIL` `menu=(0,0.0,0.0)` exit 3 | `mig-p2-6c-mut1-tests.log`、`mig-p2-6c-mut1-gallery.log` |
| M2 | `Spinner::sync` 忽略可配置颜色（恒白） | 角色色测试红（白色 vs `brand.base`） | `mig-p2-6c-mut2-tests.log` |
| M3 | `ColorPicker::open` 不建面板（打开状态不更新） | `open_state/on_open` 测试红（程序化打开返回 false） | `mig-p2-6c-mut3-tests.log` |

还原后：两条 G2 测试 + G5 + G6 测试全绿（gate 3 的 1109 passed 即最终态）。

### 11.7 残余 / 说明

1. **菜单变高行仍以 estimate 为行高**：菜单 `ListOptions.row_stretch = true` 使内容容器被拉伸
   到虚拟行高，`sync` 的实测回写恒等于当前 extents（自洽循环）；G2 判据（行窗口非空、内容
   高度 > 0、滑块可见）已满足，视觉正常。若将来需要「菜单按内容自然变高」，需要
   `row_stretch: false` 或测量内层 hit box —— 登记 P3，不是本批引入。
2. **文本行高的变高列表** —— **P2-6d 已修（F2，§11.8）**：settle 发生在 text shape 之前，
   首轮实测只读到 padding-only（约 4 px）。旧实现在此基础上把 padding-only 高度写进
   `set_virtual_extents`，内容高度塌到视口以下，`set_count` 的「保留滚动位置」契约被破坏
   （审查实测 12 行 / 120 px 视口：`offset 80 → 0`，下一帧高度修正但偏移不恢复）。P2-6d 改为
   含文本的行**延后一帧**实测（`pending_text_measure`，帧内 text pass 完成后的 `sync` 才量），
   偏移保持、帧后补齐真实高度。残余：重建 / 挂载后的首个 `sync` 之前，新挂文本行仍以 estimate
   参与前缀和（一帧的 estimate 过渡，既有语义）；若重建后没有提交帧（窗口不可见 / 无像素工作），
   延后的实测等到下一次提交帧 —— 列表不可见时无用户可见影响。
3. **Spinner `Role → Fixed` 后的主题依赖残留**（P2-6d F5 登记，P3）：`set_color` 把角色色切成
   固定色后，节点上那条只读外观依赖（`build` 里的空 `bind_style`）仍在 —— 外观切换仍会失效该
   节点并出一帧（画面不变）。引擎没有「卸载依赖边」的公开入口；如需运行时切角色，应重建节点，
   而不是在旧节点上改状态。同批登记：`with_color` / `with_role` 写共享 `Rc<Inner>` 状态（对克隆
   句柄调用会改到已挂载实例；只应在 `build` 前调用）。
4. **`--dump-overlay` 时间线**：600 ms 滚动 + 900 ms 开菜单 + 5 轮 ×600 ms；与
   `--idle-check` / `--theme-check` 不要同跑（定时器会污染 I3 基线）。慢启动软渲染机（Linux
   llvmpipe）实测正常。
5. **`populate_first_frame` 的 settle 成本**：只在变高列表且本次真的建了行内容时调用一次
   `compute_layout`（全窗口但 taffy 缓存增量）；固定行高列表与无新内容的重建不付这份成本。
6. macOS/Linux 截图限制同 §5（环境限制，非示例缺陷）。

## 11.8 P2-6d 合并后审查收尾（r-p2-6 的 F1–F8 全部关闭）

> 工作树 `E:\kytrix\.agents\worktrees\p2-6c`，分支 `mig/p2-6c`（基线 `9068a27`），追加提交 **`e94f389`**
> （`fix(base_ui): P2-6d 审查收尾（set_count/push 真实泵序、文本行高残余、文档与测试守护）`）。
> 审查报告：`E:\kytrix\.review\mig-p2-6-review.md`（0 High / 2 Medium / 6 Low）。
> 写作用域：`crates/base_ui/src/kit/{list,icon,color_picker,spinner,markdown/view}.rs`、
> `crates/base_ui/examples/gallery.rs`。**未改 `ui_core`**（F6 变异时临时改 `arena.rs`，已还原；
> 最终 diff 只有 base_ui 6 个文件）。
> 证据：`E:\kytrix\.review\mig-p2-6d-{gates.log,mut1-settle-narrow.log,mut2-f2-revert.log,
> mut3-remove-deps.log,mut4-f8-root-identity.log,gallery-win.log}`。

### 11.8.1 逐项处置

| # | 级别 | 处置 | 落点 / 理由 |
|---|---|---|---|
| **F1** | Medium（测试缺口） | **修（补测）** | 新增两条真实泵序用例（`set_count` / `push`，不先 drain 直接 `sync`）：断言首轮实测为真实行高（64/34）且偏移保持/锚点正确。现有 G2 用例只覆盖 `mount`/`set_items`；把 settle 收窄成仅 `set_items` 时新用例红（M1）。 |
| **F2** | Medium（残余后果） | **修（非登记）**：`pending_text_measure` 延后文本行一帧实测 | `rebuild`/`populate_first_frame` 记下建行时的 `FrameIndex`；`sync` 对**含文本**的行只在 `frame_index` 前进（帧内 text pass + layout 完成）后才测量。显式行高的行不延后（G2 依赖首轮可测）。修前实测 `set_count` 偏移 80→0；修后偏移保持、帧后补齐真实高度（新增文本行高用例 + M2 回退必红）。 |
| **F3** | Low（文档不符） | **修（文档 + gallery 去冗余）** | `IconSpec::build_themed` 文档改为「首帧占位色 = 主题角色，静态 `color` 在 themed 路径被忽略」；gallery 删除冗余且被忽略的 `.with_color(icon_color)`。 |
| **F4** | Low（文档不符） | **修（文档）** | `Inner.open_callback` 字段注释与 `ColorPicker::open` 文档统一为「`open_panel` 建好面板后**同步**回调一次（点击/程序化同路径，`was_open` 已置位 ⇒ 下一次 `sync` 无此边沿）」；补「淡出期间 `is_open=true` 无法重开（既有 overlay 模型）」。 |
| **F5** | Low（API 共享状态边角） | **登记（P3）+ 文档写清** | 修需引擎「卸载依赖边 / 重建节点」的公开入口（或改回按句柄字段），属 API 设计批：① `with_color`/`with_role` 写共享 `Rc<Inner>`（克隆句柄会改到已挂载实例，`build` 前调用）；② `Role → Fixed` 后节点仍留外观依赖（切换外观仍会标脏出一帧，画面不变）；③ 无 `set_role` 回切。已写入 `spinner.rs` 方法文档 + 本文件 §11.7-3。 |
| **F6** | Low（不变量未守护） | **修（守护 + 测试）** | `MarkdownNode::sync` 对每个 `NodeTree::remove` 的返回值加 `debug_assert!(removed.is_empty())`（`remove` 只交还**非空**依赖边 ⇒ 返回空 = 子树无 `bind_*`），文档声明不变量；新增测试用 `remove` 的交还面直接断言「子树返回空 + 根节点返回非空（检测面自检）」。删依赖边清理（ui_core 变异）时用例红（M3）。 |
| **F7** | Low（注释事实错） | **修（注释）** | gallery `--auto-demo` 注释改「派发同帧 `2 -> 2`，600 ms 后 `frame_index=3`」，并注明 `3→13` 是 P2-6a §10.3 打开浮层的另一条路径（不混用）。 |
| **F8** | Low（契约测试缺口） | **修（补测）** | 新增测试：可滚动页面里挂 markdown，切换外观重建后断言**根 `NodeId` 不变**、子树是真重建、父节点不变、页面 offset 240 保持；把 `sync` 改成「删根重建新根」时该用例红（M4）。 |

### 11.8.2 新增测试（5 条；base_ui lib 1109 → **1114**）

| 测试 | 断言要点 |
|---|---|
| `kit::list::ui_tests::the_first_sync_after_set_count_measures_real_heights_and_keeps_the_offset` | `set_count` 后不 drain 直接 `sync`：挂载行实测 = 64/34（不是 padding-only 4.0），滚动偏移保持 |
| `kit::list::ui_tests::the_first_sync_after_push_measures_real_heights_and_keeps_the_reader_anchor` | 变高双端列表 `push`（满容量丢最旧）后不 drain 直接 `sync`：挂载行实测 = 数据行高 + padding，读者锚点仍贴顶（offset = `top(1)` > 0） |
| `kit::list::ui_tests::set_count_on_a_text_row_height_list_keeps_the_scroll_offset` | 文本行高列表：`set_count` 后首轮 `sync` 不改已实测高度、偏移保持；帧后补测量仍不移动读者，所有挂载行 > padding-only |
| `kit::markdown::view::tests::the_markdown_subtree_holds_no_signal_dependency_edges` | markdown 子树 `remove` 返回空（无 `bind_*`）；根节点返回非空（外观依赖交还通道自检） |
| `kit::markdown::view::tests::rebuilding_keeps_the_markdown_root_identity_and_the_page_offset` | 外观重建：根 `NodeId` 不变、子树节点真重建、父节点不变、页面偏移 240 保持 |

### 11.8.3 变异（每项实跑后还原；日志 `mig-p2-6d-mut*.log`）

| # | 变异 | 结果 |
|---|---|---|
| M1 | settle 收窄为「仅 `set_items`」（`target_offset == Some(0.0)`） | **F1 两条新用例红**（`set_count`/`push`；push 报实测 4.0 vs 数据行高）；G2 `mount`/`set_items` 两条仍绿；F2 文本用例同因红（该变异同时跳过 pending 登记） |
| M2 | F2 修复回退（`defer_text = false`） | **F2 文本行高用例红**（`[20,20,4,…]` 覆盖已实测值）；F1/G2 显式行高用例全绿 |
| M3 | 删依赖边清理（`NodeTree::remove` 不再交还被删节点依赖边，ui_core 临时变异） | **F6 用例红**（根节点依赖边交还自检失败） |
| M4（额外） | `MarkdownNode::sync` 改为「删根 + 重建新根」 | **F8 用例红**（`NodeId(w1.n2.2) != NodeId(w1.n2.1)`）；F6 保持绿 |

### 11.8.4 门禁（实跑；`CARGO_TARGET_DIR=E:\kytrix\target`、`CARGO_BUILD_JOBS=6`；日志 `mig-p2-6d-gates.log`）

| # | 门禁 | 结果 |
|---|---|---|
| 1 | `cargo +nightly fmt -p ui_core -p base_ui -- --check` | ✅ exit 0 |
| 2 | `KYTRIX_UI_CORE_MANIFEST=0 cargo +nightly clippy -p base_ui --all-targets -- -D warnings` | ✅ exit 0 |
| 3 | `cargo +nightly clippy -p ui_core --all-targets --features hud,test-support -- -D warnings` | ✅ exit 0 |
| 4 | `cargo +nightly test -p ui_core --features hud,test-support --lib` | ✅ **2276 passed / 0 failed / 1 ignored**（未改 ui_core，同 P2-6c） |
| 5 | `cargo +nightly test -p ui_core --features test-support --test snapshot` | ✅ 11 passed |
| 6 | `KYTRIX_UI_CORE_MANIFEST=0 cargo +nightly test -p base_ui --lib` | ✅ **1114 passed / 0 failed**（1109 + 本批 5） |
| 7 | `cargo +nightly xtask gates --platform all --step quick --scope ui_core,base_ui` | ✅ PASS（4 commands，66.8 s；mac/linux 各 2，head `e94f389` 工作树） |

### 11.8.5 Windows gallery 复跑（F2 修复后的真实窗口回归；`mig-p2-6d-gallery-win.log`）

| 场景 | 输出 | exit |
|---|---|---|
| `--probe --auto-close=18` | 顶部/底部均 `mounted_rows [("list_fixed",8),("list_variable",6),("list_deque",5),("table",8),("tree",9)]`；`content_height=3854`、`scroll_offset=3116.4`（F2 修复没有让文本变高列表的可见窗口塌掉） | 0 |
| `--theme-check` / `--start-light` | 深/浅各 `0 failures`，PASS | 0 |
| `--idle-check` | `2 -> 2`（3 s 空闲 0 帧），PASS | 0 |
| `--dump-overlay --auto-close=7` | `menu=(6, 180.0, 126.399994)`、`picker_open=true`、`picker_children=4`，overlay-check PASS | 0 |
| `--auto-demo --auto-close=6` | 派发同帧 `2 -> 2`；600 ms 后 `frame_index=3` | 0 |

- F2 的**真实泵序**证据是 lib 用例（`set_count_on_a_text_row_height_list_keeps_the_scroll_offset`）：
  它精确覆盖「重建发生在帧钩子、下一轮 `sync` 早于帧 layout」的时序；gallery 侧以 `mounted_rows`
  非空 + 三端门禁的 `--probe`/theme/idle/overlay 判据承担回归（gallery 的页面滚动是帧驱动的，
  无法在真实窗口里稳定复现「同一钩子里 set_count 后立刻 sync」的窗口）。
- 三端：本批只在 Windows 本机实跑（合并后审查 §8 已在三端 merged HEAD 复跑过；本批改动不触碰
  平台相关代码，`xtask quick` 已覆盖 mac/linux 的 `cargo check --all-targets`）。

### 11.8.6 残余 / 后续

1. F5 的三条边角（共享 builder 状态 / `Role→Fixed` 依赖残留 / 无 `set_role`）登记 P3；修法需要
   引擎级「卸载依赖边」或重建节点 API，不在收尾批做临时实现。
2. F2 修复后的语义边界：重建/挂载后的首个 `sync` 之前，新挂文本行仍以 estimate 参与前缀和
   （一帧的 estimate 过渡，既有语义）；若重建后没有提交帧（窗口不可见 / 无像素工作），延后实测
   等到下一次提交帧 —— 列表不可见时无用户可见影响。
3. 其余 P2-6c 残余（菜单 `row_stretch` 自洽行高、Spinner 运行时启停、G1 screen-baked、嵌套滚动帧
   成本）状态不变，仍为 P3。
4. macOS/Linux gallery 截图限制同 §5。

## 12. P5 回写：§10 输入演示 / `--probe` 新增输出 / `--idle-check`·`--theme-check` 校验口径

- 背景：`gallery.rs` 在文本输入重构（P0–P4，`.docs/text-input-redesign-design.md` §11）后，第 ⑪ 节的
  输入框改由 `ui_core::text_edit` 绘制与编辑，`ui_kit::kit::input` 只是薄壳。本节记录 P5 收尾时
  该演示与三个检查开关的**最终**口径；§1 的表格与 §11.8.5 的旧数字是 P2 时期快照，不再更新。
- head：`cb965b56` + P5 工作区改动（仅 `gallery.rs` 演示数据 + 文档）。日志
  `.review/text-input/p5-gallery-probe.log`。

### 12.1 第 ⑪ 节（输入框）结构与演示数据

| 控件 | spec | 演示值 |
|---|---|---|
| `gallery.input.single` | 单行 / `ControlSize::Medium` / 包装盒 300×30 | `Kytrix 单行输入 · IME 演示` |
| `gallery.input.masked` | 掩码（`A11yRole::PasswordInput`）/ Medium / 240×30 | `s3cret-密码`（显示为每字素一个 `•`） |
| `gallery.input.disabled` | 禁用 / Medium / 240×30 | 空（不可聚焦） |
| `gallery.input.multiline` | 多行 `visible_lines: Some(3)` / `ControlSize::Small` / 包装盒 620×58 | 6 行，见下 |

多行演示文本（P5 起含 emoji，覆盖 P4 的 gallery emoji 演示要求）：

```text
第一行 🌍 彩色字形
第二行 👨‍👩‍👧 emoji 走 RGBA 图集，整簇可删可移
第三行
第四行
第五行
第六行（可视 3 行：滚轮滚动、caret 移动自动露出、右侧 hover 出现滚动条）
```

- 🌍 与 👨‍👩‍👧（ZWJ 家族）在 Windows 上由 Segoe UI Emoji 的 COLR v0 回退层渲染为**彩色**
  （RGBA 图集 + `KIND_COLOR_GLYPH`，见 `render/shaders/contract.md` §2/§7）；像素级证据是
  `crates/ui_core/tests/golden/warp/color_emoji_1_light.png` 快照用例（Windows 基线）。
- 多行视口 3 行：内容 98 px > viewport 51 px ⇒ 右侧 overlay 滚动条出现（4 px，hover 淡入，
  聚焦时半透明常驻）；`visible_lines` 由引擎 `TextEditMetrics.line_height × n + 2×(border+padding_y)` 得出。

### 12.2 `--probe` 的新增输出（输入框段）

`--probe` 在既有 `content_height` / 各控件 `rect` / `mounted_rows` 之外，新增**引擎侧**的输入框
读面，按阶段打印（`top` → `bottom` → `focused` → `multiline` → `blink`）：

```text
gallery: probe <stage> input<N> root=<rect> editable=<rect> metrics=<TextEditMetrics{line_height,
        first_baseline, line_count, content_height}> caret=<rect> selection=<a..b> composing=<?>
        focused=<?> scroll=<(axis, offset, viewport, content)> scrollbar=<(visible, thumb, ratio)>
        value=<?>
gallery: probe <stage> caret_visible=<bool> next_wake=<Instant>
```

- `root` / `editable` 是控件盒与可编辑文本节点的布局矩形（逻辑像素；底部阶段是同一布局的负 y，
  说明 caret 几何来自引擎布局而不是窗口坐标）。
- `metrics` 直接对应 `App::text_edit_metrics`；空文本也有值（多行 `line_count=6`、
  `content_height=98`）。
- `caret_visible` / `next_wake` 读 `App::text_edit_caret_visible` 与 `App::next_wake_deadline`
  （P3 新增的公开观测面），用于核对「有焦点空闲 ⇒ 每 530 ms 一次翻转帧、无焦点 ⇒ 无唤醒」。
- 实跑（`--probe --auto-close=18`，Windows，exit 0）关键值：
  - `metrics line_height=19.0 first_baseline=15.0 line_count=1 content_height=19.0`（单行 Medium）；
  - `input3 metrics line_height=17.0 line_count=6 content_height=98.0`，
    `scroll=(Vertical, (0,0), 610×51, 610×98)`，`scrollbar=(true, thumb 4×26.54, ratio 0.0)`；
  - 聚焦多行后 `scroll offset=(0,47)`、`scrollbar ratio=0.5`（ensure-visible 生效）；
  - `blink` 阶段 `caret_visible=Some(false)` + `next_wake` 有值（闪烁翻转 + 下一次唤醒已排）。

### 12.3 `--idle-check` / `--theme-check` 的校验口径

| 开关 | 判据 | P5 实测（Windows，head 同上） |
|---|---|---|
| `--idle-check` | 3 s 空闲期间渲染帧数 **≤ 2**（I3 稳态零帧）；先取 baseline `frame_index`，3 s 后再取，差值即空闲帧数 | `baseline frame_index=2` → `3000ms frame_index 2 -> 2`（**0 帧**），`PASS` |
| `--theme-check` | Dark → Light → Dark 三次切换，每次检查一组**颜色补丁差异**（含输入框 root 边框/背景、placeholder 颜色、caret/selection 颜色、文本前景），任何一项沿用旧主题即 failure | `Dark failures=0 / Light failures=0 / Dark-again failures=0`，`PASS` |
| `--probe` | 无 PASS/FAIL 判据，输出用于人工/脚本核对几何；`--auto-close=N` 保证脚本可收尾（exit 0） | exit 0 |
| `--auto-demo` | 派发主按钮 Action 并核对帧游标推进（`--auto-close=6`） | 派发同帧 `2 -> 2`；600 ms 后 `frame_index=3`，exit 0 |

### 12.4 三端 gallery 实跑与前置条件（P5 收尾第二轮）

| 平台 | 前置 | idle-check | theme-check |
|---|---|---|---|
| Windows | 桌面会话 | **PASS（0 帧/3 s）** | **PASS（Dark/Light/Dark 0 failure）** |
| Ubuntu | `XDG_RUNTIME_DIR=/run/user/$(id -u)` + `WAYLAND_DISPLAY=wayland-0`（ssh 默认没有） | **PASS（1 帧/3 s，门槛 ≤2）** | **PASS（0 failure）** |
| macOS | 无需 GUI 环境变量、无需 `timeout(1)`（两者自带定时退出），ssh 会话可直接跑 | **PASS（0 帧/3 s）** | **PASS（0 failure）** |

> 注意：Ubuntu 不设 `WAYLAND_DISPLAY` 时 gallery 以 `CompositorGone` 退出 1（环境问题，不是逻辑失败）；
> macOS 没有 `timeout(1)`，直接前台运行即可（`--idle-check` 3 s / `--theme-check` 约 4 s 后自行退出）。

> 口径说明：`--idle-check` **不聚焦**任何输入框；有焦点时的闪烁节奏（每 530 ms 一帧）由引擎测试
> `a_focused_caret_renders_one_frame_per_half_period` 与 gallery `--probe` 的 `next_wake` 承担，
> 不计入 idle 帧数。

### 12.5 输入框手动验收清单（真实桌面会话；P4-17 的载体）

`--probe` / `--idle-check` / `--theme-check` 只覆盖几何、帧数与配色；以下**平台输入语义**必须在真实窗口里
人工过一遍（每项一行，回归时逐条打勾）。这是 P4-17「P0 手动验收清单缺 AltGr / Alt+numpad」的落点。

| # | 操作 | 期望 |
|---|---|---|
| 1 | 键盘布局切到 AltGr 布局（波兰语 / 德语），`AltGr` + 字母 | 字符进入输入框（AltGr 例外放行，§11.2 ⑤） |
| 2 | `Alt` + 小键盘数字 | **不输入**正文（单按 Alt 仍按命令修饰键拒绝；已登记为事件层采样口径，P4-76） |
| 3 | 单行 readonly 输入框：Enter / Ctrl+Enter | **不派发 submit**、文本不变（P4-04 / §11.2 ㉜） |
| 4 | 多行 readonly 输入框：Enter | 不插入换行（同上） |
| 5 | Backspace / Ctrl+V / Ctrl+A / Esc | 只删 1 个字素、粘贴后无方块、无 `0x01`/`0x1B` 混入（四个实机 BUG 的第 ④ 条） |
| 6 | 点击输入框 padding / border / 滚动条 gutter | 焦点不丢（整盒可点，§11.2 ㉗）；caret 落在**点击处**（P4-10 / ㊳） |
| 7 | IME 候选窗（中文输入） | 候选窗贴在 caret 行盒位置；输入框不可见（`opacity=0`/零尺寸/被裁剪）时不定位（㉟） |

## 13. P0 浮层重构（popup-overlay-redesign）对 gallery 的影响（2026-11-26 回写；P1-12 迁移见 §13.5）

> 依据：`.docs/popup-overlay-redesign-design.md` §6 P0、§7.4/§7.5、§8、§11（P0 实施回写）；实现位于 `crates/ui_kit/examples/gallery.rs`（P2-5 的 `crates/base_ui/...` 路径已随批次迁移，语义不变）。

### 13.1 新增 `--pixel-check`（headless 持久增量像素门禁）

- 命令：`cargo run -p ui_kit --example gallery -- --pixel-check`（PowerShell 先设 `$env:CARGO_TARGET_DIR='E:\kytrix\target'`）。
- 退出码：**0 = PASS，3 = FAIL**；PASS/FAIL 行同时打印 `checks`、`sampled_frames`、`damage_full_frames`、`first_visible/stable/moved` rect。
- 实现：headless 镜像 `TestApp` + 离屏 `GpuRenderer` + `PersistentPixels`（同一 persist 纹理跨帧、按 `FrameRecord.damage/damage_full` 走增量 + damage scissor、readback 后逐像素比较）；不依赖真实窗口 readback。
- 断言（`checks` = `record()` 调用数，无手工增量）：**108 条 = 45 条逐帧采样断言（44 changed-outside-damage + 1 full-replay 首帧）（采样帧逐帧）+ 20 idle-frames + 20 background-residue + 3 restored + 2 close-frames + 18 条具名不变量**；具名不变量含 `baseline-opaque`、`open/open-settle`、`first-visible-rect`、`idle-no-animation`、`move-closed(-return)-settle`、`move-settle`、`old-region`、`old-region-coverage`、`residue-detector`、`move-panel`、`close`、`close-restore-settle`、`reopen(-settle/-rect/-close)`。
- 残影判定口径：移动后对“旧浮层区域 − 新浮层 − 新旧 trigger（各外扩 16 px）”全区域对打开前基线逐像素扫描；**覆盖量下限** `compared ≥ fixture 设备像素 × 30‰`（本机实测 26104/480000 ≈ 5.44%，防排除区膨胀假绿）；**负向自检**在比较集首/尾注入 2 个分散旧色板像素，要求检测到 `mismatched ≥ 2`；idle 采样 137 ms × 20 且先断言“无动画源”；容差通道 span ≤ 2。
- 迁移登记：该门禁绑定旧 `kit::overlay`（`Overlay::revealed/panel_node/open`）与 `ColorPicker`，P1-11 会删除这些 API → **P1-12 必须重写为 popup 栈 + `debug_stack` + 像素断言，并保留命令名与退出码**（设计 §8 已登记）。**P1-12 已完成**（2026-11-27）：现驱动 `PopupPanel`/`ColorPicker` 的 `popup_state`/`popup_node`/`panel_node`，P1 口径 94 checks 见 §13.5。

### 13.2 P0-8：旧 kit `Overlay` 首帧最小修复

- `Overlay::open` 现在在 open 内完成 **measure（`App::refresh_text` + `Window::compute_layout`）→ place → reveal**，首可见帧 rect == 稳定帧；宿主不逐帧 `sync()` 也会显示，`place()` 不再承担首次 reveal（R-05）。
- `ColorPicker::open`/`open_panel` 返回 `Result<bool, OverlayError>`：内容构建或 measure 失败会关闭回滚并返回强类型错误（Q-04）。
- gallery 的 `overlay-check`/首帧断言依赖这两条语义；P0-8 是 **P1-11 会整体删除的过渡 API 的一部分**，迁移要求同 13.1。

### 13.3 `--dump-overlay` → `--debug-stack`（P1-12 更名；旧命令已移除）

- P1-12 起真实窗口命令为 `cargo run -p ui_kit --example gallery -- --debug-stack`（`--auto-close` 可选）：dump `popup_stack` 快照（id/tier/parent/z/state/hit/paint/a11y/owner_modal/focus/node/paint_offset/constrained_size）+ probe rect，`overlay-check` 改为 popup 语义（栈条目数=1、menu tier、菜单度量、首可见帧 rect==稳定帧、关闭后无残留、结束时栈空）。**P2 起 overlay-check 另含 3 级菜单链阶段，见 §13.6。****`--debug-stack` 在最后一轮完成后自行关窗并以 0/3 退出；`--auto-close=<secs>` 只是 ≥30s 看门狗（`max(secs,30)`），无需携带**。
- 旧 `--dump-overlay` 已从 `parse_flags` 删除（P1-11 删除 `kit::overlay` 后无节点树可打印）；**传旧名不再得到 overlay-check，而是打印改名提示并 exit 2**（终审 K1-04/Q1-07：不再“忽略后 exit 0”的假绿），详见 §13.5。
- 真实窗口仍**不读像素**：真实窗口像素 readback/截图门禁登记为 **P3-6**（方案待定：readback vs 截图），届时与 headless `--pixel-check` 合并为同一退出码。`--idle-check`/`--theme-check` 语义不受影响。

### 13.4 与本文档前批次记录的关系

- 本文档 §9–§12 的 P2-5/P2-6b/P2-6c 记录保持不变；P0 的新增门禁与 P0-8 行为是**浮层重构阶段**的增量，不改变 gallery 的 ①–⑳ 场景结构与 `--theme-check`/`--idle-check` 口径。
- 旧 `overlay::Overlay`/`Layer`/`Band` 的删除与 popup 栈切换在 P1-11 进行；在那之前 gallery 仍用旧 kit Overlay，`--pixel-check` 的迁移必须与 P1-11 同批完成（P1-12）。**P1-12 已完成全部迁移**，见 §13.5。

### 13.5 P1-12 回写：popup 栈迁移后的 gallery 命令与门禁（2026-11-27）

> 依据：`.docs/popup-overlay-redesign-design.md` §12.2 P1-12、§12.3-8/9、§12.5；实现 `crates/ui_kit/examples/gallery.rs`。

- **旧机制删除**：`kit::overlay`（2871 行）/`kit::layer`（964 行）已随 P1-11 删除；`--pixel-check` 改由新 `PopupPanel` + `ColorPicker`（`popup_state`/`popup_node`/`panel_node`）驱动，命令名与退出码（**0=PASS / 3=FAIL**）保留。
- **P1 实测基线（`cargo run -p ui_kit --example gallery -- --pixel-check`，终审修复后）**：`checks=94 sampled_frames=32 open_animation_frames=8 close_animation_frames=6+6=12 damage_full_frames=0`，`first_visible==stable=(496,91,300×375)`、`moved=(496,4,300×375)`，exit 0（覆盖下限 30‰ 与 2 点注入自检保持）。
- **94 条构成**：32 条逐帧采样断言（`changed-outside-damage`；首帧整帧重放时 `full-replay`）+ 16 条 `background-residue`（open/reopen 两个采样窗各 8 帧）+ 46 条静态断言（20 `idle-frames` + 3 `restored` + 2 `close-frames` + 18 具名不变量 + 3 `sampled-animation-floor`）。
- **相对 P0 108 条的差异（口径说明）**：采样帧 45→32（场景迁到 PopupPanel 后动画时长/采样窗口变化）；idle 期在 `PixelFrame::Idle` 提前返回、不再产生 `background-residue` 逐像素比较（P0 的 20 条 idle 期 background-residue 由 20 条 `idle-frames` + `idle-no-animation` 覆盖“无动画源/无绘制帧”），因此 94 与 108 **不是逐条等价**。
- **`sampled-animation-floor` 口径**：open/close 的“已提交帧增量”下界；终审 K1-06 修复后按**单次关闭分段**校验（open ≥4、close#1 ≥3、close#2 ≥3，实测 8 / 6+6），防止动画常量变短或单次关闭完全不播动画时被累计采样掩盖；它仍**不是**“每一帧都是动画帧”的逐帧计数。
- **`--debug-stack` 实测基线（P1/P2 历史）**：`menu` 度量 `(6, 180.0, 127.5125)`、picker children=4、最终 `entries=0`、`overlay-check PASS`，exit 0；**P3 现测 `menu=(9, 270.0, 180.075)`（见 §14）**；自动化完成后自行关窗（`overlay-check complete -> closing window`），`--auto-close` 仅 ≥30s 看门狗，带 9 与不带 auto-close 均 PASS。`menu_frames`/`picker_frames` 为逐帧采样计数、运行间浮动（只断言 >0），不是固定契约。
- **旧 `--dump-overlay` 已移除且显式失败**：参数表中不再存在；传旧名会打印 `gallery: --dump-overlay was renamed; use --debug-stack --auto-close=9` 并 **exit 2**；其它未知参数会打印 `gallery: unknown argument <arg>` 与 usage 并 **exit 2**；`--help` 仍 exit 0（不再“忽略后 exit 0”假绿，终审 K1-04/Q1-07）。三端脚本与 README 一律改 `--debug-stack`。
- **P1 完成状态**：gallery 的 ⑮ ColorPicker 场景与 overlay-check 已全部跑在新 popup API 上；真实窗口像素 readback/截图与三端验收仍为 **P3-6**（`--debug-stack` 只断言栈/rect）。
- **最终数字（round-3）**：`--pixel-check` `checks=94`、`close 6+6=12` 不变；`cargo test -p ui_core -p ui_kit` 最终 3780 passed / 2 ignored（ui_kit 801），详见设计 §12.5。**P2 后记录见 §13.6。**
### 13.6 P2-7 回写：3 级链 overlay-check 与 P2 完成状态（2026-11-27）

> 依据：设计 §13.2/§13.6、`review-p2-{menu,quality}.md`；实现 `crates/ui_kit/examples/gallery.rs` 的 overlay-check。

- **P2 完成**：MenuChain（per-level 状态/timer/children-first/兄弟替换）、5 点 safe triangle + 300ms grace、完整键盘表/typeahead/checkable、2–3 级级联、链式 dismiss（祖先只关后代/Esc 逐层）、ContextMenu WindowRect/节点锚/键盘入口、a11y `active_descendant`/expanded/链嵌套均已落地（设计 §13.2；P2 审查 M2-01/02/03 与 Q2-03 由 task-58 修复）。
- **`--debug-stack` 3 级链阶段**：overlay-check 在单层 menu 阶段后新增 `chain-open`（root+level2+level3 打开）、`chain-stack-size`（3 条 popup）、`chain-z-order`（z 升序）、`chain-tier`（Auto）、`chain-parent-level2`/`chain-parent-level3`（parent 链）、`chain-close-call`、`chain-entries-zero`（关闭后栈清空）。
- **命令不变**：`cargo run -p ui_kit --example gallery -- --debug-stack`（自行退出 0/3；`--auto-close` 仅 ≥30s 看门狗）；`--pixel-check` 仍为原语义（32 sampled + 16 background-residue + 46 static + close 6+6；P2 不改像素门禁）。
- **最终数字（task-62 收口后，P2 最终）**：`cargo test -p ui_core -p ui_kit` **3847 passed / 0 failed / 2 ignored**（ui_core lib 2805、popup_behavior 111、ui_kit lib 866）；`--pixel-check` **PASS 94 checks**（32 sampled + 16 background-residue + 46 static；close 6+6）；`--debug-stack` **PASS**（含上述 3 级链阶段）；clippy/fmt/workspace check PASS。详见设计 §13.6。
- **P1 记录保留**：§13.3/§13.5 的 P1 语义与 94 checks 构成不变；P2 只新增链阶段与菜单行为。P2 的逐文件 `#[test]` 计数见设计 §13.6（menu 76 / menu_chain 13 / context_menu 20 = 109）。

### 14. P3 回写：真实窗口像素验收与动画/性能（2026-11-27）

> 依据：设计 §14（P3 回写）；实现 `crates/ui_kit/examples/gallery.rs` 的 `--real-pixel-check`、`tools/real-pixel-check-{macos,linux}.sh`。P3 只新增验收能力与动画/关闭 reason 例外，不改变 `--pixel-check`/`--debug-stack` 的既有语义。

- **`--real-pixel-check`（P3-6，真实窗口截图像素门禁）**：固定 960×720 always-on-top 窗口，内容边缘 6 px `#FF00FF` 标记环；平台截图命令抓全屏后解码，由品红框给出内容原点/物理尺寸/DPR，引擎 popup rect 乘窗口 scale 映射到物理像素；序列 `baseline → ColorPicker open/close → 3 级 MenuChain open/close → Dialog+scrim open/close`，打开必须出现可见变化（覆盖量/分散像素），关闭必须逐像素恢复（容差 2/255、coverage floor、触发器环排除）；负向自检（基线平移 (4,3)、改色）必须判红；全部通过 exit 0，失败 exit 3；看门狗 180 s；证据写 `target/real-pixels/<platform>/report.txt` + 00-warmup/00-baseline/01-picker-open/02-picker-closed/03-menu-open/04-menu-closed/05-dialog-open/06-dialog-closed PNG。
- **命令**：
  - Windows：`cargo run -p ui_kit --example gallery -- --real-pixel-check`（PowerShell `System.Drawing.CopyFromScreen` 虚拟屏幕）。
  - macOS：`tools/real-pixel-check-macos.sh`（构建后 `open -a Terminal` + osascript 执行并等待 exit 文件；`screencapture -x` 需要 **Screen Recording/TCC**，普通 ssh 会话只能拍壁纸）。
  - Linux/Wayland：`tools/real-pixel-check-linux.sh`（以父 Wayland socket 启 nested `sway --unsupported-gpu`，探测 grim-capable socket 后运行；`grim` 抓整个 wl_output 布局）。
- **三端结果（2026-11-27，P3 时点证据；合入滚动分支后已在本机 Windows 复跑 106/0，`gallery.rs` 新 SHA 以复跑 report 为准）**：**三端 HEAD `35c9381d` clean 各 106 checks / 0 failures PASS**：Windows 45.9 s/frame 280（隔离 43.3/277）、macOS 72.2 s/frame 283、Ubuntu 20.3 s/frame 311；含 dialog-scrim-theme/composite/alpha + scrim/panel fault + 交互/self-check；report 含 source_revision/status/diff_sha/source_identity/frame/elapsed/scrim_alpha；pixel-check 94 / idle-check / theme-check / debug-stack 均 PASS；`--real-pixel-check` 需独占桌面，并发会假红：

| 平台 | 结果 | 内容尺寸 / DPR | 耗时 | 证据 |
|---|---|---|---|---|
| Windows 11 | **PASS 106/0** | 1200×900 / 1.25（screen 4800×3072） | **45.9 s**（elapsed_ms=45865、frame 280；隔离 43.3/277） | `target/real-pixels/windows/20261127T143605Z-e69de29b/`（report/run.log/PNG；HEAD `35c9381d` clean、diff_sha `e69de29…`） |
| macOS | **PASS 106/0** | 1914×1437 / 2.0（screen 3360×2100；期望 1920×1440，容差 8） | **72.2 s**（elapsed_ms=72198、frame 283；TCC 需 Terminal） | `target/real-pixels/macos/20261127T143706Z-e69de29b/`（本地副本 `macos-head35c9`；`macos-remote` 同内容）（report/run.log/PNG）；远端 `/Users/chenbaiyu/kytrix-p3-accept/target/real-pixels/macos/` |
| Ubuntu nested sway | **PASS 106/0** | 1276×670 / 1.0（screen 1280×720；compositor 尺寸） | **20.3 s**（elapsed_ms=20297、frame 311；fresh nested sway parent wayland-0→wayland-1 + trap） | `target/real-pixels/linux/20261127T143842Z-e69de29b/`（本地副本 `linux-head35c9`；`linux-remote` 同内容）（report/run.log/sway.log/PNG）；远端 `/home/chenbaiyu/kytrix-p3-accept/target/real-pixels/linux/` |

- **平台差异/限制**：**`--real-pixel-check` 需独占桌面/前台真实窗口；并发 gallery/截图/窗口操作会遮挡标记环或截错窗口 ⇒ 假红（exit 3）**；**并发桌面下 `--debug-stack` 可能 300 s timeout，验收脚本自动重试 3 次，单次 timeout 只记录该次日志、不改变 gates 总结果**；macOS TCC 权限与 Terminal 启动；Linux 需 Wayland + grim，缺前置时脚本 exit 2；Dialog scrim 覆盖标记环时先重试 scrim-dimmed tone，失败回退 baseline origin，关闭后恢复检测；**三端本次均走 Scrim tone**，`dialog-marker-under-scrim` 仅在 tone==Scrim 记录（Full tone 可检测时跳过）；三端反解 scrim alpha≈0.55（dark palette）；P3A-04 修复后新增 `dialog-scrim-theme`/`dialog-scrim-composite`/`dialog-scrim-alpha`，已校验主题色/合成/alpha。标记厚度 = 6 px × DPR（Windows 8/macOS 10/Linux 6），尺寸容差 `max(4,4×DPR)`；断言数量三端一致（55）而非像素值一致。
- **证据口径**：三端证据以 `report.txt` 的 `source_revision`/`source_status`/`source_diff_sha`/`source_identity` 为准——**三端最终证据 HEAD `35c9381d` + `clean`，`diff_sha=e69de29bb2d1d6434b8b29ae775ad8c2e48c5391`，`source_identity=source-id-file`**；`tools/record-source-id.ps1` 生成 `tools/source-id.txt`（未跟踪生成物，提交前重生成；无则回退 git/unknown）；三端各含 `run.log`。**假绿修复**：`baseline-stable`/`close-residue` 按标记框内缩 `REAL_PIXEL_CORNER_INSET=4` 物理 px 比较，排除 OS 圆角 AA；`gallery.rs` SHA256=`abf26d49176b613be5b3012ca4137a36e70bd25ebd39fb08c784f34aa250d02f`。
- **`--pixel-check` 维持**：P3 后仍 **94 checks PASS**（32 sampled、8 open animation、close 6+6、`damage_full_frames=0`、首可见帧 rect==稳定帧）；动画常量（open 120 ms / close 90 ms）不改变采样下限语义。
- **`--debug-stack` 维持**：真实窗口 3 级链/取色器栈/首可见帧/rect/Z/parent 断言 PASS（exit 0；menu 度量 `(9, 270.0, 180.075)`，P2 历史基线 `(6, 180.0, 127.5)`）；P3 的 `FadeSlide`/`FadeScale` 不改变 settle 后 hit/paint rect；dump 不含 `paint_scale`（P3K-08，P4）。
- **P3 动画落点**：Menu/ContextMenu 打开动画改 `FadeSlide`（4 px slide + fade）；Dialog/ColorPicker 改 `FadeScale`（非模态 0.98→1/120 ms，modal fade-only）；Dropdown/Select 也经 `ItemPanel` 使用 `FadeSlide`。
- **P3 新增测试落点**：gallery 侧真实像素自动化 + 三端脚本；引擎/kit 单测见设计 §14.5（`popup_behavior` 124 条、ui_kit 891 条，含 FadeScale dpr/局部 damage/modal fade-only、shadow offset 向量、scrim fade-only、Glyph/Image 缩放、Dropdown/Select slide、reason 不泄漏）。
- **P3 审查后加强**：Dropdown/Select 各补 slide 断言（打开帧 `paint_offset.y=4`→settle 归零、settle rect 不变）；`popup_panel` 补 scrim fade-only/scale 断言；`--debug-stack` 仍不含 `paint_scale`、像素门禁不采样动画帧（P3K-08，P4）。
- **P4/未闭环（gallery 相关）**：P3K-07 scale 契约、P3K-08 debug-stack `scale=`/动画帧像素、R2-e WindowRect 口径、R2-f submenu 回调、外部滚动语义、reconcile Closing churn、右键自动绑定、typeahead IME、macOS TCC 一键化、acceptance 目录归档、真实像素扩展；均无用户可见验收阻塞。
