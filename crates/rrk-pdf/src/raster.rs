//! PDF L2 光栅化（方案 §3.6，自研）：内容流图形算子 → RGBA 位图。
//!
//! 覆盖（L2 基础集）：
//! - 图形状态：q/Q、cm（CTM）、rg/RG/g/G/k/K/sc/SC 颜色、w 线宽、W/W\* 裁剪（遮罩）；
//! - 路径：m/l/c/v/y/re/h + f/f\*/F/S/s/B/B\*/b/b\*/fS 填充与描边（tiny-skia 光栅原语）；
//! - 图像 XObject：DCT（JPEG）/原始位图/图像遮罩，任意仿射（最近邻逆映射）；
//! - 文本：BT/ET、Tf、Tm/Td/TD/T\*、Tj/TJ/'/" —— 字形经 swash 光栅为 alpha 遮罩，
//!   按填充色混合；字符→字形映射走 ToUnicode（与 L1 同源）。
//!
//! L3 部分覆盖（2026-10-21）：ExtGState 透明度（/ca /CA）、渐变填充（sh 轴向/径向）。
//! L3 长尾（明确不覆盖及原因）：
//! - Type1 字体轮廓（PFA/PFB charstring 解释器，样张驱动排期）；
//! - JPX/JBIG2 图像：解码需 openjpeg/jbig2dec 等 C 绑定，违反自研红线 → 降级占位；
//! - 透明组/混合模式（/BM、/Group）：tiny-skia fill_path 固定 SourceOver，需图层管线；
//! - 采样函数（FunctionType 0/3）、图案/底纹、Type3 字体、标注/表单；
//! - 双栏/复杂版面重排（L1 文本抽取保序，重排属 §3.6 L3 长尾）。
//! 失败页按 L1 文本模式降级（上层口径）。

use std::collections::HashMap;
use std::sync::Arc;

use swash::scale::{Render, ScaleContext, Source};
use swash::FontRef;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Point, Transform};

use crate::content::parse_tounicode;
use crate::objects::{Dict, Object, Parser, PdfDocument};
use rrk_model::{RrkError, RrkErrorKind, RrkResult};

// ---------------------------------------------------------------------------
// 字体
// ---------------------------------------------------------------------------

/// 已解析的光栅字体（文档级缓存，按字体对象号跨页复用——FontFile2 解压与
/// ToUnicode 解析是每页重建时的最大单项开销）。
pub(crate) struct RasterFont {
    data: Arc<[u8]>,
    to_unicode: Option<HashMap<u16, String>>,
    two_byte: bool,
    fallback_path: String,
    /// CID 字体（Type0）：两字节码即 CID，经 CIDToGIDMap → GID（子集 CID 字体常无 unicode cmap）。
    cid_mode: bool,
    /// CID → GID 映射表（每 CID 2 字节大端）；None = Identity。
    cid_to_gid: Option<Arc<[u8]>>,
}

