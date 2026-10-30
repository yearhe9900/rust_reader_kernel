//! 版式层（方案 §2.3）：`LayoutPage` 数据模型 + 自研二进制编解码（布局缓存 payload）。
//!
//! 数据面硬纪律：`LayoutPage` 不可变、可整体归档；缓存 key = contentHash × styleHash ×
//! viewportHash（三要素）。编解码自带 `RLAY` magic + schema 号，损坏即整包丢弃重算。

use crate::{ChapterId, ResourceKey, SemanticPos};

/// 对齐方式（宿主可调；取值与 FFI `align` 参数一一对应：0=Start 1=Center 2=End 3=Justify）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// 起始对齐（LTR = 左）。
    Start,
    /// 居中。
    Center,
    /// 末尾对齐（LTR = 右）。
    End,
    /// 两端对齐（最后一行仍按起始）。
    Justify,
}

/// 样式规格（styleHash 输入；任何字段变化 → 布局缓存失效）。
#[derive(Debug, Clone, PartialEq)]
pub struct StyleSpec {
    /// 字体族名（fallback 由引擎处理）。
    pub font_family: String,
    /// 正文字号（px / 逻辑像素）。
    pub font_size: f32,
    /// 行高（相对字号的倍数）。
    pub line_height: f32,
    /// 首行缩进，单位 em（基准 = `font_size`；CJK 习惯 2）。
    ///
    /// 仅对 `Block::Paragraph { indent: true }` 生效；0 = 不缩进。
    /// 该值已进 `style_hash`，改动即精确失效布局缓存。
    pub para_indent_em: f32,
    /// 对齐方式（见 [`Align`]）。
    pub align: Align,
}

impl Default for StyleSpec {
    fn default() -> Self {
        Self {
            // 字体策略（2026-10-21 拍板）：重排类一律系统字体栈（CSS family 列表，
            // 平台取首个可用）；书内字体仅固定版式（PDF 原版页）保真使用。
            font_family: "Microsoft YaHei, PingFang SC, Noto Sans CJK SC, sans-serif".to_string(),
            font_size: 18.0,
            line_height: 1.6,
            para_indent_em: 2.0,
            align: Align::Start,
        }
    }
}

/// 视口。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    pub dpr: f32,
}

/// 行表条目（紧凑四列，方案 §2.3「TypedData 紧凑化」口径）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineEntry {
    /// 行首字节偏移（章文本坐标）。
    pub start: u32,
    /// 行尾字节偏移。
    pub end: u32,
    pub height: f32,
    /// 段落首行。
    pub para_start: bool,
}

/// 文本 run（同一字体/字号/样式的连续文本段；坐标为行内布局坐标）。
#[derive(Debug, Clone, PartialEq)]
pub struct TextRun {
    pub line: u32,
    /// run 起点 x。
    pub x: f32,
    /// 基线 y（页内坐标）。
    pub baseline: f32,
    pub font_size: f32,
    pub bold: bool,
    pub italic: bool,
    /// run 像素宽度（hit_test 按比例定位用）。
    pub width: f32,
    /// 文本范围（章文本 UTF-8 字节偏移）。
    pub range: (u32, u32),
}

/// 图片占位。
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedImage {
    pub key: ResourceKey,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// 版式页（不可变产物，rkyv 等价物 = 本文件的 RLAY 编解码）。
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutPage {
    pub index: u32,
    pub chapter: ChapterId,
    pub start: SemanticPos,
    pub end: SemanticPos,
    pub lines: Vec<LineEntry>,
    pub runs: Vec<TextRun>,
    pub images: Vec<PlacedImage>,
    pub height: f32,
}

/// 渲染输出（方案 §4.3 两种消费方式）。
#[derive(Debug, Clone)]
pub enum RenderResult {
    /// 模式 A：几何 → 宿主自绘。
    Geometry(LayoutPage),
    /// 模式 B：整页栅格化。
    Bitmap { width: u32, height: u32, rgba: Vec<u8> },
}

// ---------------------------------------------------------------------------
// RLAY 编解码（布局缓存 payload）
// ---------------------------------------------------------------------------

const RLAY_MAGIC: &[u8; 4] = b"RLAY";
const RLAY_SCHEMA: u32 = 2; // v2: 新增 TextRun.width（v1 解码器不认 → 丢弃重算，安全）

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
}

