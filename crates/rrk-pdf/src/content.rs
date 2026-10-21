//! PDF 内容流文本抽取（L1，自研）：跟踪 Td/TD/Tm/T*/TL 定位与 Tj/TJ/'/" 显字算子，
//! 用字体 ToUnicode CMap 解码字符串；行/段落按 y 位置变化切分。
//!
//! L1 诚实边界：抽取顺序 = 内容流顺序（绝大多数 PDF 即阅读序）；复杂双栏/绝对定位
//! 重排属后续样张驱动项；无 ToUnicode 的 CJK 字体可能出占位符（走降级口径）。

use std::collections::HashMap;

use crate::objects::{Object, Parser, PdfDocument};

/// 字体解码数据。
pub struct FontData {
    /// ToUnicode 映射（源码 → 字符串）。
    pub to_unicode: Option<HashMap<u16, String>>,
    /// 双字节码（映射键存在 >0xFF 的源码）。
    pub two_byte: bool,
}

/// 解析 ToUnicode CMap：`beginbfchar` / `beginbfrange` 两族。
pub fn parse_tounicode(data: &[u8]) -> HashMap<u16, String> {
    let text = String::from_utf8_lossy(data);
    let bytes = text.as_bytes();
    let mut map = HashMap::new();

    let find_kw = |kw: &[u8], from: usize| -> Option<usize> {
        let mut i = from;
        while i + kw.len() <= bytes.len() {
            if &bytes[i..i + kw.len()] == kw {
                return Some(i);
            }
            i += 1;
        }
        None
    };

    // bfchar：成对 <src> <dst>
    let mut pos = 0usize;
    while let Some(start) = find_kw(b"beginbfchar", pos) {
        let Some(end) = find_kw(b"endbfchar", start) else { break };
        for pair in hex_tokens(&text[start..end]).chunks(2) {
            if pair.len() == 2 {
                if let (Some(src), Some(dst)) = (parse_hex_u16(&pair[0]), hex_to_utf16(&pair[1])) {
                    map.insert(src, dst);
                }
            }
        }
        pos = end + 10;
    }

    // bfrange：<lo> <hi> <dst>（dst 递增）或 <lo> <hi> [d1 d2 ...]
    let mut pos = 0usize;
    while let Some(start) = find_kw(b"beginbfrange", pos) {
        let Some(end) = find_kw(b"endbfrange", start) else { break };
        let toks = hex_tokens(&text[start..end]);
        let mut i = 0usize;
        while i < toks.len() {
            if toks[i] == "[" || toks[i] == "]" {
                i += 1;
                continue;
            }
            let (Some(lo), Some(hi)) = (parse_hex_u16(&toks[i]), parse_hex_u16(toks.get(i + 1).map(String::as_str).unwrap_or("")))
            else {
                i += 1;
                continue;
            };
            if toks.get(i + 2).map(String::as_str) == Some("[") {
                let mut code = lo;
                let mut j = i + 3;
                while j < toks.len() && toks[j] != "]" {
                    if let Some(dst) = hex_to_utf16(&toks[j]) {
                        map.insert(code, dst);
                    }
                    code += 1;
                    j += 1;
                }
                i = j + 1;
            } else if let Some(dst_hex) = toks.get(i + 2) {
                for k in 0..=hi.saturating_sub(lo) {
                    if let Some(d) = increment_hex(dst_hex, k as u32).and_then(|h| hex_to_utf16(&h)) {
                        map.insert(lo + k, d);
                    }
                }
                i += 3;
            } else {
                i += 1;
            }
        }
        pos = end + 10;
    }

    map
}

