//! 最小 XML/HTML 标签扫描器（自研，容错，够用即可）：
//! 标签/属性/文本三分 + 实体解码。不构建 DOM 树——EPUB 的 OPF/NCX/nav/XHTML
//! 都是「按标签线性扫」就能覆盖的结构，P0 不引 html5ever。
//!
//! **零拷贝纪律**：token 借用输入（`Token<'a>`）——文本与属性值无实体时是
//! `Cow::Borrowed` 切片，标签/属性名原串已是小写也不再分配；只有真正需要
//! 变换（实体、大写）才落到 `Cow::Owned`。载章峰值内存从「≈2× 正文」降为
//! 「1 份正文 + 少量小分配」。

use std::borrow::Cow;

/// 标签 token（借用输入；变换处才分配）。
#[derive(Debug, Clone)]
pub enum Token<'a> {
    /// 解码后的文本。
    Text(Cow<'a, str>),
    /// 开标签（含属性，属性值已解码）。
    Open { name: Cow<'a, str>, attrs: Vec<(Cow<'a, str>, Cow<'a, str>)>, self_closing: bool },
    /// 闭标签。
    Close(Cow<'a, str>),
}

/// ASCII 小写化：原串无大写则零拷贝借用。
fn lower_cow(s: &str) -> Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(s.to_ascii_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

/// 解码 XML/HTML 实体（常见集合 + 数字实体）；无 `&` 时**零拷贝**借用。
pub fn decode_entities_cow(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|&p| p <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => {
                if let Some(num) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
                    u32::from_str_radix(num, 16).ok().and_then(char::from_u32)
                } else if let Some(num) = entity.strip_prefix('#') {
                    num.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// 解码 XML/HTML 实体（拥有式便捷入口；语义与 [`decode_entities_cow`] 一致）。
pub fn decode_entities(s: &str) -> String {
    decode_entities_cow(s).into_owned()
}

/// 去命名空间前缀：`dc:title` → `title`。
pub fn local_name(name: &str) -> &str {
    name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')
}

/// 连续文本片段并成一份再解码（多片段才分配缓冲；单片段直接切片解码）。
fn flush_pieces<'a>(pieces: &mut Vec<(usize, usize)>, input: &'a str, tokens: &mut Vec<Token<'a>>) {
    match pieces.len() {
        0 => {}
        1 => {
            let (s, e) = pieces[0];
            tokens.push(Token::Text(decode_entities_cow(&input[s..e])));
        }
        _ => {
            let mut buf = String::new();
            for &(s, e) in pieces.iter() {
                buf.push_str(&input[s..e]);
            }
            // 多片段（CDATA 混插）罕见：退回收分配路径（与旧实现同价）
            tokens.push(Token::Text(Cow::Owned(decode_entities(&buf))));
        }
    }
    pieces.clear();
}

/// 把一段标记文本切成 token 流（容错：不闭合也能扫完；token 借用 `input`）。
pub fn tokenize(input: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    // 连续文本片段的 (start, end) 区间——flush 时才并段+解码，避免逐段先拷贝。
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            // 取一段文本
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            if i > start {
                pieces.push((start, i));
            }
            continue;
        }
        // 注释 / CDATA / 声明
        if input[i..].starts_with("<!--") {
            i = input[i..].find("-->").map(|p| i + p + 3).unwrap_or(bytes.len());
            continue;
        }
        if input[i..].starts_with("<![CDATA[") {
            let start = i + 9;
            let end = input[i..].find("]]>").map(|p| i + p).unwrap_or(bytes.len());
            if end > start {
                pieces.push((start, end));
            }
            i = (end + 3).min(bytes.len());
            continue;
        }
        if input[i..].starts_with("<?") || input[i..].starts_with("<!") {
            i = input[i..].find('>').map(|p| i + p + 1).unwrap_or(bytes.len());
            continue;
        }

        // 标签
        let tag_start = i + 1;
        if input[tag_start..].starts_with('/') {
            // 闭标签
            let mut j = tag_start + 1;
            while j < bytes.len() && is_name_char(bytes[j] as char) {
                j += 1;
            }
            let name = lower_cow(&input[tag_start + 1..j]);
            flush_pieces(&mut pieces, input, &mut tokens);
            tokens.push(Token::Close(name));
            i = input[j..].find('>').map(|p| j + p + 1).unwrap_or(bytes.len());
            continue;
        }

        // 开标签：解析名称与属性
        let mut j = tag_start;
        while j < bytes.len() && is_name_char(bytes[j] as char) {
            j += 1;
        }
        if j == tag_start {
            // 尖括号误用当文本
            pieces.push((i, i + 1));
            i += 1;
            continue;
        }
        let name = lower_cow(&input[tag_start..j]);
        let mut attrs = Vec::new();
        let mut self_closing = false;
        let mut k = j;
        while k < bytes.len() {
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if k >= bytes.len() {
                break;
            }
            if bytes[k] == b'>' {
                k += 1;
                break;
            }
            if bytes[k] == b'/' {
                self_closing = true;
                k += 1;
                continue;
            }
            // 属性名
            let ns = k;
            while k < bytes.len() && (is_name_char(bytes[k] as char) || bytes[k] == b'/') {
                if bytes[k] == b'/' {
                    break;
                }
                k += 1;
            }
            let aname = lower_cow(&input[ns..k]);
            // = 值（无实体直接借用输入切片）
            let mut value: &str = "";
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b'=' {
                k += 1;
                while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < bytes.len() && (bytes[k] == b'"' || bytes[k] == b'\'') {
                    let quote = bytes[k];
                    k += 1;
                    let vs = k;
                    while k < bytes.len() && bytes[k] != quote {
                        k += 1;
                    }
                    value = &input[vs..k];
                    k = (k + 1).min(bytes.len());
                } else {
                    let vs = k;
                    while k < bytes.len() && !bytes[k].is_ascii_whitespace() && bytes[k] != b'>' {
                        k += 1;
                    }
                    value = &input[vs..k];
                }
            }
            if !aname.is_empty() {
                attrs.push((aname, decode_entities_cow(value)));
            }
        }
        flush_pieces(&mut pieces, input, &mut tokens);
        tokens.push(Token::Open { name, attrs, self_closing });
        i = k.max(i + 1);
    }
    flush_pieces(&mut pieces, input, &mut tokens);
    tokens
}

/// 取属性（大小写不敏感，值为解码后文本）。
pub fn attr<'a>(attrs: &'a [(Cow<'a, str>, Cow<'a, str>)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name || local_name(k) == name)
        .map(|(_, v)| v.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities() {
        assert_eq!(decode_entities("a&amp;b&#65;&#x42;&nbsp;c"), "a&bAB\u{a0}c");
        assert_eq!(decode_entities("&unknown;"), "&unknown;");
        assert!(matches!(decode_entities_cow("无实体文本"), Cow::Borrowed(_)), "无实体应零拷贝");
        assert!(matches!(decode_entities_cow("a&amp;b"), Cow::Owned(_)), "有实体才分配");
    }

    #[test]
    fn tokenize_basic() {
        let toks = tokenize(r#"<p class="x">he&amp;llo<br/>wo</p>"#);
        match &toks[..] {
            [Token::Open { name, attrs, .. }, Token::Text(t1), Token::Open { name: n2, self_closing: sc, .. }, Token::Text(t2), Token::Close(c)] => {
                assert_eq!(name, "p");
                assert_eq!(attr(attrs, "class"), Some("x"));
                assert_eq!(t1, "he&llo");
                assert_eq!(n2, "br");
                assert!(sc);
                assert_eq!(t2, "wo");
                assert_eq!(c, "p");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn skips_comments_and_decls() {
        let toks = tokenize("<?xml version='1.0'?><!-- note --><a>x</a>");
        assert_eq!(toks.len(), 3);
    }

    /// 无实体文本与小写标签应直接借用输入（零分配路径）。
    #[test]
    fn zero_copy_borrow_fast_path() {
        let html = "<p>正文没有实体</p>";
        let toks = tokenize(html);
        assert!(matches!(&toks[0], Token::Open { name, .. } if matches!(name, Cow::Borrowed(_))));
        assert!(matches!(&toks[1], Token::Text(Cow::Borrowed(_))));
        assert!(matches!(&toks[2], Token::Close(Cow::Borrowed(_))));
    }

    /// 大写标签仍被归一为小写（分配），语义不变。
    #[test]
    fn uppercase_tag_still_lowercased() {
        let toks = tokenize("<P>x</P>");
        assert!(matches!(&toks[0], Token::Open { name, .. } if name == "p"));
        assert!(matches!(&toks[2], Token::Close(name) if name == "p"));
    }

    /// CDATA 与普通文本混排时仍先并段再解码（拼接语义与旧实现一致）。
    #[test]
    fn cdata_joins_adjacent_text() {
        let toks = tokenize("a<![CDATA[<b>]]>c");
        match &toks[..] {
            [Token::Text(t)] => assert_eq!(t, "a<b>c"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
