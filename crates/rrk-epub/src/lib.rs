//! rrk-epub：EPUB 适配器（方案 §3.3，完整实现）。
//!
//! - **zip 随机读**：只读中央目录 + 按需 inflate 单条目（自研 zip 读取，见 [`zip`]）；
//! - **结构**：container.xml → OPF（metadata/manifest/spine）；目录优先 EPUB3 nav，回退 NCX；
//! - **章内容**：XHTML → [`clean::clean_xhtml`] 清洗管线（标签白名单 + CSS 最小集）；
//! - **DRM**：`encryption.xml` 加密正文 → 明确 `UnsupportedFormat`，不静默。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rrk_format::{BookSource, ContentSource, FormatAdapter};
use rrk_markup::{attr, clean_xhtml, local_name, tokenize, Token};
use rrk_model::{
    fnv1a64, Block, BookMeta, ChapterContent, ChapterEntry, ChapterId, ChapterKind, FormatId,
    Inline, Resource, ResourceKey, RrkError, RrkResult, TocNode,
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
    /// 目录原始树（含 href+fragment；`toc()` 时惰性解析锚点）。
    toc_raw: Vec<TocRaw>,
    /// 章清洗结果缓存：inflate + clean 只做一次，`load_chapter` 与 TOC 锚点共用（见 `cached_clean`）。
    chapter_cache: std::sync::Mutex<ChapterCache>,
    /// manifest href（包内绝对路径）→ media_type（`load_resource` 免循环现算 resolve_href）。
    mime_by_href: HashMap<String, String>,
}

/// 章清洗缓存（有界 LRU：只保最近用过的章，防大书内存无界增长）。
struct ChapterCache {
    map: HashMap<ChapterId, Arc<rrk_markup::CleanResult>>,
    order: std::collections::VecDeque<ChapterId>,
}

/// 章清洗缓存保留章数（翻回上一章 / TOC 锚点复用足够；章文本大，不贪多）。
const CHAPTER_CACHE_SLOTS: usize = 4;

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

/// 取 href 的目录部分（无目录返回空串）。
fn dir_of(href: &str) -> String {
    match href.rsplit_once('/') {
        Some((d, _)) => d.to_string(),
        None => String::new(),
    }
}

/// 内链 href 归一化：非 `#frag` 的路径按文档目录解析为包内绝对路径，fragment 原样保留。
fn resolve_link_href(base_dir: &str, href: &str) -> String {
    if href.starts_with('#') {
        return href.to_string();
    }
    match href.split_once('#') {
        Some((path, frag)) => format!("{}#{frag}", resolve_href(base_dir, path)),
        None => resolve_href(base_dir, href),
    }
}

/// 书内资源 key 归一化为**包内绝对路径**（`load_resource` 的 key 契约）。
///
/// XHTML 里的 `src` 是相对**文档**目录的；`/…` 开头按包根解释。
/// 带 scheme（`http(s):` / `data:` …）的书外资源原样保留。
fn resolve_key(base_dir: &str, raw: &str) -> String {
    if raw.contains("://") || raw.starts_with("data:") {
        return raw.to_string();
    }
    if let Some(abs) = raw.strip_prefix('/') {
        return normalize_path(abs);
    }
    resolve_href(base_dir, raw)
}

