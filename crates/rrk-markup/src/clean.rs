//! XHTML → Block 清洗管线（方案 §3.3-4）：标签白名单 + CSS 最小集，
//! 产出干净 `Block` 流 + UTF-8 正文（`Inline` 偏移基于清洗后文本）。

use rrk_model::{Block, Inline, LinkRange, ResourceKey, StyleFlags, TextRange};

use crate::markup::{attr, local_name, tokenize, Token};

/// 清洗结果：规整后的正文 + 块列表 + 正文内链区间。
#[derive(Debug, Default)]
pub struct CleanResult {
    pub text: String,
    pub blocks: Vec<Block>,
    /// 正文内链区间（`<a href>` → 清洗后文本字节区间）。
    pub links: Vec<LinkRange>,
}

/// 空白折叠（XHTML 缩进空白不进正文；CJK 无空格语义不受影响）。
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            in_ws = true;
        } else {
            if in_ws {
                if !out.is_empty() {
                    out.push(' ');
                }
                in_ws = false;
            }
            out.push(c);
        }
    }
    out
}

struct Cleaner {
    out: CleanResult,
    pending: Vec<Inline>,
    heading_level: Option<u8>,
    bold_depth: u32,
    italic_depth: u32,
    skip_depth: u32,
    /// 段内是否已有非空白内容。
    has_content: bool,
    /// 未闭合 `<a href>` 栈：(href, 起始字节偏移)。
    link_stack: Vec<(String, u32)>,
}

impl Cleaner {
    fn style(&self) -> StyleFlags {
        let mut f = 0u8;
        if self.bold_depth > 0 {
            f |= StyleFlags::BOLD;
        }
        if self.italic_depth > 0 {
            f |= StyleFlags::ITALIC;
        }
        StyleFlags(f)
    }

    fn push_text(&mut self, raw: &str) {
        if self.skip_depth > 0 {
            return;
        }
        let t = collapse_ws(raw);
        if t.is_empty() {
            return;
        }
        if t.trim().is_empty() && !self.has_content {
            return;
        }
        let start = self.out.text.len() as u32;
        self.out.text.push_str(&t);
        let end = self.out.text.len() as u32;
        self.pending.push(Inline::Text { range: TextRange::new(start, end), style: self.style() });
        self.has_content = true;
    }

    fn flush(&mut self) {
        if self.pending.is_empty() {
            self.has_content = false;
            return;
        }
        let inlines = std::mem::take(&mut self.pending);
        self.has_content = false;
        // 去掉段尾空白
        if let Some(Inline::Text { range, .. }) = inlines.last() {
            let r = *range;
            let end = self.out.text.len() as u32;
            if end == r.end {
                let trimmed = self.out.text.trim_end();
                let new_len = trimmed.len() as u32;
                self.out.text.truncate(new_len as usize);
            }
        }
        let block = match self.heading_level.take() {
            Some(level) => Block::Heading { level, inlines },
            None => Block::Paragraph { inlines, indent: false },
        };
        self.out.blocks.push(block);
    }
}