/// 提取 `<...>` 十六进制串与方括号 token。
fn hex_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_hex = false;
    for c in s.chars() {
        match c {
            '<' if !in_hex => {
                in_hex = true;
                buf.clear();
            }
            '>' if in_hex => {
                in_hex = false;
                out.push(std::mem::take(&mut buf));
            }
            '[' if !in_hex => out.push("[".into()),
            ']' if !in_hex => out.push("]".into()),
            _ if in_hex => {
                if c.is_ascii_hexdigit() {
                    buf.push(c);
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_hex_u16(s: &str) -> Option<u16> {
    if s.is_empty() {
        return None;
    }
    u16::from_str_radix(s.get(..4).unwrap_or(s), 16).ok()
}

fn increment_hex(s: &str, by: u32) -> Option<String> {
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(format!("{:04x}", v + by))
}

fn hex_to_utf16(hex: &str) -> Option<String> {
    if hex.is_empty() {
        return None;
    }
    let mut bytes = Vec::new();
    let mut h = hex.to_string();
    if h.len() % 2 == 1 {
        h.push('0');
    }
    for i in 0..h.len() / 2 {
        bytes.push(u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?);
    }
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

// ---------------------------------------------------------------------------
// 文本抽取
// ---------------------------------------------------------------------------

fn decode_string(bytes: &[u8], font: Option<&FontData>) -> String {
    if let Some(f) = font {
        if let Some(map) = &f.to_unicode {
            let mut out = String::new();
            let mut i = 0;
            while i < bytes.len() {
                let (code, adv) = if f.two_byte {
                    let c = u16::from_be_bytes([bytes[i], *bytes.get(i + 1).unwrap_or(&0)]);
                    (c, 2)
                } else {
                    (bytes[i] as u16, 1)
                };
                match map.get(&code) {
                    Some(s) => out.push_str(s),
                    None => out.push('\u{FFFD}'),
                }
                i += adv;
            }
            return out;
        }
    }
    // 无 ToUnicode：UTF-16BE BOM 或 latin1 近似
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    bytes.iter().map(|&b| b as char).collect()
}

/// 从页面资源里收集字体表（name → FontData）。
fn collect_fonts(doc: &PdfDocument, resources: Option<&Object>) -> HashMap<String, FontData> {
    let mut fonts = HashMap::new();
    let Some(res) = resources else { return fonts };
    let res = doc.resolve(res);
    let Some(res_dict) = res.as_dict() else { return fonts };
    let Some(font_obj) = res_dict.get("Font") else { return fonts };
    let font_dict = doc.resolve(font_obj);
    let Some(entries) = font_dict.as_dict() else { return fonts };
    for (name, ref_obj) in entries {
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
        fonts.insert(name.clone(), FontData { to_unicode, two_byte });
    }
    fonts
}

/// 抽取一段内容流的文本（含行/段落切分）。
pub fn extract_text(doc: &PdfDocument, content: &[u8], resources: Option<&Object>) -> String {
    let fonts = collect_fonts(doc, resources);
    let mut out = String::new();
    let mut operands: Vec<Object> = Vec::new();
    let mut cur_font: Option<String> = None;
    let mut font_size = 12.0f32;
    let mut leading = 0.0f32;
    let mut y = 0.0f32;
    let mut last_y: Option<f32> = None;
    let mut dirty = false;

    let mut push_newline = |out: &mut String, dirty: &mut bool, y: f32, last_y: &mut Option<f32>, font_size: f32, leading: f32| {
        if !*dirty {
            return;
        }
        let gap = last_y.map(|ly| (y - ly).abs()).unwrap_or(0.0);
        let line = leading.max(font_size * 1.2).max(1.0);
        if gap > line * 1.8 {
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
        *last_y = Some(y);
        *dirty = false;
    };

    let mut p = Parser::new(content);
    loop {
        p.skip_ws();
        if p.pos >= p.data.len() {
            break;
        }
        let b = p.data[p.pos];
        let is_value = matches!(
            b,
            b'(' | b'<' | b'/' | b'[' | b'+'
                | b'-' | b'.' | b'0'..=b'9'
        ) || p.data[p.pos..].starts_with(b"<<");
        if is_value {
            match p.parse_object() {
                Ok(obj) => operands.push(obj),
                Err(_) => break,
            }
            continue;
        }
        // 关键字
        let start = p.pos;
        while p.pos < p.data.len() && {
            let c = p.data[p.pos];
            !(c as char).is_ascii_whitespace() && !matches!(c, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'/' | b'%')
        } {
            p.pos += 1;
        }
        if p.pos == start {
            p.pos += 1;
            continue;
        }
        let kw = &p.data[start..p.pos];
        match kw {
            b"Tf" => {
                if operands.len() >= 2 {
                    if let Some(n) = operands[operands.len() - 2].as_name() {
                        cur_font = Some(n.to_string());
                    }
                    if let Some(sz) = operands[operands.len() - 1].as_int() {
                        font_size = sz as f32;
                    }
                }
            }
            b"Td" | b"TD" => {
                if operands.len() >= 2 {
                    let ty = match &operands[operands.len() - 1] {
                        Object::Real(f) => *f as f32,
                        Object::Int(i) => *i as f32,
                        _ => 0.0,
                    };
                    if kw == b"TD" {
                        leading = -ty;
                    }
                    if ty.abs() > 0.01 {
                        y += ty;
                        push_newline(&mut out, &mut dirty, y, &mut last_y, font_size, leading);
                    }
                }
            }
            b"Tm" => {
                if operands.len() >= 6 {
                    let ny = match &operands[operands.len() - 1] {
                        Object::Real(f) => *f as f32,
                        Object::Int(i) => *i as f32,
                        _ => y,
                    };
                    if (ny - y).abs() > 0.01 {
                        y = ny;
                        push_newline(&mut out, &mut dirty, y, &mut last_y, font_size, leading);
                    } else {
                        y = ny;
                    }
                    if last_y.is_none() {
                        last_y = Some(y);
                    }
                }
            }
            b"T*" => {
                y -= leading.max(font_size * 1.2);
                push_newline(&mut out, &mut dirty, y, &mut last_y, font_size, leading);
            }
            b"TL" => {
                if let Some(l) = operands.last().and_then(|o| o.as_int()) {
                    leading = l as f32;
                }
            }
            b"Tj" | b"'" | b"\"" => {
                if kw != b"Tj" {
                    push_newline(&mut out, &mut dirty, y, &mut last_y, font_size, leading);
                }
                if let Some(s) = operands.last().and_then(|o| o.as_str()) {
                    let font = cur_font.as_ref().and_then(|n| fonts.get(n));
                    out.push_str(&decode_string(s, font));
                    dirty = true;
                }
            }
            b"TJ" => {
                if let Some(Object::Arr(items)) = operands.last() {
                    let font = cur_font.as_ref().and_then(|n| fonts.get(n));
                    for it in items {
                        match it {
                            Object::Str(s) => {
                                out.push_str(&decode_string(s, font));
                                dirty = true;
                            }
                            Object::Real(f) if (*f as f32) < -150.0 => out.push(' '),
                            Object::Int(i) if (*i as f32) < -150.0 => out.push(' '),
                            _ => {}
                        }
                    }
                }
            }
            b"BT" => {
                last_y = Some(y);
            }
            b"ET" => {
                push_newline(&mut out, &mut dirty, y, &mut last_y, font_size, leading);
            }
            _ => {}
        }
        operands.clear();
    }
    push_newline(&mut out, &mut dirty, y, &mut last_y, font_size, leading);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tounicode_bfchar_and_bfrange() {
        let cmap = br#"
        /CIDInit /ProcSet findresource begin
        1 begincodespacerange <00> <FF> endcodespacerange
        2 beginbfchar
        <41> <0041>
        <42> <4E2D>
        endbfchar
        1 beginbfrange
        <60> <62> <0061>
        endbfrange
        end
        "#;
        let map = parse_tounicode(cmap);
        assert_eq!(map.get(&0x41).map(String::as_str), Some("A"));
        assert_eq!(map.get(&0x42).map(String::as_str), Some("中"));
        assert_eq!(map.get(&0x60).map(String::as_str), Some("a"));
        assert_eq!(map.get(&0x62).map(String::as_str), Some("c"));
    }

    #[test]
    fn extract_simple_line() {
        let doc = PdfDocument::open(std::sync::Arc::from(
            b"%PDF-1.4\n9 0 obj\n<< >>\nendobj\n".to_vec().into_boxed_slice(),
        ))
        .unwrap();
        let content = b"BT /F1 12 Tf 100 700 Td (Hello) Tj 0 -14 Td (World) Tj ET";
        let text = extract_text(&doc, content, None);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
    }
}