/// 裸 CFF（FontFile3/Type1C）→ 最小 OTF 封装（自研 sfnt 容器：单 CFF 表），
/// 供轮廓光栅器按 OpenType 路径解析。
fn wrap_cff_as_otf(cff: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(cff.len() + 64);
    out.extend_from_slice(b"OTTO"); // CFF-flavor sfnt
    out.extend_from_slice(&1u16.to_be_bytes()); // numTables
    out.extend_from_slice(&16u16.to_be_bytes()); // searchRange
    out.extend_from_slice(&0u16.to_be_bytes()); // entrySelector
    out.extend_from_slice(&0u16.to_be_bytes()); // rangeShift
    let data_offset = 12 + 16;
    out.extend_from_slice(b"CFF ");
    out.extend_from_slice(&0u32.to_be_bytes()); // checksum（光栅不校验）
    out.extend_from_slice(&(data_offset as u32).to_be_bytes());
    out.extend_from_slice(&(cff.len() as u32).to_be_bytes());
    out.extend_from_slice(cff);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

/// 标准 14 → 系统字体文件（找不到退 arial）。
fn system_font_path(base: &str) -> String {
    let lower = base.to_ascii_lowercase();
    let file = if lower.contains("times") {
        "times.ttf"
    } else if lower.contains("courier") {
        "cour.ttf"
    } else if lower.contains("symbol") {
        "symbol.ttf"
    } else {
        "arial.ttf"
    };
    for root in ["C:/Windows/Fonts", "/usr/share/fonts"] {
        let p = std::path::Path::new(root).join(file);
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    file.to_string()
}

fn collect_raster_fonts(
    doc: &PdfDocument,
    resources: Option<&Object>,
) -> HashMap<String, Arc<RasterFont>> {
    let mut fonts = HashMap::new();
    let Some(res) = resources else { return fonts };
    let res = doc.resolve(res);
    let Some(res_dict) = res.as_dict() else { return fonts };
    let Some(font_obj) = res_dict.get("Font") else { return fonts };
    let font_dict = doc.resolve(font_obj);
    let Some(entries) = font_dict.as_dict() else { return fonts };
    for (name, ref_obj) in entries {
        // 文档级缓存：同一字体对象跨页只 inflate/解析一次（旧实现每页重建，
        // CJK 子集字体数百 KB 的 FontFile2 被逐页重复解压）
        let cached_num = match ref_obj {
            Object::Ref(num, _) => Some(*num),
            _ => None,
        };
        if let Some(num) = cached_num {
            if let Some(f) = doc.font_cache.lock().unwrap().get(&num) {
                fonts.insert(name.clone(), Arc::clone(f));
                continue;
            }
        }
        let font = doc.resolve(ref_obj);
        let Some(fd) = font.as_dict() else { continue };

        let mut to_unicode = None;
        if let Some(tu) = fd.get("ToUnicode") {
            if let Ok(data) = doc.stream_data(tu) {
                let map = parse_tounicode(&data);
                if !map.is_empty() {
                    to_unicode = Some(map);
                }
            }
        }
        let two_byte = to_unicode
            .as_ref()
            .map(|m| m.keys().any(|k| *k > 0xFF))
            .unwrap_or(false);

        // CID 字体（Type0）：FontDescriptor/FontFile2/CIDToGIDMap 都在 DescendantFonts[0] 里
        let mut cid_mode = false;
        let mut cid_to_gid: Option<Arc<[u8]>> = None;
        let mut dict_for_font = fd.clone();
        if let Some(desc_arr) = fd.get("DescendantFonts").and_then(|d| d.as_array()) {
            cid_mode = true;
            if let Some(cid_fd) = desc_arr.first().map(|d| doc.resolve(d)) {
                if let Some(d) = cid_fd.as_dict() {
                    dict_for_font = d.clone();
                }
                if let Some(m) = cid_fd.get("CIDToGIDMap") {
                    if let Object::Stream { .. } = doc.resolve(m) {
                        if let Ok(bytes) = doc.stream_data(m) {
                            cid_to_gid = Some(Arc::from(bytes.into_boxed_slice()));
                        }
                    }
                }
            }
        }

        let mut data: Arc<[u8]> = Arc::from(&b""[..]);
        if let Some(desc) = dict_for_font.get("FontDescriptor").map(|d| doc.resolve(d)) {
            for key in ["FontFile2", "FontFile3"] {
                if let Some(ff) = desc.get(key) {
                    if let Ok(bytes) = doc.stream_data(ff) {
                        data = if key == "FontFile3" {
                            // Type1C = 裸 CFF：自研 sfnt 封装成 OTF 供轮廓光栅
                            Arc::from(wrap_cff_as_otf(&bytes).into_boxed_slice())
                        } else {
                            Arc::from(bytes.into_boxed_slice())
                        };
                        break;
                    }
                }
            }
        }
        let base = fd
            .get("BaseFont")
            .and_then(|b| b.as_name())
            .unwrap_or("Helvetica")
            .to_string();

        let built = Arc::new(RasterFont {
            data,
            to_unicode,
            two_byte,
            fallback_path: system_font_path(&base),
            cid_mode,
            cid_to_gid,
        });
        if let Some(num) = cached_num {
            doc.font_cache.lock().unwrap().insert(num, Arc::clone(&built));
        }
        fonts.insert(name.clone(), built);
    }
    fonts
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

/// 解码后的图像位图（文档级缓存条目：同一 XObject 跨页/跨 Do 免重复
/// inflate + JPEG/原始位图解码）。
pub(crate) struct CachedImage {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// 图像解码缓存（有字节上限：超限整表清空，宁可偶发重解也不无界增长）。
pub(crate) struct RasterImageCache {
    map: HashMap<u32, Arc<CachedImage>>,
    bytes: usize,
}

impl Default for RasterImageCache {
    fn default() -> Self {
        Self { map: HashMap::new(), bytes: 0 }
    }
}

/// 图像缓存字节上限（单文档；典型 PDF 十几页插图在限内）。
const IMAGE_CACHE_MAX_BYTES: usize = 96 * 1024 * 1024;

impl RasterImageCache {
    fn get(&self, num: u32) -> Option<Arc<CachedImage>> {
        self.map.get(&num).map(Arc::clone)
    }

    fn put(&mut self, num: u32, img: Arc<CachedImage>) {
        self.bytes += img.rgba.len();
        if self.bytes > IMAGE_CACHE_MAX_BYTES {
            self.map.clear();
            self.bytes = 0;
        }
        self.map.insert(num, img);
    }
}

#[derive(Clone)]
struct GState {
    ctm: Transform,
    fill: (f32, f32, f32, f32),
    stroke: (f32, f32, f32, f32),
    line_width: f32,
    clip: Option<Arc<tiny_skia::Mask>>,
    /// ExtGState 透明度（/ca /CA）。
    fill_alpha: f32,
    stroke_alpha: f32,
}

impl Default for GState {
    fn default() -> Self {
        Self {
            ctm: Transform::identity(),
            fill: (0.0, 0.0, 0.0, 1.0),
            stroke: (0.0, 0.0, 0.0, 1.0),
            line_width: 1.0,
            clip: None,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
        }
    }
}

#[derive(Clone)]
struct TextState {
    tm: Transform,
    tlm: Transform,
    font: Option<String>,
    size: f32,
    char_space: f32,
    word_space: f32,
    leading: f32,
    h_scale: f32,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            tm: Transform::identity(),
            tlm: Transform::identity(),
            font: None,
            size: 12.0,
            char_space: 0.0,
            word_space: 0.0,
            leading: 0.0,
            h_scale: 1.0,
        }
    }
}

fn map_xy(t: &Transform, x: f32, y: f32) -> (f32, f32) {
    (t.sx * x + t.kx * y + t.tx, t.ky * x + t.sy * y + t.ty)
}

fn pdf_matrix(a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) -> Transform {
    Transform::from_row(a, b, c, d, e, f)
}

// ---------------------------------------------------------------------------
// 光栅器
// ---------------------------------------------------------------------------

pub struct Rasterizer<'a> {
    doc: &'a PdfDocument,
    resources: Option<Object>,
    pixmap: Pixmap,
    scale: f32,
    gs: GState,
    stack: Vec<GState>,
    text: TextState,
    fonts: HashMap<String, Arc<RasterFont>>,
    font_bytes: HashMap<String, Arc<[u8]>>,
    scale_cx: ScaleContext,
    pb: Option<PathBuilder>,
    last_path: Option<tiny_skia::Path>,
    last_point: (f32, f32),
}

impl<'a> Rasterizer<'a> {
    /// 创建：page_w/page_h 为 pt；输出像素 = pt × scale；y 轴翻转到设备坐标。
    pub fn new(
        doc: &'a PdfDocument,
        resources: Option<Object>,
        page_w: f32,
        page_h: f32,
        scale: f32,
    ) -> RrkResult<Self> {
        let w = (page_w * scale).round().max(1.0) as u32;
        let h = (page_h * scale).round().max(1.0) as u32;
        let mut pixmap = Pixmap::new(w, h).ok_or_else(|| {
            RrkError::new(RrkErrorKind::ResourceLimit, format!("页面过大: {w}x{h}"))
        })?;
        pixmap.fill(tiny_skia::Color::WHITE);
        let fonts = collect_raster_fonts(doc, resources.as_ref());
        let mut gs = GState::default();
        gs.ctm = pdf_matrix(scale, 0.0, 0.0, -scale, 0.0, page_h * scale);
        Ok(Self {
            doc,
            resources,
            pixmap,
            scale,
            gs,
            stack: Vec::new(),
            text: TextState::default(),
            fonts,
            font_bytes: HashMap::new(),
            scale_cx: ScaleContext::new(),
            pb: None,
            last_path: None,
            last_point: (0.0, 0.0),
        })
    }

    /// 执行内容流并取位图（RGBA8）。
    pub fn run(&mut self, content: &[u8]) -> (Vec<u8>, u32, u32) {
        self.exec(content);
        let (w, h) = (self.pixmap.width(), self.pixmap.height());
        (self.pixmap.data_mut().to_vec(), w, h)
    }

    fn exec(&mut self, content: &[u8]) {
        let mut ops: Vec<Object> = Vec::new();
        let mut p = Parser::new(content);
        loop {
            p.skip_ws();
            if p.pos >= p.data.len() {
                break;
            }
            let b = p.data[p.pos];
            let is_value = matches!(
                b,
                b'(' | b'<' | b'/' | b'[' | b'+' | b'-' | b'.' | b'0'..=b'9'
            ) || p.data[p.pos..].starts_with(b"<<");
            if is_value {
                match p.parse_object() {
                    Ok(o) => {
                        ops.push(o);
                        continue;
                    }
                    Err(_) => break,
                }
            }
            let start = p.pos;
            while p.pos < p.data.len() {
                let c = p.data[p.pos];
                if (c as char).is_ascii_whitespace()
                    || matches!(c, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'/' | b'%')
                {
                    break;
                }
                p.pos += 1;
            }
            if p.pos == start {
                p.pos += 1;
                continue;
            }
            let kw: &[u8] = &p.data[start..p.pos];
            self.op(kw, &ops);
            ops.clear();
        }
    }

    fn num(ops: &[Object], from_end: usize) -> f32 {
        match ops.get(ops.len().wrapping_sub(1 + from_end)) {
            Some(Object::Int(i)) => *i as f32,
            Some(Object::Real(f)) => *f as f32,
            _ => 0.0,
        }
    }

    fn op(&mut self, kw: &[u8], ops: &[Object]) {
        match kw {
            b"q" => self.stack.push(self.gs.clone()),
            b"Q" => {
                if let Some(g) = self.stack.pop() {
                    self.gs = g;
                }
            }
            b"cm" if ops.len() >= 6 => {
                let m = pdf_matrix(
                    Self::num(ops, 5),
                    Self::num(ops, 4),
                    Self::num(ops, 3),
                    Self::num(ops, 2),
                    Self::num(ops, 1),
                    Self::num(ops, 0),
                );
                self.gs.ctm = m.post_concat(self.gs.ctm);
            }
            b"w" => self.gs.line_width = Self::num(ops, 0).max(0.0),
            b"rg" if ops.len() >= 3 => {
                self.gs.fill = (Self::num(ops, 2), Self::num(ops, 1), Self::num(ops, 0), 1.0);
            }
            b"RG" if ops.len() >= 3 => {
                self.gs.stroke = (Self::num(ops, 2), Self::num(ops, 1), Self::num(ops, 0), 1.0);
            }
            b"g" => {
                let v = Self::num(ops, 0);
                self.gs.fill = (v, v, v, 1.0);
            }
            b"G" => {
                let v = Self::num(ops, 0);
                self.gs.stroke = (v, v, v, 1.0);
            }
            b"k" if ops.len() >= 4 => {
                self.gs.fill = cmyk(Self::num(ops, 3), Self::num(ops, 2), Self::num(ops, 1), Self::num(ops, 0));
            }
            b"K" if ops.len() >= 4 => {
                self.gs.stroke = cmyk(Self::num(ops, 3), Self::num(ops, 2), Self::num(ops, 1), Self::num(ops, 0));
            }
            b"sc" | b"scn" if ops.len() >= 3 => {
                self.gs.fill = (Self::num(ops, 2), Self::num(ops, 1), Self::num(ops, 0), 1.0);
            }
            b"SC" | b"SCN" if ops.len() >= 3 => {
                self.gs.stroke = (Self::num(ops, 2), Self::num(ops, 1), Self::num(ops, 0), 1.0);
            }

            // 路径构造
            b"m" if ops.len() >= 2 => {
                let (x, y) = (Self::num(ops, 1), Self::num(ops, 0));
                let (dx, dy) = map_xy(&self.gs.ctm, x, y);
                self.pb.get_or_insert_with(PathBuilder::new).move_to(dx, dy);
                self.last_point = (dx, dy);
            }
            b"l" if ops.len() >= 2 => {
                let (x, y) = (Self::num(ops, 1), Self::num(ops, 0));
                let (dx, dy) = map_xy(&self.gs.ctm, x, y);
                if let Some(pb) = self.pb.as_mut() {
                    pb.line_to(dx, dy);
                }
                self.last_point = (dx, dy);
            }
            b"c" if ops.len() >= 6 => {
                let (x1, y1) = map_xy(&self.gs.ctm, Self::num(ops, 5), Self::num(ops, 4));
                let (x2, y2) = map_xy(&self.gs.ctm, Self::num(ops, 3), Self::num(ops, 2));
                let (x3, y3) = map_xy(&self.gs.ctm, Self::num(ops, 1), Self::num(ops, 0));
                if let Some(pb) = self.pb.as_mut() {
                    pb.cubic_to(x1, y1, x2, y2, x3, y3);
                }
                self.last_point = (x3, y3);
            }
            b"v" | b"y" if ops.len() >= 4 => {
                let (x2, y2) = (Self::num(ops, 3), Self::num(ops, 2));
                let (x3, y3) = (Self::num(ops, 1), Self::num(ops, 0));
                let c1 = if kw == b"v" {
                    self.last_point
                } else {
                    map_xy(&self.gs.ctm, x2, y2)
                };
                let (dx2, dy2) = map_xy(&self.gs.ctm, x2, y2);
                let (dx3, dy3) = map_xy(&self.gs.ctm, x3, y3);
                if let Some(pb) = self.pb.as_mut() {
                    pb.cubic_to(c1.0, c1.1, dx2, dy2, dx3, dy3);
                }
                self.last_point = (dx3, dy3);
            }
            b"re" if ops.len() >= 4 => {
                let (x, y) = (Self::num(ops, 3), Self::num(ops, 2));
                let (w, h) = (Self::num(ops, 1), Self::num(ops, 0));
                let p1 = map_xy(&self.gs.ctm, x, y);
                let p2 = map_xy(&self.gs.ctm, x + w, y);
                let p3 = map_xy(&self.gs.ctm, x + w, y + h);
                let p4 = map_xy(&self.gs.ctm, x, y + h);
                let pb = self.pb.get_or_insert_with(PathBuilder::new);
                pb.move_to(p1.0, p1.1);
                pb.line_to(p2.0, p2.1);
                pb.line_to(p3.0, p3.1);
                pb.line_to(p4.0, p4.1);
                pb.close();
            }
            b"h" => {
                if let Some(pb) = self.pb.as_mut() {
                    pb.close();
                }
            }

            // 描绘
            b"f" | b"F" | b"f*" => {
                let rule = if kw == b"f*" { FillRule::EvenOdd } else { FillRule::Winding };
                self.paint_fill(rule);
            }
            b"S" | b"s" => {
                if kw == b"s" {
                    if let Some(pb) = self.pb.as_mut() {
                        pb.close();
                    }
                }
                self.paint_stroke();
            }
            b"B" | b"B*" | b"b" | b"b*" => {
                if matches!(kw, b"b" | b"b*") {
                    if let Some(pb) = self.pb.as_mut() {
                        pb.close();
                    }
                }
                let rule = if matches!(kw, b"B*" | b"b*") { FillRule::EvenOdd } else { FillRule::Winding };
                self.paint_fill(rule);
                self.paint_stroke_last();
            }
            b"fS" | b"Sf" => {
                self.paint_fill(FillRule::Winding);
                self.paint_stroke_last();
            }
            b"n" => {
                self.pb = None;
                self.last_path = None;
            }
            b"W" | b"W*" => {
                // 裁剪：当前路径 → alpha 遮罩；与既有裁剪求交（byte-min）
                if let Some(pb) = self.pb.take() {
                    if let Some(path) = pb.finish() {
                        if let Some(mut mask) =
                            tiny_skia::Mask::new(self.pixmap.width(), self.pixmap.height())
                        {
                            mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
                            if let Some(old) = &self.gs.clip {
                                // 相交限定在路径包围盒内（旧行为全页逐字节 min；
                                // 包围盒外 mask 必为 0，min 结果不变）
                                let b = path.bounds();
                                let pw = self.pixmap.width() as i32;
                                let ph = self.pixmap.height() as i32;
                                let w = self.pixmap.width() as usize;
                                let x0 = b.left().floor().max(0.0) as i32;
                                let y0 = b.top().floor().max(0.0) as i32;
                                let x1 = b.right().ceil().min(pw as f32) as i32;
                                let y1 = b.bottom().ceil().min(ph as f32) as i32;
                                let (a, bdata) = (mask.data_mut(), old.data());
                                for row in y0.max(0)..y1.max(0) {
                                    let rs = row as usize * w;
                                    let (ra, rb) = (&mut a[rs..rs + w], &bdata[rs..rs + w]);
                                    for col in x0.max(0)..x1.max(0) {
                                        let c = col as usize;
                                        ra[c] = ra[c].min(rb[c]);
                                    }
                                }
                            }
                            // Arc 包装：q/Q 克隆 GState 不再整页 memcpy 裁剪遮罩
                            self.gs.clip = Some(Arc::new(mask));
                        }
                    }
                }
            }

            // 文本
            b"BT" => {
                self.text.tm = Transform::identity();
                self.text.tlm = Transform::identity();
            }
            b"Tf" if ops.len() >= 2 => {
                self.text.font = ops[ops.len() - 2].as_name().map(String::from);
                self.text.size = Self::num(ops, 0);
            }
            b"TL" => self.text.leading = Self::num(ops, 0),
            b"Tc" => self.text.char_space = Self::num(ops, 0),
            b"Tw" => self.text.word_space = Self::num(ops, 0),
            b"Tz" => self.text.h_scale = Self::num(ops, 0) / 100.0,
            b"Td" | b"TD" if ops.len() >= 2 => {
                let (tx, ty) = (Self::num(ops, 1), Self::num(ops, 0));
                if kw == b"TD" {
                    self.text.leading = -ty;
                }
                let m = pdf_matrix(1.0, 0.0, 0.0, 1.0, tx, ty);
                self.text.tlm = m.post_concat(self.text.tlm);
                self.text.tm = self.text.tlm;
            }
            b"Tm" if ops.len() >= 6 => {
                self.text.tlm = pdf_matrix(
                    Self::num(ops, 5),
                    Self::num(ops, 4),
                    Self::num(ops, 3),
                    Self::num(ops, 2),
                    Self::num(ops, 1),
                    Self::num(ops, 0),
                );
                self.text.tm = self.text.tlm;
            }
            b"T*" => {
                let m = pdf_matrix(1.0, 0.0, 0.0, 1.0, 0.0, -self.text.leading);
                self.text.tlm = m.post_concat(self.text.tlm);
                self.text.tm = self.text.tlm;
            }
            b"Tj" | b"'" | b"\"" => {
                if kw != b"Tj" {
                    let m = pdf_matrix(1.0, 0.0, 0.0, 1.0, 0.0, -self.text.leading);
                    self.text.tlm = m.post_concat(self.text.tlm);
                    self.text.tm = self.text.tlm;
                }
                if let Some(s) = ops.last().and_then(|o| o.as_str()) {
                    self.show_text(s);
                }
            }
            b"TJ" => {
                if let Some(Object::Arr(items)) = ops.last() {
                    for it in items {
                        match it {
                            Object::Str(s) => self.show_text(s),
                            Object::Real(f) => self.kern(*f as f32),
                            Object::Int(i) => self.kern(*i as f32),
                            _ => {}
                        }
                    }
                }
            }

            // ExtGState（L3 可行部分：/ca /CA 透明度；/BM 混合模式受 tiny-skia
            // fill_path 固定 SourceOver 限制，归 L3 长尾）
            b"gs" => {
                if let Some(name) = ops.last().and_then(|o| o.as_name()) {
                    let name = name.to_string();
                    if let Some(res) = self.resources.clone() {
                        let res = self.doc.resolve(&res);
                        if let Some(eg) = res.get("ExtGState").map(|x| self.doc.resolve(x)) {
                            if let Some(g) = eg.get(&name).map(|o| self.doc.resolve(o)) {
                                if let Some(d) = g.as_dict() {
                                    if let Some(ca) = d.get("ca").and_then(|v| v.as_int().map(|i| i as f32).or(Some(match v { Object::Real(f) => *f as f32, _ => 1.0 }))) {
                                        self.gs.fill_alpha = ca.clamp(0.0, 1.0);
                                    }
                                    if let Some(Object::Real(ca)) = d.get("ca") {
                                        self.gs.fill_alpha = (*ca as f32).clamp(0.0, 1.0);
                                    }
                                    if let Some(Object::Real(ca)) = d.get("CA") {
                                        self.gs.stroke_alpha = (*ca as f32).clamp(0.0, 1.0);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // 渐变填充（L3 可行部分：ShadingType 2 轴向 / 3 径向，FunctionType 2）
            b"sh" => {
                if let Some(name) = ops.last().and_then(|o| o.as_name()) {
                    let name = name.to_string();
                    self.paint_shading(&name);
                }
            }

            // 图像
            b"Do" => {
                if let Some(name) = ops.last().and_then(|o| o.as_name()) {
                    let name = name.to_string();
                    self.do_xobject(&name);
                }
            }
            _ => {}
        }
    }

    fn kern(&mut self, thousandths: f32) {
        let dx = -thousandths / 1000.0 * self.text.size * self.text.h_scale;
        let m = pdf_matrix(1.0, 0.0, 0.0, 1.0, dx, 0.0);
        self.text.tm = m.post_concat(self.text.tm);
    }

    fn paint_fill(&mut self, rule: FillRule) {
        let path = self.pb.take().and_then(|pb| pb.finish());
        if let Some(path) = path {
            let mut paint = Paint::default();
            let (r, g, b, a) = self.gs.fill;
            paint.set_color_rgba8(fc(r), fc(g), fc(b), fc(a * self.gs.fill_alpha));
            let clip = self.gs.clip.as_deref();
            self.pixmap.fill_path(&path, &paint, rule, Transform::identity(), clip);
            self.last_path = Some(path);
        }
    }

    fn paint_stroke(&mut self) {
        let path = self.pb.take().and_then(|pb| pb.finish());
        if let Some(path) = path {
            self.stroke_path(&path);
            self.last_path = Some(path);
        }
    }

    fn paint_stroke_last(&mut self) {
        if let Some(path) = self.last_path.clone() {
            self.stroke_path(&path);
        }
    }

    fn stroke_path(&mut self, path: &tiny_skia::Path) {
        let mut paint = Paint::default();
        let (r, g, b, a) = self.gs.stroke;
        paint.set_color_rgba8(fc(r), fc(g), fc(b), fc(a * self.gs.stroke_alpha));
        let mut stroke = tiny_skia::Stroke::default();
        stroke.width = self.gs.line_width * self.scale;
        let clip = self.gs.clip.as_deref();
        self.pixmap.stroke_path(path, &paint, &stroke, Transform::identity(), clip);
    }

    // ---- 文本 ----

    fn show_text(&mut self, bytes: &[u8]) {
        let Some(fname) = self.text.font.clone() else { return };
        let (cid_mode, cid_map) = match self.fonts.get(&fname) {
            Some(f) => (f.cid_mode, f.cid_to_gid.clone()),
            None => (false, None),
        };
        let font_data = self.load_font_data(&fname);
        let Some(font_ref) = FontRef::from_index(&font_data[..], 0) else { return };

        let size = self.text.size;
        let upem = font_ref.metrics(&[]).units_per_em.max(1) as f32;
        // 设备缩放来自 CTM（已含 pt→px scale），勿再乘 self.scale（否则字形重叠）
        let ppem = size * self.gs.ctm.sy.abs().max(0.0001);
        let glyph_metrics = font_ref.glyph_metrics(&[]);
        let charmap = font_ref.charmap();

        // 码流 → (gid, 占位宽度)：CID 字体两字节码即 CID（经 CIDToGIDMap），
        // 其余走 ToUnicode → unicode → cmap（与 L1 同源）。汉字上屏靠 CID 路径。
        let cid_gid = |cid: u16| -> u16 {
            match &cid_map {
                Some(map) => {
                    let i = cid as usize * 2;
                    if i + 2 <= map.len() {
                        u16::from_be_bytes([map[i], map[i + 1]])
                    } else {
                        cid
                    }
                }
                None => cid,
            }
        };
        let targets: Vec<u16> = if cid_mode {
            // 混合策略：ToUnicode→unicode→cmap 优先（TrueType CID 常带 unicode 表），
            // 缺字（gid=0）时按两字节 CID→CIDToGIDMap 兜底——两条路都不中才放弃该字形
            let chars = self.decode_chars(bytes);
            let cids: Vec<u16> = bytes
                .chunks(2)
                .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
                .collect();
            chars
                .iter()
                .zip(cids.iter().chain(std::iter::repeat(&0u16)))
                .map(|(ch, cid)| {
                    let g = if *ch != '\u{FFFD}' { charmap.map(*ch as u32) } else { 0 };
                    if g != 0 { g } else { cid_gid(*cid) }
                })
                .collect()
        } else {
            self.decode_chars(bytes)
                .into_iter()
                .map(|ch| {
                    if ch == '\u{FFFD}' {
                        0
                    } else {
                        charmap.map(ch as u32)
                    }
                })
                .collect()
        };

        // scaler/Render 提到循环外：同一字体同一 ppem 整段文本不变——
        // 旧实现在逐字形循环里每次 builder().build()（swash 内部查表）+ Render::new
        let mut scaler = self.scale_cx.builder(font_ref).size(ppem).build();
        let render = Render::new(&[Source::Outline]);
        for gid in targets {
            let adv_units = if gid == 0 { upem * 0.5 } else { glyph_metrics.advance_width(gid) };
            let adv = adv_units / upem * size * self.text.h_scale + self.text.char_space;

            if gid != 0 {
                // 设备位置 = CTM ∘ Tm（文本空间先作用）
                let t = self.text.tm.post_concat(self.gs.ctm);
                let (dx, dy) = map_xy(&t, 0.0, 0.0);
                if let Some(img) = render.render(&mut scaler, gid) {
                    let (mw, mh) = (img.placement.width as u32, img.placement.height as u32);
                    if mw > 0 && mh > 0 && img.data.len() == (mw * mh) as usize {
                        let px = dx.round() + img.placement.left as f32;
                        let py = dy.round() - img.placement.top as f32;
                        let color = self.gs.fill;
                        // 只借 pixmap 字段（scaler 仍借着 scale_cx，不能 &mut self）
                        Self::blend_mask(&mut self.pixmap, &img.data, mw, mh, px, py, color);
                    }
                }
            }
            let m = pdf_matrix(1.0, 0.0, 0.0, 1.0, adv, 0.0);
            self.text.tm = m.post_concat(self.text.tm);
        }
    }

    /// 字节流 → 字符（ToUnicode 映射；无映射按 UTF-16BE BOM / 单字节兜底）。
    fn decode_chars(&self, bytes: &[u8]) -> Vec<char> {
        let mut chars = Vec::new();
        if let Some(f) = self.text.font.as_ref().and_then(|n| self.fonts.get(n)) {
            if let Some(map) = &f.to_unicode {
                let mut i = 0;
                while i < bytes.len() {
                    let (code, adv) = if f.two_byte {
                        let c = u16::from_be_bytes([bytes[i], *bytes.get(i + 1).unwrap_or(&0)]);
                        (c, 2)
                    } else {
                        (bytes[i] as u16, 1)
                    };
                    if let Some(s) = map.get(&code) {
                        chars.extend(s.chars());
                    } else {
                        chars.push('\u{FFFD}');
                    }
                    i += adv;
                }
                return chars;
            }
        }
        if bytes.starts_with(&[0xFE, 0xFF]) {
            for c in bytes[2..].chunks(2) {
                let u = u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]);
                chars.push(char::from_u32(u as u32).unwrap_or('\u{FFFD}'));
            }
        } else {
            chars.extend(bytes.iter().map(|&b| b as char));
        }
        chars
    }

    fn load_font_data(&mut self, name: &str) -> Arc<[u8]> {
        let (has_embedded, embedded, fallback) = match self.fonts.get(name) {
            Some(f) => (!f.data.is_empty(), Arc::clone(&f.data), f.fallback_path.clone()),
            None => return Arc::from(&b""[..]),
        };
        if has_embedded {
            return embedded;
        }
        if let Some(b) = self.font_bytes.get(&fallback) {
            return Arc::clone(b);
        }
        let data: Arc<[u8]> = std::fs::read(&fallback)
            .map(|v| Arc::from(v.into_boxed_slice()))
            .unwrap_or_else(|_| Arc::from(&b""[..]));
        self.font_bytes.insert(fallback, Arc::clone(&data));
        data
    }

    /// alpha 遮罩按填充色混合进位图（关联函数：字形上屏循环持有 scale_cx
    /// 借用，只借 pixmap 字段避免全量 &mut self）。
    fn blend_mask(
        pixmap: &mut Pixmap,
        mask: &[u8],
        w: u32,
        h: u32,
        x0: f32,
        y0: f32,
        color: (f32, f32, f32, f32),
    ) {
    let (pw_i, ph_i) = (pixmap.width() as i32, pixmap.height() as i32);
    if pw_i <= 0 || ph_i <= 0 || w == 0 || h == 0 {
        return;
    }
    let (tx, ty) = (x0 as i32, y0 as i32);
    //先裁出与位图相交的矩形：行内切片混合，免逐像素边界判断与索引重算
    let cs = tx.max(0) as u32;
    let ce = (tx.saturating_add(w as i32)).min(pw_i) as u32;
    let rs = ty.max(0) as u32;
    let re = (ty.saturating_add(h as i32)).min(ph_i) as u32;
    if cs >= ce || rs >= re {
        return;
    }
    let pw = pixmap.width();
    let (cr, cg, cb) = (fc(color.0) as u32, fc(color.1) as u32, fc(color.2) as u32);
    let data = pixmap.data_mut();
    for row in rs..re {
        let mrow = ((row as i32 - ty) as usize) * w as usize;
        let ms = &mask[mrow + cs as usize..mrow + ce as usize];
        let dstart = (row * pw + cs) as usize * 4;
        let dend = (row * pw + ce) as usize * 4;
        for (m, d) in ms.iter().zip(data[dstart..dend].chunks_exact_mut(4)) {
            let alpha = *m;
            if alpha == 0 {
                continue;
            }
            let sa = alpha as u32;
            let inv = 255 - sa;
            d[0] = ((cr * sa + d[0] as u32 * inv) / 255).min(255) as u8;
            d[1] = ((cg * sa + d[1] as u32 * inv) / 255).min(255) as u8;
            d[2] = ((cb * sa + d[2] as u32 * inv) / 255).min(255) as u8;
            d[3] = ((sa + d[3] as u32 * inv) / 255).min(255) as u8;
        }
    }
}

    // ---- 图像 XObject ----

    fn do_xobject(&mut self, name: &str) {
        let Some(res) = self.resources.clone() else { return };
        let res = self.doc.resolve(&res);
        let Some(xo) = res.get("XObject").map(|x| self.doc.resolve(x)) else { return };
        // 对象号（多数 XObject 是间接引用）：文档级解码缓存的 key
        let num = match xo.get(name) {
            Some(Object::Ref(n, _)) => Some(*n),
            _ => None,
        };
        if let Some(n) = num {
            if let Some(c) = self.doc.image_cache.lock().unwrap().get(n) {
                self.blit_image(&c.rgba, c.w, c.h);
                return;
            }
        }
        let Some(obj) = xo.get(name).map(|o| self.doc.resolve(o)) else { return };
        let Some(dict) = obj.as_dict() else { return };
        if dict.get("Subtype").and_then(|s| s.as_name()) != Some("Image") {
            return;
        }
        let Ok(data) = self.doc.stream_data(&obj) else { return };
        let w = dict.get("Width").and_then(|v| v.as_int()).unwrap_or(0) as u32;
        let h = dict.get("Height").and_then(|v| v.as_int()).unwrap_or(0) as u32;
        if w == 0 || h == 0 {
            return;
        }
        let rgba = decode_image_rgba(dict, &data, w, h);
        self.blit_image(&rgba, w, h);
        if let Some(n) = num {
            self.doc.image_cache.lock().unwrap().put(
                n,
                Arc::new(CachedImage { w, h, rgba }),
            );
        }
    }

    /// 图像按 CTM 单位方映射（任意仿射，最近邻逆映射）。
    fn blit_image(&mut self, rgba: &[u8], iw: u32, ih: u32) {
        let t = self.gs.ctm;
        let corners = [
            map_xy(&t, 0.0, 0.0),
            map_xy(&t, 1.0, 0.0),
            map_xy(&t, 1.0, 1.0),
            map_xy(&t, 0.0, 1.0),
        ];
        let min_x = corners.iter().map(|c| c.0).fold(f32::INFINITY, f32::min).floor().max(0.0) as i32;
        let max_x = corners.iter().map(|c| c.0).fold(f32::NEG_INFINITY, f32::max).ceil() as i32;
        let min_y = corners.iter().map(|c| c.1).fold(f32::INFINITY, f32::min).floor().max(0.0) as i32;
        let max_y = corners.iter().map(|c| c.1).fold(f32::NEG_INFINITY, f32::max).ceil() as i32;
        let (pw, ph) = (self.pixmap.width() as i32, self.pixmap.height() as i32);

        let det = t.sx * t.sy - t.kx * t.ky;
        if det.abs() < 1e-12 {
            return;
        }
        let ia = t.sy / det;
        let ib = -t.kx / det;
        let ic = -t.ky / det;
        let id = t.sx / det;
        let ie = (t.kx * t.ty - t.sy * t.tx) / det;
        let if_ = (t.ky * t.tx - t.sx * t.ty) / det;

        let data = self.pixmap.data_mut();
        for py in min_y..max_y.min(ph) {
            for px in min_x..max_x.min(pw) {
                let fx = px as f32 + 0.5;
                let fy = py as f32 + 0.5;
                let u = ia * fx + ib * fy + ie;
                let v = ic * fx + id * fy + if_;
                if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                    continue;
                }
                let ix = ((u * iw as f32) as u32).min(iw - 1);
                let iy = (((1.0 - v) * ih as f32) as u32).min(ih - 1);
                let si = (iy * iw + ix) as usize * 4;
                let a = rgba[si + 3] as u32;
                if a == 0 {
                    continue;
                }
                let idx = (py * pw + px) as usize * 4;
                let inv = 255 - a;
                data[idx] = ((rgba[si] as u32 * a + data[idx] as u32 * inv) / 255).min(255) as u8;
                data[idx + 1] = ((rgba[si + 1] as u32 * a + data[idx + 1] as u32 * inv) / 255).min(255) as u8;
                data[idx + 2] = ((rgba[si + 2] as u32 * a + data[idx + 2] as u32 * inv) / 255).min(255) as u8;
                data[idx + 3] = ((a + data[idx + 3] as u32 * inv) / 255).min(255) as u8;
            }
        }
    }
}

fn fc(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

impl<'a> Rasterizer<'a> {
    /// sh 算子：渐变铺满当前裁剪区（无裁剪 = 整页设备区）。
    fn paint_shading(&mut self, name: &str) {
        let Some(res) = self.resources.clone() else { return };
        let res = self.doc.resolve(&res);
        let Some(sh_dict) = res
            .get("Shading")
            .map(|x| self.doc.resolve(x))
            .and_then(|d| d.get(name).map(|o| self.doc.resolve(o)))
        else {
            return;
        };
        let Some(d) = sh_dict.as_dict() else { return };
        let ty = d.get("ShadingType").and_then(|v| v.as_int()).unwrap_or(0);
        let coords: Vec<f32> = d
            .get("Coords")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .map(|o| match o {
                        Object::Int(i) => *i as f32,
                        Object::Real(f) => *f as f32,
                        _ => 0.0,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (c0, c1) = shading_colors(self.doc, d);
        let stops = vec![
            tiny_skia::GradientStop::new(0.0, tiny_skia::Color::from_rgba8(c0.0, c0.1, c0.2, 255)),
            tiny_skia::GradientStop::new(1.0, tiny_skia::Color::from_rgba8(c1.0, c1.1, c1.2, 255)),
        ];
        let t = self.gs.ctm;
        let shader = match ty {
            2 if coords.len() >= 4 => {
                let p1 = map_xy(&t, coords[0], coords[1]);
                let p2 = map_xy(&t, coords[2], coords[3]);
                tiny_skia::LinearGradient::new(
                    Point::from_xy(p1.0, p1.1),
                    Point::from_xy(p2.0, p2.1),
                    stops,
                    tiny_skia::SpreadMode::Pad,
                    Transform::identity(),
                )
            }
            3 if coords.len() >= 6 => {
                // 双点锥形近似：起点=内圆心、终点=外圆心、半径取大者
                let c = map_xy(&t, coords[0], coords[1]);
                let c2 = map_xy(&t, coords[3], coords[4]);
                let r = (coords[2].max(coords[5])) * t.sy.abs();
                tiny_skia::RadialGradient::new(
                    Point::from_xy(c.0, c.1),
                    Point::from_xy(c2.0, c2.1),
                    r.max(0.0),
                    stops,
                    tiny_skia::SpreadMode::Pad,
                    Transform::identity(),
                )
            }
            _ => None,
        };
        let Some(shader) = shader else { return };
        let mut paint = Paint::default();
        paint.shader = shader;
        paint.anti_alias = true;
        // 铺满设备区（PDF 语义 = 当前裁剪区；L2 无裁剪时 = 整页）
        let (pw, ph) = (self.pixmap.width() as f32, self.pixmap.height() as f32);
        let mut pb = PathBuilder::new();
        pb.move_to(0.0, 0.0);
        pb.line_to(pw, 0.0);
        pb.line_to(pw, ph);
        pb.line_to(0.0, ph);
        pb.close();
        if let Some(path) = pb.finish() {
            let clip = self.gs.clip.as_deref();
            self.pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), clip);
        }
    }
}

/// Shading /Function（Type 2 指数）→ (C0, C1) RGB8。
fn shading_colors(doc: &PdfDocument, d: &Dict) -> ((u8, u8, u8), (u8, u8, u8)) {
    let f = d.get("Function").map(|o| doc.resolve(o));
    let to_rgb = |arr: Option<&Object>, fallback: (u8, u8, u8)| -> (u8, u8, u8) {
        match arr.and_then(|a| a.as_array()) {
            Some(vals) if vals.len() >= 3 => (
                fc(num_of(&vals[0])),
                fc(num_of(&vals[1])),
                fc(num_of(&vals[2])),
            ),
            Some(vals) if vals.len() == 1 => {
                let v = fc(num_of(&vals[0]));
                (v, v, v)
            }
            _ => fallback,
        }
    };
    match f.as_ref().and_then(|o| o.as_dict()) {
        Some(fd) => (to_rgb(fd.get("C0"), (255, 255, 255)), to_rgb(fd.get("C1"), (0, 0, 0))),
        None => ((255, 255, 255), (0, 0, 0)),
    }
}

fn num_of(o: &Object) -> f32 {
    match o {
        Object::Int(i) => *i as f32,
        Object::Real(f) => *f as f32,
        _ => 0.0,
    }
}

fn cmyk(c: f32, m: f32, y: f32, k: f32) -> (f32, f32, f32, f32) {
    ((1.0 - c) * (1.0 - k), (1.0 - m) * (1.0 - k), (1.0 - y) * (1.0 - k), 1.0)
}

/// 图像字节 → RGBA8（DCT 走 image crate；原始位图按 BPC/ColorSpace 拼）。
fn decode_image_rgba(dict: &Dict, data: &[u8], w: u32, h: u32) -> Vec<u8> {
    let filter = dict
        .get("Filter")
        .and_then(|f| match f {
            Object::Name(n) => Some(n.clone()),
            Object::Arr(a) => a.first().and_then(|x| x.as_name().map(String::from)),
            _ => None,
        })
        .unwrap_or_default();
    if filter.contains("DCT") {
        if let Ok(img) = image::load_from_memory(data) {
            return img.to_rgba8().into_raw();
        }
    }
    let bpc = dict.get("BitsPerComponent").and_then(|v| v.as_int()).unwrap_or(8) as u32;
    let cs = dict
        .get("ColorSpace")
        .and_then(|c| c.as_name())
        .unwrap_or("DeviceRGB")
        .to_string();
    let ncomp = if cs.contains("Gray") { 1 } else if cs.contains("CMYK") { 4 } else { 3 };
    let mut out = vec![0u8; (w * h * 4) as usize];
    if bpc != 8 {
        for i in 0..(w * h) as usize {
            let byte = data.get(i / 8).copied().unwrap_or(0);
            let bit = (byte >> (7 - (i % 8))) & 1;
            if bit == 1 {
                out[i * 4 + 3] = 255;
            }
        }
        return out;
    }
    for i in 0..(w * h) as usize {
        let base = i * ncomp as usize;
        let (r, g, b) = match ncomp {
            1 => {
                let v = data.get(base).copied().unwrap_or(255);
                (v, v, v)
            }
            4 => {
                let c = data.get(base).copied().unwrap_or(0) as f32 / 255.0;
                let m = data.get(base + 1).copied().unwrap_or(0) as f32 / 255.0;
                let y = data.get(base + 2).copied().unwrap_or(0) as f32 / 255.0;
                let k = data.get(base + 3).copied().unwrap_or(0) as f32 / 255.0;
                (fc((1.0 - c) * (1.0 - k)), fc((1.0 - m) * (1.0 - k)), fc((1.0 - y) * (1.0 - k)))
            }
            _ => (
                data.get(base).copied().unwrap_or(0),
                data.get(base + 1).copied().unwrap_or(0),
                data.get(base + 2).copied().unwrap_or(255),
            ),
        };
        out[i * 4] = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = b;
        out[i * 4 + 3] = 255;
    }
    out
}

/// 渲染一页（L2 入口）：拼接内容流 → 光栅 → (RGBA, w, h)。
pub fn render_page(
    doc: &PdfDocument,
    content_refs: &[Object],
    resources: Option<Object>,
    page_w: f32,
    page_h: f32,
    target_width_px: u32,
) -> RrkResult<(Vec<u8>, u32, u32)> {
    let scale = target_width_px as f32 / page_w.max(1.0);
    let mut content = Vec::new();
    for r in content_refs {
        if let Ok(data) = doc.stream_data(r) {
            content.extend_from_slice(&data);
            content.push(b'\n');
        }
    }
    let mut raster = Rasterizer::new(doc, resources, page_w, page_h, scale)?;
    Ok(raster.run(&content))
}
