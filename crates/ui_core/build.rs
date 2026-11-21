//! `ui_core` 的构建脚本：编译三套 shader + 构建期契约静态校验。
//!
//! 契约：`src/render/shaders/contract.md`。
//!
//! - 零依赖（std only）：不引 `regex`（手写扫描器）、不引 `sha2`（Cargo 的 fingerprint 就是缓存）。
//! - 只编译本平台：Windows → `dxc`（DXIL）、macOS → `xcrun metal`（metallib）、Linux → `glslangValidator`（SPIR-V）。
//! - `ui_core_SKIP_SHADERS=1` 或 host ≠ target 时只写**占位产物**（P09 的 `include_bytes!` 仍可解析）。
//! - Windows 目标额外把**本仓自带**的进程清单（`RT_MANIFEST`，DPI awareness = PerMonitorV2，
//!   R07-1）编进 `.res` 并链接到每个链接本 crate 的可执行文件 —— 不再依赖 `gpui-pre` 之类
//!   依赖的资源合并副作用；清单内容与 `1 RT_MANIFEST` 锚点在构建期静态校验。
//! - **迁移过渡措施**（P5 删除 `gpui-kit` 后连同开关一起删除）：设置
//!   `KYTRIX_UI_CORE_MANIFEST=0` 时跳过清单的 **rc 编译与链接**（清单存在性、XML/RC
//!   锚点、MSVC 目标与 rerun 校验照旧），供仍依赖 `gpui-pre`（自带 manifest）的 crate
//!   在 Windows 上链接测试二进制；见
//!   `.docs/gui-migration/spec-deltas/build-transition.md`。

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

/// 契约里的 15 个源文件（相对 `src/render/shaders`）。静态校验与 `rerun-if-changed` 的唯一清单。
const SOURCES: [&str; 15] = [
    "common.hlsl",
    "quad.hlsl",
    "geometry.hlsl",
    "common.metal",
    "quad.metal",
    "geometry.metal",
    "common.glsl",
    "common.frag.glsl",
    "quad.vert.glsl",
    "quad.frag.glsl",
    "geometry.ui.vert.glsl",
    "geometry.textured.vert.glsl",
    "geometry.ui.frag.glsl",
    "geometry.textured_image.frag.glsl",
    "geometry.textured_glyph.frag.glsl",
];

/// 每套语言里声明共享结构的文件。
const COMMON_FILES: [&str; 3] = ["common.hlsl", "common.metal", "common.glsl"];

/// `paint/instance.rs` 中的字段顺序（唯一事实源；三套 shader 必须一致）。
const GPU_FIELDS: [&str; 8] = [
    "bounds",
    "corner_radius",
    "color0",
    "color1",
    "params0",
    "params1",
    "uv",
    "clip_rect",
];
const CLIP_FIELDS: [&str; 3] = ["rect", "corner_radius", "meta"];
const FRAME_FIELDS: [&str; 2] = ["viewport", "params"];
const DRAW_FIELDS: [&str; 5] = ["basis", "translation", "color0", "clip_rect", "clip"];

/// 与 `paint/instance.rs` 的 `const NAME: f32` 逐一比对的常量（去掉 `KYTRIX_` 前缀）。
///
/// 覆盖**全部**跨端数值契约：`kind` 取值、clip 项类型 / 链上界、阴影 σ（= blur/2 及其下限）。
/// AR2 P1-⑤：这张表漏掉任何一个，单端改动都会静默通过（实验 E4/E10b/E15/E16/E13 的教训）。
///
/// D-9-c 起 `params1.y` 不再是「选哪张图集」（`TEXTURE_GLYPH/TEXTURE_IMAGE` 已删），而是
/// 图集页（texture array 层号）：采样哪张图集由 `params1.z` 的 `KIND_*` 决定；层号取值由
/// `paint/atlas.rs` 的 `MAX_IMAGE_PAGES` 约束（shader 不读该常量，故不进本表）。
const CONSTANTS: [&str; 13] = [
    "KIND_QUAD",
    "KIND_GLYPH",
    "KIND_IMAGE",
    "KIND_SHADOW",
    "GRADIENT_NONE",
    "GRADIENT_HORIZONTAL",
    "GRADIENT_VERTICAL",
    "GRADIENT_RADIAL",
    "CLIP_KIND_RECT",
    "CLIP_KIND_ROUNDED",
    "MAX_CLIP_ITEMS",
    "SHADOW_SIGMA_SCALE",
    "SHADOW_SIGMA_MIN",
];

/// 只在 shader 侧存在的常量：Rust 侧没有同名常量（只被 shader 读），因此三方比对
/// **只比三份源码之间**。漏掉它们时单端分叉同样静默通过（AR2 实验 E10b / E15）。
const SHADER_ONLY_CONSTANTS: [&str; 2] = ["KYTRIX_CLIP_VERTEX_SLACK", "KYTRIX_AA_INSIDE"];

/// 禁止出现在任何 shader 源码里的位重解释构造（contract §9）。
const FORBIDDEN: [&str; 6] = [
    "asuint(",
    "asfloat(",
    "as_type<",
    "floatBitsToInt",
    "floatBitsToUint",
    "bitcast",
];

/// `contract.md` 必须包含的锚点（防止契约文档被删改而构建仍通过）。
const CONTRACT_ANCHORS: [&str; 7] = [
    "TriangleStrip",
    "cull = NONE",
    "ONE_MINUS_SRC_ALPHA",
    "_UNORM_SRGB",
    "aa_padding",
    "size_of == 128",
    "instance_base",
];

