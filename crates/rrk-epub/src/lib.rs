//! rrk-epub：EPUB 适配器（方案 §3.3，完整实现）。
//!
//! - **zip 随机读**：只读中央目录 + 按需 inflate 单条目（自研 zip 读取，见 [`zip`]）；
//! - **结构**：container.xml → OPF（metadata/manifest/spine）；目录优先 EPUB3 nav，回退 NCX；
//! - **章内容**：XHTML → [`clean::clean_xhtml`] 清洗管线（标签白名单 + CSS 最小集）；
//! - **DRM**：`encryption.xml` 加密正文 → 明确 `UnsupportedFormat`，不静默。

use std::collections::HashMap;
use std::sync::Arc;

use rrk_format::{BookSource, ContentSource, FormatAdapter};
use rrk_markup::{attr, clean_xhtml, local_name, tokenize, Token};
use rrk_model::{
    fnv1a64, BookMeta, ChapterContent, ChapterEntry, ChapterId, ChapterKind, FormatId, Resource,
    ResourceKey, RrkError, RrkResult, TocNode,
};
use rrk_zip::{ZipArchive, ZipEntry};

const XHTML_TYPES: &[&str] = &["application/xhtml+xml", "text/html", "text/xml", "application/x-dtbook+xml"];

/// manifest 条目。
#[derive(Debug, Clone)]
struct ManifestItem {
    id: String,
    href: String,
    media_type: String,
    properties: String,
}

/// EPUB 书源。
pub struct EpubBook {
    meta: BookMeta,
    zip: Arc<ZipArchive>,
    opf_dir: String,
    manifest: HashMap<String, ManifestItem>,
    /// spine 阅读序（manifest id）。
    spine: Vec<String>,
    /// href（相对 OPF 目录、无锚点）→ 章 id。
    href_to_chapter: HashMap<String, ChapterId>,
    entries: Vec<ChapterEntry>,
    toc: Vec<TocNode>,
}

fn resolve_href(base_dir: &str, href: &str) -> String {
    let path = href.split('#').next().unwrap_or("");
    if base_dir.is_empty() {
        return normalize_path(path);
    }
    normalize_path(&format!("{base_dir}/{path}"))
}

