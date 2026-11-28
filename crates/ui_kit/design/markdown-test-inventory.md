# markdown 模块测试清单（173 条逐条处置台账，可审计）

- 来源：`%TEMP%\markdown-impl\test-inventory.md`（2026-11-26 逐条清点：测试名 / `#[test]` 行号 / 断言摘要 / 分组）
- 关联设计：`markdown-redesign-design.md` §8.6（组级处置与总数）、§8.1（替代语料）、§8.2（outline 断言）、§8.4（门禁）
- 行号口径：`行` = `#[test]` 属性所在行；文件路径见各节标题（`crates/ui_kit/src/kit/markdown/` 下；`source.rs` 在 R2 迁移为 `markdown/assets.rs`）
- 总账：**173 条 = 保留 / 改写 101 + 删除 72**（R3 复核把 2 条改判删除，理由见各行的「落点」列与 §7；分组 51/62、16/3、3/2、11/5、20/0）
- 用途：R2 提交说明必须按本表**逐条勾选**（`#` 编号 + 测试名 + 处置）；删除项必须给出本表编号与替代落点，禁止静默丢弃

## 0. 记号

| 记号 | 含义 |
|---|---|
| 保留（断言不变） | 测试名与断言值都不变（可能随文件迁移） |
| 保留并改写 | 语义保留；断言改为「节点树轮廓 + 渲染图元」（§8.2）或新契约字面量（§6.3） |
| 落点（R3） | 该条断言在 R3 的落点测试名（`tests::`/`gates::`/`corpus::`/`plain::`/`assets::`）；「删除：…」表示 R3 复核后改判删除并给出理由 |
| 删除 | 依赖被删除的公开面 / IR / 行号编号；语义由「替代 / 新落点」承接 |
| R2 | 在 R2（组件全量替换）内完成 |
| R2/R3 | R2 删除测试，R3 用新语料 / 门禁承接同一语义 |

---

## 1. `parse_tests.rs`（113 条：保留并改写 51 / 删除 62）

组标签：**A**=嵌套上限(11)　**B**=`<p align>`/HTML 方言(34)　**C**=`<kbd>`(12)　**D**=图片(13)　**E**=空行(10)　**F**=IR 内部(7)　**G**=穷举 sweep(3)　**H**=块构造(14)　**I**=行内标记(6)　**J**=其他(3)