/// 三套 uber-quad 顶点着色器必须都按 `draw.clip.y` 加实例基址（P0-1.1 的防回归锚点）。
///
/// 三个图形 API 的硬件实例基址语义不同：D3D12 的 `SV_InstanceID` **不含** `StartInstanceLocation`，
/// 而 Vulkan 的 `gl_InstanceIndex` / Metal 的 `instance_id` **含** `firstInstance` / `baseInstance`。
/// 所以三平台都必须把硬件基址传 0、只经 uniform 加偏移；任一 shader 回退到硬件基址都会静默错位。
const INSTANCE_BASE_ANCHORS: [(&str, &str); 3] = [
    ("quad.hlsl", "instances[instance_id + base]"),
    ("quad.vert.glsl", "instances[instance]"),
    ("quad.metal", "instances[instance]"),
];

/// D3D12 的 7 个 entry：(源文件, profile, 入口名, 产物名)。
const D3D12_ENTRIES: [(&str, &str, &str, &str); 7] = [
    ("quad.hlsl", "vs_6_2", "vs_main", "quad.vs.dxil"),
    ("quad.hlsl", "ps_6_2", "ps_main", "quad.ps.dxil"),
    ("geometry.hlsl", "vs_6_2", "vs_ui", "geometry.vs_ui.dxil"),
    (
        "geometry.hlsl",
        "vs_6_2",
        "vs_textured",
        "geometry.vs_textured.dxil",
    ),
    ("geometry.hlsl", "ps_6_2", "ps_ui", "geometry.ps_ui.dxil"),
    (
        "geometry.hlsl",
        "ps_6_2",
        "ps_textured_image",
        "geometry.ps_textured_image.dxil",
    ),
    (
        "geometry.hlsl",
        "ps_6_2",
        "ps_textured_glyph",
        "geometry.ps_textured_glyph.dxil",
    ),
];

/// Vulkan 的 7 个 stage 文件 → SPIR-V 产物（入口恒为 `main`）。
const VULKAN_ENTRIES: [(&str, &str); 7] = [
    ("quad.vert.glsl", "quad.vs.spv"),
    ("quad.frag.glsl", "quad.ps.spv"),
    ("geometry.ui.vert.glsl", "geometry.vs_ui.spv"),
    ("geometry.textured.vert.glsl", "geometry.vs_textured.spv"),
    ("geometry.ui.frag.glsl", "geometry.ps_ui.spv"),
    (
        "geometry.textured_image.frag.glsl",
        "geometry.ps_textured_image.spv",
    ),
    (
        "geometry.textured_glyph.frag.glsl",
        "geometry.ps_textured_glyph.spv",
    ),
];

/// Metal：每个文件一个 metallib（内含该族的全部入口）。
const METAL_LIBS: [(&str, &str); 2] = [
    ("quad.metal", "quad.metallib"),
    ("geometry.metal", "geometry.metallib"),
];

/// R07-1：本仓自带的进程清单（`RT_MANIFEST`）与其 rc 包装。
///
/// 清单只声明「期望的 awareness」，实际值由 OS / 宿主决定，所以运行期还会再校验一次
/// （`src/platform/windows/dpi_awareness.rs`）。这里要守的是：清单必须**本仓拥有** ——
/// 依赖（`gpui-pre` 的默认特性 `windows-manifest`）带进来的清单只是副作用，依赖一换就没了。
const MANIFEST_RC: &str = "src/platform/windows/ui_core_manifest.rc";
const MANIFEST_XML: &str = "src/platform/windows/ui_core.manifest.xml";

/// 清单里缺失即构建失败的锚点：老/新 Windows 读不同的元素（1607 及更早读 `dpiAware`，
/// 1703+ 读 `dpiAwareness`），少一条就会在对应版本上静默退回 96 dpi。
const MANIFEST_XML_ANCHORS: [&str; 2] = [">true/pm</dpiAware>", ">PerMonitorV2</dpiAwareness>"];

/// rc 侧锚点：清单必须是 `RT_MANIFEST` 的 **ID 1**（`CREATEPROCESS_MANIFEST_RESOURCE_ID`）。
/// ID 不对时资源不会生效，但构建仍然成功 —— 所以必须静态校验。
const MANIFEST_RC_ANCHOR: &str = "1 RT_MANIFEST";

/// 迁移过渡开关：值为 `0` 时跳过清单的 **rc 编译与链接**（其它值/未设置 = 不启用）。
///
/// 迁移期 `base_ui` / `widgets/*_ui` 仍依赖 `gpui-pre`，而它的 `gpui.lib` 也带
/// `RT_MANIFEST` ID 1；同一个 exe 拿到两份清单时 `link.exe` 报
/// `CVT1100: duplicate resource. type:MANIFEST, name:1`。本开关只影响 `.res` 的
/// 编译与 `rustc-link-*` 指令；rc/xml 存在性、锚点、MSVC 与 rerun 校验一律照旧。
///
/// **迁移过渡措施**：P5 删除 `gpui-kit` / `gpui-pre` 后，连同开关与
/// `xtask` 的注入规则一起删除（`.docs/gui-migration/spec-deltas/build-transition.md`）。
const TRANSITION_MANIFEST_ENV: &str = "KYTRIX_UI_CORE_MANIFEST";

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let shader_dir = manifest.join("src/render/shaders");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("shaders");

    emit_rerun(&manifest, &shader_dir);
    validate(&manifest, &shader_dir);
    embed_manifest(&manifest);
    build(&shader_dir, &out_dir);
}

// ---------------------------------------------------------------------------
// 构建触发
// ---------------------------------------------------------------------------

