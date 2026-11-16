# Linux 平台集成审查

> 审查对象：`crates/ui_core/src/platform/linux/`（surface.rs / event_loop.rs / frame_gate.rs / clipboard.rs / a11y.rs / dpi.rs / ime.rs / wake.rs / window.rs）与 `crates/ui_core/src/platform/linux.rs`，HEAD `a9a4220`。
>
> 方式：Windows 上静态审查，**未实机运行**。涉及具体 compositor（GNOME/Mutter、KWin、Sway/wlroots）行为的判断均为静态推断，已在置信度中标注，需在实机矩阵复核。

## 总体判断

整体架构方向是正确的：只保留 Wayland 后端、`calloop::EventLoop` + `WaylandSource` + `calloop::ping` 唤醒、以 `wl_surface.frame` 回调为唯一节流信号的 FrameGate、fractional-scale 与 viewport 成对创建并按 present 同帧生效、`xdg_surface.configure` 即 ack、多 `wl_output` 实例与热插拔整数 scale 选择等，均体现了对 Wayland 协议细节的掌握；`frame_gate`、`dpi`、`clipboard` 的纯逻辑单测也比较扎实，明显经过多轮真实缺陷修复（F-146/F-147/F-183/V-110/V-122 等注释可追溯）。

主要风险集中在**输入协议完整性与窗口/剪贴板生命周期**：IME 与 `wl_keyboard` 文本双通路、键盘 `repeat_info`/layout group、`wl_pointer` 离散滚动三项会直接影响输入正确性和可用性（P1）；CSD decoration 协商、`CloseRequested` 事件、clipboard source/offer 生命周期、`xdg_toplevel.suspended`、触摸与光标是明确的平台缺口（P2）。静态审查**未发现可证实的 P0 崩溃或协议断连路径**；但 IME 重复插入在价格/数量等输入框中可能产生错误的业务数据，应按最高优先级修复。

## 发现（P0-P3）

### P0

未发现可静态确认的 P0 问题。

### P1

#### 1. IME 组合期间仍从 `wl_keyboard` 生成 `Text`，会与 `zwp_text_input_v3` 的 Commit 重复插入

- **证据**：`crates/ui_core/src/platform/linux/surface.rs:673-694`（`translate_key` 在每个 Key Down 上无条件 `key_get_utf8` 并 push `InputEvent::Text`，见 `:686-692`）；`crates/ui_core/src/platform/linux/surface.rs:1214-1237`（同窗口的 text-input-v3 `CommitString` 也转成 `ImeEvent::Commit`）；`crates/ui_core/src/platform/linux/ime.rs:10-64` 的 `composing` 状态从未参与 `translate_key` 判断。
- **问题**：Wayland 的 `zwp_text_input_v3` 与 `wl_keyboard` 是两条并行通路。窗口获得键盘焦点后本实现立即 `text_input.enable()`（`surface.rs:600-626`），而 Gtk/Qt/Chromium 等客户端的做法是在 IME 组合期间忽略/消费 `wl_keyboard` 的字符输入，只接受 `preedit_string` / `commit_string`。本实现把两条通路都交给应用：demo 的 `ImeField::insert`（`examples/demo/scene/ime.rs:85-89`）会先插入每个原始按键字符，再追加 IME 提交串。macOS 后端已正确处理这一点（`platform/macos/view.rs:108-119`：`composing` 或 `commit_consumed` 时不发 `Text`），Windows 也有 `take_char_suppression`（`platform/windows/ime.rs:57-63`），Linux 是唯一缺少抑制的路径。
- **影响**：中文/日文/韩文输入时，输入“ni”很可能得到“`ni你`”而不是“你”；价格、数量、搜索框等业务输入会出现错误值，且用户不一定能立即发现。属于用户可见的数据正确性问题。
- **成熟实现对照**：GTK 的 `gtk_im_context_filter_keypress`、Qt 的 `QInputMethod`、Chromium 的 Wayland `WaylandInputMethodContext` 都在 preedit 活动时消费原始按键；本项目 macOS 后端的 `view.rs:108-119` 也可直接作为对照。
- **建议**：让 `translate_key` 读取 `ImeState::composing`（或“当前有可编辑焦点”的显式状态），在 IME 组合或本键已被 IME 消费时不再发 `InputEvent::Text`；更进一步可把纯文本输入统一交给 text-input-v3（不支持时再走 `wl_keyboard` 文本）。补一条平台翻译层回归：注入 Key Down('n') + `ImeEvent::Commit("你")`，断言应用只收到一次提交。
- **置信度**：高（代码路径确定；具体 compositor 是否同时向客户端下发被 IME 使用的 `wl_keyboard` 按键需 GNOME/KWin 实测确认）。