/// 递归归一化块树里的图片 key（块级图片、行内图片、引用块内层）。
fn resolve_image_keys(blocks: &mut [Block], base_dir: &str) {
    for b in blocks {
        match b {
            Block::Image { key, .. } => key.0 = resolve_key(base_dir, &key.0),
            Block::Quote { blocks } => resolve_image_keys(blocks, base_dir),
            Block::Heading { inlines, .. } | Block::Paragraph { inlines, .. } => {
                for i in inlines {
                    if let Inline::Image { key } = i {
                        key.0 = resolve_key(base_dir, &key.0);
                    }
                }
            }
            Block::Divider => {}
        }
    }
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

        // 目录：nav 优先，NCX 回退；用目录标题回填章标题（锚点惰性解析，见 toc()）
        let toc_raw = build_toc_raw(&zip, &opf, &opf_dir);
        backfill_titles(&toc_raw, &href_to_chapter, &mut entries);

        // issue #13：无 nav/NCX 的书，用正文首个最小级标题回填章名——
        // 限流读前缀（64KB）+ 同一条清洗管线（清洗器对截断输入容错），
        // 不全量 inflate；前缀内无标题则保持 None（宿主回落占位）。
        if toc_raw.is_empty() {
            for e in entries.iter_mut().filter(|e| e.title.is_none()) {
                if let Some(t) = probe_heading_title(&zip, &e.href) {
                    e.title = Some(t);
                }
            }
        }

        let title = opf
            .title
            .clone()
            .or_else(|| {
                src.name().map(|n| n.rsplit_once('.').map(|(s, _)| s).unwrap_or(n).to_string())
            })
            .unwrap_or_else(|| "未命名".to_string());

        // manifest 反查表：href（包内绝对路径）→ media_type（load_resource O(1)）
        let mut mime_by_href = HashMap::with_capacity(opf.manifest.len());
        for i in opf.manifest.values() {
            mime_by_href.insert(resolve_href(&opf_dir, &i.href), i.media_type.clone());
        }

        let meta = BookMeta {
            title,
            authors: opf.authors.clone(),
            language: opf.language.clone(),
            description: opf.description.clone(),
            format: FormatId::Epub,
            content_hash: fnv1a64(&opf_xml.as_bytes()[..opf_xml.len().min(64 * 1024)]),
            // cover 字段存的是 manifest **href**（包内绝对路径），不是 manifest id：
            // `cover()` 直接走 `load_resource`，id 是查表键不是资源键。
            cover: opf
                .cover_id
                .as_ref()
                .and_then(|id| opf.manifest.get(id))
                .map(|item| ResourceKey(resolve_href(&opf_dir, &item.href))),
        };

        Ok(Self {
            meta,
            zip,
            opf_dir,
            manifest: opf.manifest,
            spine: opf.spine,
            href_to_chapter,
            entries,
            toc_raw,
            chapter_cache: std::sync::Mutex::new(ChapterCache {
                map: HashMap::new(),
                order: std::collections::VecDeque::new(),
            }),
            mime_by_href,
        })
    }

    /// 目录原始树 → `TocNode` 树（按需解析锚点；只清洗被 fragment 引用的文档）。
    fn build_toc_nodes(
        &self,
        raws: &[TocRaw],
        cache: &mut HashMap<ChapterId, BTreeMap<String, u32>>,
    ) -> Vec<TocNode> {
        let mut out = Vec::new();
        for r in raws {
            let path = normalize_path(r.href.split('#').next().unwrap_or(""));
            let Some(&chapter) = self.href_to_chapter.get(&path) else { continue };
            let anchor = match r.href.split_once('#') {
                Some((_, frag)) if !frag.is_empty() => {
                    if !cache.contains_key(&chapter) {
                        let m = self.chapter_anchors(chapter).unwrap_or_default();
                        cache.insert(chapter, m);
                    }
                    cache.get(&chapter).and_then(|m| m.get(frag)).copied()
                }
                _ => None,
            };
            out.push(TocNode {
                title: r.title.clone(),
                chapter,
                anchor,
                children: self.build_toc_nodes(&r.children, cache),
            });
        }
        out
    }

    /// 章内锚点表：清洗该章后 `id → 文本字节偏移`（命中清洗缓存，不重解压重清洗）。
    fn chapter_anchors(&self, chapter: ChapterId) -> RrkResult<BTreeMap<String, u32>> {
        Ok(self.cached_clean(chapter)?.anchors.clone())
    }

    /// 取章清洗结果（缓存优先）：inflate + clean + 出口归一化只做一次，
    /// `load_chapter`（正文/块/内链）与 `chapter_anchors`（TOC 锚点）共用同一份。
    fn cached_clean(&self, id: ChapterId) -> RrkResult<Arc<rrk_markup::CleanResult>> {
        {
            let cache = self.chapter_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = cache.map.get(&id) {
                return Ok(Arc::clone(c));
            }
        }
        let entry = self.entry_for_chapter(id)?;
        let html = self.zip.read_string(entry)?;
        let mut cleaned = clean_xhtml(&html);
        // 图片 key 出口契约 = 包内绝对路径：清洗层保留的是文档里的原始 `src`
        // （相对**文档**目录），在出口统一归一化，宿主可直接拿去 `load_resource`。
        let base = dir_of(&entry.name);
        resolve_image_keys(&mut cleaned.blocks, &base);
        // 内链出口契约：路径部分按文档目录归一化为包内绝对路径，`#frag` 原样保留。
        for l in &mut cleaned.links {
            l.href = resolve_link_href(&base, &l.href);
        }
        let cleaned = Arc::new(cleaned);
        let mut cache = self.chapter_cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.map.len() >= CHAPTER_CACHE_SLOTS {
            if let Some(old) = cache.order.pop_front() {
                cache.map.remove(&old);
            }
        }
        cache.order.push_back(id);
        cache.map.insert(id, Arc::clone(&cleaned));
        Ok(cleaned)
    }

    /// 回退 2：spine 首个可渲染文档里的首个 `<img src>` → 包内绝对路径。
    fn first_spine_image(&self) -> Option<ResourceKey> {
        let first_id = self.spine.first()?;
        let item = self.manifest.get(first_id)?;
        let doc_path = resolve_href(&self.opf_dir, &item.href);
        let entry = self.zip.entry(&doc_path)?;
        let html = self.zip.read_string(entry).ok()?;
        let base = dir_of(&doc_path);
        for t in tokenize(&html) {
            if let Token::Open { name, attrs, .. } = &t {
                if local_name(name) == "img" {
                    if let Some(src) = attr(attrs, "src") {
                        let src = src.trim();
                        if !src.is_empty() {
                            return Some(ResourceKey(resolve_href(&base, src)));
                        }
                    }
                }
            }
        }
        None
    }

    /// 章 id → zip 条目（id 即 `entries` 下标，O(1) 直取；不再反扫 `href_to_chapter`）。
    fn entry_for_chapter(&self, id: ChapterId) -> RrkResult<&ZipEntry> {
        let entry = self
            .entries
            .get(id as usize)
            .filter(|e| e.id == id)
            .ok_or_else(|| RrkError::invalid(format!("章 id 越界: {id}")))?;
        self.zip
            .entry(&entry.href)
            .ok_or_else(|| RrkError::corrupt(format!("EPUB: 章条目缺失 {}", entry.href)))
    }
}