fn emit_rerun(manifest: &Path, shader_dir: &Path) {
    for source in SOURCES.iter().chain(["contract.md"].iter()) {
        println!(
            "cargo:rerun-if-changed={}",
            shader_dir.join(source).display()
        );
    }
    // 静态校验读的**全部** Rust 文件都必须登记，否则增量构建会放过漂移：
    // - 契约常量 / 结构体布局：`paint/instance.rs`、`paint/scene.rs`；
    // - 渲染状态锚点：`RENDER_STATE_ANCHORS` / `RENDER_STATE_FORBIDDEN` 指向的三端
    //   `{pipeline,swapchain}.rs` 与共享层 `colorspace.rs`。这些文件既不在 rerun 清单里、
    //   也不被 `include!` 进 crate，漏登记 ⇒ 改 blend/cull 后 build script 不重跑、门禁静默
    //   （AR2b P2-N3 复现：改 `SrcBlend` 后 BUILD-PASS）。
    //
    // 清单**从锚点表本身汇聚**，不另立一份手写列表（那正是漂移的来源）。
    let mut watched: Vec<&str> = vec!["build.rs", "src/paint/instance.rs", "src/paint/scene.rs"];
    watched.extend(RENDER_STATE_ANCHORS.iter().map(|(file, _, _)| *file));
    watched.extend(RENDER_STATE_FORBIDDEN.iter().map(|(file, _, _)| *file));
    watched.sort_unstable();
    watched.dedup();
    for source in watched {
        println!("cargo:rerun-if-changed={}", manifest.join(source).display());
    }
    println!("cargo:rerun-if-env-changed=ui_core_SKIP_SHADERS");
    println!("cargo:rerun-if-env-changed=KYTRIX_DXC");
}

// ---------------------------------------------------------------------------
// 进程清单（R07-1；只对 Windows 目标）
// ---------------------------------------------------------------------------

/// 把本仓自带的进程清单编成 `.res` 并让**最终 exe** 链接它（R07-1）。
///
/// 传播链：`cargo:rustc-link-search` + `cargo:rustc-link-lib` 写进 ui_core 的 rlib 元数据，
/// 依赖本 crate 的每个可执行文件在链接时都会带上这条 `-l`（`gpui-pre` 的 `gpui.lib` 正是这样
/// 把它的清单带进 exe 的 —— 本函数把同一机制变成**本仓声明**）。
///
/// **迁移过渡措施**：`KYTRIX_UI_CORE_MANIFEST=0` 时跳过清单的 rc 编译与链接（下游 exe 同时
/// 链接 `gpui-pre` 的 manifest 会 `CVT1100`），但清单存在性 / 锚点校验与 rerun 触发照旧。
/// P5 删除 `gpui-kit` 后删除该开关（`.docs/gui-migration/spec-deltas/build-transition.md`）。
fn embed_manifest(project: &Path) {
    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }
    println!("cargo:rerun-if-env-changed={TRANSITION_MANIFEST_ENV}");

    let rc_file = project.join(MANIFEST_RC);
    let xml_file = project.join(MANIFEST_XML);
    println!("cargo:rerun-if-changed={}", rc_file.display());
    println!("cargo:rerun-if-changed={}", xml_file.display());

    let xml = read(&xml_file);
    for anchor in MANIFEST_XML_ANCHORS {
        if !xml.contains(anchor) {
            panic!(
                "\nui_core: 进程清单漂移（R07-1）\n  文件:   {MANIFEST_XML}\n  缺失:   {anchor}\n  \
                 修复:   DPI awareness 必须是 PerMonitorV2，且老/新 Windows 两条声明都要有；\
                 否则窗口跨屏时收不到每屏 DPI\n"
            );
        }
    }
    if !read(&rc_file).contains(MANIFEST_RC_ANCHOR) {
        panic!(
            "\nui_core: 进程清单漂移（R07-1）\n  文件:   {MANIFEST_RC}\n  缺失:   {MANIFEST_RC_ANCHOR}\n  \
             修复:   进程清单必须写成 `1 RT_MANIFEST`（ID ≠ 1 的资源不会被系统当作进程清单）\n"
        );
    }

    // `rc.exe` 只覆盖 MSVC 目标（`windows-gnu` 走 windres，本仓构建矩阵没有这一格）。
    // 宁可构建失败，也不要静默产出一个没有清单的 exe。
    if env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() != "msvc" {
        panic!(
            "ui_core: R07-1 的进程清单只支持 MSVC 目标（需要 rc.exe）。\n  \
             当前 CARGO_CFG_TARGET_ENV 不是 msvc；请改用 *-pc-windows-msvc 目标。"
        );
    }

    // 迁移过渡期：清单本身仍然校验，只是不把它塞进下游 exe（`gpui-pre` 自带一份，
    // 同时链接就是 CVT1100）。产物只服务于被跳过的链接，故连 rc 编译一起跳过。
    if env::var(TRANSITION_MANIFEST_ENV).is_ok_and(|value| value == "0") {
        println!(
            "cargo:warning=ui_core: 过渡期跳过进程清单链接（KYTRIX_UI_CORE_MANIFEST=0；与 gpui-pre manifest 冲突，P5 后移除）"
        );
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let artifact = out_dir.join("ui_core_manifest.lib");
    let rc_tool = find_rc().unwrap_or_else(|| {
        panic!(
            "ui_core: 找不到 rc.exe（把进程清单编进 exe，R07-1）。\n  \
             已尝试：%WindowsSdkDir%\\bin\\10.0.*\\<arch>\\rc.exe、\
             %ProgramFiles(x86)%\\Windows Kits\\10\\bin\\10.0.*\\<arch>\\rc.exe、PATH\n  \
             安装 Windows SDK 10.0.26100+，或设置 KYTRIX_RC=<rc.exe 的绝对路径>。"
        )
    });
    let output = Command::new(&rc_tool)
        .args(["/nologo", "/fo"])
        .arg(&artifact)
        .arg("/I")
        .arg(project.join("src/platform/windows"))
        .arg(&rc_file)
        .current_dir(project)
        .output();
    report(output, "rc.exe（进程清单）");

    // 产物是 `.res` 格式（**不是** COFF 库，只是沿用 `.lib` 扩展名给 link.exe 当输入）：
    // 32 字节空头，首两个 dword 恒为 `0` / `0x20`。
    let bytes = fs::read(&artifact)
        .unwrap_or_else(|err| panic!("无法读取进程清单产物 {}：{err}", artifact.display()));
    if !bytes.starts_with(b"\0\0\0\0\x20\0\0\0") {
        panic!(
            "ui_core: rc.exe 没有产出 .res 资源（magic 不符）：{}",
            artifact.display()
        );
    }

    // 注意：同一条 `-l` 若被两个清单资源满足，`link.exe` 会以
    // `CVT1100: duplicate resource. type:MANIFEST, name:1` 硬失败 —— 这正是「不要把清单
    // 重复塞进同一个 exe」的机器证明（见 `.review` 报告里的 gpui-pre 冲突结论）。
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=dylib=ui_core_manifest");
}