#### 2. 完全没有实现键盘重复（`wl_keyboard.repeat_info`），长按不会重复

- **证据**：`crates/ui_core/src/platform/linux/surface.rs:1003-1077` 的 `Dispatch<wl_keyboard::WlKeyboard>` 只处理 `Keymap` / `Modifiers` / `Key` / `Enter` / `Leave`，其余分支 `_ => {}`（`:1074`）；全文件没有 `RepeatInfo`、repeat timer 或合成 Key Down 的逻辑。`surface.rs:224` 把 `wl_seat` 绑定到 5..=9，客户端拿到的是 v9 的 `wl_keyboard`，不会收到 v10 的 `key_state.repeated`。
- **问题**：Wayland 协议规定键盘重复由客户端根据 `wl_keyboard.repeat_info`（since v4，rate 字符/秒、delay 毫秒）自行实现；compositor 不会重复发送 `wl_keyboard.key`（除非 v10 且客户端声明 rate=0）。当前实现既未保存 rate/delay，也没有定时器，长按 Backspace/Delete/方向键只有第一次按下生效。
- **影响**：文本编辑（连续删除、移动光标）、图表键盘导航、列表连续翻页全部不可用或极难操作；对于键盘密集的行情应用影响明显。
- **成熟实现对照**：GTK、Qt、Chromium 以及 winit 的 Wayland 后端均保存 `repeat_info` 并用定时器合成重复按键；xkbcommon 负责重复键的 keysym 解析。
- **建议**：在 `WaylandState` 保存 `(rate, delay)`，用 calloop timer（或现有 `FRAME_TICK` 旁路的独立 timer source）在 delay 后按 rate 生成合成 Key Down，并在 Key Up、焦点离开、keyboard capability 撤销时取消；后续若绑定 `wl_seat` v10，可同时处理 `KeyState::Repeated`。
- **置信度**：高（协议事实明确，代码中无任何重复实现）。

#### 3. `wl_pointer.axis` 被当作 1/120 行的离散步长，且忽略 `axis_discrete` / `axis_value120` / `frame`

- **证据**：`crates/ui_core/src/platform/linux/surface.rs:1138-1153`：`Axis { value, .. }` 直接计算 `delta = -(value as f32) / 120.0`；`surface.rs:1079-1159` 的 `Dispatch<wl_pointer::WlPointer>` 没有 `AxisDiscrete`、`AxisValue120`、`Frame` 分支（都落入 `_ => {}`，`:1157`）。
- **问题**：`wl_pointer.axis.value` 是 **surface-local 坐标空间的连续向量长度**（不是“1/120 行”的离散 ticks）；滚轮一个刻度的连续值通常只有约 10–15（因 compositor/设备而异），除以 120 只剩 0.08–0.12 行；真正的离散步长在 `axis_discrete`（v5–v7）或 `axis_value120`（v8+）中。本实现绑定到 pointer v9，因此 compositor 会发 `axis_value120` 而不会发 `axis_discrete`，两个离散事件又都被忽略。
- **影响**：鼠标滚轮滚动约慢一个数量级（转很多格才动一点），触控板/高分辨率滚轮还会因逐个 `axis` 事件派发而抖动；行情列表/图表缩放等滚动交互基本不可用。
- **成熟实现对照**：GTK/Qt/Chromium 在 `wl_pointer.frame` 内累计 `axis_value120`（旧版本用 `axis_discrete`）来得到滚轮行数，连续 `axis` 只用于像素级/触控板平滑滚动；winit 也按 `axis_value120 / 120` 处理。
- **建议**：按 `wl_pointer.frame` 批处理；同时累计 `axis_value120`（每 120 = 1 行）和连续 `axis`（用于触控板像素滚动），二者分别映射到引擎的行滚动/像素滚动语义；补 120/-120/240 的单测和 GNOME/KWin/Sway 实机滚动测试。
- **置信度**：高（协议语义可直接核对；实际 axis 数值需实机采样确认，但“/120”不成立是确定的）。