// ---------------------------------------------------------------------------
// OPF / container / TOC 解析（线性扫 token，不建 DOM）
// ---------------------------------------------------------------------------

struct OpfData {
    title: Option<String>,
    authors: Vec<String>,
    language: Option<String>,
    description: Option<String>,
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
        description: None,
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
                    "title" | "creator" | "language" | "description" => {
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
                Some("description") if data.description.is_none() => {
                    // 折叠空白（OPF 里常见多行/缩进），截断上限 4000 字符
                    let mut t = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if t.chars().count() > 4000 {
                        t = t.chars().take(4000).collect();
                    }
                    if !t.is_empty() {
                        data.description = Some(t);
                    }
                }
                _ => {}
            },
            Token::Close(name) => {
                if matches!(local_name(&name), "title" | "creator" | "language" | "description") {
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
/// 目录原始树（解析期；href 已归一化为包内绝对路径、保留 fragment）。
#[derive(Debug, Clone)]
struct TocRaw {
    title: String,
    href: String,
    children: Vec<TocRaw>,
}

/// 递归归一化目录树 href（路径按 base 解析，fragment 保留）。
/// 探测章首标题（issue #13）：限流读前缀 → 清洗 → 取 **level 最小**的标题块
/// （同级取文档序首个）。前缀内探测不到（标题在 64KB 之后/无标题）回 None，
/// 不是错误——宿主回落「第 N 章」占位。
const HEADING_PROBE_BYTES: usize = 64 * 1024;

fn probe_heading_title(zip: &ZipArchive, href: &str) -> Option<String> {
    let entry = zip.entry(href)?;
    let head = zip.read_prefix(entry, HEADING_PROBE_BYTES).ok()?;
    let s = String::from_utf8_lossy(&head);
    let cleaned = clean_xhtml(&s);
    let mut best: Option<(u8, String)> = None;
    for b in &cleaned.blocks {
        let Block::Heading { level, inlines } = b else { continue };
        let mut text = String::new();
        for i in inlines {
            if let Inline::Text { range, .. } = i {
                text.push_str(range.slice(&cleaned.text));
            }
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let better = match &best {
            None => true,
            Some((l, _)) => *level < *l,
        };
        if better {
            best = Some((*level, text));
        }
    }
    best.map(|(_, t)| t)
}

fn resolve_toc_tree(items: Vec<TocRaw>, base: &str) -> Vec<TocRaw> {
    items
        .into_iter()
        .map(|mut r| {
            r.href = resolve_link_href(base, &r.href);
            r.children = resolve_toc_tree(r.children, base);
            r
        })
        .collect()
}

/// 用目录标题回填章标题（仅填空缺，不覆盖已解析出的标题）。
fn backfill_titles(
    raws: &[TocRaw],
    href_to_chapter: &HashMap<String, ChapterId>,
    entries: &mut [ChapterEntry],
) {
    for r in raws {
        let path = normalize_path(r.href.split('#').next().unwrap_or(""));
        if let Some(&ch) = href_to_chapter.get(&path) {
            // 章 id 即 entries 下标（parse 时顺序分配），直取免 O(章数) 线性扫
            if let Some(e) = entries.get_mut(ch as usize).filter(|e| e.id == ch) {
                if e.title.is_none() {
                    e.title = Some(r.title.clone());
                }
            }
        }
        backfill_titles(&r.children, href_to_chapter, entries);
    }
}

fn build_toc_raw(zip: &Arc<ZipArchive>, opf: &OpfData, opf_dir: &str) -> Vec<TocRaw> {
    // EPUB3 nav：manifest 中 properties 含 nav
    if let Some(item) = opf.manifest.values().find(|i| i.properties.contains("nav")) {
        let href = resolve_href(opf_dir, &item.href);
        if let Some(entry) = zip.entry(&href) {
            if let Ok(xml) = zip.read_string(entry) {
                let items = parse_nav_toc(&xml);
                if !items.is_empty() {
                    let base = dir_of(&href);
                    return resolve_toc_tree(items, &base);
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
                    return resolve_toc_tree(items, &base);
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
                return resolve_toc_tree(items, &base);
            }
        }
    }
    Vec::new()
}

/// nav.xhtml：按 `<ol>/<li>` 嵌套构建目录树（文档序即阅读序）。
fn parse_nav_toc(xml: &str) -> Vec<TocRaw> {
    let mut root: Vec<TocRaw> = Vec::new();
    let mut levels: Vec<Vec<TocRaw>> = Vec::new(); // 嵌套 ol 层级
    let mut in_nav = 0u32;
    let mut pending: Option<String> = None; // 当前 <a> 的 href
    let mut title = String::new();
    for t in tokenize(xml) {
        match t {
            Token::Open { name, attrs, .. } => match local_name(&name) {
                "nav" => in_nav += 1,
                "ol" if in_nav > 0 => levels.push(Vec::new()),
                "a" if in_nav > 0 => {
                    if let Some(href) = attr(&attrs, "href") {
                        pending = Some(href.to_string());
                        title.clear();
                    }
                }
                _ => {}
            },
            Token::Text(text) => {
                if pending.is_some() {
                    title.push_str(&text);
                }
            }
            Token::Close(name) => match local_name(&name) {
                "nav" => in_nav = in_nav.saturating_sub(1),
                "a" => {
                    if let Some(href) = pending.take() {
                        let title = title.trim().to_string();
                        if !title.is_empty() {
                            let item = TocRaw { title, href, children: Vec::new() };
                            match levels.last_mut() {
                                Some(level) => level.push(item),
                                None => root.push(item),
                            }
                        }
                    }
                }
                "ol" => {
                    if let Some(mut kids) = levels.pop() {
                        match levels.last_mut().and_then(|p| p.last_mut()) {
                            Some(parent) => parent.children.append(&mut kids),
                            None => root.append(&mut kids),
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    root
}

/// NCX：`<navPoint>` 递归构建目录树。
fn parse_ncx_toc(xml: &str) -> Vec<TocRaw> {
    let mut root: Vec<TocRaw> = Vec::new();
    let mut stack: Vec<TocRaw> = Vec::new();
    let mut in_text = 0u32;
    let mut text_buf = String::new();
    for t in tokenize(xml) {
        match t {
            Token::Open { name, attrs, .. } => match local_name(&name) {
                "navpoint" => stack.push(TocRaw {
                    title: String::new(),
                    href: String::new(),
                    children: Vec::new(),
                }),
                "text" => {
                    in_text += 1;
                    text_buf.clear();
                }
                "content" => {
                    if let (Some(src), Some(top)) = (attr(&attrs, "src"), stack.last_mut()) {
                        top.href = src.to_string();
                    }
                }
                _ => {}
            },
            Token::Text(text) => {
                if in_text > 0 {
                    text_buf.push_str(&text);
                }
            }
            Token::Close(name) => match local_name(&name) {
                "text" => {
                    in_text = in_text.saturating_sub(1);
                    if let Some(top) = stack.last_mut() {
                        if top.title.is_empty() {
                            top.title = text_buf.trim().to_string();
                        }
                    }
                }
                "navpoint" => {
                    if let Some(np) = stack.pop() {
                        let attach = if !np.title.is_empty() && !np.href.is_empty() {
                            Some(np)
                        } else {
                            // 无标题/无 src 的容器：子项上提
                            if let Some(parent) = stack.last_mut() {
                                parent.children.extend(np.children);
                            }
                            None
                        };
                        if let Some(np) = attach {
                            match stack.last_mut() {
                                Some(parent) => parent.children.push(np),
                                None => root.push(np),
                            }
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    root
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
        // 锚点惰性解析：只清洗被 fragment 引用的文档；结果驻 `chapter_cache`，
        // 跨 toc() 调用与 load_chapter 复用（旧实现每次 toc() 重解压重清洗全书带锚点章）。
        let mut cache: HashMap<ChapterId, BTreeMap<String, u32>> = HashMap::new();
        Ok(self.build_toc_nodes(&self.toc_raw, &mut cache))
    }

    fn chapters(&self) -> RrkResult<Vec<ChapterEntry>> {
        Ok(self.entries.clone())
    }

    fn load_chapter(&self, id: ChapterId) -> RrkResult<ChapterContent> {
        // 缓存命中时免 inflate + 重清洗，只需结构克隆（比重新解析便宜一个量级）
        let cleaned = self.cached_clean(id)?;
        let link_ranges = cleaned.links.clone();
        Ok(ChapterContent {
            id,
            text: Arc::from(cleaned.text.as_str()),
            blocks: cleaned.blocks.clone(),
            link_ranges,
        })
    }

    fn unload_chapter(&self, id: ChapterId) {
        let mut cache = self.chapter_cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.map.remove(&id).is_some() {
            cache.order.retain(|&c| c != id);
        }
    }

    fn load_resource(&self, key: &ResourceKey) -> RrkResult<Resource> {
        // key 契约 = 包内绝对路径（load_chapter 归一化的图片 key / 封面 href）；
        // 兼容 OPF 相对键（旧口径）：绝对路径 miss 时再按 OPF 目录解析一次。
        let direct = normalize_path(&key.0);
        let href = if self.zip.entry(&direct).is_some() {
            direct
        } else {
            resolve_href(&self.opf_dir, &key.0)
        };
        let entry = self
            .zip
            .entry(&href)
            .ok_or_else(|| RrkError::invalid(format!("资源不存在: {}", key.0)))?;
        let mime = self
            .mime_by_href
            .get(&href)
            .cloned()
            .unwrap_or_else(|| "application/octet-stream".to_string());
        Ok(Resource {
            key: ResourceKey(href),
            mime,
            data: self.zip.read(entry)?,
        })
    }

    fn cover(&self) -> RrkResult<Option<Resource>> {
        // 1) 显式封面（EPUB3 properties=cover-image / EPUB2 meta name=cover）
        if let Some(cover_key) = &self.meta.cover {
            return Ok(Some(self.load_resource(cover_key)?));
        }
        // 2) spine 首个文档里的首个图片资源
        if let Some(key) = self.first_spine_image() {
            if let Ok(r) = self.load_resource(&key) {
                return Ok(Some(r));
            }
        }
        // 3) manifest 里首个 image/* 资源
        if let Some(item) = self
            .manifest
            .values()
            .find(|i| i.media_type.to_ascii_lowercase().starts_with("image/"))
        {
            let key = ResourceKey(resolve_href(&self.opf_dir, &item.href));
            if let Ok(r) = self.load_resource(&key) {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    fn estimate_weights(&self) -> RrkResult<Vec<(ChapterId, u32)>> {
        Ok(self.entries.iter().map(|e| (e.id, e.char_len)).collect())
    }
}

#[cfg(test)]
mod tests {
    //! 内存构造最小 EPUB（STORED 条目，零依赖）做资源 key 归一化契约测试。

    use super::*;

    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
        }
        !c
    }

    /// 最小 zip 写入器（STORED；布局照 rrk-zip 回环测试的构造器）。
    fn build_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, content) in files {
            let offset = out.len() as u32;
            let crc = crc32(content);
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local header
            out.extend_from_slice(&20u16.to_le_bytes()); // version
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method = stored
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0u16.to_le_bytes()); // date
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(content.len() as u32).to_le_bytes());
            out.extend_from_slice(&(content.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(content);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central header
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // method = stored
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(content.len() as u32).to_le_bytes());
            central.extend_from_slice(&(content.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attr
            central.extend_from_slice(&0u32.to_le_bytes()); // external attr
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        let cd_size = out.len() as u32 - cd_offset;
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // EOCD
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

    const OPF: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>测试书</dc:title>
    <dc:creator>测试作者</dc:creator>
    <meta name="cover" content="cover-img"/>
  </metadata>
  <manifest>
    <item id="cover-img" href="images/cover.jpg" media-type="image/jpeg"/>
    <item id="ch1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;

    const CH1: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>
<p>你好<img src="../images/pic.png" alt="图"/></p>
</body></html>"#;

    fn test_book() -> EpubBook {
        let zip = build_zip(&[
            ("META-INF/container.xml", CONTAINER.as_bytes()),
            ("OEBPS/content.opf", OPF.as_bytes()),
            ("OEBPS/text/ch1.xhtml", CH1.as_bytes()),
            ("OEBPS/images/pic.png", b"\x89PNG-fake"),
            ("OEBPS/images/cover.jpg", b"\xff\xd8\xff-fake"),
        ]);
        let src = ContentSource::from_bytes(
            Some("t.epub".to_string()),
            Arc::<[u8]>::from(zip.into_boxed_slice()),
        );
        EpubBook::parse(&src).unwrap()
    }

    #[test]
    fn cover_key_is_manifest_href_not_id() {
        let book = test_book();
        let cover = book.cover().unwrap().expect("封面应可取");
        assert_eq!(cover.key.0, "OEBPS/images/cover.jpg");
    }

    #[test]
    fn image_key_normalized_to_package_path() {
        let book = test_book();
        // 文档在 text/ 子目录，src="../images/pic.png" → 出口必须是包内绝对路径。
        let content = book.load_chapter(0).unwrap();
        let img = content
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Image { key, .. } => Some(key.0.clone()),
                _ => None,
            })
            .expect("应有图片块");
        assert_eq!(img, "OEBPS/images/pic.png");
    }

    #[test]
    fn load_resource_accepts_absolute_and_opf_relative_keys() {
        let book = test_book();
        // 归一化后的绝对路径直查命中
        let res = book
            .load_resource(&ResourceKey("OEBPS/images/pic.png".into()))
            .unwrap();
        assert_eq!(res.data.as_ref(), b"\x89PNG-fake");
        // OPF 相对键（旧口径）兜底也命中
        let res = book
            .load_resource(&ResourceKey("images/cover.jpg".into()))
            .unwrap();
        assert_eq!(res.data.as_ref(), b"\xff\xd8\xff-fake");
    }

    #[test]
    fn opf_description_parsed_and_whitespace_collapsed() {
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <metadata>
    <dc:title>书名</dc:title>
    <dc:description>
        第一段  简介
        第二段  简介
    </dc:description>
  </metadata>
</package>"#;
        let d = parse_opf(opf);
        assert_eq!(d.description.as_deref(), Some("第一段 简介 第二段 简介"));
    }

    #[test]
    fn opf_without_description_is_none() {
        let opf = r#"<package><metadata><dc:title>无简介</dc:title></metadata></package>"#;
        assert!(parse_opf(opf).description.is_none());
    }

    /// #1：内链 href 归一化——路径按文档目录解析为包内绝对路径，fragment 原样保留。
    #[test]
    fn link_href_normalized_fragment_preserved() {
        assert_eq!(resolve_link_href("OEBPS/text", "ch2.xhtml#s1"), "OEBPS/text/ch2.xhtml#s1");
        assert_eq!(resolve_link_href("OEBPS/text", "../images/a.png"), "OEBPS/images/a.png");
        assert_eq!(resolve_link_href("OEBPS/text", "#note1"), "#note1");
    }

    // ---- #5 封面三级回退 ----

    fn book_with(opf: &str, ch1: &str, extra: &[(&str, &[u8])]) -> EpubBook {
        let mut files: Vec<(&str, &[u8])> = vec![
            ("META-INF/container.xml", CONTAINER.as_bytes()),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/text/ch1.xhtml", ch1.as_bytes()),
        ];
        files.extend_from_slice(extra);
        let zip = build_zip(&files);
        let src = ContentSource::from_bytes(
            Some("t.epub".to_string()),
            Arc::<[u8]>::from(zip.into_boxed_slice()),
        );
        EpubBook::parse(&src).unwrap()
    }

    const OPF_NO_COVER_META: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>无显式封面</dc:title></metadata>
  <manifest>
    <item id="ch1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;

    const OPF_MANIFEST_IMAGE: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>manifest 首图</dc:title></metadata>
  <manifest>
    <item id="ch1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="pic" href="images/pic.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;

    const CH1_NO_IMG: &str = r#"<html><body><p>纯文字</p></body></html>"#;

    #[test]
    fn cover_falls_back_to_first_spine_image() {
        // spine 首文档含首图（manifest 未登记该图）→ 回退 2 命中
        let book = book_with(
            OPF_NO_COVER_META,
            CH1,
            &[("OEBPS/images/pic.png", b"\x89PNG-fake")],
        );
        let cover = book.cover().unwrap().expect("应回退到 spine 首图");
        assert_eq!(cover.key.0, "OEBPS/images/pic.png");
    }

    #[test]
    fn cover_falls_back_to_manifest_image() {
        // spine 首文档无图 → 回退 3：manifest 首个 image/*
        let book = book_with(
            OPF_MANIFEST_IMAGE,
            CH1_NO_IMG,
            &[("OEBPS/images/pic.png", b"\x89PNG-fake")],
        );
        let cover = book.cover().unwrap().expect("应回退到 manifest 首图");
        assert_eq!(cover.key.0, "OEBPS/images/pic.png");
    }

    #[test]
    fn cover_none_when_no_image_anywhere() {
        let book = book_with(OPF_NO_COVER_META, CH1_NO_IMG, &[]);
        assert!(book.cover().unwrap().is_none());
    }

    // ---- issue #13：无 TOC 书用正文首个最小级标题回填章名 ----

    #[test]
    fn tocless_book_titles_from_smallest_heading() {
        // 无 nav/NCX；ch1 先 h3 后 h1 → 取 level 最小的 h1
        let ch = r#"<html><body><h3>小节</h3><h1>第一章 起点</h1><p>正文</p></body></html>"#;
        let book = book_with(OPF_NO_COVER_META, ch, &[]);
        assert!(book.toc_raw.is_empty(), "无 nav/NCX 应无目录树");
        assert_eq!(
            book.entries[0].title.as_deref(),
            Some("第一章 起点"),
            "应取 level 最小的标题"
        );
        // h1 仍在正文块序列里（v9 部件流按 type 过滤是宿主职责，内核不删块）
        let content = book.load_chapter(0).unwrap();
        assert!(content
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Heading { level: 1, .. })), "h1 应保留在块流");
    }

    #[test]
    fn tocless_book_without_heading_keeps_none() {
        let book = book_with(OPF_NO_COVER_META, CH1_NO_IMG, &[]);
        assert_eq!(book.entries[0].title, None, "无标题应回 None（宿主回落占位）");
    }

    #[test]
    fn toc_book_titles_unaffected_by_probe() {
        // 有 nav 的书：标题仍来自目录回填，不被正文标题覆盖（backfill 只填空缺）
        let nav = r#"<html><body><nav><ol><li><a href="text/ch1.xhtml">目录名</a></li></ol></nav></body></html>"#;
        let book = book_with(OPF_NAV, CH1, &[("OEBPS/nav.xhtml", nav.as_bytes())]);
        assert_eq!(book.entries[0].title.as_deref(), Some("目录名"));
    }

    // ---- #3 目录层级 + 章内锚点 ----

    const OPF_NAV: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>目录书</dc:title></metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ch1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;

    const NAV_NESTED: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
<nav epub:type="toc"><ol>
  <li><a href="text/ch1.xhtml#sec1">第一节</a>
    <ol><li><a href="text/ch1.xhtml#sec2">小节</a></li></ol>
  </li>
</ol></nav></body></html>"#;

    const CH1_WITH_IDS: &str = r#"<html><body>
<p id="sec1">第一节内容</p><p id="sec2">小节内容</p>
</body></html>"#;

    #[test]
    fn toc_nesting_and_anchor_resolved() {
        let book = book_with(
            OPF_NAV,
            CH1_WITH_IDS,
            &[("OEBPS/nav.xhtml", NAV_NESTED.as_bytes())],
        );
        let toc = book.toc().unwrap();
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].title, "第一节");
        assert_eq!(toc[0].children.len(), 1, "nav 嵌套应保留层级");
        assert_eq!(toc[0].children[0].title, "小节");
        // 锚点 = 目标元素在清洗后章文本中的字节偏移
        assert_eq!(toc[0].anchor, Some(0));
        assert_eq!(toc[0].children[0].anchor, Some("第一节内容".len() as u32));
    }

    #[test]
    fn toc_without_fragment_has_no_anchor() {
        let nav = r#"<html><body><nav><ol><li><a href="text/ch1.xhtml">整章</a></li></ol></nav></body></html>"#;
        let book = book_with(
            OPF_NAV,
            CH1_WITH_IDS,
            &[("OEBPS/nav.xhtml", nav.as_bytes())],
        );
        let toc = book.toc().unwrap();
        assert_eq!(toc.len(), 1);
        assert!(toc[0].anchor.is_none());
    }
}