// ---------------------------------------------------------------------------
// 静态校验（永远执行，不受 SKIP 影响）
// ---------------------------------------------------------------------------

fn validate(manifest: &Path, shader_dir: &Path) {
    let instance_path = manifest.join("src/paint/instance.rs");
    let instance_src = strip_comments(&read(&instance_path));

    // 1) Rust 侧参照：字段顺序 + 类型。
    let rust_fields = rust_struct(&instance_src, "GpuPrimitive", "src/paint/instance.rs");
    let rust_names: Vec<&str> = rust_fields.iter().map(|(_, name)| name.as_str()).collect();
    if rust_names != GPU_FIELDS {
        fail(
            "src/paint/instance.rs",
            "GpuPrimitive",
            &format!("{GPU_FIELDS:?}"),
            &format!("{rust_names:?}"),
        );
    }
    if let Some((ty, _)) = rust_fields.iter().find(|(ty, _)| ty != "[f32; 4]") {
        fail("src/paint/instance.rs", "GpuPrimitive", "[f32; 4]", ty);
    }

    // 2) 三套 common.* 的结构体字段顺序。
    let mut shader_sources = Vec::new();
    for file in COMMON_FILES {
        let src = strip_comments(&read(&shader_dir.join(file)));
        check_struct(file, &src, "GpuPrimitive", &GPU_FIELDS, &["float4", "vec4"]);
        check_struct(
            file,
            &src,
            "ClipEntry",
            &CLIP_FIELDS,
            &["float4", "vec4", "uint4", "uvec4"],
        );
        check_struct(
            file,
            &src,
            "FrameUniform",
            &FRAME_FIELDS,
            &["float4", "vec4"],
        );
        check_struct(
            file,
            &src,
            "GeometryDrawUniform",
            &DRAW_FIELDS,
            &["float4", "vec4", "uint4", "uvec4"],
        );
        shader_sources.push((file, defines(&src)));
    }

    // 3) 常量：三套源码一致 + 与 Rust 一致。
    let instance_consts = rust_consts(&instance_src);
    for name in CONSTANTS {
        let key = format!("KYTRIX_{name}");
        let expected = const_value(&instance_consts, name, "src/paint/instance.rs");
        for (file, values) in &shader_sources {
            let actual = const_value(values, &key, file);
            if actual != expected {
                fail(
                    file,
                    &key,
                    &format!("{expected}（Rust {name}）"),
                    &actual.to_string(),
                );
            }
        }
    }

    // 4) MAX_CLIP_DEPTH：三套源码 == 8 == Rust `paint/scene.rs`。
    let scene_src = strip_comments(&read(&manifest.join("src/paint/scene.rs")));
    let scene_consts = rust_consts(&scene_src);
    let rust_depth = const_value(&scene_consts, "MAX_CLIP_DEPTH", "src/paint/scene.rs");
    for (file, values) in &shader_sources {
        let depth = const_value(values, "KYTRIX_MAX_CLIP_DEPTH", file);
        if depth != rust_depth {
            fail(
                file,
                "KYTRIX_MAX_CLIP_DEPTH",
                &format!("{rust_depth}（Rust paint/scene.rs）"),
                &depth.to_string(),
            );
        }
        // 4b) 链上界与预算的关系：KYTRIX_MAX_CLIP_ITEMS = KYTRIX_MAX_CLIP_DEPTH + 1
        //（链上至多一条矩形项 + MAX_CLIP_DEPTH 条圆角项；AR2 P1-A / P1-⑥）。
        let items = const_value(values, "KYTRIX_MAX_CLIP_ITEMS", file);
        if items != depth + 1.0 {
            fail(
                file,
                "KYTRIX_MAX_CLIP_ITEMS",
                &format!("{}（= KYTRIX_MAX_CLIP_DEPTH + 1）", depth + 1.0),
                &items.to_string(),
            );
        }
    }

    // 4c) 只在 shader 侧存在的常量：三份源码必须逐值相同（Rust 侧没有参照）。
    for name in SHADER_ONLY_CONSTANTS {
        let mut reference: Option<(&str, f32)> = None;
        for (file, values) in &shader_sources {
            let value = const_value(values, name, file);
            match reference {
                None => reference = Some((file, value)),
                Some((expected_file, expected)) if value != expected => fail(
                    file,
                    name,
                    &format!("{expected}（同 {expected_file}）"),
                    &value.to_string(),
                ),
                Some(_) => {}
            }
        }
    }

    // 5) 禁用 token（位重解释）。
    for file in SOURCES {
        let src = read(&shader_dir.join(file));
        for token in FORBIDDEN {
            if src.contains(token) {
                fail(
                    file,
                    "禁用 token",
                    "不得出现（禁止位重解释，contract §9）",
                    token,
                );
            }
        }
    }

    // 6) contract.md 锚点。
    let contract = read(&shader_dir.join("contract.md"));
    for anchor in CONTRACT_ANCHORS {
        if !contract.contains(anchor) {
            fail("contract.md", "契约锚点", anchor, "缺失");
        }
    }

    // 7) 实例基址：三套 quad 顶点着色器都必须经 uniform 加偏移，且都读 `draw.clip.y`。
    for (file, anchor) in INSTANCE_BASE_ANCHORS {
        let src = read(&shader_dir.join(file));
        if !src.contains(anchor) {
            fail(
                file,
                "实例基址",
                &format!("包含 `{anchor}`（经 draw.clip.y 加偏移）"),
                "找不到（回退到硬件 baseInstance 会三平台分叉）",
            );
        }
        if !src.contains("draw.clip.y") && !src.contains("root.draw.clip.y") {
            fail(
                file,
                "实例基址",
                "读取 `draw.clip.y` 作为 instance_base",
                "找不到",
            );
        }
    }

    // 8) 数学函数体跨语言等价（R06-5 a）：归一化后逐语句比对，首个差异带文件与行号。
    validate_math_equivalence(shader_dir);

    // 9) 渲染状态 / 色彩空间的文本锚点（R06-5 c / R06-10）。
    validate_render_state(manifest);
}