#### 4. `wl_keyboard.modifiers.group` 被丢弃，切换键盘布局后字符仍按 group 0 解析

- **证据**：`crates/ui_core/src/platform/linux/surface.rs:1033-1041` 的 `Modifiers` 事件只读 `mods_depressed` / `mods_latched` / `mods_locked`，用 `..` 忽略 `group`；`surface.rs:182` 的 `mods` 是三元组；`surface.rs:678` 与 `surface.rs:702` 都调用 `xkb_state.update_mask(depressed, latched, locked, 0, 0, 0)`，group 恒为 0。
- **问题**：Wayland `wl_keyboard.modifiers` 的 `group` 是当前 XKB layout group。忽略它会让 `xkb_state.key_get_one_sym`/`key_get_utf8` 永远从第一个布局取符号。
- **影响**：用户切换到第二/第三键盘布局（常见于中文用户的美式+拼音、欧洲多语言布局）后，输入字符仍是第一个布局的字符；快捷键和文本输入都会错。
- **成熟实现对照**：winit、GTK、Qt 的 Wayland 键盘实现都把 `group` 存入状态并传给 `xkb_state_update_mask`；xkbcommon 文档要求 group 参与状态更新。
- **建议**：`mods` 改为含 `group` 的结构体，`Modifiers` 事件和 `translate_key` 的 `update_mask` 都传入 `group`；补一个两 group keymap 的单测。
- **置信度**：高（协议字段与代码路径均确定）。

### P2

#### 5. CSD decoration 的 `configure` 模式被忽略，遇到 ServerSide 会双标题栏

- **证据**：`crates/ui_core/src/platform/linux/surface.rs:285-297` 绑定 `zxdg_decoration_manager_v1` 并请求 `Mode::ClientSide`；`surface.rs:1240-1251` 的 `Dispatch<zxdg_toplevel_decoration_v1>` 对 `Event::Configure` 完全忽略（注释“引擎始终自绘标题栏”）；`crates/ui_core/src/window.rs:64-78` 提供自绘标题栏所需的 `WindowControl`，demo 的 `examples/demo/scene/chrome.rs:141-211` 确实始终绘制标题栏。
- **问题**：xdg-decoration 协议明确“`configure` 指定的 mode 必须被客户端遵守”。合成器可以不同意 `ClientSide` 而返回 `ServerSide`；本实现仍绘制自己的标题栏。协议也未要求合成器必须支持该扩展——不支持时客户端应按自身策略自绘，但无法响应合成器强制模式。
- **影响**：在 KWin 等服务端装饰配置下可能出现双标题栏/双套窗口按钮，拖动/最大化/关闭的热区和窗口几何错位；在部分合成器上还可能因 decoration 配置事件与首帧 buffer 的时序产生协议错误（当前只等待 `xdg_toplevel.configure`，没有显式等待 decoration configure）。
- **成熟实现对照**：Qt/GTK/SDL 的 Wayland 后端会根据 decoration `configure` 在 CSD/SSD 之间切换；GNOME 场景通常依赖 GTK/libdecor 的 CSD 协商；libdecor 就是为解决“客户端自绘与服务端装饰协商”而存在。
- **建议**：保存 decoration 的实际模式；收到 `ServerSide` 时隐藏引擎自绘标题栏/边框并调整内容区域，必要时设置 `xdg_surface.set_window_geometry` 与阴影 extents；manager 不存在时明确采用 CSD 并测试 GNOME/KWin/Sway 三种会话。另建议绑定 manager v2，允许在已有 buffer 后创建 decoration 并避免 v1 的“首帧前必须收到 configure”约束。
- **置信度**：中高（协议要求确定；具体 compositor 是否强制 ServerSide 需实机）。