fn normalize_path(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

impl EpubBook {
    pub fn parse(src: &ContentSource) -> RrkResult<Self> {
        let bytes = src.read_all()?;
        let zip = Arc::new(ZipArchive::parse(bytes)?);

        // DRM 探测：正文被加密 → 明确拒绝（字体伪装除外，P0 一律拒绝 EncryptedData）
        if let Some(enc) = zip.entry("META-INF/encryption.xml") {
            let xml = zip.read_string(enc)?;
            if xml.contains("<EncryptedData") {
                return Err(RrkError::unsupported("EPUB 含 DRM 加密内容，明确不支持"));
            }
        }

        // container.xml → OPF
        let container = zip
            .entry("META-INF/container.xml")
            .ok_or_else(|| RrkError::corrupt("EPUB: 缺少 META-INF/container.xml"))?;
        let container_xml = zip.read_string(container)?;
        let opf_path = parse_rootfile(&container_xml)
            .ok_or_else(|| RrkError::corrupt("EPUB: container.xml 无 rootfile"))?;
        let opf_entry = zip
            .entry(&opf_path)
            .ok_or_else(|| RrkError::corrupt(format!("EPUB: OPF 缺失 {opf_path}")))?;
        let opf_xml = zip.read_string(opf_entry)?;
        let opf_dir = match opf_path.rsplit_once('/') {
            Some((dir, _)) => dir.to_string(),
            None => String::new(),
        };

        let opf = parse_opf(&opf_xml);

        // 章索引：spine 中的可重排文档（建索引不搬内容，权重用条目尺寸估算）
        let mut entries = Vec::new();
        let mut href_to_chapter = HashMap::new();
        for idref in &opf.spine {
            let Some(item) = opf.manifest.get(idref) else { continue };
            if !XHTML_TYPES.contains(&item.media_type.as_str()) {
                continue;
            }
            let href = resolve_href(&opf_dir, &item.href);
            let uncomp = zip.entry(&href).map(|e| e.uncomp_size).unwrap_or(0);
            let id = entries.len() as ChapterId;
            href_to_chapter.insert(href.clone(), id);
            entries.push(ChapterEntry {
                id,
                title: None,
                href,
                byte_range: (0, uncomp),
                char_len: (uncomp / 2) as u32, // CJK 估算
                kind: ChapterKind::Text,
                part: 0,
            });
        }
        if entries.is_empty() {
            return Err(RrkError::corrupt("EPUB: spine 无可渲染文档"));
        }

        // 目录：nav 优先，NCX 回退；并用目录标题回填章标题
        let toc_raw = build_toc_raw(&zip, &opf, &opf_dir);
        let mut toc: Vec<TocNode> = Vec::new();
        for (title, href) in toc_raw {
            let path = normalize_path(href.split('#').next().unwrap_or(""));
            let Some(&chapter) = href_to_chapter.get(&path) else { continue };
            if let Some(entry) = entries.iter_mut().find(|e| e.id == chapter) {
                if entry.title.is_none() {
                    entry.title = Some(title.clone());
                }
            }
            toc.push(TocNode { title, chapter, anchor: None, children: Vec::new() });
        }

        let title = opf
            .title
            .clone()
            .or_else(|| {
                src.name().map(|n| n.rsplit_once('.').map(|(s, _)| s).unwrap_or(n).to_string())
            })
            .unwrap_or_else(|| "未命名".to_string());

        let meta = BookMeta {
            title,
            authors: opf.authors.clone(),
            language: opf.language.clone(),
            format: FormatId::Epub,
            content_hash: fnv1a64(&opf_xml.as_bytes()[..opf_xml.len().min(64 * 1024)]),
            cover: opf.cover_id.clone().map(ResourceKey),
        };

        Ok(Self {
            meta,
            zip,
            opf_dir,
            manifest: opf.manifest,
            spine: opf.spine,
            href_to_chapter,
            entries,
            toc,
        })
    }

    fn entry_for_chapter(&self, id: ChapterId) -> RrkResult<&ZipEntry> {
        let href = self
            .href_to_chapter
            .iter()
            .find(|(_, &cid)| cid == id)
            .map(|(h, _)| h.as_str())
            .ok_or_else(|| RrkError::invalid(format!("章 id 越界: {id}")))?;
        self.zip
            .entry(href)
            .ok_or_else(|| RrkError::corrupt(format!("EPUB: 章条目缺失 {href}")))
    }
}

// ---------------------------------------------------------------------------
// OPF / container / TOC 解析（线性扫 token，不建 DOM）
// ---------------------------------------------------------------------------

struct OpfData {
    title: Option<String>,
    authors: Vec<String>,
    language: Option<String>,
    manifest: HashMap<String, ManifestItem>,
    spine: Vec<String>,
    cover_id: Option<String>,
}

fn parse_rootfile(container_xml: &str) -> Option<String> {
    for t in tokenize(container_xml) {
        if let Token::Open { name, attrs, .. } = &t {
            if local_name(name) == "rootfile" {
                if let Some(path) = attr(attrs, "full-path") {
                    return Some(path.to_string());
                }
            }
        }
    }
    None
}

fn parse_opf(opf_xml: &str) -> OpfData {
    let mut data = OpfData {
        title: None,
        authors: Vec::new(),
        language: None,
        manifest: HashMap::new(),
        spine: Vec::new(),
        cover_id: None,
    };
    let mut ctx: Option<String> = None; // 当前元数据标签
    let mut spine_toc_id: Option<String> = None;

    for t in tokenize(opf_xml) {
        match t {
            Token::Open { name, attrs, self_closing } => {
                match local_name(&name) {
                    "title" | "creator" | "language" => {
                        ctx = Some(local_name(&name).to_string());
                    }
                    "meta" => {
                        // EPUB2: <meta name="cover" content="id"/>
                        if attr(&attrs, "name") == Some("cover") {
                            if let Some(c) = attr(&attrs, "content") {
                                data.cover_id = Some(c.to_string());
                            }
                        }
                        // EPUB3: <meta refines> 等 P0 忽略
                    }
                    "item" => {
                        if let (Some(id), Some(href)) = (attr(&attrs, "id"), attr(&attrs, "href")) {
                            data.manifest.insert(
                                id.to_string(),
                                ManifestItem {
                                    id: id.to_string(),
                                    href: href.to_string(),
                                    media_type: attr(&attrs, "media-type").unwrap_or("").to_string(),
                                    properties: attr(&attrs, "properties").unwrap_or("").to_string(),
                                },
                            );
                        }
                    }
                    "itemref" => {
                        if let Some(idref) = attr(&attrs, "idref") {
                            data.spine.push(idref.to_string());
                        }
                    }
                    "spine" => {
                        if let Some(toc) = attr(&attrs, "toc") {
                            spine_toc_id = Some(toc.to_string());
                        }
                    }
                    _ => {}
                }
                if self_closing {
                    ctx = None;
                }
            }
            Token::Text(text) => match ctx.as_deref() {
                Some("title") if data.title.is_none() => {
                    let t = text.trim();
                    if !t.is_empty() {
                        data.title = Some(t.to_string());
                    }
                }
                Some("creator") => {
                    let t = text.trim();
                    if !t.is_empty() {
                        data.authors.push(t.to_string());
                    }
                }
                Some("language") if data.language.is_none() => {
                    let t = text.trim();
                    if !t.is_empty() {
                        data.language = Some(t.to_string());
                    }
                }
                _ => {}
            },
            Token::Close(name) => {
                if matches!(local_name(&name), "title" | "creator" | "language") {
                    ctx = None;
                }
            }
        }
    }

    // 封面回退：manifest properties=cover-image
    if data.cover_id.is_none() {
        if let Some(item) = data.manifest.values().find(|i| i.properties.contains("cover-image")) {
            data.cover_id = Some(item.id.clone());
        }
    }
    // NCX 挂进 manifest（spine toc 引用的 id）
    if let Some(id) = spine_toc_id {
        data.manifest.entry(id).or_insert(ManifestItem {
            id: String::new(),
            href: String::new(),
            media_type: String::new(),
            properties: String::new(),
        });
    }
    data
}

/// 目录原始项（title, **zip 根相对路径**）；nav 优先，NCX 回退。
fn build_toc_raw(zip: &Arc<ZipArchive>, opf: &OpfData, opf_dir: &str) -> Vec<(String, String)> {
    let dir_of = |href: &str| match href.rsplit_once('/') {
        Some((d, _)) => d.to_string(),
        None => String::new(),
    };
    // EPUB3 nav：manifest 中 properties 含 nav
    if let Some(item) = opf.manifest.values().find(|i| i.properties.contains("nav")) {
        let href = resolve_href(opf_dir, &item.href);
        if let Some(entry) = zip.entry(&href) {
            if let Ok(xml) = zip.read_string(entry) {
                let items = parse_nav_toc(&xml);
                if !items.is_empty() {
                    let base = dir_of(&href);
                    return items
                        .into_iter()
                        .map(|(t, h)| (t, resolve_href(&base, &h)))
                        .collect();
                }
            }
        }
    }
    // NCX：media-type
    if let Some(item) = opf.manifest.values().find(|i| i.media_type == "application/x-dtbncx+xml") {
        let href = resolve_href(opf_dir, &item.href);
        if let Some(entry) = zip.entry(&href) {
            if let Ok(xml) = zip.read_string(entry) {
                let items = parse_ncx_toc(&xml);
                if !items.is_empty() {
                    let base = dir_of(&href);
                    return items
                        .into_iter()
                        .map(|(t, h)| (t, resolve_href(&base, &h)))
                        .collect();
                }
            }
        }
    }
    // 兜底：任意 .ncx
    if let Some(entry) = zip.entry_by_suffix(".ncx") {
        if let Ok(xml) = zip.read_string(entry) {
            let items = parse_ncx_toc(&xml);
            if !items.is_empty() {
                let base = dir_of(&entry.name);
                return items
                    .into_iter()
                    .map(|(t, h)| (t, resolve_href(&base, &h)))
                    .collect();
            }
        }
    }
    Vec::new()
}

/// nav.xhtml：收集 `<a href="...">标题</a>`（文档序即阅读序）。
fn parse_nav_toc(xml: &str) -> Vec<(String, String)> {
    let mut items = Vec::new();
    let mut in_nav = 0u32;
    let mut current: Option<(String, String)> = None; // (href, accumulating title)
    for t in tokenize(xml) {
        match t {
            Token::Open { name, attrs, self_closing } => match local_name(&name) {
                "nav" => in_nav += 1,
                "a" if in_nav > 0 => {
                    if let Some(href) = attr(&attrs, "href") {
                        current = Some((href.to_string(), String::new()));
                    }
                    if self_closing {
                        if let Some((h, title)) = current.take() {
                            items.push((title, h));
                        }
                    }
                }
                _ => {}
            },
            Token::Text(text) => {
                if let Some((_, title)) = &mut current {
                    title.push_str(&text);
                }
            }
            Token::Close(name) => match local_name(&name) {
                "nav" => in_nav = in_nav.saturating_sub(1),
                "a" => {
                    if let Some((h, title)) = current.take() {
                        let title = title.trim().to_string();
                        if !title.is_empty() {
                            items.push((title, h));
                        }
                    }
                }
                _ => {}
            },
        }
    }
    items
}

/// NCX：`<navPoint><navLabel><text>..</text></navLabel><content src=".."/>`。
fn parse_ncx_toc(xml: &str) -> Vec<(String, String)> {
    let mut items = Vec::new();
    let mut in_text = false;
    let mut title = String::new();
    for t in tokenize(xml) {
        match t {
            Token::Open { name, attrs, self_closing } => match local_name(&name) {
                "text" => {
                    in_text = true;
                    title.clear(); // 嵌套 navPoint 各有各的标题，不串接
                }
                "content" => {
                    if let Some(src) = attr(&attrs, "src") {
                        let title_taken = std::mem::take(&mut title);
                        items.push((title_taken, src.to_string()));
                    }
                    if self_closing {
                        in_text = false;
                    }
                }
                _ => {}
            },
            Token::Text(text) => {
                if in_text {
                    title.push_str(&text);
                }
            }
            Token::Close(name) => {
                if local_name(&name) == "text" {
                    in_text = false;
                }
            }
        }
    }
    items.retain(|(t, _)| !t.trim().is_empty());
    items
}

// ---------------------------------------------------------------------------
// FormatAdapter / BookSource
// ---------------------------------------------------------------------------

/// EPUB 适配器。
pub struct EpubAdapter;

impl FormatAdapter for EpubAdapter {
    fn id(&self) -> FormatId {
        FormatId::Epub
    }

    fn sniff(&self, head: &[u8], name: Option<&str>) -> u32 {
        if !head.starts_with(b"PK\x03\x04") {
            return 0;
        }
        let window = &head[..head.len().min(200)];
        if window.windows(28).any(|w| w == b"mimetypeapplication/epub+zip") {
            return 95;
        }
        if name.map(|n| n.to_ascii_lowercase().ends_with(".epub")).unwrap_or(false) {
            return 80;
        }
        0 // zip 容器但无 epub 特征（如 CBZ）——不认领，留给正确格式或明确报错
    }

    fn open(&self, src: &ContentSource) -> RrkResult<Box<dyn BookSource>> {
        Ok(Box::new(EpubBook::parse(src)?))
    }
}

impl BookSource for EpubBook {
    fn meta(&self) -> &BookMeta {
        &self.meta
    }

    fn toc(&self) -> RrkResult<Vec<TocNode>> {
        Ok(self.toc.clone())
    }

    fn chapters(&self) -> RrkResult<Vec<ChapterEntry>> {
        Ok(self.entries.clone())
    }

    fn load_chapter(&self, id: ChapterId) -> RrkResult<ChapterContent> {
        let entry = self.entry_for_chapter(id)?;
        let html = self.zip.read_string(entry)?;
        let cleaned = clean_xhtml(&html);
        Ok(ChapterContent { id, text: Arc::from(cleaned.text), blocks: cleaned.blocks })
    }

    fn load_resource(&self, key: &ResourceKey) -> RrkResult<Resource> {
        let href = resolve_href(&self.opf_dir, &key.0);
        let entry = self
            .zip
            .entry(&href)
            .ok_or_else(|| RrkError::invalid(format!("资源不存在: {}", key.0)))?;
        let mime = self
            .manifest
            .values()
            .find(|i| resolve_href(&self.opf_dir, &i.href) == href)
            .map(|i| i.media_type.clone())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        Ok(Resource {
            key: ResourceKey(href),
            mime,
            data: self.zip.read(entry)?,
        })
    }

    fn cover(&self) -> RrkResult<Option<Resource>> {
        let Some(cover_key) = &self.meta.cover else {
            // 三级回退之「spine 首图」略：P0 只走显式封面
            return Ok(None);
        };
        Ok(Some(self.load_resource(cover_key)?))
    }

    fn estimate_weights(&self) -> RrkResult<Vec<(ChapterId, u32)>> {
        Ok(self.entries.iter().map(|e| (e.id, e.char_len)).collect())
    }
}
