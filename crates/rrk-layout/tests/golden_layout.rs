//! 金样（golden）测试：排版纯函数的跨版本回归真相。
//!
//! 原理：注入**确定性字体**（`system_fonts: false` + 仓内子集字体
//! `fonts/RRKGoldenSans.otf`），排版结果不依赖宿主系统字体栈，同一输入必须
//! 逐项复现同一 `Vec<LayoutPage>`。升级 parley / 调断行参数 / 改排版内核时，
//! 金样 diff 就是「排版真相差异」——断行、分页、坐标的变化一目了然。
//!
//! 金样位于 `tests/golden/<case>.txt`（人类可读文本，便于 review）。
//! 确认差异是**预期改动**后再生成并提交（金样 diff 即排版变更日志，必须 review）：
//!
//! ```sh
//! RRK_GOLDEN_BLESS=1 cargo test -p rrk-layout --test golden_layout
//! ```

use std::sync::Arc;

use parley::fontique::{Blob, Collection, CollectionOptions};
use parley::FontContext;
use rrk_layout::Layouter;
use rrk_model::{
    Align, Block, ChapterContent, ChapterId, Inline, LayoutPage, ResourceKey, StyleFlags,
    StyleSpec, TextRange, Viewport,
};

/// 仓内金样字体（Noto Sans SC 子集 + 改名，见 `fonts/README.md`；OFL 1.1）。
const FONT_BYTES: &[u8] = include_bytes!("fonts/RRKGoldenSans.otf");
const FONT_FAMILY: &str = "RRK Golden Sans";

/// 确定性排版器：无系统字体 + 仅仓内金样字体（跨平台逐位可复现的前提）。
fn golden_layouter() -> Layouter {
    let mut collection = Collection::new(CollectionOptions { shared: false, system_fonts: false });
    let registered = collection.register_fonts(Blob::from(FONT_BYTES.to_vec()), None);
    assert!(!registered.is_empty(), "金样字体注册失败");
    assert!(
        collection.family_id(FONT_FAMILY).is_some(),
        "金样字体族名不符（应为 {FONT_FAMILY}）——子集改名步骤是否漏了？"
    );
    Layouter::with_font_context(FontContext { collection, source_cache: Default::default() })
}

// ---------------------------------------------------------------------------
// 章节构造（全确定性：无随机、无时间、无环境依赖）
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ChapterBuilder {
    text: String,
    blocks: Vec<Block>,
}

impl ChapterBuilder {
    fn para_spans(&mut self, spans: &[(&str, bool, bool)], indent: bool) -> &mut Self {
        let mut inlines = Vec::new();
        for (t, bold, italic) in spans {
            let start = self.text.len() as u32;
            self.text.push_str(t);
            inlines.push(Inline::Text {
                range: TextRange::new(start, self.text.len() as u32),
                style: StyleFlags(
                    (if *bold { StyleFlags::BOLD } else { 0 })
                        | (if *italic { StyleFlags::ITALIC } else { 0 }),
                ),
            });
        }
        let role = if indent {
            rrk_model::ParagraphRole::Body
        } else {
            rrk_model::ParagraphRole::FirstOfSection
        };
        self.blocks.push(Block::Paragraph { inlines, role });
        self
    }

    fn para(&mut self, s: &str, indent: bool) -> &mut Self {
        self.para_spans(&[(s, false, false)], indent)
    }

    fn heading(&mut self, level: u8, s: &str) -> &mut Self {
        let start = self.text.len() as u32;
        self.text.push_str(s);
        self.blocks.push(Block::Heading {
            level,
            inlines: vec![Inline::Text {
                range: TextRange::new(start, self.text.len() as u32),
                style: StyleFlags::default(),
            }],
        });
        self
    }

    fn quote(&mut self, s: &str) -> &mut Self {
        let start = self.text.len() as u32;
        self.text.push_str(s);
        self.blocks.push(Block::Quote {
            blocks: vec![Block::Paragraph {
                inlines: vec![Inline::Text {
                    range: TextRange::new(start, self.text.len() as u32),
                    style: StyleFlags::default(),
                }],
                role: rrk_model::ParagraphRole::FirstOfSection,
            }],
        });
        self
    }

    fn divider(&mut self) -> &mut Self {
        self.blocks.push(Block::Divider);
        self
    }