#### 6. `xdg_toplevel.close` 没有转换成 `Event::CloseRequested`，应用无法拦截关闭

- **证据**：`crates/ui_core/src/platform/linux/surface.rs:977-979` 的 `xdg_toplevel::Event::Close` 只置 `close_requested`；`crates/ui_core/src/platform/linux/event_loop.rs:168-172` 在下一轮直接 `app.close_window(id); return Ok(())`。对比：`Event::CloseRequested` 已定义于 `crates/ui_core/src/event.rs:233`，Windows 在 `platform/windows/event_loop.rs:916` 生成它，macOS 在 `platform/macos/window.rs:255-260` 生成并 push。
- **问题**：Linux 平台跳过了应用层的关闭事件，直接销毁窗口。应用无法保存未提交内容、弹确认框、拒绝关闭或执行清理；同一份应用代码在三平台上的关闭语义不一致。
- **影响**：带未保存状态（布局、指标配置、下单草稿）的窗口被合成器关闭按钮直接销毁；用户可能丢失数据。属于跨平台行为缺口。
- **成熟实现对照**：GTK 的 `delete-event`、Qt 的 `closeEvent`、macOS 的 `windowShouldClose:`、Windows 的 `WM_CLOSE` 都先交给应用决定；本项目 Windows/macOS 已经采用该模式。
- **建议**：`xdg_toplevel::Close` 只 push `Event::CloseRequested`，由应用决定是否 `close_window`；若应用未处理（例如无监听者），事件循环可保留默认关闭策略，但必须与其它平台一致。补一条“关闭请求 → 应用 veto → 窗口仍存在”的回归测试。
- **置信度**：高（代码路径与其它平台对照明确）。

#### 7. 剪贴板复用已 `cancelled` 的单个 `wl_data_source`，且从不销毁 `wl_data_offer`

- **证据**：`crates/ui_core/src/platform/linux/surface.rs:237-241` 启动时只创建一个 `wl_data_source`；`crates/ui_core/src/platform/linux/clipboard.rs:104-113` 每次发布都把这同一个 source 交给 `set_selection`；`clipboard.rs:178-180` 收到 `Cancelled` 只清 `published`，不 destroy、不重建 source；`clipboard.rs:123-149` 收到新 `Selection`/`Leave` 时直接覆盖/清空 `self.offer`，从不调用 `wl_data_offer.destroy()`，`clipboard.rs:155-159` 也不处理 offer 的生命周期。
- **问题**：`wl_data_source.cancelled` 的协议语义是“source 不再有效，客户端应清理并销毁它”。之后继续用同一 source `set_selection`，在严格实现上可能被忽略，导致第二次复制失败。每次 clipboard 变化产生的新 `wl_data_offer` 也一直保留到连接关闭，长期运行会累积服务端对象。
- **影响**：从 Kytrix 复制 → 其它应用复制 → 再回 Kytrix 复制，可能只有第一次成功；剪贴板对象泄漏；对高频交易应用复制价格/合约代码是高频操作，影响直接。
- **成熟实现对照**：GTK 每次 `set_selection` 创建新的 `wl_data_source`，在 `cancelled` 后销毁；Qt/Chromium 同样按 selection 建立/销毁 source，并在替换 selection 时 `wl_data_offer.destroy()`。
- **建议**：把 `published` 与 source 绑定为每轮发布新建的对象；`Cancelled` 时销毁 source 并清空；每次新 `DataOffer`/`Selection(None)` 时销毁旧 offer；补“两次复制夹一次外部复制”的集成测试（可用 weston/wlroots 或 mock compositor）。
- **置信度**：高（协议文档明确；compositor 是否严格拒绝复用需实测，但对象泄漏是确定的）。