/// 三套 shader 的数学函数体必须逐语句等价（只允许语言拼写差异）。
fn validate_math_equivalence(shader_dir: &Path) {
    let load = |files: &[&str]| {
        files
            .iter()
            .map(|file| ((*file).to_owned(), read(&shader_dir.join(file))))
            .collect::<Vec<_>>()
    };
    let hlsl = ShaderSource::new(
        Language::Hlsl,
        load(&["common.hlsl", "geometry.hlsl", "quad.hlsl"]),
    );
    let metal = ShaderSource::new(
        Language::Metal,
        load(&["common.metal", "geometry.metal", "quad.metal"]),
    );
    let glsl = ShaderSource::new(
        Language::Glsl,
        load(&[
            "common.glsl",
            "common.frag.glsl",
            "geometry.ui.vert.glsl",
            "geometry.ui.frag.glsl",
            "geometry.textured.vert.glsl",
            "geometry.textured_image.frag.glsl",
            "geometry.textured_glyph.frag.glsl",
            "quad.vert.glsl",
            "quad.frag.glsl",
        ]),
    );
    if let Err(mismatch) = compare_math_functions(&hlsl, &[&metal, &glsl]) {
        panic!(
            "\nui_core: 三套 shader 的数学函数体不一致（R06-5）{mismatch}\n  \
             修复: 三份源码必须逐语句等价（只允许语言拼写差异：类型名 / lerp↔mix / root. 前缀）\n  \
             契约: src/render/shaders/contract.md §4\n"
        );
    }
}

/// 渲染状态与呈现色彩空间的锚点：三端源码必须同时出现同一组取值。
///
/// 文本锚点而不是"读结构体值"：D3D12 / Metal / Vulkan 的状态枚举类型互不兼容，抽成共享常量
/// 会强迫三端共享一个抽象层（`render::state`），而真正要守的是「三端语义相同」这一条。
/// 锚点缺失即构建失败，因此任何一端被改成别的因子/格式都会立刻暴露（R06-5 c）。
const RENDER_STATE_ANCHORS: &[(&str, &str, &str)] = &[
    // ── Windows / D3D12 ──
    (
        "src/render/windows/pipeline.rs",
        "blend enable",
        "BlendEnable: true.into()",
    ),
    (
        "src/render/windows/pipeline.rs",
        "blend src",
        "SrcBlend: D3D12_BLEND_ONE",
    ),
    (
        "src/render/windows/pipeline.rs",
        "blend dst",
        "DestBlend: D3D12_BLEND_INV_SRC_ALPHA",
    ),
    (
        "src/render/windows/pipeline.rs",
        "blend op",
        "BlendOp: D3D12_BLEND_OP_ADD",
    ),
    (
        "src/render/windows/pipeline.rs",
        "alpha src",
        "SrcBlendAlpha: D3D12_BLEND_ONE",
    ),
    (
        "src/render/windows/pipeline.rs",
        "alpha dst",
        "DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA",
    ),
    (
        "src/render/windows/pipeline.rs",
        "alpha op",
        "BlendOpAlpha: D3D12_BLEND_OP_ADD",
    ),
    (
        "src/render/windows/pipeline.rs",
        "颜色写掩码",
        "RenderTargetWriteMask: 0x0F",
    ),
    (
        "src/render/windows/pipeline.rs",
        "cull",
        "CullMode: D3D12_CULL_MODE_NONE",
    ),
    (
        "src/render/windows/pipeline.rs",
        "fill",
        "FillMode: D3D12_FILL_MODE_SOLID",
    ),
    (
        "src/render/windows/pipeline.rs",
        "quad 拓扑",
        "D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP",
    ),
    (
        "src/render/windows/pipeline.rs",
        "RT 格式（sRGB）",
        "rtv_formats[0] = DXGI_FORMAT_B8G8R8A8_UNORM_SRGB",
    ),
    (
        "src/render/windows/pipeline.rs",
        "MSAA",
        "SampleDesc: DXGI_SAMPLE_DESC {",
    ),
    (
        "src/render/windows/pipeline.rs",
        "geometry Ui 覆盖率属性",
        "DXGI_FORMAT_R32_FLOAT, 0, 8",
    ),
    (
        "src/render/windows/swapchain.rs",
        "呈现面字节格式",
        "Format: DXGI_FORMAT_B8G8R8A8_UNORM",
    ),
    (
        "src/render/windows/swapchain.rs",
        "呈现色彩空间（R06-10）",
        "SetColorSpace1",
    ),
    // ── macOS / Metal ──
    (
        "src/render/macos/pipeline.rs",
        "blend enable",
        "attachment.setBlendingEnabled(true)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "blend src",
        "setSourceRGBBlendFactor(MTLBlendFactor::One)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "blend dst",
        "setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "blend op",
        "setRgbBlendOperation(MTLBlendOperation::Add)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "alpha src",
        "setSourceAlphaBlendFactor(MTLBlendFactor::One)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "alpha dst",
        "setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "alpha op",
        "setAlphaBlendOperation(MTLBlendOperation::Add)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "颜色写掩码",
        "setWriteMask(MTLColorWriteMask::All)",
    ),
    (
        "src/render/macos/pipeline.rs",
        "geometry Ui 顶点布局（12 B）",
        "Some(12)",
    ),
    (
        "src/render/macos/swapchain.rs",
        "呈现面格式（sRGB）",
        "MTLPixelFormat::BGRA8Unorm_sRGB",
    ),
    (
        "src/render/macos/swapchain.rs",
        "呈现色彩空间（R06-10）",
        "kCGColorSpaceSRGB",
    ),
    // ── Linux / Vulkan ──
    (
        "src/render/linux/pipeline.rs",
        "blend enable",
        ".blend_enable(true)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "blend src",
        ".src_color_blend_factor(vk::BlendFactor::ONE)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "blend dst",
        ".dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "blend op",
        ".color_blend_op(vk::BlendOp::ADD)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "alpha src",
        ".src_alpha_blend_factor(vk::BlendFactor::ONE)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "alpha dst",
        ".dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "alpha op",
        ".alpha_blend_op(vk::BlendOp::ADD)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "颜色写掩码",
        ".color_write_mask(vk::ColorComponentFlags::RGBA)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "cull",
        ".cull_mode(vk::CullModeFlags::NONE)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "fill",
        ".polygon_mode(vk::PolygonMode::FILL)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "quad 拓扑",
        ".topology(vk::PrimitiveTopology::TRIANGLE_STRIP)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "RT 格式（sRGB）",
        "vk::Format::B8G8R8A8_SRGB",
    ),
    (
        "src/render/linux/pipeline.rs",
        "MSAA",
        ".samples(vk::SampleCountFlags::TYPE_1)",
    ),
    (
        "src/render/linux/pipeline.rs",
        "geometry Ui 覆盖率属性",
        "12 => vec![position, coverage]",
    ),
    (
        "src/render/linux/swapchain.rs",
        "呈现色彩空间（R06-10）",
        "SRGB_NONLINEAR",
    ),
    // ── 共享层：色彩空间的唯一决定点 ──
    (
        "src/render/colorspace.rs",
        "唯一呈现色彩空间声明",
        "PRESENTED_COLOR_SPACE: PresentedColorSpace = PresentedColorSpace::SrgbRec709",
    ),
];

