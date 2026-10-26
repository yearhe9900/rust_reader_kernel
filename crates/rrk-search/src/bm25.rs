//! BM25 索引：构建 / 序列化 / 反序列化 / 查询。
//!
//! 参数取业界默认：`k1 = 1.2`、`b = 0.75`；`doc` = **章**（与内核章索引同一粒度），
//! 故「全书搜索 → 命中章 → BM25 排序」天然对齐。
//!
//! 序列化格式（小端，自描述、可审计）：
//!
//! ```text
//! magic  : "RRKI"            (4B)
//! version: u32 = 1
//! doc_cnt: u32   total_tokens: u64   term_cnt: u32
//! docs   : doc_cnt × u32     （章 id）
//! doc_len: doc_cnt × u32     （该章词项数，BM25 的 dl）
//! terms  : term_cnt × { token_len:u32, token:bytes, df:varint,
//!                       df × { doc_delta:varint, tf:varint } }
//! ```
//!
//! `doc_delta` 相对上一个 doc index 的增量（postings 按 doc 升序追加）；
//! `df`/`doc_delta`/`tf` 用 LEB128 varint——CJK bigram 词表里 tf 多为 1、章号增量
//! 远小于 2^14，实测比定长 u32 省约一半体积（24.5MB → 约 12MB）。

use std::collections::{HashMap, HashSet};

use rrk_model::{ChapterId, RrkError, RrkResult};

use crate::{fold_ascii, for_each_term};

const MAGIC: [u8; 4] = *b"RRKI";
const VERSION: u32 = 1;
/// BM25 词频饱和参数。
pub const K1: f32 = 1.2;
/// BM25 文档长度归一参数。
pub const B: f32 = 0.75;

/// 一条排序结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoredHit {
    pub chapter: ChapterId,
    pub score: f32,
}

/// 索引构建器：逐章投喂（可分步、可中途放弃）。
///
/// 构建期用逐词项临时 `Vec` 攒 postings（新词追加天然是随机位置，扁平 arena
/// 无法就地续写）；`build()` 一次压实成 CSR，临时结构随构建器释放。
#[derive(Default)]
pub struct Bm25Builder {
    docs: Vec<u32>,
    doc_len: Vec<u32>,
    dict: HashMap<Box<str>, u32>,
    terms: Vec<Vec<(u32, u32)>>,
    total_tokens: u64,
}

impl Bm25Builder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 投喂一章正文（应为 `flatten_blocks_spans` 产出的 plain 文本拼接）。
    ///
    /// 返回该章词项数（0 = 空章，不影响索引）。
    pub fn add_doc(&mut self, chapter: ChapterId, text: &str) -> u32 {
        let doc_idx = self.docs.len() as u32;
        let folded = fold_ascii(text);
        let mut tf: HashMap<&str, u32> = HashMap::new();
        let mut tokens = 0u32;
        for_each_term(&folded, |t| {
            *tf.entry(t).or_insert(0) += 1;
            tokens += 1;
        });
        for (term, count) in tf {
            // 命中路径单次哈希查找；仅「新词」时才 insert（stable Rust 的 entry
            // 必须先持有所有权键，会让每次命中都多一次分配，反而更慢）。
            // 键用 Box<str>：比 String 少 8 字节（无 capacity 字段）。
            let id = match self.dict.get(term) {
                Some(&id) => id,
                None => {
                    let id = self.terms.len() as u32;
                    self.dict.insert(term.into(), id);
                    self.terms.push(Vec::new());
                    id
                }
            };
            self.terms[id as usize].push((doc_idx, count));
        }
        self.docs.push(chapter);
        self.doc_len.push(tokens);
        self.total_tokens += u64::from(tokens);
        tokens
    }

    /// 已投喂章数。
    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }

    /// 已收词项数（唯一词项）。
    pub fn term_count(&self) -> usize {
        self.terms.len()
    }

    /// 收尾成可查询索引（逐词项临时倒排一次压实成扁平 CSR）。
    pub fn build(self) -> Bm25 {
        let avgdl = if self.docs.is_empty() {
            0.0
        } else {
            self.total_tokens as f32 / self.docs.len() as f32
        };
        let total: usize = self.terms.iter().map(|p| p.len()).sum();
        let mut postings = Vec::with_capacity(total);
        let mut offsets = Vec::with_capacity(self.terms.len() + 1);
        offsets.push(0);
        for p in &self.terms {
            postings.extend_from_slice(p);
            offsets.push(postings.len() as u32);
        }
        Bm25 {
            docs: self.docs,
            doc_len: self.doc_len,
            dict: self.dict,
            postings,
            offsets,
            total_tokens: self.total_tokens,
            avgdl,
        }
    }
}