#### 8. 忽略 `xdg_toplevel.suspended`，窗口被遮挡/锁屏/最小化时仍持续出帧

- **证据**：`crates/ui_core/src/platform/linux/surface.rs:957-981` 的 `Configure` 处理只解析 `State::Maximized`（`:968-973`），`Suspended`（xdg-shell v6，`:909-915` 的协议描述）和其它状态都被忽略；`crates/ui_core/src/platform/linux/event_loop.rs:232-243` 在 `needs_continuous_frames()` 时固定 16ms tick；`crates/ui_core/src/platform/linux/frame_gate.rs:26` 还有 200ms 活性兜底。
- **问题**：`suspended` 明确表示“surface 当前不应重绘（例如被遮挡或屏幕锁定）”。本实现没有 suspended 标志，动画/HUD 仍按 60Hz 请求并提交帧；若 compositor 因 suspended 停止发 frame callback，FrameGate 每 200ms 兜底开门再发布帧，形成“无 callback 的 5fps 重试”。
- **影响**：窗口被遮挡/锁屏/最小化时仍消耗 CPU/GPU 和电量；对长时间挂机的行情终端不友好，也可能在电池设备上明显发热。
- **成熟实现对照**：Chromium/GTK/Qt 在 `xdg_toplevel.suspended` 时暂停渲染与动画定时器，恢复时再请求首帧。
- **建议**：解析全部 `xdg_toplevel.state`（至少 `Suspended`、`Activated`、`Tiled*`、`Resizing`）；维护 `suspended` 状态，suspended 时跳过 `request_frame`/`app.frame`/连续帧 tick，并禁用 200ms 兜底；收到非 suspended configure 后主动 wake 一次。实机用 GNOME 锁屏/KWin 最小化测试功耗。
- **置信度**：高（协议状态语义和代码缺失均确定；具体 compositor 是否发 suspended 需实测）。

#### 9. 触摸（wl_touch）完全未实现

- **证据**：`crates/ui_core/src/platform/linux/surface.rs:123-130` 的 `WaylandState` 只有 `keyboard: Option<wl_keyboard::WlKeyboard>` / `pointer: Option<wl_pointer::WlPointer>`，无 `wl_touch`；`surface.rs:1283-1300` 的 `create_devices` 只检查 `Capability::Keyboard` / `Capability::Pointer`，从不检查 `Capability::Touch`；全文件无 `Dispatch<wl_touch::WlTouch>`。
- **问题**：`wl_seat` 的 Touch capability 被完全忽略，触摸屏的 down/motion/up/cancel 不会进入引擎。
- **影响**：触摸屏笔记本、平板和一体机上的点击/拖动/滚动全部失效；桌面 Linux 发行版对触摸的支持是常见预期，尤其适用于图表拖拽查看。
- **成熟实现对照**：GTK、Qt、SDL、Chromium 的 Wayland 后端都绑定 `wl_touch` 并把 touch point 映射为指针/触摸事件；wlroots 与 Mutter 均提供该接口。
- **建议**：增加 `touch: Option<wl_touch::WlTouch>`，在 capabilities 处理中按 Touch 位创建/释放；把 `Down`/`Motion`/`Up`/`Cancel` 映射为带 pointer id 的触摸事件（或至少映射为单指 Pointer 事件），并处理 `wl_touch.frame` 批处理。
- **置信度**：高（功能缺失可直接确认）。

#### 10. 光标形状请求被直接丢弃，Wayland 没有任何 `wl_pointer.set_cursor`