/// 不得出现的东西：某端偷偷加 cull / 换 fill 模式会让三端像素分叉。
const RENDER_STATE_FORBIDDEN: &[(&str, &str, &str)] = &[
    (
        "src/render/macos/pipeline.rs",
        "cull（Metal 默认 none，不得显式设置）",
        "setCullMode",
    ),
    (
        "src/render/macos/pipeline.rs",
        "fill 模式（Metal 默认 fill）",
        "setTriangleFillMode",
    ),
    (
        "src/render/windows/pipeline.rs",
        "cull 回退",
        "D3D12_CULL_MODE_BACK",
    ),
    (
        "src/render/linux/pipeline.rs",
        "cull 回退",
        "vk::CullModeFlags::BACK",
    ),
];

fn validate_render_state(manifest: &Path) {
    for (file, what, expected) in RENDER_STATE_ANCHORS {
        let contents = read(&manifest.join(file));
        if let Err(detail) = check_anchors(file, &contents, &[(what, expected)]) {
            panic!(
                "\nui_core: 渲染状态漂移（R06-5）\n  {detail}\n  \
                 修复: 三端渲染状态必须语义一致（contract.md §7）；改一处必须三端同步\n"
            );
        }
    }
    for (file, what, token) in RENDER_STATE_FORBIDDEN {
        let contents = read(&manifest.join(file));
        if let Err(detail) = check_forbidden(file, &contents, &[(what, token)]) {
            panic!(
                "\nui_core: 渲染状态漂移（R06-5）\n  {detail}\n  \
                 修复: 三端渲染状态必须语义一致（contract.md §7）\n"
            );
        }
    }
}

fn check_struct(file: &str, src: &str, name: &str, expected: &[&str], allowed_types: &[&str]) {
    let fields = c_struct(src, name, file);
    let names: Vec<&str> = fields.iter().map(|(_, field)| field.as_str()).collect();
    if names != expected {
        fail(file, name, &format!("{expected:?}"), &format!("{names:?}"));
    }
    if let Some((ty, _)) = fields
        .iter()
        .find(|(ty, _)| !allowed_types.contains(&ty.as_str()))
    {
        fail(file, name, &format!("{allowed_types:?}"), ty);
    }
}

// ---------------------------------------------------------------------------
// 编译
// ---------------------------------------------------------------------------

fn build(shader_dir: &Path, out_dir: &Path) {
    fs::create_dir_all(out_dir)
        .unwrap_or_else(|err| panic!("无法创建 {}：{err}", out_dir.display()));

    let target = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if skip_requested() || target != env::consts::OS {
        placeholders(out_dir, &target);
        return;
    }

    match target.as_str() {
        "windows" => build_windows(shader_dir, out_dir),
        "macos" => build_macos(shader_dir, out_dir),
        "linux" => build_linux(shader_dir, out_dir),
        other => {
            println!("cargo:warning=ui_core: 未知目标平台 {other}，shader 产物为占位符");
            placeholders(out_dir, other);
        }
    }
}

/// `ui_core_SKIP_SHADERS` 只认 `1` / `true`（大小写不敏感）；其它值视为未设置，并打印说明。
fn skip_requested() -> bool {
    match env::var("ui_core_SKIP_SHADERS") {
        Ok(value) if value == "1" || value.eq_ignore_ascii_case("true") => true,
        Ok(value) => {
            println!(
                "cargo:warning=ui_core: ui_core_SKIP_SHADERS={value} 无法识别（只认 1/true），按未设置处理"
            );
            false
        }
        Err(_) => false,
    }
}

