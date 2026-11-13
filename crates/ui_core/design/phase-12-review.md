# Phase 12 代码审查结论（macOS + Linux 后端）

> **P18 迁移**：demo 已并入 `..`，不再是独立 crate；本文中的
> `crates/ui_core_demo` 路径与命令是当时的历史记录，当前命令见 `design/phase-18-demo-example.md`。
>
> 审查方式：静态审查 + 两 target 交叉编译 + 与 D3D12/共享契约逐项对照。本机无法运行时验证。
> 结论：**FAIL**（15 项阻塞；门禁本身全绿，但后端有静态可证的死锁/泄漏/崩溃/不出帧问题）。

## 阻塞项（必须修复）

### Linux Vulkan / Wayland

1. **fence 首帧死锁**（`render/linux/device.rs:263,333`）：fence 创建未带 `SIGNALED` 就 `wait_for_fences(u64::MAX)`；`:336`
   先 reset，`OUT_OF_DATE`/Geometry 早退后该槽不再 signal，3 帧后死锁。→ 创建带 `SIGNALED`、`wait == 0` 跳过、submit 前才
   reset。
2. **resize 泄漏/空 handle**（`render/linux/swapchain.rs:246,254`）：先把**新** handle 置 null 再赋给 `self.raw`。→ 先保存
   `new_raw`，再置空 replacement。
3. **copy 子资源非法**（`render/linux/compositor.rs:520-524`）：`ImageCopy::default()` 的 `src/dst_subresource`
   未设（aspect/layerCount=0）。→ 两侧 `aspect=COLOR, layer_count=1`。
4. **timeline semaphore 用法错误**（`render/linux/compositor.rs:554-565`）：timeline semaphore 走 binary
   `signal_semaphores`，无 `TimelineSemaphoreSubmitInfo`/值，`wait_idle` 永不返回。→ 链 timeline info + `FrameIndex` 值。
5. **persist 初始布局**（`render/linux/pipeline.rs:255`）：persist 以 `UNDEFINED` 创建，但两个 render pass 都
   `initialLayout=COLOR_ATTACHMENT_OPTIMAL`，首帧/resize 无 barrier。→ clear pass 用 `UNDEFINED`，load pass 保持。
6. **FS 描述符不可见**（`render/linux/pipeline.rs:45-49`）：binding0 仅 `VERTEX`，`quad.frag.glsl:12` 在 FS 读 instances。→
   `VERTEX | FRAGMENT`。
7. **OUT_OF_DATE 永久跳帧**（`render/linux/compositor.rs:139,189-192`）：同尺寸时 resize 早退。→ OUT_OF_DATE 直接重建，或
   resize 不短路。
8. **frame callback 请求时机**（`platform/linux/event_loop.rs:116-118` + `surface.rs:266`）：在 `queue_present`（唯一
   commit）之后 request，`Done` 不来、首帧后停摆。→ submit/present **之前** request_frame。
9. **平台事件后不唤醒**（`platform/linux/event_loop.rs:86-88`、`platform/macos/event_loop.rs:162-168`）：输入/resize/IME 后不
   `dispatcher.wake()`（Win32 每条消息都 wake）；动画 timer/timeout 也不 wake。→ 事件后 wake，tick 也 wake。
10. **每轮无条件 set_scale/set_size**（`platform/linux/event_loop.rs:90-98`）：`full_damage_pending`、`scale_epoch++`、根
    LAYOUT、a11y 每轮失效。→ 仅值变化时设置（对照 macOS 快照）。

### macOS AppKit / Metal

11. **关闭判定错误**（`platform/macos/window.rs:223-228`）：`!isVisible` 把最小化/隐藏/orderOut 当关闭。→ 排除
    `isMiniaturized`/`NSApp.isHidden` 或显式关闭标志。
12. **RefCell 跨 raise**（`platform/macos/view.rs:374-387` + `a11y.rs:64-74`）：`borrow_mut` 跨 `events.raise()`，AX 重入
    double-borrow panic。→ 先取走/drop borrow，再 raise。
13. **CFRunLoop mode 为 None**（`platform/macos/wake.rs:58,120-122`）：`add_source/add_timer/remove_timer` 传 `None`，CF
    要求非空。→ 传 `kCFRunLoopDefaultMode`/`CommonModes`。
14. **run loop 退出与 UAF**（`platform/macos/event_loop.rs:226-265,134`）：回调内 `exit=true` 只在 Err 分支 `stop()`；Ok 分支不
    stop ⇒ 关最后一窗后卡死；run 返回后 source/WAKE 未卸载，`Box::from_raw` 后回调可 UAF。→ 回调后按 exit stop；退出前
    remove+invalidate+WAKE=0。
15. **Metal 错误码**（`render/macos/device.rs:189-192`）：写成 11/12；实际 `AccessRevoked=4`、`StackOverflow=12`。→ 用常量并校验
    domain。

## 非阻塞（优先处理）

- Linux：readback 在 submit 前 map（test-only 恒脏）；空实例 descriptor `range=0`；resize 未 `queue_wait_idle`；device-lost 不进
  sticky；atlas 局部上传 `image_offset`；退出时 WaylandState 先于 Renderer drop；`apply_min_size` 物理值；`sync_viewport` 的
  buffer_scale+commit；IME content_type/cursor/commit 去重；clipboard serial。
- macOS：keyDown 先 Key 后 IME、commit 后重复 Text、flagsChanged 发 Key、滚轮 Y、NSTrackingArea/AppearanceChanged、
  `note_commit` 去重、`firstRect` actualRange、`presentDrawable` 时机、drawable None 仍提交、D8 maxBufferLength。
- 文档：S-124 与 F-137 的矛盾（mac 真读回/Linux 返回 `Gpu`）；`present_dirty` 文档；Linux GpuCaps 缺
  wayland_surface/swapchain 字段；`Cargo.toml` 空行。

## 修复后门禁

```powershell
$env:ui_core_SKIP_SHADERS='1'
cargo check -p ui_core --target aarch64-apple-darwin
# linux: stub pkg-config
cargo check -p ui_core --target x86_64-unknown-linux-gnu
cargo check -p ui_core --target aarch64-apple-darwin --features test-support
cargo test -p ui_core
cargo test -p ui_core --features test-support
cargo +nightly clippy -p ui_core -p ui_core_demo --all-targets --all-features -- -D warnings
cargo +nightly fmt --all -- --check
```