- **证据**：`crates/ui_core/src/platform/linux/event_loop.rs:164-165`：`let _ = app.take_cursor_change();`，注释称“Linux 没有全局光标 API（由客户端自绘）”；`crates/ui_core/src/platform/linux/surface.rs` 全文没有 `set_cursor` 调用；`crates/ui_core/src/paint/style.rs:132` 定义了 `Cursor` 枚举，`app.rs:2131-2133` 也会产生 `cursor_pending`。
- **问题**：Wayland 确实没有“全局光标”，但有**per-surface** `wl_pointer.set_cursor`（配合 `wayland-cursor`/`wl_cursor_theme`，或 `wp_cursor_shape_v1`）。本实现既不加载 cursor theme，也不设置 cursor，注释把“非全局”误解为“无需实现”。
- **影响**：所有 UI 元素都显示默认箭头：文本输入框没有 I-beam、CSD 边框没有 resize cursor、按钮/链接没有 hand、图表十字光标无法表达；CSD 拖拽边缘尤其难发现。
- **成熟实现对照**：GTK/Qt/Chromium 都在 `wl_pointer.enter`/`motion` 时调用 `wl_pointer.set_cursor`，根据命中的 widget 切换 cursor；`wayland-cursor` 是标准做法。
- **建议**：增加 `wayland-cursor`（或 `wp_cursor_shape_v1`）依赖与 `wl_shm` 绑定；在 pointer enter/motion/leave 时按 `App::take_cursor_change()` 设置/清除 cursor；按 `wl_surface` scale 选择 cursor 尺寸；补各 cursor 类型的实机截图。
- **置信度**：高（功能缺失明确；具体 cursor theme 依赖需打包检查）。

#### 11. IME 激活绑定整个窗口焦点，未处理 text-input-v3 的 enter/leave 与 surrounding text

- **证据**：`crates/ui_core/src/platform/linux/surface.rs:600-626` 的 `set_ime_focus` 在 `focused=true` 时无条件 `text_input.enable()`，`surface.rs:1063-1073` 的 `wl_keyboard::Enter/Leave` 调用它；`surface.rs:1214-1237` 的 `Dispatch<ZwpTextInputV3>` 只处理 `PreeditString` / `CommitString` / `Done`，没有 `Enter` / `Leave`；全文件没有 `set_surrounding_text` / `set_content_type` / `set_text_change_cause`。
- **问题**：text-input-v3 协议要求 `enable`/`disable` 随“可编辑元素焦点”变化（包括同一 surface 内切换输入框），并建议发送 surrounding text/content type 以让 IME 正确组词。当前实现只在窗口级键盘焦点变化时 enable/disable，且未维护 text-input enter/leave。
- **影响**：没有输入框时 IME 仍处于启用状态，用户在图表区域按键时可能弹出候选窗/被 IME 截获；从输入框 A 切到 B 时 IME 状态不重置，候选窗可能沿用上一个字段的上下文；中文候选词排序/上下文联想质量差；纯触摸设备（无 keyboard enter）永远不会 enable IME。
- **成熟实现对照**：GTK 的 `GtkIMContext` 只在可编辑 widget 获得焦点时 enable text-input，并发送 surrounding text/content purpose；Qt/Chromium 同样按输入框焦点管理 text-input-v3 生命周期。
- **建议**：给 App 增加“当前是否有可编辑焦点”的显式状态（`ime_cursor_area.is_some()` 可作为第一版判据），据此 enable/disable；处理 text-input enter/leave；为输入框发送 `set_surrounding_text`、`set_content_type`（普通文本/密码/数字）和 `set_text_change_cause`；补输入框 A→B 切换与无输入框场景测试。
- **置信度**：中高（协议要求明确；具体 compositor 的 IME 行为需实机确认）。

### P3

#### 12. `Event::Resized` / `Event::DpiChanged` 在窗口真值写入前就分发给应用