fn build_windows(shader_dir: &Path, out_dir: &Path) {
    let dxc = find_dxc().unwrap_or_else(|| {
        panic!(
            "ui_core: 找不到 dxc.exe。\n  \
             已尝试：%WindowsSdkDir%\\bin\\10.0.*\\<arch>\\dxc.exe、\
             %ProgramFiles(x86)%\\Windows Kits\\10\\bin\\10.0.*\\<arch>\\dxc.exe、PATH\n  \
             安装 Windows SDK 10.0.26100+，或设置 KYTRIX_DXC=<dxc.exe 的绝对路径>；\n  \
             仅为跨 target 类型检查时可设 ui_core_SKIP_SHADERS=1（产物是占位符，不可运行）。"
        )
    });

    for (source, profile, entry, artifact) in D3D12_ENTRIES {
        let output = out_dir.join(artifact);
        let status = Command::new(&dxc)
            .args([
                "-T", profile, "-E", entry, "-HV", "2018", "-WX", "-nologo", "-Fo",
            ])
            .arg(&output)
            .arg(shader_dir.join(source))
            .output();
        report(status, &format!("dxc {entry} ({source})"));
        check_magic(&output, b"DXBC");
    }
}

fn build_macos(shader_dir: &Path, out_dir: &Path) {
    require_tool(
        "xcrun",
        "安装 Xcode Command Line Tools（`xcode-select --install`）",
    );

    for (source, artifact) in METAL_LIBS {
        let air = out_dir.join(format!("{source}.air"));
        let output = out_dir.join(artifact);
        let status = Command::new("xcrun")
            .args(["-sdk", "macosx", "metal", "-c"])
            .arg(shader_dir.join(source))
            .arg("-o")
            .arg(&air)
            .output();
        report(status, &format!("metal -c {source}"));
        let status = Command::new("xcrun")
            .args(["-sdk", "macosx", "metallib"])
            .arg(&air)
            .arg("-o")
            .arg(&output)
            .output();
        report(status, &format!("metallib {source}"));
        let _ = fs::remove_file(&air);
        check_non_empty(&output);
    }
}

fn build_linux(shader_dir: &Path, out_dir: &Path) {
    require_tool(
        "glslangValidator",
        "安装 glslang（Debian/Ubuntu: `apt install glslang-tools`）",
    );

    for (source, artifact) in VULKAN_ENTRIES {
        let output = out_dir.join(artifact);
        // glslang 的 `-I` **必须**紧贴目录（`-I<dir>`）；写成两个参数会直接报
        // `-I<dir> include path must immediately follow option (no spaces)` 并退出 1。
        let status = Command::new("glslangValidator")
            .args(["-V", "--target-env", "vulkan1.2"])
            .arg(format!("-I{}", shader_dir.display()))
            .arg("-o")
            .arg(&output)
            .arg(shader_dir.join(source))
            .output();
        report(status, &format!("glslangValidator {source}"));
        check_magic(&output, &[0x03, 0x02, 0x23, 0x07]);
    }
}

fn placeholders(out_dir: &Path, target: &str) {
    println!(
        "cargo:warning=ui_core: 跳过 shader 编译（ui_core_SKIP_SHADERS 或 host≠target），产物为占位符"
    );
    let names: Vec<&str> = match target {
        "windows" => D3D12_ENTRIES.iter().map(|entry| entry.3).collect(),
        "linux" => VULKAN_ENTRIES.iter().map(|entry| entry.1).collect(),
        "macos" => METAL_LIBS.iter().map(|entry| entry.1).collect(),
        _ => Vec::new(),
    };
    for name in names {
        fs::write(out_dir.join(name), b"KYTRIX-SHADER-SKIPPED\n")
            .unwrap_or_else(|err| panic!("无法写入占位产物：{err}"));
    }
}

fn find_dxc() -> Option<PathBuf> {
    if let Some(raw) = env::var_os("KYTRIX_DXC") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(path);
        }
        panic!("ui_core: KYTRIX_DXC 指向的文件不存在：{}", path.display());
    }

    find_windows_sdk_tool("dxc.exe").or_else(|| {
        let on_path = PathBuf::from("dxc.exe");
        Command::new(&on_path)
            .arg("--version")
            .output()
            .is_ok()
            .then_some(on_path)
    })
}

/// Windows SDK 的 `rc.exe`（资源编译器；编进程清单用，R07-1）。
///
/// `KYTRIX_RC` 可覆盖（与 `KYTRIX_DXC` 同规矩）：构建机把 SDK 装在非标准位置时用。
fn find_rc() -> Option<PathBuf> {
    if let Some(raw) = env::var_os("KYTRIX_RC") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(path);
        }
        panic!("ui_core: KYTRIX_RC 指向的文件不存在：{}", path.display());
    }

    find_windows_sdk_tool("rc.exe").or_else(|| {
        let on_path = PathBuf::from("rc.exe");
        Command::new(&on_path)
            .arg("/?")
            .output()
            .is_ok()
            .then_some(on_path)
    })
}

/// Windows SDK 里的宿主工具（`<root>/bin/<version>/<arch>/<tool>`，取版本号最大的一个）。
///
/// 按**宿主**架构选：`dxc.exe` / `rc.exe` 都在构建机上运行，产物与目标架构无关，
/// build script 的 `cfg!(target_arch)` 就是宿主架构。
fn find_windows_sdk_tool(tool: &str) -> Option<PathBuf> {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "x64"
    };
    let mut roots = Vec::new();
    if let Some(dir) = env::var_os("WindowsSdkDir") {
        roots.push(PathBuf::from(dir));
    }
    if let Some(dir) = env::var_os("ProgramFiles(x86)") {
        roots.push(PathBuf::from(dir).join("Windows Kits").join("10"));
    }
    if let Some(dir) = env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(dir).join("Windows Kits").join("10"));
    }
    roots
        .into_iter()
        .find_map(|root| newest_sdk_tool(&root, arch, tool))
}

/// 在 `<root>/bin/<version>/<arch>/<tool>` 中取版本号最大的一个。
fn newest_sdk_tool(root: &Path, arch: &str, tool: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root.join("bin")).ok()?;
    let mut versions: Vec<(Vec<u32>, PathBuf)> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?.to_owned();
            let key: Vec<u32> = name
                .split('.')
                .map(|part| part.parse().ok())
                .collect::<Option<Vec<u32>>>()?;
            Some((key, path))
        })
        .collect();
    versions.sort_by(|left, right| right.0.cmp(&left.0));
    versions
        .into_iter()
        .map(|(_, dir)| dir.join(arch).join(tool))
        .find(|candidate| candidate.is_file())
}