/// 清洗一段 XHTML/HTML 为统一 Block 流。
///
/// 白名单块级：p / h1..h6 / div / section / li / blockquote / hr / br / img；
/// 行内样式：strong/b（加粗）、em/i（斜体）；其余标签丢弃但保留文本内容。
/// `head`/`script`/`style` 整体跳过。blockquote 内文 P0 平铺为段落（Quote 嵌套后置）。
pub fn clean_xhtml(html: &str) -> CleanResult {
    let mut c = Cleaner {
        out: CleanResult::default(),
        pending: Vec::new(),
        heading_level: None,
        bold_depth: 0,
        italic_depth: 0,
        skip_depth: 0,
        has_content: false,
        link_stack: Vec::new(),
    };

    for token in tokenize(html) {
        match token {
            Token::Text(t) => c.push_text(&t),
            Token::Open { name, attrs, self_closing } => {
                let n = local_name(&name).to_string();
                match n.as_str() {
                    "head" | "script" | "style" | "title" | "meta" | "link" => {
                        if !self_closing {
                            c.skip_depth += 1;
                        }
                    }
                    "br" => c.flush(),
                    "hr" => {
                        c.flush();
                        c.out.blocks.push(Block::Divider);
                    }
                    "img" => {
                        c.flush();
                        // MOBI 的图片引用是 recindex（记录号），EPUB 是 src 路径
                        let key = attr(&attrs, "src")
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .or_else(|| {
                                attr(&attrs, "recindex")
                                    .and_then(|s| s.parse::<u32>().ok())
                                    .map(|n| format!("rec:{}", n.saturating_sub(1)))
                            })
                            .unwrap_or_default();
                        if !key.is_empty() {
                            let size_hint = match (
                                attr(&attrs, "width").and_then(|w| w.parse().ok()),
                                attr(&attrs, "height").and_then(|h| h.parse().ok()),
                            ) {
                                (Some(w), Some(h)) => Some((w, h)),
                                _ => None,
                            };
                            c.out.blocks.push(Block::Image {
                                key: ResourceKey(key),
                                alt: attr(&attrs, "alt").map(String::from),
                                size_hint,
                            });
                        }
                    }
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        c.flush();
                        c.heading_level = Some(n[1..2].parse().unwrap_or(1));
                    }
                    "p" | "div" | "section" | "article" | "li" | "tr" | "td" | "blockquote"
                    | "header" | "footer" | "pre" | "table" | "ul" | "ol" => {
                        c.flush();
                    }
                    "strong" | "b" => c.bold_depth += 1,
                    "em" | "i" => c.italic_depth += 1,
                    "a" => {
                        // 内链旁路：记录 href 与区间起点（跳过 head/script 等忽略区）
                        if c.skip_depth == 0 {
                            if let Some(href) = attr(&attrs, "href") {
                                if !href.trim().is_empty() {
                                    c.link_stack
                                        .push((href.trim().to_string(), c.out.text.len() as u32));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                if self_closing {
                    // 自闭合回滚 skip 计数
                    if matches!(n.as_str(), "head" | "script" | "style" | "title" | "meta" | "link")
                        && c.skip_depth > 0
                    {
                        c.skip_depth -= 1;
                    }
                }
            }
            Token::Close(name) => {
                let n = local_name(&name).to_string();
                match n.as_str() {
                    "head" | "script" | "style" | "title" => {
                        if c.skip_depth > 0 {
                            c.skip_depth -= 1;
                        }
                    }
                    "p" | "div" | "section" | "article" | "li" | "tr" | "td" | "blockquote"
                    | "header" | "footer" | "pre" | "table" | "ul" | "ol" => c.flush(),
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => c.flush(),
                    "strong" | "b" => {
                        if c.bold_depth > 0 {
                            c.bold_depth -= 1;
                        }
                    }
                    "em" | "i" => {
                        if c.italic_depth > 0 {
                            c.italic_depth -= 1;
                        }
                    }
                    "a" => {
                        if let Some((href, start)) = c.link_stack.pop() {
                            let end = c.out.text.len() as u32;
                            if end > start {
                                c.out.links.push(LinkRange {
                                    range: TextRange::new(start, end),
                                    href,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    c.flush();
    // 段尾空白裁剪可能让已记录的内链区间越界：夹到文本长度并丢弃空区间。
    let len = c.out.text.len() as u32;
    for l in &mut c.out.links {
        if l.range.end > len {
            l.range = TextRange::new(l.range.start.min(len), len);
        }
    }
    c.out.links.retain(|l| l.range.end > l.range.start);
    c.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para_texts(r: &CleanResult) -> Vec<String> {
        r.blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph { inlines, .. } | Block::Heading { inlines, .. } => inlines
                    .iter()
                    .map(|i| match i {
                        Inline::Text { range, .. } => range.slice(&r.text).to_string(),
                        Inline::Image { key } => format!("<img:{}>", key.0),
                    })
                    .collect::<String>(),
                Block::Image { key, .. } => format!("<img:{}>", key.0),
                Block::Divider => "<hr>".into(),
                Block::Quote { .. } => "<quote>".into(),
            })
            .collect()
    }

    #[test]
    fn cleans_basic_chapter() {
        let html = r#"<?xml version="1.0"?>
<html><head><title>忽略</title><style>p{}</style></head>
<body>
<h1>第一章 测试</h1>
<p>　　<strong>加粗</strong>正文 &amp; 实体。</p>
<p>第二段<br/>换行后。</p>
<img src="images/a.jpg" alt="封面" width="100" height="50"/>
</body></html>"#;
        let r = clean_xhtml(html);
        let texts = para_texts(&r);
        assert_eq!(texts[0], "第一章 测试");
        assert_eq!(texts[1], "加粗正文 & 实体。");
        assert_eq!(texts[2], "第二段");
        assert_eq!(texts[3], "换行后。");
        assert_eq!(texts[4], "<img:images/a.jpg>");
        assert!(!r.text.contains("忽略"));
        assert!(!r.text.contains("p{}"));
    }

    #[test]
    fn heading_level_and_style() {
        let html = "<h2>节</h2><p><em>斜</em>体</p>";
        let r = clean_xhtml(html);
        assert!(matches!(&r.blocks[0], Block::Heading { level: 2, .. }));
        if let Block::Paragraph { inlines, .. } = &r.blocks[1] {
            match &inlines[0] {
                Inline::Text { style, .. } => assert!(style.is_italic()),
                _ => panic!(),
            }
        } else {
            panic!()
        }
    }

    /// #1：`<a href>` 必须产出「文本区间 → href」映射，区间正好覆盖链接文字。
    #[test]
    fn captures_link_ranges() {
        let html = r#"<p>前文<a href="ch2.xhtml#s1">链接文字</a>后文</p>"#;
        let r = clean_xhtml(html);
        assert_eq!(r.links.len(), 1, "应捕获一条内链");
        let l = &r.links[0];
        assert_eq!(l.href, "ch2.xhtml#s1");
        let slice = &r.text[l.range.start as usize..l.range.end as usize];
        assert_eq!(slice, "链接文字");
    }

    /// 无 href 的锚点（`<a name="x">`）不产生内链；`head`/`script` 内的 `<a>` 也不产生。
    #[test]
    fn link_ranges_ignore_anchor_and_skipped_blocks() {
        let html = r#"<a name="top"></a><script><a href="x">坏</a></script><p><a>纯锚</a></p>"#;
        let r = clean_xhtml(html);
        assert!(r.links.is_empty(), "不应捕获：{:?}", r.links);
    }
}