- **证据**：`crates/ui_core/src/platform/linux/event_loop.rs:128-159` 先 `app.dispatch(id, event)`（`:131-133`），之后才在 `:145-159` 调 `window.set_scale` / `window.set_size` / `window.set_surface`；而 Windows 在翻译 `PlatformEvent::Resize` 时就 `window.set_size` / `set_surface`（`platform/windows/event_loop.rs:866-885`），macOS 在 `sync_state` 里先写窗口状态再 push `Event::Resized`（`platform/macos/window.rs:205-242`）。
- **问题**：Linux 的 `Event::Resized`/`Event::DpiChanged` 处理器读取 `Window::size()` / `scale()` 时可能拿到上一轮的值；依赖事件尺寸做布局/业务计算的平台无关代码会出现一帧延迟或逻辑不一致。
- **影响**：一般 UI 可能看不出来，但自定义标题栏、浮动面板定位、图表坐标换算等对尺寸敏感的逻辑可能在 resize 首帧用错尺寸；三平台行为不一致，增加应用层适配成本。
- **成熟实现对照**：Win32、AppKit、GTK 都在派发 configure/resize 事件前更新窗口几何与 scale。
- **建议**：在 `xdg_toplevel::Configure` / `recompute_scale` 中先写 `window.set_size/set_scale/set_surface`，再把 `Event::Resized/DpiChanged` 交给 `App::dispatch`；或在同一轮先做镜像、后 dispatch 事件；补一个事件处理器读取窗口尺寸的时序测试。
- **置信度**：高（代码顺序确定，跨平台对照明确）。

## 做得好的地方

- **FrameGate 状态机设计完整**：`request` 幂等、`note_not_committed` 只开门不清 pending、`expire` 用 token 区分迟到回调、回调对象 id 匹配，且有 6 个单测覆盖 Idle/超时/迟到场景（`frame_gate.rs:57-124`、`:133-257`）。在 `wl_surface.frame` 节流这一最容易出错的点上，思路优于很多手写 Wayland 后端。
- **configure 生命周期正确**：建 surface 后立即 `surface.commit()` 触发初始 configure，`xdg_surface::Configure` 立即 `ack_configure`（`surface.rs:933-946`），`initial_roundtrip` 明确要求首个 `xdg_toplevel.configure` 才继续（`surface.rs:1259-1277`），`width/height == 0` 时保留应用请求尺寸（`surface.rs:963-975`）。
- **fractional-scale / viewport 成对处理**：只有 `wp_viewporter` 与 `wp_fractional_scale_manager_v1` 同时存在才创建 fractional + viewport（`surface.rs:247-284`），`take_viewport` 把 destination 所有权交给 GPU 线程，避免 UI 线程单独 commit 造成旧 buffer 被新 destination 拉伸（`surface.rs:506-519`）；renderer 在 present 前写 destination、buffer scale（`render/linux/compositor.rs:367-411`），并用 `surface_local`/`buffer_scale` 与 buffer 同帧配对（`render/frame.rs:159-171`）。
- **多 `wl_output` 与整数 scale 回退考虑周到**：启动时逐个绑定所有 output（不是 `GlobalList::bind` 只绑第一个），支持热插拔 `global_remove`（`surface.rs:355-405`、`:761-833`），`choose_integer_scale` 有 6 个单测覆盖 preferred/entered/bound/leave 保留旧值等分支（`surface.rs:90-107`、`:1348-1402`）。
- **IME 双缓冲状态机**：`preedit_string`/`commit_string`/`done` 按批次原子生效，commit 先于 preedit 处理，并有 `CommitDeduper` 处理 GNOME 重复提交（`ime.rs:10-64`）；虽然激活策略有缺口，但状态机本身结构清晰。
- **剪贴板读取遵循协议顺序**：先 `accept(mime)` 再 `receive`（`clipboard.rs:138-145`），mime 按 offer 声明动态选择，读取线程有 1MiB 上限并清 NUL（`clipboard.rs:191-203`）；这些细节比常见手写实现完整。
- **calloop/唤醒集成简洁**：`WaylandSource` 插入、`calloop::ping` 作为跨线程唤醒、`frame_pending` 合并位与 `posted` 失败保留语义（`dispatcher.rs:112-135`、`wake.rs:11-26`）与 Win32/macOS 的唤醒契约一致。
- **a11y 直连**：`accesskit_unix::Adapter` 直接构造、ActionRequest 经 Dispatcher 回投主线程，`set_root_window_bounds`/`update_window_focus_state` 接线齐全（`a11y.rs:30-81`），与 Windows/macOS 的懒激活模式一致。

