# ui_core 修复验收报告（2026-11-16/17）

> 分支 `ui-core-p0-p2-remediation`，基线 `a9a4220`，当前 **86 个 commit**。
> 审查报告：`review/ui_core-review-2026-11-16.md` + 8 份 `findings-*.md`。

## 1. 结论摘要

| 类别 | 数量 | 状态 |
|---|---|---|
| P0（报告原列） | 2 | ✅ 全部修复 |
| P0（本会话新发现，报告未列） | 1 | ✅ 已修复（静态几何从未上传，见 §3） |
| P1（报告原列） | 13 | ✅ 全部修复 |
| P2（报告原列） | 约 50 | ✅ 45 修复 / 5 待决（见 §5） |
| 第二轮对抗式复审新增 P1/P2/P3 | 3 组报告 | ✅ 全部处理（P1 4 / P2 4 / P3 20+） |

三平台质量门禁（每次提交后验证）：

| 平台 | 测试 | clippy | fmt | doc |
|---|---|---|---|---|
| Windows（本机） | **893** + 54 | `-D warnings` 干净 | 干净 | 无警告 |
| Linux（192.168.130.128） | **822** + 54 | 干净 | 干净 | — |
| macOS（192.168.1.6） | **812** + 54 | 干净 | 干净 | — |

## 2. 修复方法论

1. **先证伪再动手**：审查报告的结论逐条用代码/规范/测试核实。两条被证伪为
   非缺陷（浮层根边框收敛、Windows SRV 堆加锁——后者有 D3D12 规范支撑：
   "All methods for creating descriptors are free threaded"）。
2. **参照成熟实现**：D3D12/Vulkan/Metal 规范、Wayland 协议、AccessKit 语义、taffy、
   Parley/Swash/Skrifa、Chrome/Qt/Skia 的做法。
3. **修复必须被复审**：第一轮复审（3 个只读 subagent）审出 11 条 P1；**第二轮复审
   审的是第一轮的修复本身**，又抓出 4 条真 P1（macOS VBO 覆盖、空转测试、捕获
   suppress 修一半、a11y pos_in_set 语义）与 4 条 P2。��证了"自审通过 ≠ 没问题"。
4. **测试必须能抓回归**：第二轮复审发现 3 个"空转测试"（旧代码同样通过），全部改为
   精确断言 + 边界用例，并补 App 级端到端测试（原缺陷在调用点、只测编码器抓不到）。

## 3. 本会话新发现并修复的 P0 级缺陷

**静态几何/Path 从未真正上传**（报告未列）：`InstanceEncoder` 各持一个空的
`GeometryStore`，`App.geometry` 从未接入任何编码器（全仓无 store 赋值点）⇒
`geometry_desc()` 恒走 MISSING 空描述 ⇒ `Primitive::Geometry`/`Primitive::Path`
**永远不产生像素**。修复：`encode(..., &GeometryStore)` 参数化 + `paint_frame` 接收
几何表 + App 字段级借用拆分传入；新增「App 注册 → 帧 → 编码器顶点字节」端到端回归。
随后又修了它的两个衍生问题：几何 damage 用 `desc.bounds`（原按节点布局盒 ⇒ 溢出残影）、
隐藏/卸载/移动路径的 damage 升级。

## 4. 量化收益（criterion，50 samples，Windows）

| 基准 | 修复前 | 修复后 |
|---|---|---|
| `frame_full_512` | 222 µs | **142 µs** |
| `frame_incremental_512` | 288 µs | **72 µs** |
| `text_shape_32` | 3.23 ms | **1.75 ms** |
| `text_layout_32` | 3.59 ms | **2.79 ms** |

## 5. 待决 5 条原始 P2（需要决策或真机验证）

| 项 | 内容 | 阻塞点 |
|---|---|---|
| a11y-1 | 选区 `TextRun` / `character_lengths` | 可代码内解决（需选区数据通道） |
| win-1 | Windows IME 禁用路径（`ImmAssociateContext`） | 可代码内解决（跨 app/platform 接口） |
| mac-2 | macOS IME `replacementRange` 被忽略 | 可代码内解决（需 AppKit 文本模型接缝） |
| **win-2** | 标题栏拖动/系统菜单进入 Win32 模态循环阻塞泵 | **需决策**：根治=自绘非客户区（2–3 轮）；替代=文档化 |
| **txt-1** | 亚像素定位 + hint 策略 | **需真机目视验证**；可按 Skia 默认实现并标注可回退 |

## 6. 复审记录

- 第一轮对抗式复审（3 个只读 subagent，审原始修复）：11 条 P1 + 24 条 P2 → 全部处理。
- 第二轮对抗式复审（3 个只读 subagent，审第一轮修复本身）：3 份报告无新 P0；
  报出 4 条真 P1、4 条 P2、20+ 条 P3 → 全部处理。
- 两条原报告结论经核实**不是缺陷**（见 §2.1）。

## 7. 台账

逐条状态见 `review/remediation-progress.md`。