    fn image(&mut self, key: &str, size_hint: Option<(u32, u32)>) -> &mut Self {
        self.blocks.push(Block::Image {
            key: ResourceKey(key.to_string()),
            alt: None,
            size_hint,
        });
        self
    }

    fn build(self, id: ChapterId) -> ChapterContent {
        ChapterContent {
            id,
            text: Arc::from(self.text.as_str()),
            blocks: self.blocks,
            link_ranges: Vec::new(),
        }
    }
}

/// 章 A：中文主排（标题 / 缩进段 / 粗斜体跨度 / 引文 / 图片 / 分隔线）。
fn chapter_cjk(id: ChapterId) -> ChapterContent {
    let mut b = ChapterBuilder::default();
    b.heading(1, "第一章 排版内核的分页原理");
    b.para("排版内核负责把结构化的章节内容转换成可以逐页呈现的行与坐标。它的输入是块与行内元素的序列，输出是页表：每一页包含行切分、文本区间与字形游程。字号、行高、缩进与对齐方式都由样式表驱动，同一份输入永远得到同一份输出。", true);
    b.para("在实现上，先测量再断行，最后按视口高度分页。段落可以跨页，标题不与后文拆断，图片按尺寸提示缩放后占位。缓存以内容哈希、样式哈希与视口哈希三要素为键，命中即免排版。", true);
    b.heading(2, "断行与进度");
    b.para_spans(
        &[
            ("重点：", true, false),
            ("断行决定了进度坐标", true, true),
            ("。若断行随版本漂移，阅读进度就会跳动，因此必须用金样测试把每一次断行变化都显式地记录下来。", false, false),
        ],
        true,
    );
    b.quote("任何排版改动都应当能被金样差异解释——要么是明确的改进，要么是回归。");
    b.image("img/cover.png", Some((800, 600)));
    b.divider();
    b.para("正文在分隔线后继续。The quick brown fox jumps over the lazy dog. 排版测试需要覆盖中西文混排、数字 12345 与标点符号（顿号、括号「」和省略号……）的断行行为。", true);
    b.build(id)
}

/// 章 B：中西文混排（拉丁词、数字、长不可断词、引号、单字段落）。
fn chapter_mixed(id: ChapterId) -> ChapterContent {
    let mut b = ChapterBuilder::default();
    b.heading(2, "Mixed 中西文与长单词");
    b.para("This paragraph mixes English words with 中文词组 and numbers like 2026-10-29, plus symbols: v0.3.0, ABI 10, 390x842px.", true);
    b.para("Supercalifragilisticexpialidocious_antidisestablishmentarianism 这是一个无法断开的超长西文词后面跟着可以断开的中文句子，用来验证溢出断行与压力测试。", true);
    b.para("「引号」内的内容与 ASCII \"quotes\" 混排；全角标点（。，！？）与半角标点 (.,!?) 的间距处理。", true);
    b.para("短", true);
    b.build(id)
}

/// 章 C：边界样张（样式跨度交错、空段落、单字符、收尾压分页边界）。
fn chapter_edge(id: ChapterId) -> ChapterContent {
    let mut b = ChapterBuilder::default();
    b.heading(1, "边界样张");
    b.para_spans(
        &[
            ("全粗体段落", true, false),
            ("与", false, false),
            ("全斜体段落", false, true),
            ("交错排布，验证样式跨度在断行处的游程切分。", false, false),
        ],
        true,
    );
    b.para("", true); // 空段落（排版层应跳过）
    b.para("A", true); // 单字符段落
    b.para("数字与单位：3.14159、100%、2x、-40℃、9:41、第 3 页 / 共 12 页。", true);
    b.quote("嵌套引文里只允许段落，不会出现引文套引文。");
    b.para("收尾段落用于让分页落在非整行边界上。", true);
    b.build(id)
}

// ---------------------------------------------------------------------------
// 样式与视口
// ---------------------------------------------------------------------------

fn style_default() -> StyleSpec {
    let mut s = StyleSpec::default();
    s.font_family = FONT_FAMILY.to_string();
    s
}

fn style_noindent_justify() -> StyleSpec {
    let mut s = style_default();
    s.para_indent_em = 0.0;
    s.align = Align::Justify;
    s.font_size = 20.0;
    s
}

fn style_large() -> StyleSpec {
    let mut s = style_default();
    s.font_size = 28.0;
    s.line_height = 1.8;
    s
}