struct R<'a> {
    d: &'a [u8],
    p: usize,
}
impl<'a> R<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.d.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        let v = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_le_bytes(v.try_into().unwrap()))
    }
    fn f32(&mut self) -> Option<f32> {
        let v = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(f32::from_le_bytes(v.try_into().unwrap()))
    }
    fn done(&self) -> bool {
        self.p >= self.d.len()
    }
}

/// 编码一组布局页为缓存 payload。
pub fn encode_pages(pages: &[LayoutPage]) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(1024 * pages.len().max(1)));
    w.0.extend_from_slice(RLAY_MAGIC);
    w.u32(RLAY_SCHEMA);
    w.u32(pages.len() as u32);
    for p in pages {
        w.u32(p.index);
        w.u32(p.chapter);
        w.u32(p.start.char_offset);
        w.u32(p.end.char_offset);
        w.f32(p.height);
        w.u32(p.lines.len() as u32);
        for l in &p.lines {
            w.u32(l.start);
            w.u32(l.end);
            w.f32(l.height);
            w.u8(l.para_start as u8);
        }
        w.u32(p.runs.len() as u32);
        for r in &p.runs {
            w.u32(r.line);
            w.f32(r.x);
            w.f32(r.baseline);
            w.f32(r.font_size);
            w.u8(((r.bold as u8) << 1) | r.italic as u8);
            w.u32(r.range.0);
            w.u32(r.range.1);
            w.f32(r.width);
        }
        w.u32(p.images.len() as u32);
        for im in &p.images {
            w.u32(im.key.0.len() as u32);
            w.0.extend_from_slice(im.key.0.as_bytes());
            w.f32(im.x);
            w.f32(im.y);
            w.f32(im.w);
            w.f32(im.h);
        }
    }
    w.0
}

/// 解码缓存 payload；**任何不一致返回 None（损坏隔离，整包丢弃）**。
pub fn decode_pages(data: &[u8]) -> Option<Vec<LayoutPage>> {
    let mut r = R { d: data, p: 0 };
    if r.d.len() < 12 || &r.d[0..4] != RLAY_MAGIC {
        return None;
    }
    r.p = 4;
    let schema = r.u32()?;
    if schema != 1 && schema != RLAY_SCHEMA {
        return None;
    }
    let n = r.u32()? as usize;
    if n > 1_000_000 {
        return None;
    }
    let mut pages = Vec::with_capacity(n);
    for _ in 0..n {
        let index = r.u32()?;
        let chapter = r.u32()?;
        let start_off = r.u32()?;
        let end_off = r.u32()?;
        let height = r.f32()?;
        let nl = r.u32()? as usize;
        if nl > 1_000_000 {
            return None;
        }
        let mut lines = Vec::with_capacity(nl);
        for _ in 0..nl {
            lines.push(LineEntry {
                start: r.u32()?,
                end: r.u32()?,
                height: r.f32()?,
                para_start: r.u8()? != 0,
            });
        }
        let nr = r.u32()? as usize;
        if nr > 10_000_000 {
            return None;
        }
        let mut runs = Vec::with_capacity(nr);
        for _ in 0..nr {
            let line = r.u32()?;
            let x = r.f32()?;
            let baseline = r.f32()?;
            let font_size = r.f32()?;
            let flags = r.u8()?;
            let range = (r.u32()?, r.u32()?);
            // v2 新增字段；v1 缺省为 0（调用方需容错）
            let width = if schema >= 2 { r.f32()? } else { 0.0 };
            runs.push(TextRun {
                line,
                x,
                baseline,
                font_size,
                bold: flags & 2 != 0,
                italic: flags & 1 != 0,
                width,
                range,
            });
        }
        let ni = r.u32()? as usize;
        if ni > 1_000_000 {
            return None;
        }
        let mut images = Vec::with_capacity(ni);
        for _ in 0..ni {
            let len = r.u32()? as usize;
            if len > 10_000 || r.p + len > r.d.len() {
                return None;
            }
            let key = String::from_utf8(r.d[r.p..r.p + len].to_vec()).ok()?;
            r.p += len;
            images.push(PlacedImage {
                key: ResourceKey(key),
                x: r.f32()?,
                y: r.f32()?,
                w: r.f32()?,
                h: r.f32()?,
            });
        }
        pages.push(LayoutPage {
            index,
            chapter,
            start: SemanticPos { chapter, char_offset: start_off },
            end: SemanticPos { chapter, char_offset: end_off },
            lines,
            runs,
            images,
            height,
        });
    }
    if r.done() {
        Some(pages)
    } else {
        None
    }
}