/// BM25 倒排索引（内存态；`encode`/`decode` 与宿主落盘字节互转）。
///
/// 倒排表用扁平 CSR 布局：所有词项的 postings 串接在单一 `postings` arena 里，
/// `offsets[id]..offsets[id + 1]` 是词项 `id` 的区间——百万词项只有两次 Vec
/// 分配，替代原先每词项一次的小堆分配（碎片/耗时主源）。
#[derive(Debug, Clone)]
pub struct Bm25 {
    docs: Vec<u32>,
    doc_len: Vec<u32>,
    dict: HashMap<Box<str>, u32>,
    /// CSR 的 data 段：全部词项倒排串接（词内按 doc 升序）。
    postings: Vec<(u32, u32)>,
    /// CSR 的索引段：长度恒为 `term_cnt + 1`，词项 `id` 的倒排是
    /// `postings[offsets[id] as usize..offsets[id + 1] as usize]`。
    offsets: Vec<u32>,
    total_tokens: u64,
    avgdl: f32,
}

impl Bm25 {
    /// 章数。
    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }

    /// 唯一词项数。
    pub fn term_count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// 取词项 `id` 的倒排切片（CSR 区间）。
    #[inline]
    fn postings_of(&self, id: u32) -> &[(u32, u32)] {
        let id = id as usize;
        &self.postings[self.offsets[id] as usize..self.offsets[id + 1] as usize]
    }

    /// 全库词项总数。
    pub fn total_tokens(&self) -> u64 {
        self.total_tokens
    }

    /// 平均章长（词项数），BM25 的 `avgdl`。
    pub fn avgdl(&self) -> f32 {
        self.avgdl
    }

    /// 取所有 doc 的章 id（按索引内顺序）。
    pub fn chapters(&self) -> &[u32] {
        &self.docs
    }

    /// 查询：返回按 BM25 得分降序的前 `top_k` 章。
    pub fn query(&self, q: &str, top_k: usize) -> Vec<ScoredHit> {
        let n = self.docs.len();
        if n == 0 {
            return Vec::new();
        }
        let folded = fold_ascii(q.trim());
        let mut seen: HashSet<u32> = HashSet::new();
        let mut qterms: Vec<u32> = Vec::new();
        for_each_term(&folded, |t| {
            if let Some(&id) = self.dict.get(t) {
                if seen.insert(id) {
                    qterms.push(id);
                }
            }
        });
        if qterms.is_empty() {
            return Vec::new();
        }

        let mut scores = vec![0f32; n];
        let avgdl = self.avgdl.max(1e-6);
        for id in qterms {
            let postings = self.postings_of(id);
            let df = postings.len() as f32;
            // Lucene 口径的 idf（恒正，避免 df > N/2 时出现负分）
            let idf = (1.0 + (n as f32 - df + 0.5) / (df + 0.5)).ln();
            for &(doc, tf) in postings {
                let dl = self.doc_len[doc as usize] as f32;
                let denom = tf as f32 + K1 * (1.0 - B + B * dl / avgdl);
                scores[doc as usize] += idf * (tf as f32 * (K1 + 1.0)) / denom;
            }
        }

        let mut hits: Vec<ScoredHit> = scores
            .iter()
            .enumerate()
            .filter(|(_, s)| **s > 0.0)
            .map(|(i, s)| ScoredHit { chapter: self.docs[i], score: *s })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.chapter.cmp(&b.chapter))
        });
        hits.truncate(top_k.max(1));
        hits
    }

    /// 序列化为自描述字节（宿主落盘；内核不写盘）。
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.postings.len() * 12 + self.offsets.len() * 16);
        out.extend_from_slice(&MAGIC);
        put_u32(&mut out, VERSION);
        put_u32(&mut out, self.docs.len() as u32);
        out.extend_from_slice(&self.total_tokens.to_le_bytes());
        put_u32(&mut out, self.term_count() as u32);
        for d in &self.docs {
            put_u32(&mut out, *d);
        }
        for l in &self.doc_len {
            put_u32(&mut out, *l);
        }
        // term id → token（dict 是 token→id，这里反向取一次）
        let mut tokens: Vec<&str> = vec![""; self.term_count()];
        for (t, &id) in &self.dict {
            tokens[id as usize] = &**t;
        }
        for id in 0..self.term_count() {
            let postings = self.postings_of(id as u32);
            let t = tokens[id].as_bytes();
            put_u32(&mut out, t.len() as u32);
            out.extend_from_slice(t);
            put_varint(&mut out, postings.len() as u32);
            let mut prev = 0u32;
            for &(doc, tf) in postings {
                put_varint(&mut out, doc - prev);
                put_varint(&mut out, tf);
                prev = doc;
            }
        }
        out
    }

    /// 反序列化（宿主把落盘字节交回内核时用）；损坏数据返回 `CorruptData`。
    pub fn decode(bytes: &[u8]) -> RrkResult<Bm25> {
        let mut cur = Cursor { bytes, pos: 0 };
        if cur.take(4)? != MAGIC {
            return Err(RrkError::corrupt("BM25 索引：magic 不符（不是本内核产出的索引）"));
        }
        let ver = cur.u32()?;
        if ver != VERSION {
            return Err(RrkError::corrupt(format!(
                "BM25 索引：版本 {ver} 不受支持（当前 {VERSION}）"
            )));
        }
        let doc_cnt = cur.u32()? as usize;
        let total_tokens = u64::from_le_bytes(cur.take(8)?.try_into().expect("8 字节"));
        let term_cnt = cur.u32()? as usize;

        let mut docs = Vec::with_capacity(doc_cnt);
        for _ in 0..doc_cnt {
            docs.push(cur.u32()?);
        }
        let mut doc_len = Vec::with_capacity(doc_cnt);
        for _ in 0..doc_cnt {
            doc_len.push(cur.u32()?);
        }
        // 直接解进扁平 CSR：dict 只 insert（无查找），postings 顺序追加。
        let mut dict: HashMap<Box<str>, u32> = HashMap::with_capacity(term_cnt);
        let mut postings: Vec<(u32, u32)> = Vec::new();
        let mut offsets: Vec<u32> = Vec::with_capacity(term_cnt + 1);
        offsets.push(0);
        for id in 0..term_cnt {
            let len = cur.u32()? as usize;
            let token: Box<str> = std::str::from_utf8(cur.take(len)?)
                .map_err(|e| RrkError::corrupt(format!("BM25 索引：词项非 UTF-8: {e}")))?
                .into();
            let df = cur.varint()? as usize;
            let mut prev = 0u32;
            for _ in 0..df {
                let delta = cur.varint()?;
                let doc = prev
                    .checked_add(delta)
                    .ok_or_else(|| RrkError::corrupt("BM25 索引：doc 增量溢出"))?;
                let tf = cur.varint()?;
                if doc as usize >= doc_cnt {
                    return Err(RrkError::corrupt(format!(
                        "BM25 索引：postings 的 doc {doc} 越界（doc_cnt={doc_cnt}）"
                    )));
                }
                postings.push((doc, tf));
                prev = doc;
            }
            dict.insert(token, id as u32);
            offsets.push(postings.len() as u32);
        }

        let avgdl = if docs.is_empty() { 0.0 } else { total_tokens as f32 / docs.len() as f32 };
        Ok(Bm25 { docs, doc_len, dict, postings, offsets, total_tokens, avgdl })
    }
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// LEB128 无符号 varint（第 7 位为续位）。
fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> RrkResult<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| RrkError::corrupt("BM25 索引：长度溢出"))?;
        if end > self.bytes.len() {
            return Err(RrkError::corrupt(format!(
                "BM25 索引：数据截断（需要到 {end}，实有 {}）",
                self.bytes.len()
            )));
        }
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> RrkResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 字节")))
    }

    fn varint(&mut self) -> RrkResult<u32> {
        let mut out = 0u32;
        let mut shift = 0u32;
        loop {
            let b = *self
                .take(1)?
                .first()
                .ok_or_else(|| RrkError::corrupt("BM25 索引：varint 读取越界"))?;
            if shift >= 32 {
                return Err(RrkError::corrupt("BM25 索引：varint 溢出"));
            }
            out |= ((b & 0x7F) as u32) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handbook() -> Vec<(u32, &'static str)> {
        vec![
            (0, "萧炎在斗气大陆上修炼斗气，斗气为尊。"), // 两词都出现
            (1, "少年萧炎握紧玄重尺，萧家的荣耀由他扛起。"), // 只有「萧炎」
            (2, "路人甲在集市上卖菜，与修炼无关。"),       // 两词都无
            (3, "斗气修炼法门很多，斗气灌体最关键。"),     // 只有「斗气」
        ]
    }

    fn build() -> Bm25 {
        let mut b = Bm25Builder::new();
        for (id, t) in handbook() {
            b.add_doc(id, t);
        }
        b.build()
    }

    #[test]
    fn ranking_favors_more_relevant_chapter() {
        let idx = build();
        assert_eq!(idx.doc_count(), 4);
        let hits = idx.query("萧炎 斗气", 4);
        let ch: Vec<u32> = hits.iter().map(|h| h.chapter).collect();
        // 两词齐全的章排第一；各含一词的章被召回；不含任何词的章不出现
        assert_eq!(hits[0].chapter, 0, "两词齐全应领先: {ch:?}");
        assert!(ch.contains(&1) && ch.contains(&3), "单词命中应召回: {ch:?}");
        assert!(!ch.contains(&2), "无关章不应召回: {ch:?}");
        // 得分降序
        for w in hits.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    #[test]
    fn query_of_absent_term_is_empty() {
        let idx = build();
        assert!(idx.query("量子计算机", 5).is_empty());
        assert!(idx.query("   ", 5).is_empty());
    }

    #[test]
    fn ascii_case_folded_query_matches() {
        let mut b = Bm25Builder::new();
        b.add_doc(7, "The Hunter wandered into the forest.");
        let idx = b.build();
        let hits = idx.query("HUNTER", 3);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chapter, 7);
    }

    #[test]
    fn encode_decode_roundtrip_is_identical() {
        let idx = build();
        let bytes = idx.encode();
        let back = Bm25::decode(&bytes).expect("应能解回");
        assert_eq!(back.doc_count(), idx.doc_count());
        assert_eq!(back.term_count(), idx.term_count());
        assert_eq!(back.total_tokens(), idx.total_tokens());
        assert_eq!(back.chapters(), idx.chapters());
        // 重编码字节完全一致（序列化确定性）
        assert_eq!(back.encode(), bytes);
        // 查询结果一致
        assert_eq!(back.query("斗气", 4), idx.query("斗气", 4));
    }

    #[test]
    fn decode_rejects_corruption() {
        let idx = build();
        let bytes = idx.encode();
        // 截断
        assert!(Bm25::decode(&bytes[..bytes.len() - 3]).is_err());
        // magic 错
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert_eq!(Bm25::decode(&bad).unwrap_err().kind, rrk_model::RrkErrorKind::CorruptData);
        // 版本错
        let mut bad2 = bytes.clone();
        bad2[4] = 99;
        assert!(Bm25::decode(&bad2).is_err());
        // 空输入
        assert!(Bm25::decode(&[]).is_err());
    }

    /// wire 格式回归：按模块文档的格式规范**独立**解析 `encode()` 输出，
    /// 再用测试内的参考编码器重编码，必须逐字节一致——证明内存布局改为
    /// CSR 后，字段、顺序、varint/delta 编码逻辑均未变。
    #[test]
    fn encode_wire_format_unchanged() {
        let idx = build();
        let bytes = idx.encode();

        // ── 独立解析（只依赖文档里的格式说明，不碰内部布局）──
        let mut c = Cursor { bytes: &bytes, pos: 0 };
        assert_eq!(c.take(4).unwrap(), b"RRKI");
        assert_eq!(c.u32().unwrap(), 1, "version 应为 1");
        let doc_cnt = c.u32().unwrap() as usize;
        let total_tokens = u64::from_le_bytes(c.take(8).unwrap().try_into().unwrap());
        let term_cnt = c.u32().unwrap() as usize;
        assert_eq!(doc_cnt, idx.doc_count());
        assert_eq!(term_cnt, idx.term_count());
        assert_eq!(total_tokens, idx.total_tokens());
        let docs: Vec<u32> = (0..doc_cnt).map(|_| c.u32().unwrap()).collect();
        let doc_len: Vec<u32> = (0..doc_cnt).map(|_| c.u32().unwrap()).collect();
        assert_eq!(docs, vec![0, 1, 2, 3], "docs 按投喂顺序逐个 u32");
        // handbook() 各章词项数（for_each_term 分词稳定，与旧实现同值）
        assert_eq!(doc_len, vec![14, 16, 12, 13], "doc_len 段格式未变");
        assert_eq!(total_tokens as u32, doc_len.iter().sum::<u32>());
        let mut terms: Vec<(String, Vec<(u32, u32)>)> = Vec::with_capacity(term_cnt);
        for _ in 0..term_cnt {
            let len = c.u32().unwrap() as usize;
            let token = std::str::from_utf8(c.take(len).unwrap()).unwrap().to_string();
            let df = c.varint().unwrap();
            let mut postings = Vec::with_capacity(df as usize);
            let mut prev = 0u32;
            for _ in 0..df {
                let doc = prev + c.varint().unwrap();
                let tf = c.varint().unwrap();
                postings.push((doc, tf));
                prev = doc;
            }
            assert!(postings.windows(2).all(|w| w[0].0 < w[1].0), "postings 按 doc 升序");
            terms.push((token, postings));
        }
        assert_eq!(c.pos, bytes.len(), "解析后应恰好耗尽全部字节");

        // ── 参考编码器（照格式注释独立实现）→ 必须与 encode() 逐字节一致 ──
        let mut out = Vec::new();
        out.extend_from_slice(b"RRKI");
        put_u32(&mut out, 1);
        put_u32(&mut out, doc_cnt as u32);
        out.extend_from_slice(&total_tokens.to_le_bytes());
        put_u32(&mut out, term_cnt as u32);
        for d in &docs {
            put_u32(&mut out, *d);
        }
        for l in &doc_len {
            put_u32(&mut out, *l);
        }
        for (t, postings) in &terms {
            put_u32(&mut out, t.len() as u32);
            out.extend_from_slice(t.as_bytes());
            put_varint(&mut out, postings.len() as u32);
            let mut prev = 0u32;
            for &(doc, tf) in postings {
                put_varint(&mut out, doc - prev);
                put_varint(&mut out, tf);
                prev = doc;
            }
        }
        assert_eq!(out, bytes, "wire 格式（字段顺序/编码方式）应与改造前逐字节一致");

        // ── decode 后 query 结果与内存态一致 ──
        let back = Bm25::decode(&bytes).expect("应能解回");
        assert_eq!(back.encode(), bytes);
        for q in ["萧炎", "斗气", "修炼 斗气", "萧炎的"] {
            assert_eq!(back.query(q, 4), idx.query(q, 4), "query({q}) 应一致");
        }
    }

    #[test]
    fn empty_index_is_safe() {
        let idx = Bm25Builder::new().build();
        assert_eq!(idx.doc_count(), 0);
        assert!(idx.query("任何", 5).is_empty());
        let bytes = idx.encode();
        let back = Bm25::decode(&bytes).unwrap();
        assert_eq!(back.doc_count(), 0);
    }
}