fn vp(width: f32, height: f32) -> Viewport {
    Viewport { width, height, dpr: 2.0 }
}

// ---------------------------------------------------------------------------
// 金样转储 / 比对
// ---------------------------------------------------------------------------

fn dump(case: &str, pages: &[LayoutPage]) -> String {
    let mut s = String::new();
    s.push_str(&format!("== {case} ==\n"));
    s.push_str(&format!("pages: {}\n", pages.len()));
    for p in pages {
        s.push_str(&format!(
            "page {} [{}..{}) lines={} runs={} images={} height={:.3}\n",
            p.index,
            p.start.char_offset,
            p.end.char_offset,
            p.lines.len(),
            p.runs.len(),
            p.images.len(),
            p.height
        ));
        for (i, l) in p.lines.iter().enumerate() {
            s.push_str(&format!(
                "  line {i} [{}..{}) h={:.3} para={}\n",
                l.start,
                l.end,
                l.height,
                l.para_start as u32
            ));
        }
        for r in &p.runs {
            s.push_str(&format!(
                "    run l={} x={:.3} base={:.3} fs={:.3} [{}..{})\n",
                r.line, r.x, r.baseline, r.font_size, r.range.0, r.range.1
            ));
        }
        for im in &p.images {
            s.push_str(&format!(
                "    img {} x={:.3} y={:.3} w={:.3} h={:.3}\n",
                im.key.0, im.x, im.y, im.w, im.h
            ));
        }
    }
    s
}

fn check(name: &str, chapter: &ChapterContent, style: &StyleSpec, vp: &Viewport) {
    let mut l = golden_layouter();
    let pages = l.layout(chapter, style, vp);
    // 纯函数口径：同一输入两次排版必须完全一致（缓存契约的前提）。
    let mut l2 = golden_layouter();
    let pages2 = l2.layout(chapter, style, vp);
    assert_eq!(pages, pages2, "{name}: 同一输入两次排版不一致（违背纯函数口径）");

    let actual = dump(name, &pages);
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let path = dir.join(format!("{name}.txt"));
    if std::env::var_os("RRK_GOLDEN_BLESS").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "金样缺失或不可读: {path:?} ({e})——先跑 \
             RRK_GOLDEN_BLESS=1 cargo test -p rrk-layout --test golden_layout 生成"
        )
    });
    if expected != actual {
        let mut msg = format!(
            "金样不一致: {name}\n（排版真相变化——确认是预期改动后 RRK_GOLDEN_BLESS=1 再生成并 review diff）\n"
        );
        let mut shown = 0;
        for (i, (e, a)) in expected.lines().zip(actual.lines()).enumerate() {
            if e != a {
                msg.push_str(&format!("  line {}:\n    期望: {e}\n    实际: {a}\n", i + 1));
                shown += 1;
                if shown >= 20 {
                    msg.push_str("  ...（更多差异省略）\n");
                    break;
                }
            }
        }
        if expected.lines().count() != actual.lines().count() {
            msg.push_str(&format!(
                "  总行数: 期望 {} / 实际 {}\n",
                expected.lines().count(),
                actual.lines().count()
            ));
        }
        panic!("{msg}");
    }
}

// ---------------------------------------------------------------------------
// 金样用例（改动文本/样式/视口 = 主动变更排版真相，须 review 金样 diff）
// ---------------------------------------------------------------------------

#[test]
fn golden_cjk_default_phone() {
    check("cjk_default_phone", &chapter_cjk(0), &style_default(), &vp(390.0, 842.0));
}

#[test]
fn golden_cjk_noindent_justify_tablet() {
    check("cjk_noindent_justify_tablet", &chapter_cjk(0), &style_noindent_justify(), &vp(768.0, 500.0));
}

#[test]
fn golden_cjk_large_phone() {
    check("cjk_large_phone", &chapter_cjk(0), &style_large(), &vp(390.0, 842.0));
}

#[test]
fn golden_mixed_default_phone() {
    check("mixed_default_phone", &chapter_mixed(0), &style_default(), &vp(390.0, 842.0));
}

#[test]
fn golden_mixed_default_tablet() {
    check("mixed_default_tablet", &chapter_mixed(0), &style_default(), &vp(768.0, 500.0));
}

#[test]
fn golden_edge_default_phone() {
    check("edge_default_phone", &chapter_edge(0), &style_default(), &vp(390.0, 842.0));
}