/// 样式哈希输入（三要素之二）。
pub fn style_hash(s: &StyleSpec) -> u64 {
    // 增量喂入（免 64B Vec 拼接；每次 pagesOf/page_at 都会调）；字节序与旧实现
    // 逐字段拼接完全一致，已有磁盘缓存不失效。
    use crate::{fnv1a64_update, FNV_INIT};
    let mut h = FNV_INIT;
    h = fnv1a64_update(h, s.font_family.as_bytes());
    h = fnv1a64_update(h, &s.font_size.to_bits().to_le_bytes());
    h = fnv1a64_update(h, &s.line_height.to_bits().to_le_bytes());
    h = fnv1a64_update(h, &s.para_indent_em.to_bits().to_le_bytes());
    fnv1a64_update(h, &[s.align as u8])
}

/// 视口哈希输入（三要素之三）。
pub fn viewport_hash(v: &Viewport) -> u64 {
    // 只含 width/height：排版引擎只消费这两项（width 断行 / height 分页）；
    // dpr 不参与排版，不入 key——免「改 dpr / 旋转设备白打散整章布局缓存」。
    // 旧 key 含 dpr，口径变更后旧缓存自然 miss 重算，无正确性影响。
    use crate::{fnv1a64_update, FNV_INIT};
    let mut h = FNV_INIT;
    h = fnv1a64_update(h, &v.width.to_bits().to_le_bytes());
    fnv1a64_update(h, &v.height.to_bits().to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pages() -> Vec<LayoutPage> {
        vec![LayoutPage {
            index: 0,
            chapter: 3,
            start: SemanticPos { chapter: 3, char_offset: 10 },
            end: SemanticPos { chapter: 3, char_offset: 99 },
            lines: vec![
                LineEntry { start: 10, end: 50, height: 28.0, para_start: true },
                LineEntry { start: 50, end: 99, height: 28.0, para_start: false },
            ],
            runs: vec![TextRun {
                line: 0,
                x: 36.0,
                baseline: 22.0,
                font_size: 18.0,
                bold: true,
                italic: false,
                range: (10, 50),
                width: 40.0,
            }],
            images: vec![PlacedImage {
                key: ResourceKey("img/1.png".into()),
                x: 0.0,
                y: 60.0,
                w: 100.0,
                h: 80.0,
            }],
            height: 800.0,
        }]
    }

    #[test]
    fn rlay_roundtrip() {
        let pages = sample_pages();
        let bytes = encode_pages(&pages);
        let back = decode_pages(&bytes).unwrap();
        assert_eq!(pages, back);
    }

    #[test]
    fn rlay_corruption_returns_none() {
        // 校验和由 RRKC 容器层负责（rrk-cache）；RLAY 层负责结构自洽（截断/长度错乱 → None）
        let bytes = encode_pages(&sample_pages());
        assert!(decode_pages(&bytes[..bytes.len() - 5]).is_none(), "截断应拒收");
        let mut truncated_len = bytes.clone();
        truncated_len[4] = 0xFF; // schema 号篡改
        assert!(decode_pages(&truncated_len).is_none(), "schema 不符应拒收");
    }

    #[test]
    fn hashes_change_with_inputs() {
        let s1 = StyleSpec::default();
        let mut s2 = s1.clone();
        s2.font_size = 20.0;
        assert_ne!(style_hash(&s1), style_hash(&s2));
        let v = Viewport { width: 390.0, height: 844.0, dpr: 3.0 };
        let v2 = Viewport { width: 400.0, height: 844.0, dpr: 3.0 };
        assert_ne!(viewport_hash(&v), viewport_hash(&v2));
    }
}
