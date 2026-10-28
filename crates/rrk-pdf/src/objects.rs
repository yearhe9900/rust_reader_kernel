//! PDF 对象层（自研）：值/字典/流解析、对象表（扫描式 + ObjStm 合并）、流过滤器。
//!
//! L1 口径（方案 §3.6）：不追求完整 xref 表解析，用「全文扫描 `N G obj` 建对象表 +
//! ObjStm 解压合并」的鲁棒路线（对样张与真实书都成立）；流过滤器支持
//! Flate/LZW/ASCIIHex/ASCII85/RunLength（DCT/CCITT 等图像编码不属于文本链路）。

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::Arc;

use rrk_model::{RrkError, RrkResult};

/// PDF 对象。
#[derive(Debug, Clone, PartialEq)]
pub enum Object {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Name(String),
    Str(Vec<u8>),
    Arr(Vec<Object>),
    Dict(BTreeMap<String, Object>),
    Stream { dict: BTreeMap<String, Object>, data: Vec<u8> },
    Ref(u32, u16),
}

pub type Dict = BTreeMap<String, Object>;

impl Object {
    pub fn as_name(&self) -> Option<&str> {
        match self {
            Object::Name(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Object::Int(i) => Some(*i),
            Object::Real(f) => Some(*f as i64),
            _ => None,
        }
    }
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Object::Dict(d) => Some(d),
            Object::Stream { dict, .. } => Some(dict),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[Object]> {
        match self {
            Object::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&[u8]> {
        match self {
            Object::Str(s) => Some(s),
            _ => None,
        }
    }
    /// 字典取值便捷方法（非字典返回 None）。
    pub fn get(&self, key: &str) -> Option<&Object> {
        self.as_dict().and_then(|d| d.get(key))
    }
}

// ---------------------------------------------------------------------------
// 词法/语法
// ---------------------------------------------------------------------------

const WS: &[u8] = &[0x00, 0x09, 0x0A, 0x0C, 0x0D, 0x20];

fn is_ws(b: u8) -> bool {
    WS.contains(&b)
}

fn is_delim(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

pub struct Parser<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Parser<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn skip_ws(&mut self) {
        while self.pos < self.data.len() {
            let b = self.data[self.pos];
            if is_ws(b) {
                self.pos += 1;
            } else if b == b'%' {
                while self.pos < self.data.len() && self.data[self.pos] != b'\n' {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    /// 解析一个对象（不解析 stream 本体——由调用方处理）。
    pub fn parse_object(&mut self) -> RrkResult<Object> {
        self.skip_ws();
        let Some(b) = self.peek() else {
            return Ok(Object::Null);
        };
        match b {
            b'(' => self.parse_literal_string(),
            b'<' => {
                if self.data.get(self.pos + 1) == Some(&b'<') {
                    self.parse_dict_or_stream_body()
                } else {
                    self.parse_hex_string()
                }
            }
            b'[' => {
                self.pos += 1;
                let mut arr = Vec::new();
                loop {
                    self.skip_ws();
                    if self.peek() == Some(b']') {
                        self.pos += 1;
                        break;
                    }
                    if self.peek().is_none() {
                        break;
                    }
                    // 防御：子对象解析若未推进游标（如非法分隔符处容错返回
                    // Null），跳过该字节——否则循环无限压入，Vec 无限增长
                    // （fuzz pdf_open OOM 实锤：230 字节输入诱导 24GiB realloc）
                    let before = self.pos;
                    let o = self.parse_object()?;
                    if self.pos == before {
                        self.pos += 1;
                        continue;
                    }
                    arr.push(o);
                }
                Ok(Object::Arr(arr))
            }
            b'/' => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.data.len() && !is_ws(self.data[self.pos]) && !is_delim(self.data[self.pos]) {
                    self.pos += 1;
                }
                let raw = &self.data[start..self.pos];
                let mut name = String::new();
                let mut i = 0;
                while i < raw.len() {
                    if raw[i] == b'#' && i + 2 < raw.len() + 1 && i + 2 < raw.len() {
                        if let Ok(v) = u8::from_str_radix(&String::from_utf8_lossy(&raw[i + 1..i + 3]), 16) {
                            name.push(v as char);
                            i += 3;
                            continue;
                        }
                    }
                    name.push(raw[i] as char);
                    i += 1;
                }
                Ok(Object::Name(name))
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.parse_number_or_ref(),
            _ => {
                // 关键字：true/false/null
                let start = self.pos;
                while self.pos < self.data.len() && !is_ws(self.data[self.pos]) && !is_delim(self.data[self.pos]) {
                    self.pos += 1;
                }
                let word = &self.data[start..self.pos];
                Ok(match word {
                    b"true" => Object::Bool(true),
                    b"false" => Object::Bool(false),
                    b"null" => Object::Null,
                    _ => {
                        // 防御：空词（游标停在非法分隔符上）必须前进，
                        // 否则上层循环（数组/dict）会原地空转
                        if word.is_empty() {
                            self.pos += 1;
                        }
                        Object::Null
                    }
                })
            }
        }
    }

    fn parse_number_or_ref(&mut self) -> RrkResult<Object> {
        let first = self.parse_number()?;
        if let Object::Int(n1) = first {
            // 试探 "G R" → 间接引用
            let save = self.pos;
            self.skip_ws();
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                if let Ok(Object::Int(n2)) = self.parse_number() {
                    self.skip_ws();
                    if self.peek() == Some(b'R') {
                        self.pos += 1;
                        return Ok(Object::Ref(n1 as u32, n2 as u16));
                    }
                }
            }
            self.pos = save;
            Ok(Object::Int(n1))
        } else {
            Ok(first)
        }
    }

    fn parse_number(&mut self) -> RrkResult<Object> {
        self.skip_ws();
        let start = self.pos;
        if matches!(self.peek(), Some(b'+') | Some(b'-')) {
            self.pos += 1;
        }
        let mut is_real = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => self.pos += 1,
                b'.' => {
                    is_real = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let s = std::str::from_utf8(&self.data[start..self.pos]).unwrap_or("0");
        if is_real {
            Ok(Object::Real(s.parse().unwrap_or(0.0)))
        } else {
            Ok(Object::Int(s.parse().unwrap_or(0)))
        }
    }

    fn parse_literal_string(&mut self) -> RrkResult<Object> {
        self.pos += 1; // (
        let mut out = Vec::new();
        let mut depth = 1;
        while self.pos < self.data.len() {
            let c = self.data[self.pos];
            self.pos += 1;
            match c {
                b'(' => {
                    depth += 1;
                    out.push(b'(');
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(Object::Str(out));
                    }
                    out.push(b')');
                }
                b'\\' => {
                    let Some(e) = self.peek() else { break };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'\n' => {}
                        b'\r' => {
                            if self.peek() == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'0'..=b'7' => {
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                if let Some(d @ b'0'..=b'7') = self.peek() {
                                    v = v * 8 + (d - b'0') as u32;
                                    self.pos += 1;
                                } else {
                                    break;
                                }
                            }
                            out.push((v & 0xFF) as u8);
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
        Ok(Object::Str(out))
    }

    fn parse_hex_string(&mut self) -> RrkResult<Object> {
        self.pos += 1; // <
        let start = self.pos;
        while self.pos < self.data.len() && self.data[self.pos] != b'>' {
            self.pos += 1;
        }
        let hex = &self.data[start..self.pos];
        self.pos = (self.pos + 1).min(self.data.len());
        let mut digits = String::new();
        for &c in hex {
            if c.is_ascii_hexdigit() {
                digits.push(c as char);
            }
        }
        if digits.len() % 2 == 1 {
            digits.push('0');
        }
        let out = (0..digits.len() / 2)
            .map(|i| u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16).unwrap_or(0))
            .collect();
        Ok(Object::Str(out))
    }

    /// `<< ... >>`；若后面紧跟 `stream` 关键字则接流数据。
    fn parse_dict_or_stream_body(&mut self) -> RrkResult<Object> {
        self.pos += 2; // <<
        let mut dict: Dict = BTreeMap::new();
        loop {
            self.skip_ws();
            if self.data[self.pos..].starts_with(b">>") {
                self.pos += 2;
                break;
            }
            if self.peek().is_none() {
                break;
            }
            // 防御：解析未推进游标时强制前进一字节（与数组分支同理，
            // 防止 key/value 卡在非法分隔符上原地空转）
            let kb = self.pos;
            let key = match self.parse_object()? {
                Object::Name(n) => n,
                _ => {
                    if self.pos == kb {
                        self.pos += 1;
                    }
                    continue;
                }
            };
            let vb = self.pos;
            let value = self.parse_object()?;
            if self.pos == vb {
                self.pos += 1;
            }
            dict.insert(key, value);
        }
        // stream？
        let save = self.pos;
        self.skip_ws();
        if self.data[self.pos..].starts_with(b"stream") {
            self.pos += 6;
            if self.data[self.pos..].starts_with(b"\r\n") {
                self.pos += 2;
            } else if self.data[self.pos..].starts_with(b"\n") {
                self.pos += 1;
            }
            let data_start = self.pos;
            // /Length 是直接整数时信它，但用 endstream 验证兜底
            let declared = match dict.get("Length") {
                Some(Object::Int(n)) if *n >= 0 => Some(data_start + *n as usize),
                _ => None,
            };
            let end = match declared {
                Some(e) if e <= self.data.len() && self.data[e.saturating_sub(10)..e.min(self.data.len())].windows(9).any(|w| w == b"endstream") || self.data.get(e) == Some(&b'\r') || self.data.get(e) == Some(&b'\n') => e,
                _ => {
                    // 扫描 endstream
                    let mut p = data_start;
                    let mut found = self.data.len();
                    while p + 9 <= self.data.len() {
                        if &self.data[p..p + 9] == b"endstream" {
                            found = p;
                            break;
                        }
                        p += 1;
                    }
                    // 回退掉 endstream 前的换行
                    while found > data_start && matches!(self.data[found - 1], b'\n' | b'\r') {
                        found -= 1;
                    }
                    found
                }
            };
            self.pos = (end + 9).min(self.data.len());
            return Ok(Object::Stream { dict, data: self.data[data_start..end].to_vec() });
        }
        self.pos = save;
        Ok(Object::Dict(dict))
    }
}

// ---------------------------------------------------------------------------
// 流过滤器
// ---------------------------------------------------------------------------

/// 应用 /Filter 链解码流数据；不支持的过滤器跳过（按原样返回）。
pub fn decode_stream(dict: &Dict, data: &[u8]) -> RrkResult<Vec<u8>> {
    let filters: Vec<String> = match dict.get("Filter") {
        Some(Object::Name(n)) => vec![n.clone()],
        Some(Object::Arr(a)) => a
            .iter()
            .filter_map(|f| f.as_name().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    let mut out = data.to_vec();
    for f in filters {
        out = match f.as_str() {
            "FlateDecode" | "Fl" => flate2::read::ZlibDecoder::new(&out[..])
                .read_to_end_or_fallback(&out),
            "ASCIIHexDecode" | "AHx" => ascii_hex_decode(&out),
            "ASCII85Decode" | "A85" => ascii85_decode(&out),
            "RunLengthDecode" | "RL" => runlength_decode(&out),
            "LZWDecode" | "LZW" => lzw_decode(&out, dict),
            _ => out, // 图像类过滤器（DCT/CCITT…）不属于文本链路
        };
    }
    Ok(out)
}

trait ReadToEndFallback {
    fn read_to_end_or_fallback(self, original: &[u8]) -> Vec<u8>;
}

impl<R: std::io::Read> ReadToEndFallback for R {
    fn read_to_end_or_fallback(mut self, original: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        match std::io::Read::read_to_end(&mut self, &mut out) {
            Ok(_) => out,
            Err(_) => {
                // 有些流不带 zlib 头：试原始 deflate
                match flate2::read::DeflateDecoder::new(original).read_to_end(&mut out) {
                    Ok(_) => out,
                    Err(_) => original.to_vec(),
                }
            }
        }
    }
}

fn ascii_hex_decode(data: &[u8]) -> Vec<u8> {
    let mut digits = String::new();
    for &c in data {
        if c == b'>' {
            break;
        }
        if c.is_ascii_hexdigit() {
            digits.push(c as char);
        }
    }
    if digits.len() % 2 == 1 {
        digits.push('0');
    }
    (0..digits.len() / 2)
        .map(|i| u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect()
}

fn ascii85_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut group: Vec<u8> = Vec::new();
    for &c in data {
        match c {
            b'~' => break,
            b' ' | b'\n' | b'\r' | b'\t' | 0x0C => {}
            b'z' if group.is_empty() => out.extend_from_slice(&[0; 4]),
            b'!'..=b'u' => {
                group.push(c);
                if group.len() == 5 {
                    let mut v: u32 = 0;
                    for &g in &group {
                        v = v * 85 + (g - b'!') as u32;
                    }
                    out.extend_from_slice(&v.to_be_bytes());
                    group.clear();
                }
            }
            _ => {}
        }
    }
    if !group.is_empty() {
        let n = group.len();
        while group.len() < 5 {
            group.push(b'u');
        }
        let mut v: u32 = 0;
        for &g in &group {
            v = v * 85 + (g - b'!') as u32;
        }
        out.extend_from_slice(&v.to_be_bytes());
        out.truncate(out.len() - (5 - n));
    }
    out
}

fn runlength_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let len = data[i] as i32;
        i += 1;
        if len == 128 {
            break;
        }
        if len < 128 {
            let take = (len as usize + 1).min(data.len() - i);
            out.extend_from_slice(&data[i..i + take]);
            i += take;
        } else {
            if i >= data.len() {
                break;
            }
            let byte = data[i];
            i += 1;
            out.extend(std::iter::repeat(byte).take(257 - len as usize));
        }
    }
    out
}

/// LZW 解码（PDF 变体：9~12 位码、early-change 1）。
fn lzw_decode(data: &[u8], dict: &Dict) -> Vec<u8> {
    let early = match dict.get("DecodeParms").and_then(|p| p.as_dict()).and_then(|d| d.get("EarlyChange")) {
        Some(Object::Int(0)) => 0u32,
        _ => 1u32,
    };
    let mut table: Vec<Vec<u8>> = (0..256).map(|i| vec![i as u8]).collect();
    let mut out = Vec::new();
    let mut bitpos = 0usize;
    let mut code_len = 9u32;
    let mut prev: Option<Vec<u8>> = None;
    let total_bits = data.len() * 8;
    while bitpos + code_len as usize <= total_bits {
        // 取 code_len 位（MSB 优先）
        let mut code = 0u32;
        for k in 0..code_len {
            let p = bitpos + k as usize;
            let byte = data[p >> 3];
            let bit = (byte >> (7 - (p & 7))) & 1;
            code = (code << 1) | bit as u32;
        }
        bitpos += code_len as usize;
        if code == 256 {
            table.truncate(258);
            code_len = 9;
            prev = None;
            continue;
        }
        if code == 257 {
            break;
        }
        let entry = if (code as usize) < table.len() {
            table[code as usize].clone()
        } else if let Some(p) = &prev {
            let mut e = p.clone();
            e.push(p[0]);
            e
        } else {
            break;
        };
        out.extend_from_slice(&entry);
        if let Some(p) = &prev {
            let mut new_entry = p.clone();
            new_entry.push(entry[0]);
            table.push(new_entry);
        }
        prev = Some(entry);
        let limit = (table.len() as u32 + early) >> code_len;
        if limit > 0 && code_len < 12 {
            code_len += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 文档对象表
// ---------------------------------------------------------------------------

/// PDF 文档：字节源 + 对象表（扫描式）+ 解析缓存 + ObjStm 虚拟对象。
pub struct PdfDocument {
    data: Arc<[u8]>,
    offsets: HashMap<u32, usize>,
    cache: std::sync::Mutex<HashMap<u32, Object>>,
    virtuals: std::sync::Mutex<HashMap<u32, Object>>,
    /// 光栅字体缓存（raster 层填充：FontFile 解压/ToUnicode 解析跨页复用）。
    pub(crate) font_cache: std::sync::Mutex<HashMap<u32, Arc<crate::raster::RasterFont>>>,
    /// 图像 XObject 解码缓存（raster 层填充：同一图像跨页/跨 Do 只 inflate+解码
    /// 一次；有字节上限，超限整表清空重填）。
    pub(crate) image_cache: std::sync::Mutex<crate::raster::RasterImageCache>,
}

impl PdfDocument {
    /// 建对象表：全文扫描 `N G obj`（鲁棒，对 xref 流/损坏 xref 都成立）。
    pub fn open(data: Arc<[u8]>) -> RrkResult<Self> {
        if !data.starts_with(b"%PDF-") {
            return Err(RrkError::corrupt("PDF: 缺少 %PDF 头"));
        }
        let mut offsets = HashMap::new();
        let mut i = 0usize;
        while i + 4 <= data.len() {
            if &data[i..i + 4] == b" obj" && i >= 8 {
                // 向前解析 "N G "（纯字节：对象前可能是二进制流数据，禁止 UTF-8 校验）
                let back_start = i.saturating_sub(48);
                let back = &data[back_start..i];
                if let Some((num, gen, num_at)) = parse_obj_header(back) {
                    offsets
                        .entry(num)
                        .or_insert(back_start + num_at);
                    let _ = gen;
                }
            }
            i += 1;
        }
        if offsets.is_empty() {
            return Err(RrkError::corrupt("PDF: 无对象"));
        }
        Ok(Self {
            data,
            offsets,
            cache: std::sync::Mutex::new(HashMap::new()),
            virtuals: std::sync::Mutex::new(HashMap::new()),
            font_cache: std::sync::Mutex::new(HashMap::new()),
            image_cache: std::sync::Mutex::new(crate::raster::RasterImageCache::default()),
        })
    }

    /// 对象号列表（扫描式表）。
    pub fn object_nums(&self) -> impl Iterator<Item = &u32> {
        self.offsets.keys()
    }

    /// 取对象（含间接引用解引用一层；ObjStm 惰性合并）。
    pub fn get(&self, num: u32) -> RrkResult<Object> {
        if let Some(o) = self.cache.lock().unwrap().get(&num) {
            return Ok(o.clone());
        }
        if let Some(o) = self.virtuals.lock().unwrap().get(&num) {
            return Ok(o.clone());
        }
        let &offset = self
            .offsets
            .get(&num)
            .ok_or_else(|| RrkError::corrupt(format!("PDF: 对象缺失 {num}")))?;
        let mut p = Parser::new(&self.data);
        p.pos = offset;
        // 跳过 "N G obj"
        p.skip_ws();
        let _ = p.parse_object()?; // obj 号
        p.skip_ws();
        let _ = p.parse_object()?; // gen 号
        p.skip_ws();
        if p.data[p.pos..].starts_with(b"obj") {
            p.pos += 3;
        }
        let obj = p.parse_object()?;
        self.cache.lock().unwrap().insert(num, obj.clone());
        Ok(obj)
    }

    /// 解引用：`Ref → 对象`，最多 16 层。
    pub fn resolve(&self, obj: &Object) -> Object {
        let mut cur = obj.clone();
        for _ in 0..16 {
            match cur {
                Object::Ref(n, _) => match self.get(n) {
                    Ok(next) => cur = next,
                    Err(_) => return Object::Null,
                },
                other => return other,
            }
        }
        cur
    }

    /// 读取流数据并解过滤器。
    pub fn stream_data(&self, obj: &Object) -> RrkResult<Vec<u8>> {
        match self.resolve(obj) {
            Object::Stream { dict, data } => decode_stream(&dict, &data),
            _ => Err(RrkError::corrupt("PDF: 非流对象")),
        }
    }

    /// ObjStm 合并（解析 /Type/ObjStm 流内的压缩对象）。
    /// 只在解析失败时惰性触发一次。
    pub fn merge_objstms(&self) {
        let mut merged = self.virtuals.lock().unwrap().len() > 0;
        if merged {
            return;
        }
        merged = true;
        let nums: Vec<u32> = self.offsets.keys().copied().collect();
        for num in nums {
            let Ok(obj) = self.get(num) else { continue };
            if let Object::Stream { dict, .. } = &obj {
                if dict.get("Type").and_then(|t| t.as_name()) != Some("ObjStm") {
                    continue;
                }
                let Ok(decoded) = self.stream_data(&obj) else { continue };
                let n = dict.get("N").and_then(|v| v.as_int()).unwrap_or(0) as usize;
                let first = dict.get("First").and_then(|v| v.as_int()).unwrap_or(0) as usize;
                let mut p = Parser::new(&decoded);
                let mut pairs = Vec::new();
                for _ in 0..n {
                    let Ok(a) = p.parse_object() else { break };
                    let Ok(b) = p.parse_object() else { break };
                    if let (Some(id), Some(off)) = (a.as_int(), b.as_int()) {
                        pairs.push((id as u32, off as usize));
                    }
                }
                for (id, off) in pairs {
                    let mut inner = Parser::new(&decoded);
                    inner.pos = first + off;
                    if let Ok(o) = inner.parse_object() {
                        self.virtuals.lock().unwrap().entry(id).or_insert(o);
                    }
                }
            }
        }
    }
}

/// 在尾部切片里找 "N G "（obj 关键字之前）；返回 (num, gen, num 起始下标)。
fn parse_obj_header(back: &[u8]) -> Option<(u32, u16, usize)> {
    let mut i = back.len();
    while i > 0 && (back[i - 1] as char).is_ascii_whitespace() {
        i -= 1;
    }
    let end_gen = i;
    while i > 0 && back[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == end_gen {
        return None;
    }
    let gen: u16 = std::str::from_utf8(&back[i..end_gen]).ok()?.parse().ok()?;
    while i > 0 && (back[i - 1] as char).is_ascii_whitespace() {
        i -= 1;
    }
    let end_num = i;
    while i > 0 && back[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == end_num {
        return None;
    }
    // 必须以空白（或窗口边界）与前文分隔，避免从流数据中间误认
    if i > 0 && !(back[i - 1] as char).is_ascii_whitespace() {
        return None;
    }
    let num: u32 = std::str::from_utf8(&back[i..end_num]).ok()?.parse().ok()?;
    Some((num, gen, i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dict_stream_and_refs() {
        let src = b"1 0 obj\n<< /Type /Catalog /Kids [2 0 R 2 0 R] /S (a\\n b) >>\nendobj\n";
        let mut p = Parser::new(src);
        assert!(matches!(p.parse_object().unwrap(), Object::Int(1)));
        assert!(matches!(p.parse_object().unwrap(), Object::Int(0)));
        assert!(matches!(p.parse_object().unwrap(), Object::Null)); // "obj" 关键字
        match p.parse_object().unwrap() {
            Object::Dict(d) => {
                assert_eq!(d.get("Type").unwrap().as_name(), Some("Catalog"));
                match d.get("Kids").unwrap() {
                    Object::Arr(a) => assert_eq!(a.len(), 2),
                    _ => panic!(),
                }
                assert_eq!(d.get("S").unwrap().as_str(), Some(&b"a\n b"[..]));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_hex_and_nested_string() {
        let mut p = Parser::new(b"<48656C6C6F> (a(b)c)");
        assert_eq!(p.parse_object().unwrap().as_str(), Some(&b"Hello"[..]));
        assert_eq!(p.parse_object().unwrap().as_str(), Some(&b"a(b)c"[..]));
    }

    #[test]
    fn filters_roundtrip() {
        // Flate
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, b"hello flate").unwrap();
        let z = enc.finish().unwrap();
        let mut d = BTreeMap::new();
        d.insert("Filter".to_string(), Object::Name("FlateDecode".into()));
        assert_eq!(decode_stream(&d, &z).unwrap(), b"hello flate");

        // ASCIIHex + RunLength + ASCII85
        let mut d2 = BTreeMap::new();
        d2.insert("Filter".to_string(), Object::Name("ASCIIHexDecode".into()));
        assert_eq!(decode_stream(&d2, b"48656C6C6F>").unwrap(), b"Hello");
        let mut d3 = BTreeMap::new();
        d3.insert("Filter".to_string(), Object::Name("RunLengthDecode".into()));
        assert_eq!(decode_stream(&d3, &[0x02, b'a', b'b', b'c', 0xFE, b'x', 0x80]).unwrap(), b"abcxxx");
        let mut d4 = BTreeMap::new();
        d4.insert("Filter".to_string(), Object::Name("ASCII85Decode".into()));
        assert_eq!(decode_stream(&d4, b"87cUR~>").unwrap(), b"Hell");
    }

    #[test]
    fn objstm_merge() {
        // 手工构造：对象 1 = ObjStm，含对象 5（<< /X 42 >>）
        let mut body = Vec::new();
        body.extend_from_slice(b"5 0 ");
        let first = body.len();
        body.extend_from_slice(b"<< /X 42 >>");
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &body).unwrap();
        let z = enc.finish().unwrap();
        let mut file = Vec::new();
        file.extend_from_slice(b"%PDF-1.5\n");
        file.extend_from_slice(b"1 0 obj\n<< /Type /ObjStm /N 1 /First ");
        file.extend_from_slice(first.to_string().as_bytes());
        file.extend_from_slice(b" /Filter /FlateDecode /Length ");
        file.extend_from_slice(z.len().to_string().as_bytes());
        file.extend_from_slice(b" >>\nstream\n");
        file.extend_from_slice(&z);
        file.extend_from_slice(b"\nendstream\nendobj\n");
        let doc = PdfDocument::open(Arc::from(file.into_boxed_slice())).unwrap();
        doc.merge_objstms();
        match doc.get(5).unwrap() {
            Object::Dict(d) => assert_eq!(d.get("X").unwrap().as_int(), Some(42)),
            other => panic!("unexpected {other:?}"),
        }
    }
}