| # | 测试名 | 行 | 组 | 断言摘要 | 处置 | 替代 / 新落点 | 轮次 | 落点（R3） |
|---:|---|---:|:-:|---|---|---|------|---|
| 1 | `deeply_nested_emphasis_is_dropped_instead_of_overflowing_the_stack` | 267 | A | 旧断言：9600 个 `*` 的强调在超限时整段被丢弃、结果为空文档（不栈溢出）；R3 定稿改为**保留文字、只截断超限 marks** | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `tests::inline_nesting_beyond_the_cap_keeps_the_text_and_clips_the_marks` |
| 2 | `emphasis_nested_within_the_limit_is_kept` | 274 | A | 16 个 `*`（8 层）在限内 → 一个段落、文字 `a` 为 strong | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `tests::emphasis_nested_within_the_limit_is_kept` |
| 3 | `emphasis_nested_exactly_at_the_limit_is_kept` | 297 | A | 恰好 64 层（128 个 `*`）在上限 → 仍有 1 个段落且文字 `a` 存活 | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `tests::emphasis_nested_exactly_at_the_limit_is_kept` |
| 4 | `emphasis_nested_past_the_limit_is_dropped` | 314 | A | 旧断言：65 层（130 个 `*`）超限 → 空文档；R3 定稿改为**文字保留、超限 marks 截断** | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `ledger::emphasis_nested_past_the_limit_keeps_the_text_inside_the_cap` |
| 5 | `dropping_an_over_deep_nesting_keeps_its_siblings` | 326 | A | 旧断言：200 层强调被丢弃，但同段落兄弟文字 `keep ` 与 ` tail` 保留（只丢子树文字）；R3 定稿改为**文字全部保留、只丢超限 marks** | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `ledger::a_deep_emphasis_keeps_the_text_and_drops_the_excess_marks` |
| 6 | `headings_take_their_level_from_the_number_of_hashes` | 343 | H | `#`→H1、`######`→H6 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::headings_scale_with_their_level_and_stay_bold_and_left` |
| 7 | `seven_hashes_are_not_a_heading` | 355 | H | `#######` 不是标题，按普通段落读原文 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::seven_hashes_are_not_a_heading` |
| 8 | `plain_text_is_one_paragraph_and_no_links` | 363 | F | `plain` → 单段落，`links` 为空 | 删除 | §5.5 flush 判定 + §8.2 outline | R2/R3 | — |
| 9 | `strong_and_emphasis_are_piece_styles` | 371 | I | `**`/`*`/`_`/`***` 分别映射 strong、emphasis、二者叠加 | 保留并改写 | §8.1「行内」「删除线」 | R2 | `tests::inline_marks_cut_runs_and_leave_plain_text_runless`、`corpus::the_gfm_inline_corpus_emits_canonical_html` |
| 10 | `neighbouring_text_of_different_styles_is_not_merged` | 391 | F | 相邻不同样式文本不合并：`a**b**c`、`a*b*c`、`a**b**` 各成多个 piece | 删除 | §5.5 flush 判定 + §8.2 outline | R2/R3 | — |
| 11 | `link_text_carries_the_link_id_of_its_url` | 407 | I | `[x](u)` 的文本 piece 带 `LinkId(0)`，`links` 记录 URL | 删除 | §8.1「行内」「删除线」 | R2 | — |
| 12 | `link_style_stops_at_the_end_of_the_link` | 419 | I | 链接样式在 `]` 后停止，` tail` 是普通文本 | 保留并改写 | §8.1「行内」「删除线」 | R2 | `tests::link_style_stops_at_the_end_of_the_link` |
| 13 | `styles_inside_a_link_are_kept` | 431 | I | 链接内 `**b**` 同时保留 strong 与 link 标记 | 保留并改写 | §8.1「行内」「删除线」 | R2 | `tests::styles_inside_a_link_are_kept` |
| 14 | `a_kbd_pair_marks_the_pieces_between_its_tags` | 451 | C | `<kbd>` 对之间的文本带 `KbdId(0)`，前后文本不带 | 保留并改写 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | `tests::inline_kbd_and_br_cut_a_kbd_run_and_a_break` |
| 15 | `adjacent_kbd_pairs_are_separate_runs` | 463 | C | `<kbd>a</kbd><kbd>b</kbd>` → 两个独立 kbd run（id 0/1） | 删除 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | — |
| 16 | `nested_kbd_tags_share_the_outermost_run` | 471 | C | 嵌套 `<kbd>` 共用最外层 run，文字合并为 `abc` | 保留并改写 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | `tests::nested_kbd_tags_share_the_outermost_run` |
| 17 | `kbd_marks_the_inline_styles_inside_it` | 479 | C | kbd 内 `**bold**` 保留 strong+kbd；kbd 内的行内图片带 kbd | 保留并改写 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | `tests::kbd_marks_the_inline_styles_inside_it` |
| 18 | `a_link_inside_kbd_keeps_both_marks` | 505 | C | `<kbd>[x](u)</kbd>` 同时带 link 与 kbd | 删除 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | — |
| 19 | `unmatched_kbd_tags_follow_the_nodes_comrak_produces` | 525 | C | 落单 `</kbd>` 当文本（`a  b`）；未闭合 `<kbd>` 仍标到行尾 | 保留并改写 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | `tests::unmatched_kbd_tags_follow_the_parsed_tags` |
| 20 | `only_the_exact_kbd_tags_count` | 534 | C | `<KBD>`、`<kbd class="k">` 不算 kbd，文字保持纯文本 | 保留并改写 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | `tests::kbd_is_exact_and_may_wrap_an_image` |
| 21 | `an_escaped_kbd_tag_stays_plain_text` | 545 | C | `\<kbd>` 转义 → 文本为字面量 `<kbd>x</kbd>` | 删除 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | — |
| 22 | `an_empty_kbd_pair_produces_no_piece` | 553 | C | `<kbd></kbd>` 不产生 piece、结果为空文档 | 删除 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | — |
| 23 | `a_soft_break_inside_kbd_keeps_the_run_number_on_both_lines` | 558 | C | kbd 内软换行：两行文本共用同一 kbd run 编号，中间为 LineBreak | 保留并改写 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | `tests::a_soft_break_inside_kbd_keeps_one_run` |
| 24 | `a_link_without_content_is_not_recorded` | 570 | F | `[]()` → 空文档，不记录链接 | 删除 | §5.5 flush 判定 + §8.2 outline | R2/R3 | — |
| 25 | `a_file_image_becomes_a_piece` | 575 | D | `file:` 图片成 InlineImage piece，格式按扩展名、尺寸按 `?size` | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `assets::the_scheme_is_file_and_the_slashes_are_optional` |
| 26 | `an_image_without_a_size_takes_the_asset_size` | 591 | D | 无 `size` 时取资源内禀尺寸 40×30 | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `assets::a_missing_size_resolves_to_the_asset_size` |
| 27 | `a_type_overrides_the_extension` | 603 | D | `?type=` 覆盖扩展名（svg/img），带引号亦可 | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `assets::type_overrides_the_extension_and_may_be_quoted` |
| 28 | `a_size_may_carry_one_number_or_two` | 627 | D | `size=24` → 24×24；`size=24,48` → 24×48 | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `assets::size_takes_one_number_for_both_edges_or_two_for_width_and_height` |
| 29 | `a_path_with_at_signs_or_non_ascii_survives_the_markdown_parser` | 643 | D | 含 `@` 与非 ASCII 的路径原样到达 IR | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `assets::the_asset_path_is_taken_verbatim` |
| 30 | `a_quoted_size_survives_the_markdown_parser` | 659 | D | `?size="24,48"` 引号形式被正确解析 | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `assets::size_may_be_quoted_and_lives_anywhere_in_the_query` |
| 31 | `a_very_long_asset_path_reaches_the_ir_verbatim` | 671 | D | 200 段超长路径（带/不带 size）原样保留 | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `assets::the_asset_path_is_taken_verbatim` |
| 32 | `an_image_inside_a_link_carries_the_link` | 685 | D | 图片被链接包裹时图片 piece 带 LinkId | 删除 | §6.4 公开面删除（见落点理由） | R2 | 删除：§6.4 删除 `LinkId`：`InlineBoxSpec` 不承载链接区间，旧断言「图片 piece 带 LinkId」在新架构不可观察（D9 无交互） |
| 33 | `an_unusable_size_falls_back_to_the_asset_size` | 703 | D | 6 种非法 `size` → 回退内禀尺寸 | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `assets::an_unusable_size_falls_back_to_the_intrinsic_one` |
| 34 | `an_explicit_size_is_clamped_to_the_supported_range` | 720 | D | `size=0`→1、`size=9999`→256（夹紧） | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `assets::an_explicit_size_is_clamped` |
| 35 | `unusable_images_leave_no_block_behind` | 732 | D | 6 类不可用 URI（`icon:`、`https:`、`file://`、坏 type、重复 size、大写 `FILE`）整块丢弃 | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `tests::images_that_are_not_file_uris_fall_back_to_their_alt_text` |
| 36 | `an_ignored_image_still_splits_the_text_around_it` | 750 | D | 被忽略的图片仍把左右文本切成两个 piece | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `tests::an_ignored_image_still_splits_the_text_around_it` |
| 37 | `plain_text_skips_an_image_instead_of_reading_its_alt_text` | 758 | D | `plain_text` 跳过图片不读 alt：`a![Markets](…)b` → `ab` | 保留并改写 | §8.1「行内」（`file:` 图片与尺寸）+ `assets.rs` | R2 | `plain::the_legacy_plain_text_baseline_keeps_seventy_nine_literals_frozen` |
| 38 | `breaks_inside_a_paragraph_become_line_breaks` | 765 | E | 软换行与行尾两空格硬换行都产 LineBreak | 删除 | §8.4 门禁 3（spacer 形状）+ §8.1「块级」（空行重建） | R2/R3 | — |
| 39 | `two_hard_breaks_in_a_row_leave_a_blank_line_inside_the_block` | 777 | E | 两个 `\` 硬换行 → 块内两个 LineBreak，`plain_text` 得 `a\n\nb`；`<p align>` 内同样 | 删除 | §8.4 门禁 3（spacer 形状）+ §8.1「块级」（空行重建） | R2/R3 | — |
| 40 | `a_blank_line_starts_a_new_paragraph` | 798 | E | `a\n\nb` → 两段落 + 一个 BlankLine 块 | 删除 | §8.4 门禁 3（spacer 形状）+ §8.1「块级」（空行重建） | R2/R3 | — |
| 41 | `every_blank_line_is_kept` | 810 | E | 4 个换行 → 3 个 BlankLine 块 | 删除 | §8.4 门禁 3（spacer 形状）+ §8.1「块级」（空行重建） | R2/R3 | — |
| 42 | `only_the_blank_lines_of_the_source_count` | 824 | E | 列表前后空行按源计数，列表自身不合成空行 | 删除 | §8.4 门禁 3（spacer 形状）+ §8.1「块级」（空行重建） | R2/R3 | — |
| 43 | `a_line_of_spaces_is_a_blank_line` | 838 | E | 只含空格的行算空行 | 删除 | §8.4 门禁 3（spacer 形状）+ §8.1「块级」（空行重建） | R2/R3 | — |
| 44 | `blank_lines_at_the_edges_of_the_document_are_not_kept` | 850 | E | 文档首尾空行不保留 | 删除 | §8.4 门禁 3（spacer 形状）+ §8.1「块级」（空行重建） | R2/R3 | — |
| 45 | `blank_lines_are_counted_for_every_line_ending` | 864 | E | CRLF 与 CR 换行各算一个空行 | 删除 | §8.4 门禁 3（spacer 形状）+ §8.1「块级」（空行重建） | R2/R3 | — |
| 46 | `an_align_tag_pair_becomes_a_paragraph_with_its_alignment` | 879 | B | `<p align="center/right/left">` 与无属性 `<p>` → 段落对齐 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 47 | `an_align_tag_pair_keeps_its_place_between_other_blocks` | 900 | B | 对齐段落按源顺序位于前后段落之间（含空行） | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 48 | `the_inside_of_an_align_tag_pair_is_inline_markdown` | 914 | B | 对内的强调、链接、图片、kbd 都按行内 markdown 解析 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 49 | `the_inside_of_an_align_tag_pair_may_span_lines` | 940 | B | 对内跨行成 LineBreak，首尾换行被剥掉 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 50 | `an_align_tag_pair_may_span_blank_lines` | 952 | B | 对内空行 → 拆成多个同对齐段落 + BlankLine | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 51 | `blank_lines_inside_an_align_tag_pair_are_kept` | 964 | B | 对内 4 个换行 → 3 个 BlankLine | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 52 | `a_blank_line_in_a_tag_pairs_content_keeps_the_whole_message` | 978 | B | 生产形态 `are:\nBinance\n\nOKX` 不丢内容 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 53 | `an_align_tag_pair_may_span_several_paragraphs` | 994 | B | 对内多段落各自带该对的对齐 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 54 | `blank_lines_at_the_edges_of_a_tag_pair_are_not_kept` | 1006 | B | 对内容首尾多余空行不保留 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 55 | `blank_lines_inside_a_pair_are_counted_for_every_line_ending` | 1018 | B | `\r\r` 在对内也算一个空行 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 56 | `blank_lines_between_align_tag_pairs_are_kept` | 1030 | B | 两个对之间的空行保留为 BlankLine | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 57 | `a_pair_after_a_consumed_close_tag_is_still_drawn` | 1042 | B | `</p>` 被消费后紧跟的第二对仍被画出 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 58 | `an_open_tag_inside_an_html_block_is_a_pair` | 1053 | B | HTML 块内的 `<p align>` 也算对；跨度内的标题等块仍画出（5 组断言） | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 59 | `an_unpaired_open_tag_does_not_swallow_the_pair_after_it` | 1089 | B | 未闭合开标签不吞掉后面完整闭合的对 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 60 | `a_supported_open_tag_nested_in_one_block_is_content` | 1098 | B | 同一 HTML 块内嵌套的第二个 `<p align>` 当内容而非新对 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 61 | `block_constructs_inside_an_align_tag_pair_are_drawn` | 1106 | B | 对内围栏代码块/有序列表成块级节点；对内 HTML 块仍被丢弃 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 62 | `headings_inside_an_align_tag_pair_are_drawn` | 1127 | B | 对内 ATX 与 setext 标题成标题块 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 63 | `a_tag_pair_that_is_not_markup_is_not_an_align_tag` | 1143 | B | 注释/pre/script/style/textarea/属性内的 `<p align>` 不算对（9 例全空文档） | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 64 | `a_comment_beside_an_align_tag_pair_does_not_hide_it` | 1164 | B | 旁侧 HTML 注释不阻断扫描 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 65 | `a_comment_does_not_disturb_the_align_tags_after_it` | 1172 | B | 注释内的 `'` 与 `>` 不提前结束注释，后续对仍画出（3 组） | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 66 | `a_close_tag_inside_a_comment_does_not_end_the_pair` | 1204 | B | 注释内的 `</p>` 不结束配对 → 文本合并 `ab` | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 67 | `an_abbreviated_comment_does_not_swallow_the_tags_after_it` | 1212 | B | `<!-->`/`<!--->` 即刻结束、不吞后续标签 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 68 | `an_unterminated_comment_at_the_end_of_a_multibyte_source_does_not_panic` | 1225 | B | 多字节源末尾未闭合注释不 panic | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 69 | `a_stray_less_than_sign_does_not_swallow_the_closing_tag` | 1234 | B | 裸 `<` 不吞掉闭合标签（3 组） | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 70 | `a_heading_inside_an_align_tag_pair_is_still_a_heading` | 1262 | B | 对内标题是标题，其前后段落保持该对对齐 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 71 | `a_tag_name_that_only_starts_with_p_is_not_the_align_tag` | 1275 | B | `<palign=…>` 不是 `<p>` → 空文档 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 72 | `several_align_tag_pairs_in_one_html_block_become_several_paragraphs` | 1283 | B | 一个 HTML 块内多个对 → 多个对齐段落；块内剩余原始 HTML 仍隐藏 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 73 | `the_content_of_an_align_tag_pair_ends_at_the_first_close_tag` | 1308 | B | 内容在第一个 `</p>` 结束（中间的 `<p class>` 段落被跳过） | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 74 | `only_the_exact_align_tag_pair_counts` | 1328 | B | 11 种非规范写法（大写/单引号/无引号/多属性/justify/未闭合…）整块丢弃 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 75 | `an_align_tag_pair_wrapped_in_other_html_still_counts` | 1351 | B | 被 `<div>` 包裹的对仍生效 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 76 | `an_open_align_tag_without_a_closing_tag_produces_nothing` | 1359 | B | 未闭合对不产块；已闭合的第一对保留 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 77 | `a_tag_whose_name_merely_starts_with_p_is_not_an_align_tag` | 1371 | B | `<pre>`/`<param>` 不算 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 78 | `an_empty_align_tag_pair_produces_no_block` | 1382 | B | 空/纯空白对不产块，其后空行也不画 | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 79 | `an_inline_align_tag_changes_nothing` | 1398 | B | 行内出现的 `<p align>` 只剥标签、不改对齐（文字 `text x`） | 删除 | §8.1「HTML 语义子集」27 条（容错语义）+「段落对齐」3 条 | R2/R3 | — |
| 80 | `formerly_dropped_block_nodes_are_now_drawn` | 1406 | J | 列表/引用/围栏代码块都成块；HTML 块仍不留内容 | 保留并改写 | §8.1「HTML 语义子集」+ §8.2 | R2 | `tests::code_blocks_mount_one_text_node_with_background_padding_and_mono` |
| 81 | `an_empty_source_has_no_blocks_and_no_links` | 1431 | J | 空源 → 空文档 | 保留并改写 | §8.1「HTML 语义子集」+ §8.2 | R2 | `tests::an_empty_source_mounts_only_the_root` |
| 82 | `the_same_source_parses_to_the_same_document` | 1436 | J | 同一源两次解析结果相等（确定性） | 保留并改写 | §8.1「HTML 语义子集」+ §8.2 | R2 | `tests::the_same_source_parses_to_the_same_document` |
| 83 | `push_text_merges_one_style_and_lands_it_before_a_style_change` | 1443 | F | IR 文本缓冲：同样式合并、样式变化前落地、空串不入缓冲、flush 清空 | 删除 | §5.5 flush 判定 + §8.2 outline | R2/R3 | — |
| 84 | `flush_without_a_buffer_does_nothing` | 1486 | F | 无缓冲时 flush 无副作用 | 删除 | §5.5 flush 判定 + §8.2 outline | R2/R3 | — |
| 85 | `flush_lands_a_buffer_built_by_hand` | 1497 | F | 手工构造缓冲经 flush 变成带完整样式的 Text piece | 删除 | §5.5 flush 判定 + §8.2 outline | R2/R3 | — |
| 86 | `a_kbd_change_lands_the_buffer_before_the_new_run` | 1528 | C | kbd 标记变化时先把前一段缓冲落地再开新 run | 删除 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | — |
| 87 | `with_link_stamps_text_and_image_and_leaves_a_line_break_alone` | 1552 | F | `with_link` 给 Text/InlineImage 盖 LinkId 且保 kbd；LineBreak 不变 | 删除 | §5.5 flush 判定 + §8.2 outline | R2/R3 | — |
| 88 | `a_sweep_of_shape_combinations_never_draws_a_line_twice` | 1583 | G | 4000 组随机行组合，任一 token 在 `plain_text` 出现不超过一次 | 删除 | §8.1 / §8.3 语料 | R2/R3 | — |
| 89 | `a_sweep_of_paragraphs_and_blank_lines_keeps_every_line_once` | 1644 | G | 1000 组段落/空行组合，每 token 恰好出现一次 | 删除 | §8.1 / §8.3 语料 | R2/R3 | — |
| 90 | `a_sweep_of_tag_pairs_and_blank_lines_keeps_every_pair_once` | 1682 | G | 1000 组对齐对组合，每 token 恰好出现一次 | 删除 | §8.1 / §8.3 语料 | R2/R3 | — |
| 91 | `a_fenced_code_block_takes_the_first_word_of_its_info_as_the_language` | 1720 | H | 围栏 info 第一个词作语言，多余 info 忽略 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::a_fenced_code_block_takes_the_first_info_word` |
| 92 | `a_fenced_code_block_strips_exactly_one_trailing_newline` | 1736 | H | 只剥一个尾换行，块内空行保留 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::a_fenced_code_block_strips_exactly_one_trailing_newline` |
| 93 | `an_indented_code_block_is_a_code_block` | 1749 | H | 4 空格缩进代码块成立并去掉缩进 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::code_blocks_mount_one_text_node_with_background_padding_and_mono` |
| 94 | `a_block_quote_holds_the_blocks_inside_it` | 1757 | H | 引用含段落/多段落/标题；引用自身不合成空行 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::quotes_mount_a_bar_a_content_column_and_nest` |
| 95 | `a_block_quote_may_contain_other_blocks` | 1780 | H | 引用可嵌套引用与列表 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::quotes_mount_a_bar_a_content_column_and_nest` |
| 96 | `a_bullet_list_has_items_and_starts_at_one` | 1801 | H | 无序列表 2 项、`start=1` | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::tight_lists_mount_markers_without_a_content_column` |
| 97 | `an_ordered_list_keeps_its_start` | 1816 | H | 有序列表 `start=3` 保留；`1.` 时 `start=1` | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::ordered_markers_start_at_the_parsed_number` |
| 98 | `a_list_item_may_contain_a_nested_list` | 1839 | H | 列表项内含嵌套列表 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `corpus::the_gfm_block_corpus_emits_canonical_html` |
| 99 | `a_list_item_may_contain_a_code_block` | 1854 | H | 列表项内含围栏代码块 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::a_list_item_may_contain_a_code_block` |
| 100 | `a_thematic_break_is_a_block` | 1869 | H | `---` 成 ThematicBreak 块，前后空行保留 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::thematic_breaks_mount_a_one_pixel_rule` |
| 101 | `a_table_has_a_header_row_and_aligned_columns` | 1883 | H | 表格首行是 header，三列对齐来自分隔行（左/中/右） | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::tables_mount_rows_cells_and_one_rule_per_row` |
| 102 | `table_cells_keep_their_inline_marks` | 1908 | H | 单元格内 strong/code/link/strike 均保留 | 保留并改写 | §8.1「表格」「块级」+ §8.2 | R2 | `tests::table_cells_keep_their_inline_marks` |
| 103 | `inline_code_is_a_text_piece_with_the_code_mark` | 1934 | I | 行内代码成 code 标记的 Text；可与 strong 叠加；代码内 `**x**` 是字面量 | 保留并改写 | §8.1「行内」「删除线」 | R2 | `tests::inline_marks_cut_runs_and_leave_plain_text_runless`、`corpus::the_gfm_inline_corpus_emits_canonical_html` |
| 104 | `strikethrough_is_a_piece_mark` | 1960 | I | `~~x~~` 成 strike，可与 strong 叠加 | 保留并改写 | §8.1「行内」「删除线」 | R2 | `corpus::the_gfm_strikethrough_corpus_emits_canonical_html` |
| 105 | `a_kbd_pair_is_counted_per_table_cell` | 1978 | C | 每个表格单元格的 kbd run 编号各自从 0 开始 | 删除 | §8.1「行内」（kbd 精确形态 / `<kbd>` 内图片） | R2 | — |
| 106 | `blank_lines_around_the_new_block_kinds_are_kept` | 1992 | E | 代码块/列表/引用周围的源空行保留 | 删除 | §8.4 门禁 3（spacer 形状）+ §8.1「块级」（空行重建） | R2/R3 | — |
| 107 | `container_blocks_do_not_synthesize_blank_lines` | 2021 | E | 列表/引用内部不合成空行 | 删除 | §8.4 门禁 3（spacer 形状）+ §8.1「块级」（空行重建） | R2/R3 | — |
| 108 | `the_deep_nesting_guard_applies_inside_containers` | 2043 | A | 超深行内强调在引用/列表项内只截断 marks、文字保留，容器块本身保留（R3 定稿语义） | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `ledger::the_deep_nesting_guard_applies_inside_containers` |
| 109 | `deeply_nested_quotes_are_capped_instead_of_overflowing_the_stack` | 2102 | A | 1000 层引用：文字仍可见、IR 容器深度 ≤ 64 | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `tests::degrading_an_over_deep_container_keeps_its_siblings` |
| 110 | `quotes_exactly_at_the_block_nesting_limit_stay_structured` | 2119 | A | 64 层引用全部保留为 Quote 结构 | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `tests::degrading_an_over_deep_container_keeps_its_siblings` |
| 111 | `quotes_past_the_block_nesting_limit_degrade_to_flat_text` | 2132 | A | 65 层 → 深度停在 64，第 65 层降级为段落文字 `deep` | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `tests::degrading_an_over_deep_container_keeps_its_siblings` |
| 112 | `deeply_nested_lists_are_capped_instead_of_overflowing_the_stack` | 2165 | A | 300 层列表：文字可见、容器深度 ≤ 64 | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `tests::deeply_nested_lists_are_capped_at_thirty_two_levels` |
| 113 | `degrading_an_over_deep_container_keeps_its_siblings` | 2181 | A | 深引用降级后兄弟段落仍保留（3 块：深引用、空行、兄弟段落） | 保留并改写 | §8.1「退化」+ §8.2 outline | R2 | `tests::degrading_an_over_deep_container_keeps_its_siblings` |

