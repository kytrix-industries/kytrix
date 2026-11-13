# 像素 golden（L3）

这些 PNG 是 **Windows WARP** 的离屏读回字节，按设备类型分目录（v1 只有 `warp/`）。
`lavapipe/`（Linux）与 `metal-real/`（macOS 实机）在对应 job 真正产出 golden 时再建。

## 字节语义（重要）

golden 是 `SnapshotImage.rgba8` 的**原样**落盘：**预乘 sRGB 编码的 RGBA8**。

- 不反预乘：反预乘要除 alpha，低 alpha 的量化误差会被放大，破坏「逐字节可复现」。
- 因此半透明区域在普通看图器里会比最终显示**偏暗**，这是预期行为。
- 尺寸是**物理像素**：同一个 case 在 `scale=2` 下是逻辑尺寸的两倍。

## 命名

`<scene>_<scale>_<theme>.png`，例如 `shapes_1.25_dark.png`（`scale` 去尾零：`1` / `1.25` / `2`）。

| scene      | 内容                                                                 | 尺寸（逻辑） | scale      | theme       |
|------------|----------------------------------------------------------------------|--------------|------------|-------------|
| `shapes`   | 直角矩形 / 四角不同圆角 / 1px 边框 / 圆角 + 边框                     | 240×160      | 1、1.25、2 | light、dark |
| `shadow`   | 解析阴影（blur 4 与 16）                                             | 240×160      | 1          | light       |
| `gradient` | 线性水平 / 垂直 / 径向（OKLCH + OKLab 端点）                         | 240×160      | 1          | light       |
| `colors`   | 纯色 / 半透明叠加 / 高彩度 OKLCH（gamut 边界）                       | 160×120      | 1          | light、dark |
| `image`    | 图片图集：1:1 / 2× / 半透明调制（16×16 四象限 alpha = 255/170/85/0） | 160×120      | 1、2       | light       |
| `clip`     | 嵌套 clip + 溢出子矩形                                               | 240×160      | 1          | light       |

**没有文字 case**：字形像素依赖宿主字体文件与版本，跨机必然分叉（`../../design/DEVIATIONS.md` F-144 / V-96）。
字形由 `rendered_text_produces_visible_glyph_pixels`（覆盖率 sanity）与 L2 的 `GlyphRun` 断言覆盖。

## 生成 / 更新 / 比对

```powershell
# 生成或更新（Windows / WARP）
$env:UPDATE_GOLDEN = '1'
cargo test -p ui_core --features test-support --test snapshot -- --nocapture
Remove-Item Env:UPDATE_GOLDEN

# 正常比对
cargo test -p ui_core --features test-support --test snapshot
```

```bash
UPDATE_GOLDEN=1 cargo test -p ui_core --features test-support --test snapshot
cargo test -p ui_core --features test-support --test snapshot
```

`UPDATE_GOLDEN` 存在即写（不看值）；写完同一次运行里会立刻读回并比较，形成自检。

## 容差（D19：本阶段**不冻结数值**）

| 项                                               | 值  | 状态                                 |
|--------------------------------------------------|-----|--------------------------------------|
| `Tolerance::default().per_channel`               | `1` | P08 §7.7 的提案值，待 L3/P7 实测校准 |
| `Tolerance::default().max_over_tolerance_pixels` | `0` | 同上                                 |

判定规则：`over_tolerance_pixels <= max_over_tolerance_pixels` 才算通过；
`max_channel_diff` / `avg_channel_diff` 只进报告，不参与判定。校准只改常量 + 本文档，不改机制。

## 失败时

- 失败信息带 `case`、`max`/`avg`/`over` 统计与 golden 路径；
- 同时写出热力图到 `target/golden-diff/<case>.png`（超容差像素涂洋红，其余取实际值的一半亮度），**不入库**。

## 平台

- Windows：跑（WARP 是系统组件）。
- macOS / Linux：`skip_reason()` 非空 ⇒ 每个 case 打印一行 `skip <name>: <reason>` 并返回。
  Linux 的 `new_offscreen` 恒 `UnsupportedGpu`（`../../design/phase-12-macos-linux.md` F-137）；macOS 有真实
  Metal 离屏渲染，但没有 `metal-real/` golden 可对。
- Windows 上的 `SnapshotError::Gpu` 是**测试失败**（设备/渲染/读回真故障），不是跳过（I10）。