fn require_tool(tool: &str, hint: &str) {
    if Command::new(tool).arg("--version").output().is_err() {
        panic!("ui_core: 找不到 {tool}。{hint}；或设 ui_core_SKIP_SHADERS=1 只做类型检查。");
    }
}

fn report(output: std::io::Result<std::process::Output>, what: &str) {
    let output = output.unwrap_or_else(|err| panic!("ui_core: 无法启动编译器（{what}）：{err}"));
    if !output.status.success() {
        panic!(
            "ui_core: 着色器编译失败（{what}）\n  status: {}\n  stdout:\n{}\n  stderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn check_magic(path: &Path, magic: &[u8; 4]) {
    let bytes =
        fs::read(path).unwrap_or_else(|err| panic!("无法读取产物 {}：{err}", path.display()));
    if bytes.len() < magic.len() || bytes[..4] != *magic {
        panic!(
            "ui_core: 产物 magic 不符：{}（期望 {:02X?}）",
            path.display(),
            magic
        );
    }
}

fn check_non_empty(path: &Path) {
    let bytes =
        fs::read(path).unwrap_or_else(|err| panic!("无法读取产物 {}：{err}", path.display()));
    if bytes.len() < 16 {
        panic!("ui_core: 产物为空或过小：{}", path.display());
    }
}

// ---------------------------------------------------------------------------
// 文本解析（手写扫描器；无 regex）
// ---------------------------------------------------------------------------

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("无法读取 {}：{err}", path.display()))
}

/// 剥离 `//` 行注释（用空格替换、保留换行）。必须先做：`float4 bounds; // 112` 的分号会把注释粘到下一字段。
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        match line.find("//") {
            Some(index) => {
                out.push_str(&line[..index]);
                out.extend(
                    line[index..]
                        .chars()
                        .map(|ch| if ch == '\n' { '\n' } else { ' ' }),
                );
            }
            None => out.push_str(line),
        }
    }
    out
}

fn struct_body<'a>(src: &'a str, name: &str, file: &str) -> &'a str {
    let needle = format!("struct {name}");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| fail(file, name, "存在该结构体声明", "找不到"));
    let open = start
        + src[start..]
            .find('{')
            .unwrap_or_else(|| fail(file, name, "结构体带 `{`", "找不到"));
    let mut depth = 0i32;
    for (offset, ch) in src[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    fail(file, name, "结构体闭合", "未闭合")
}

/// C 系（HLSL / MSL / GLSL）：按 `;` 切分，每段「类型 字段名」。
fn c_struct(src: &str, name: &str, file: &str) -> Vec<(String, String)> {
    struct_body(src, name, file)
        .split(';')
        .map(str::trim)
        .filter(|decl| !decl.is_empty())
        .map(|decl| {
            let mut parts = decl.split_whitespace();
            let ty = parts
                .next()
                .unwrap_or_else(|| fail(file, name, "类型", "空声明"))
                .to_owned();
            let field = parts
                .next()
                .unwrap_or_else(|| fail(file, name, "字段名", decl))
                .to_owned();
            if parts.next().is_some() {
                fail(file, name, "两个 token 的字段声明", decl);
            }
            (ty, field)
        })
        .collect()
}

/// Rust：按 `,` 切分，剥 `pub `，按 `:` 切（`[f32; 4]` 里有分号，绝不能按 `;` 切）。
fn rust_struct(src: &str, name: &str, file: &str) -> Vec<(String, String)> {
    struct_body(src, name, file)
        .split(',')
        .map(str::trim)
        .filter(|decl| !decl.is_empty())
        .map(|decl| {
            let decl = decl.strip_prefix("pub ").unwrap_or(decl);
            let (field, ty) = decl
                .split_once(':')
                .unwrap_or_else(|| fail(file, name, "`字段: 类型`", decl));
            (ty.trim().to_owned(), field.trim().to_owned())
        })
        .collect()
}

fn defines(src: &str) -> Vec<(String, f32)> {
    src.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("#define ")?;
            let mut parts = rest.split_whitespace();
            let name = parts.next()?.to_owned();
            let value = number(parts.next()?)?;
            Some((name, value))
        })
        .collect()
}

fn rust_consts(src: &str) -> Vec<(String, f32)> {
    src.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line
                .strip_prefix("pub(crate) const ")
                .or_else(|| line.strip_prefix("const "))?;
            let (name, tail) = rest.split_once(':')?;
            let value = tail.split('=').nth(1)?.trim().trim_end_matches(';').trim();
            Some((name.trim().to_owned(), number(value)?))
        })
        .collect()
}

fn number(token: &str) -> Option<f32> {
    token
        .trim_end_matches(['u', 'U', 'f', 'F'])
        .parse::<f32>()
        .ok()
}

fn const_value(values: &[(String, f32)], name: &str, file: &str) -> f32 {
    values
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| *value)
        .unwrap_or_else(|| fail(file, name, "存在该常量", "找不到"))
}

fn fail(file: &str, item: &str, expected: &str, actual: &str) -> ! {
    panic!(
        "\nui_core: 着色器契约漂移\n  文件:   {file}\n  对象:   {item}\n  期望:   {expected}\n  实际:   {actual}\n  修复:   同时改 Rust 与三套源码，并跑像素快照回归（src/render/shaders/contract.md）\n"
    )
}

// 三套 shader 的构建期契约校验（归一化函数体比对 / 渲染状态锚点）的唯一实现：
// 与 lib 测试共用同一份源码（`src/render/shader_contract.rs`），`#[cfg(test)]` 的测试
// 在 build script 里不参与编译。只用 std，不得依赖 crate 内符号。
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/render/shader_contract.rs"
));