> 校核：保留并改写 51（A11 + C6 + D12 + H14 + I5 + J3）、删除 62（B34 + C6 + D1 + E10 + F7 + G3 + I1）。C 组保留的 6 条名单：`a_kbd_pair_marks_the_pieces_between_its_tags`(#14)、`nested_kbd_tags_share_the_outermost_run`(#16)、`kbd_marks_the_inline_styles_inside_it`(#17)、`unmatched_kbd_tags_follow_the_nodes_comrak_produces`(#19)、`only_the_exact_kbd_tags_count`(#20)、`a_soft_break_inside_kbd_keeps_the_run_number_on_both_lines`(#23)。

---

## 2. `view.rs`（19 条：保留 5 / 改写 11 / 删除 3）

| # | 测试名 | 行 | 类别 | 断言摘要 | 处置 | 替代 / 新落点 | 轮次 | 落点（R3） |
|---:|---|---:|---|---|---|---|------|---|
| 1 | `paragraph_mounts_one_text_node_with_layout_and_glyphs` | 786 | §8.6：渲染断言 | 单段落挂载出 1 个文本节点、有非零布局盒、首帧产生 GlyphRun 图元 | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::a_paragraph_mounts_one_text_node_with_layout_and_glyphs` |
| 2 | `heading_uses_a_larger_font_than_the_body` | 812 | §8.6：渲染断言 | H1 行高 > 正文行高 | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::headings_scale_with_their_level_and_stay_bold_and_left` |
| 3 | `links_carry_underlines_and_are_hit_by_byte_index` | 841 | §8.6：渲染断言（含链接命中） | 链接产下划线 Quad（高≤2），`link_at(点)` 命中/不命中 | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::links_carry_underlines_in_the_paint_stream`（下划线 Quad：1 条、高 1.0px、宽 == 自然 advance、取色 link）；`link_at` 命中语义随 §6.4 删除，不可观察（D9） |
| 4 | `blank_line_mounts_one_line_height_spacer` | 876 | §8.6：渲染断言（空行占位） | 每个空行一个 `MARKDOWN_BLANK` 节点，高度 == 行高 | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::blank_lines_become_one_spacer_each`、`gates::gate_three_the_release_notes_stay_inside_the_node_budget` |
| 5 | `inline_images_become_painted_inline_boxes` | 904 | §8.6：渲染断言 | 行内图片按解析尺寸画成 24×24 Quad，段落不产链接 | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::file_images_become_painted_inline_boxes` |
| 6 | `code_quote_list_table_and_rule_mount_their_node_kinds` | 926 | §8.6：渲染断言 | `MARKDOWN_CODE/QUOTE/LIST/LIST_ITEM/RULE/TABLE/TABLE_CELL/LIST_MARKER` 的节点计数与 `text_blocks` 数 | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::code_blocks_mount_one_text_node_with_background_padding_and_mono`、`tests::tables_mount_rows_cells_and_one_rule_per_row` |
| 7 | `ordered_list_markers_start_at_the_parsed_number` | 988 | §8.6：渲染断言 | 有序列表标记节点存在且有布局盒 | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::ordered_markers_have_a_layout_box` |
| 8 | `empty_document_mounts_only_the_root` | 1006 | §8.6：渲染断言 | 空文档只挂根、根有布局盒、`MARKDOWN` id 计数 1 | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::an_empty_source_mounts_only_the_root` |
| 9 | `rich_text_can_be_used_standalone` | 1015 | §8.6：渲染断言 | 直接调用 `rich_text()` 也能独立挂出文本节点 | 删除 | §6.4 公开面删除（`rich_text` / `ImagePaintSource` / `link_at`） | R2 | — |
| 10 | `placeholder_images_are_used_by_default_build` | 1044 | §8.6：渲染断言 | 默认 `ImagePaintSource` 对图片给出 Quad 占位色块 | 删除 | §6.4 公开面删除（`rich_text` / `ImagePaintSource` / `link_at`） | R2 | — |
| 11 | `alignment_maps_to_engine_text_align` | 1061 | §8.6：渲染断言 | `ParagraphAlign` 三向映射到 `ui_core::text::TextAlign` | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::table_cells_take_the_column_alignment_and_pad_missing_cells`、`corpus::the_gfm_alignment_corpus_emits_canonical_html` |
| 12 | `kbd_and_code_pieces_do_not_break_paragraph_mounting` | 1069 | §8.6：渲染断言 | kbd/code piece 的段落仍能挂出布局盒 | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::kbd_and_code_paragraphs_keep_their_runs_and_layout` |
| 13 | `link_at_without_links_is_none` | 1096 | §8.6：渲染断言 | 无链接文档的 `link_at` 返回 `None` | 删除 | §6.4 公开面删除（`rich_text` / `ImagePaintSource` / `link_at`） | R2 | — |
| 14 | `root_uses_the_static_probe_id` | 1114 | §8.6：生命周期（静态 probe id） | 根节点 probe id 是 `"MARKDOWN"` 且等于 `handles.root` | 保留（断言不变） | — | R2 | `tests::an_empty_source_mounts_only_the_root`、`gates::gate_five_no_legacy_block_or_blank_ids_survive` |
| 15 | `a_heading_after_a_blank_line_keeps_the_extra_space_above_it` | 1125 | §8.6：渲染断言（间距） | 空行后标题上间距 = `HEADING_SPACE_ABOVE`、普通段落 = 0；无空行时 = `BLOCK_GAP`(+`HEADING_SPACE_ABOVE`) | 保留并改写 | §8.2 outline + §8.4 门禁 | R2 | `tests::blocks_take_the_block_gap_and_headings_take_the_extra_space` |
| 16 | `switching_appearance_rebuilds_the_markdown_with_the_new_theme` | 1251 | §8.6：生命周期（外观重建） | 外观信号变化 → `sync` 重建子树，首文本色随主题变；未变时 no-op | 保留（断言不变） | — | R2 | `tests::switching_the_appearance_rebuilds_the_markdown_with_the_new_theme` |
| 17 | `releasing_the_markdown_unloads_its_root_and_stops_syncing` | 1298 | §8.6：生命周期（release） | `release` 卸载根、清空句柄、重复释放幂等；释放后 `sync` no-op | 保留（断言不变） | — | R2 | `tests::releasing_the_markdown_unloads_its_root_and_stops_syncing` |
| 18 | `rebuilding_keeps_the_markdown_root_identity_and_the_page_offset` | 1346 | §8.6：生命周期（root 身份+滚动偏移） | 重建后根身份/父节点不变、子节点真换新、页面滚动偏移 240 不变 | 保留（断言不变） | — | R2 | `tests::rebuilding_keeps_the_root_identity_and_the_page_offset` |
| 19 | `the_markdown_subtree_holds_no_signal_dependency_edges` | 1426 | §8.6：生命周期（无依赖边） | markdown 子树 `remove` 返回空依赖边；根节点仍持有外观依赖边 | 保留（断言不变） | — | R2 | `tests::the_markdown_subtree_holds_no_signal_dependency_edges`、`gates::gate_six_the_markdown_subtree_holds_no_dependency_edges`（子树空 + 根恰好 1 条 appearance 边） |

---

## 3. `document.rs`（5 条：保留 3 / 删除 2）

| # | 测试名 | 行 | 类别 | 断言摘要 | 处置 | 替代 / 新落点 | 轮次 | 落点（R3） |
|---:|---|---:|---|---|---|---|------|---|
| 1 | `heading_levels_map_to_scales` | 266 | §8.6：移入新内部模块并保留 | `HeadingLevel::from_u8(1/6/7)` → H1/H6/H6（越界兜底不 panic）；`H1.scale()=1.75`、`H6.scale()=1.0`、H1>H3 | 保留并改写 | 内部 `HeadingLevel` scale 常量表（1.75/1.5/1.3/1.15/1.05/1.0） | R2 | `tests::headings_scale_with_their_level_and_stay_bold_and_left` |
| 2 | `with_link_keeps_every_other_field` | 280 | §8.6：删除 | `with_link` 只改 `link`，`text` 等其它字段不变 | 删除 | IR 不存在（`with_link` / `pieces()`） | R2 | — |
| 3 | `plain_text_reads_blocks_lines_and_inline_marks` | 292 | §8.6：按 §6.3 新契约改写 | `plain_text` 拼接 Heading/BlankLine/Paragraph/LineBreak/图片(跳过) → `title\n\na\nbc` | 保留并改写 | §6.3 冻结字面量（`tests/fixtures/legacy_plain_text.rs`） | R2 | `plain::plain_text_deviation_08_folds_the_document_baseline_blank_lines` |
| 4 | `plain_text_reads_code_quote_list_and_table_blocks` | 321 | §8.6：按 §6.3 新契约改写 | 代码/引用/列表/分隔线/表格的 `plain_text` 拼接规则 → `let x = 1;\n\nquoted\n\none\n\ntwo\n\nh1 h2\na b` | 保留并改写 | §6.3 冻结字面量（`tests/fixtures/legacy_plain_text.rs`） | R2 | `plain::plain_text_deviation_08_folds_the_document_baseline_blank_lines` |
| 5 | `pieces_only_reports_inline_content_blocks` | 391 | §8.6：删除 | `pieces()` 只对 Paragraph 等返回 `Some`；ThematicBreak/BlankLine 返回 `None` | 删除 | IR 不存在（`with_link` / `pieces()`） | R2 | — |

---

## 4. `inline_flow.rs`（16 条：保留并改写 11 / 删除 5）

| # | 测试名 | 行 | 类别 | 断言摘要 | 处置 | 替代 / 新落点 | 轮次 | 落点（R3） |
|---:|---|---:|---|---|---|---|------|---|
| 1 | `empty_pieces_make_an_empty_flow` | 397 | §8.6：删除 | 空 piece 列表 → 空 flow（`text`/`runs`/`link_spans` 全空） | 保留并改写 | §5.5 flush 判定（空内联缓冲不建节点） | R2 | `tests::an_empty_source_mounts_only_the_root`、`corpus::the_degenerate_corpus_keeps_the_model_normalized_output` |
| 2 | `plain_text_becomes_one_run_spanning_the_whole_text` | 406 | §8.6：保留语义改写 | 纯文本 3 piece → 3 个 run（映射层不合并），字节区间 0..5/5..6/6..11，样式默认 | 保留并改写 | §8.2 outline（run / inline box / 装饰集合 / 背景） | R2 | `tests::inline_marks_cut_runs_and_leave_plain_text_runless` |
| 3 | `strong_and_emphasis_only_affect_their_own_range` | 420 | §8.6：保留语义改写 | strong 只把 b 的 weight 提到 700，italic 只覆盖 c | 保留并改写 | §8.2 outline（run / inline box / 装饰集合 / 背景） | R2 | `tests::styles_inside_a_link_are_kept`、`corpus::the_gfm_inline_corpus_emits_canonical_html` |
| 4 | `heading_style_bolds_every_run` | 438 | §8.6：保留语义改写 | 标题基准样式 `with_bold(true)` → 所有 run weight 700，italic 仍保留 | 保留并改写 | §8.2 outline（run / inline box / 装饰集合 / 背景） | R2 | `tests::heading_style_bolds_every_run` |
| 5 | `links_carry_color_underline_and_byte_spans` | 449 | §8.6：删除 | 链接 run 带下划线+链接色，`link_spans` = 3..7，`link_at` 半开区间边界（7 → None） | 删除 | §6.4 公开面删除（`LinkSpan` / `link_at` / `ImagePaintSource`） | R2 | — |
| 6 | `strikethrough_is_its_own_decoration` | 475 | §8.6：保留语义改写 | strike run 的装饰是 `Strikethrough`，颜色 `color(1.0)` | 保留并改写 | §8.2 outline（run / inline box / 装饰集合 / 背景） | R2 | `corpus::the_gfm_strikethrough_corpus_emits_canonical_html`、`tests::inline_marks_cut_runs_and_leave_plain_text_runless` |
| 7 | `inline_code_uses_monospace_and_code_color` | 484 | §8.6：保留语义改写 | code run 用 `MONO_FAMILY` 与 code 颜色 `color(0.25)` | 保留并改写 | §8.2 outline（run / inline box / 装饰集合 / 背景） | R2 | `tests::inline_code_uses_monospace_and_the_code_color` |
| 8 | `kbd_runs_are_tracked_and_styled` | 495 | §8.6：保留语义改写 | `kbd_spans` = [0..4, 5..6]、`kbd_at` 边界正确、kbd 降级为下划线 | 保留并改写 | §8.2 outline（run / inline box / 装饰集合 / 背景） | R2 | `tests::kbd_and_code_paragraphs_keep_their_runs_and_layout` |
| 9 | `hard_breaks_become_newline_bytes_and_reset_marks` | 511 | §8.6：保留语义改写 | LineBreak → `\n` 字节，run 样式回到基准 weight 400 | 保留并改写 | §8.2 outline（run / inline box / 装饰集合 / 背景） | R2 | `tests::hard_breaks_become_newline_bytes_and_reset_marks` |
| 10 | `images_do_not_consume_text_bytes_and_carry_their_offset` | 524 | §8.6：保留语义改写 | 图片不占文本字节；`inline_boxes` 带插入点 index/尺寸/对齐/id/paint | 保留并改写 | §8.2 outline（run / inline box / 装饰集合 / 背景） | R2 | `tests::file_images_become_painted_inline_boxes`、`tests::inline_images_keep_their_text_offset`（`ab![](file://x)cd` → text `abcd`、box index 2） |
| 11 | `an_image_only_paragraph_keeps_a_box_at_offset_zero` | 558 | §8.6：保留语义改写 | 纯图片段落 `text` 为空、box index 0、无 paint | 保留并改写 | §8.2 outline（run / inline box / 装饰集合 / 背景） | R2 | `tests::inline_images_keep_their_text_offset`（纯图片段落 text 为 ZWSP 占位、box index 0）；`paint` 由 §6.4 的占位 Quad 替代 |
| 12 | `multi_byte_offsets_stay_on_char_boundaries` | 568 | §8.6：保留语义改写 | 多字节文本后 run 区间 6..9 且落在字符边界 | 保留并改写 | §8.2 outline（run / inline box / 装饰集合 / 背景） | R2 | `tests::multi_byte_offsets_stay_on_char_boundaries` |
| 13 | `link_span_covers_multi_byte_text_exactly` | 580 | §8.6：删除 | 中文链接 `link_spans[0]` = 0..6，`link_at(5)=Some`、`link_at(6)=None` | 删除 | §6.4 公开面删除（`LinkSpan` / `link_at` / `ImagePaintSource`） | R2 | — |
| 14 | `link_span_and_kbd_span_close_at_piece_boundaries` | 592 | §8.6：删除 | 相邻 piece 的 link span 分成两段、kbd span 只在首段 | 删除 | §6.4 公开面删除（`LinkSpan` / `link_at` / `ImagePaintSource`） | R2 | — |
| 15 | `links_around_images_keep_their_ranges` | 607 | §8.6：删除（R3 改判） | 链接内图片不打断 link span（0..2，含图片插入点） | 删除 | §6.4 公开面删除（见落点理由） | R2 | 删除：§6.4 删除 `LinkSpan`/`link_at`：`InlineBoxSpec` 不承载链接区间，旧断言「链接内图片不打断 link span」不可观察（D9） |
| 16 | `image_paint_source_can_choose_image_or_skip` | 632 | §8.6：删除 | 自定义 `ImagePaintSource` 可决定图片绘制方式（Quad 颜色由 size 派生） | 删除 | §6.4 公开面删除（`LinkSpan` / `link_at` / `ImagePaintSource`） | R2 | — |

---

## 5. `source.rs`（20 条：全部保留）

| # | 测试名 | 行 | 断言摘要 | 处置 | 替代 / 新落点 | 轮次 | 落点（R3） |
|---:|---|---:|---|---|---|------|---|
| 1 | `the_scheme_is_file_and_the_slashes_are_optional` | 214 | `file://x` 与 `file:x` 都给同一 path、格式 Image | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::the_scheme_is_file_and_the_slashes_are_optional` |
| 2 | `anything_that_is_not_a_file_uri_is_rejected` | 228 | `icon:`、`https:`、裸路径、大写 `FILE://`、`file://` 空 → `None` | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::anything_that_is_not_a_file_uri_is_rejected` |
| 3 | `a_single_slash_is_part_of_the_path` | 245 | `file:/x` 的单个 `/` 属于 path 本身 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::a_single_slash_is_part_of_the_path` |
| 4 | `the_asset_path_is_taken_verbatim` | 254 | 路径原样保留（`@`、中文、`#`、`&`、`"`）；`.svg` → Svg | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::the_asset_path_is_taken_verbatim` |
| 5 | `a_path_cannot_escape_the_embedded_assets` | 276 | `..`/绝对路径/反斜杠等 5 例：可解析但 `load` 为 `None`、回退默认 16×16 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::a_path_cannot_escape_the_embedded_assets` |
| 6 | `the_format_follows_the_extension_when_type_is_absent` | 302 | `.svg`/`.SVG` → Svg；`.png`/无扩展名/`.svga`/`a.svg/b` → Image | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::the_format_follows_the_extension_when_type_is_absent` |
| 7 | `type_overrides_the_extension_and_may_be_quoted` | 325 | `?type=` 覆盖扩展名，可带引号 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::type_overrides_the_extension_and_may_be_quoted` |
| 8 | `an_unusable_type_is_rejected` | 341 | `ico`/空/大写/重复/半个引号 → 整体拒绝 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::an_unusable_type_is_rejected` |
| 9 | `size_takes_one_number_for_both_edges_or_two_for_width_and_height` | 359 | `size=24`/`24,48`/`3.5`/`24.5,48.25`/`00024` 都解析正确 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::size_takes_one_number_for_both_edges_or_two_for_width_and_height` |
| 10 | `size_may_be_quoted_and_lives_anywhere_in_the_query` | 380 | `?size="24,48"` 及其与 `type` 的任意顺序 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::size_may_be_quoted_and_lives_anywhere_in_the_query` |
| 11 | `an_explicit_size_is_clamped` | 396 | `0`/`0.5`/`.5`/`0,0` → 1；`256`/`9999`/`256.5` → 256；两端各自夹紧 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::an_explicit_size_is_clamped` |
| 12 | `an_absurdly_long_number_is_clamped_instead_of_overflowing` | 425 | 400 个 `9` → 256，不溢出 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::an_absurdly_long_number_is_clamped_instead_of_overflowing` |
| 13 | `an_unusable_size_falls_back_to_the_intrinsic_one` | 432 | 17 种非法 `size` → 尺寸段 `None`；重复冲突 size → 整体拒绝 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::an_unusable_size_falls_back_to_the_intrinsic_one` |
| 14 | `unknown_and_empty_query_segments_are_ignored` | 453 | `?color=red`、`?&&size=24&&`、尾部 `?` 都被忽略 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::unknown_and_empty_query_segments_are_ignored` |
| 15 | `a_query_segment_without_a_value_is_rejected` | 471 | `?size`/`?type`/`?color`/`file://a?b/c.png` → `None` | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::a_query_segment_without_a_value_is_rejected` |
| 16 | `a_missing_size_resolves_to_the_asset_size` | 487 | 各品牌 png 的内禀尺寸（128²/44×72/81×72/180²/200²），未知与 svg → 16×16 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::a_missing_size_resolves_to_the_asset_size` |
| 17 | `an_explicit_size_wins_over_the_asset_size` | 525 | `?size=24,40` 覆盖内禀尺寸；未知资源也接受显式 size | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::an_explicit_size_wins_over_the_asset_size` |
| 18 | `a_resource_that_is_not_a_bitmap_falls_back_to_the_default_size` | 543 | svg 资源声明 `?type=img` → 回退 16×16 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::a_resource_that_is_not_a_bitmap_falls_back_to_the_default_size` |
| 19 | `an_unreadable_asset_falls_back_to_the_default_size` | 553 | 自定义 assets 只能读到 svg 时仍回退默认 16×16 | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::an_unreadable_asset_falls_back_to_the_default_size` |
| 20 | `an_empty_path_names_no_asset` | 572 | 空路径 `load` 返回 `None` | 保留（断言不变） | 原样迁移到 markdown/assets.rs | R2 | `assets::an_empty_path_names_no_asset` |

---

## 5.1 R3 对抗性审查后的追加加固

`review-r3.md` 的 P1×3 / P2×8 已处理：spacer 加 `flex_shrink(0)` 并把门禁 3 改回默认根（34 个 spacer 各 19.6px）；规范一致性语料补齐到 **90** 条（表格 12 / 任务清单 9 / 删除线 6 / 块级 15 / 行内 12 / 段落对齐 3 / HTML 子集 27 / 退化 6）并扩展发射器（表格/任务/HTML 子集）；`i18n_markdown.rs` 改为 §8.5 的 39 key 矩阵 + 21 条 `PRODUCTION_NOTICES` + 冻结可见文本；gate2 加阳性对照；gate6 加根 appearance 边；补 `outline()`、图片 index 断言、plain_text 解析计数、skip 编号修正。设计文档 §6.1/§8.6/§10.1 表头的数字与模块清单属于 R4 文档回写（本轮未改设计主文档）→ **已由 `c3fa5c52` 完成**（设计文档回写提交）。

---

## 6. 相对 test-inventory 的 3 处调整（须在 R2 提交说明列明）

1. **`inline_flow.rs` #1 `empty_pieces_make_an_empty_flow` 由「删除」改判「保留并改写」**：inventory 的逐条表判删除（`Flow`/`flow()` 删除），但该测试的语义（空输入不产节点）在新架构中对应 §5.5 的 flush 判定，可断言；相应地 #16 `image_paint_source_can_choose_image_or_skip` 必须删除（`ImagePaintSource` 按 D9 删除）。R3 复核又把 #15 `links_around_images_keep_their_ranges` 改判删除（`LinkSpan`/`link_at` 已删，见 §7），因此 `inline_flow.rs` 最终收口为 11/5，全文总账 101/72。
2. **`parse_tests.rs` A 组「11 条」的前缀清单补齐 4 条**：inventory §6-2 指出 v2 文档列出的前缀只覆盖 7 条；本表按测试名逐条列出，不再依赖前缀。
3. **`parse_tests.rs` D 组「13 条」按语义补入 2 条**：`a_type_overrides_the_extension`、`a_path_with_at_signs_or_non_ascii_survives_the_markdown_parser` 不被 v2 列出的前缀覆盖，本表已按测试名列出。

> 另：`parse_tests.rs` 的 B 组 34 条测试**删除的是 IR 形态断言**，其容错输入（精确形态 / 同名嵌套与非配对 / 注释 / 引号 / 未闭合 / CRLF / 同块多对 / 尾巴文本 / type 1–5 排除 / 行中 `<p>`）必须逐条迁移到 §8.1 的 HTML 语义子集 27 条——这是「删除但不丢语义」的判据。

---

## 7. R3 复核：`review-r2-repo.md` P2-2 的 23 条处置

R3 对 P2-2 点名的 23 条逐条补测或改判（下表 24 行：document §3 #3/#4 合并为一行，另补 1 条台账 §4 #15）；每条对应的落点测试都在 `cargo test -p ui_kit --lib kit::markdown` 内执行成功。

| P2-2 条目 | 台账编号 | R3 处置 | 落点 / 理由 |
|---|---|---|---|
| 深强调保留文字、只丢超限 marks | §1 #5 | 补测（语义随 F-1 修正） | `ledger::a_deep_emphasis_keeps_the_text_and_drops_the_excess_marks`（`keep x tail` 逐字符 + marks 截断） |
| 嵌套上限作用于容器内 | §1 #108 | 补测 | `ledger::the_deep_nesting_guard_applies_inside_containers` |
| 深列表封顶 | §1 #112 | 补测 | `tests::deeply_nested_lists_are_capped_at_thirty_two_levels`（32 list / 32 item / 129 节点） |
| 降级容器保兄弟 | §1 #113 | 补测 | `tests::degrading_an_over_deep_container_keeps_its_siblings`（64 quote + sibling + 1 spacer） |
| 链接内样式 | §1 #13 | 补测 | `tests::styles_inside_a_link_are_kept`（2 run：strong+link / link） |
| 嵌套 kbd 共用最外层 | §1 #16 | 补测 | `tests::nested_kbd_tags_share_the_outermost_run`（`abc` / 1 run） |
| kbd 内的行内样式 | §1 #17 | 补测 | `tests::kbd_marks_the_inline_styles_inside_it`（strong+kbd run + 图片 box） |
| 落单/未闭合 kbd | §1 #19 | 补测 | `tests::unmatched_kbd_tags_follow_the_parsed_tags`（`a b` 无 run；未闭合 `a` 带 kbd run） |
| kbd 内软换行 | §1 #23 | 补测 | `tests::a_soft_break_inside_kbd_keeps_one_run`（`a\nb` / 1 run 0..3） |
| 链接内图片携带链接 | §1 #32 | 改判删除 | §6.4 删除 `LinkId`，`InlineBoxSpec` 无链接区间可断言（D9） |
| 被忽略图片切分文本 | §1 #36 | 补测（语义改判） | `tests::an_ignored_image_still_splits_the_text_around_it`：非 `file:` 图片回落 alt 文本（D6-④），`axb` |
| 同源解析确定 | §1 #82 | 补测 | `tests::the_same_source_parses_to_the_same_document`（24 节点逐项相等） |
| 围栏 info 首词 | §1 #91 | 补测 | `tests::a_fenced_code_block_takes_the_first_info_word`（语言不上屏） |
| 只剥一个尾随换行 | §1 #92 | 补测 | `tests::a_fenced_code_block_strips_exactly_one_trailing_newline`（`a\n`） |
| 列表项内代码块 | §1 #99 | 补测 | `tests::a_list_item_may_contain_a_code_block`（1 content 列 + 1 code 节点） |
| 多字节 run 区间 | §4 #12 | 补测 | `tests::multi_byte_offsets_stay_on_char_boundaries`（`中文` / run 0..3） |
| 链接下划线 Quad | §2 #3 | 补测 | `tests::links_carry_underlines_in_the_paint_stream`（1 条 Quad，高 1.0px、宽 == 自然 advance、取色 `style.link`） |
| 有序标记布局盒 | §2 #7 | 补测 | `tests::ordered_markers_have_a_layout_box`（20×19.6px） |
| kbd/code 段落可挂载 | §2 #12 | 补测 | `tests::kbd_and_code_paragraphs_keep_their_runs_and_layout`（2 run + 非零布局盒 + GlyphRun） |
| heading 额外间距 16px | §2 #15 | 已有断言 | `tests::blocks_take_the_block_gap_and_headings_take_the_extra_space` 的 `a\n# B` 场景 = 16px（R2 已补，P2-6 结论过时） |
| release 卸载根 | §2 #17 | 补测 | `tests::releasing_the_markdown_unloads_its_root_and_stops_syncing`（`tree.get(root).is_err()`） |
| 重建保根身份与页面偏移 | §2 #18 | 补测 | `tests::rebuilding_keeps_the_root_identity_and_the_page_offset`（root 不变 / 首子节点换新 / 父节点不变 / 偏移 240±0.5） |
| document 两条 plain_text 冻结值 | §3 #3/#4 | 补测 | `plain::plain_text_deviation_08_folds_the_document_baseline_blank_lines`（冻结字面量逐字符 + 仅折行不改字） |
| 链接内图片链接区间 | §4 #15 | 改判删除 | 同上：`LinkSpan`/`link_at` 已删（§6.4，D9） |

R3 的其余加固：§8.3 规范一致性语料（`corpus::*` 11 个测试 / **90 条**用例：表格 12、任务清单 9、删除线 6、块级 15、行内 12、段落对齐 3、HTML 子集 27、退化 6；skip 5 条指向 §10.1-3/-3/-5/-6/-16）、§8.2 `outline()` 分类器（`tests::outline` 2 条）、§8.4 七条门禁（`gates::gate_one_a_stable_frame_commits_nothing` … `gates::gate_seven_table_columns_take_their_measured_natural_widths`，另加 spacer flex 固定与 plain_text 计数 2 条）、§8.5 冻结夹具（`fixtures/legacy_plain_text.rs` 81 条 + 2 条 document 值 + 2 条 release 折叠值 + 4 条生产 plain_text 入参；`fixtures/i18n_markdown.rs` 39 key × {en,zh,en_rich,zh_rich,en_visible,zh_visible} + 21 条 `PRODUCTION_NOTICES`）。