## 存疑 / 需实测

- **compositor 矩阵差异**：`zxdg_decoration_manager_v1` 是否被 GNOME/Mutter 实现、KWin 是否强制 ServerSide、Sway/wlroots 的 `zwp_text_input_v3` 是否可用，都会改变第 5、11 条的严重度；必须在 GNOME Wayland、KWin Wayland、Sway 三个会话实机验证。
- **text-input-v3 与 wl_keyboard 的按键并行行为**：第 1 条的重复插入是基于“compositor 同时向客户端发送 `wl_keyboard.key` 和 text-input 事件”的推断；需用 GNOME/KWin 的中文输入法实测 `n`/`i` 的最终文本。
- **`wl_pointer.axis` 实际数值**：第 3 条的“慢一个数量级”依赖 compositor 发送的连续 axis 值（常见 10–15/刻度）；需在 GNOME/KWin/Sway 下用真实滚轮采样验证比例。
- **指针捕获**：Windows 路径使用 `SetCapture`，Linux 直接丢弃 `take_capture_request`（`event_loop.rs:165-166`）。Wayland 的按钮按下隐式 grab 通常已覆盖拖拽出窗口，因此本次未列为缺陷；但若产品依赖显式 pointer lock/confine（例如图表十字线锁定），需要 `zwp_pointer_constraints_v1` + `relative_pointer`，当前未实现，需产品确认。
- **Vulkan WSI 与 calloop 共享 `wl_display`**：GPU 线程通过 `raw_pointers` 拿 `wl_display*`，UI 线程用 wayland-client 的 `EventQueue`；两者共用一个连接、不同 event queue 的互操作在静态上可行，但必须实机验证 WSI present、frame callback 与 `WaylandSource::prepare_read` 不会互相饥饿。
- **多窗口**：当前 Linux 实现只处理 `app.windows().next()`（`event_loop.rs:37-66`、`:147-159`），而 Windows/macOS 已按全部窗口循环；设计文档明确 v1 只保证单窗口（`design/custom-gui-engine-design.md:1711`），故本次未作为缺陷；如果产品需要多窗口（多图表分离），Linux 需要单独设计一套 surface/FrameGate 映射。
- **AccessKit 激活时序**：`UnixActivation::request_initial_tree` 返回 `None`（`a11y.rs:52-59`），依赖首次 `update_if_active` 提供完整树；如果辅助技术在应用空闲后才连接，`a11y_pending` 可能为 false，需实机用 screen reader/AT-SPI 工具验证首棵树是否送达。这是三平台共性问题，Linux 侧尤其依赖 D-Bus 时序。
- **字体与系统依赖**：文本栈使用 `fontdb::load_system_fonts`（`text/context.rs:78-79`），Linux 下依赖 fontconfig；最小桌面/容器环境可能没有字体配置，需要实机确认后备字体与 CJK 字体是否存在。
- **协议对象 destroy/release 细节**：`wl_keyboard`/`wl_pointer` 在 capability 撤销时直接置 `None`（`surface.rs:1283-1300`），未调用 `release`；`zxdg_decoration_manager_v1`、`wl_data_device_manager` 等工厂对象也未 destroy。单窗口进程退出时由连接关闭清理，影响较小，故未单列为 P3；若未来支持多窗口/长期运行应一并补齐。
- **静态审查限制**：本次无法在 Linux 实机运行，也未执行 `cargo check --target x86_64-unknown-linux-gnu`；所有行号基于 HEAD `a9a4220` 的工作区文件。建议在实机 CI 上补 Wayland 协议 trace（WAYLAND_DEBUG=1）与输入事件回放。
