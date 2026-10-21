//! 最小 XML/HTML 标签扫描器（自研，容错，够用即可）：
//! 标签/属性/文本三分 + 实体解码。不构建 DOM 树——EPUB 的 OPF/NCX/nav/XHTML
//! 都是「按标签线性扫」就能覆盖的结构，P0 不引 html5ever。

/// 标签 token。
#[derive(Debug, Clone)]
pub enum Token {
    /// 解码后的文本。
    Text(String),
    /// 开标签（含属性，属性值已解码）。
    Open { name: String, attrs: Vec<(String, String)>, self_closing: bool },
    /// 闭标签。
    Close(String),
}

/// 解码 XML/HTML 实体（常见集合 + 数字实体）。
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
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
    out
}

/// 去命名空间前缀：`dc:title` → `title`。
pub fn local_name(name: &str) -> &str {
    name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')
}

/// 把一段标记文本切成 token 流（容错：不闭合也能扫完）。
pub fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut text = String::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;

    let flush_text = |text: &mut String, tokens: &mut Vec<Token>| {
        if !text.is_empty() {
            tokens.push(Token::Text(decode_entities(text)));
            text.clear();
        }
    };

    while i < bytes.len() {
        if bytes[i] != b'<' {
            // 取一段文本
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            text.push_str(&input[start..i]);
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
            text.push_str(&input[start..end]);
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
            let name = input[tag_start + 1..j].to_ascii_lowercase();
            flush_text(&mut text, &mut tokens);
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
            text.push('<');
            i += 1;
            continue;
        }
        let name = input[tag_start..j].to_ascii_lowercase();
        let mut attrs: Vec<(String, String)> = Vec::new();
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
            let aname = input[ns..k].to_ascii_lowercase();
            // = 值
            let mut value = String::new();
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
                    value = input[vs..k].to_string();
                    k = (k + 1).min(bytes.len());
                } else {
                    let vs = k;
                    while k < bytes.len() && !bytes[k].is_ascii_whitespace() && bytes[k] != b'>' {
                        k += 1;
                    }
                    value = input[vs..k].to_string();
                }
            }
            if !aname.is_empty() {
                attrs.push((aname, decode_entities(&value)));
            }
        }
        flush_text(&mut text, &mut tokens);
        tokens.push(Token::Open { name, attrs, self_closing });
        i = k.max(i + 1);
    }
    flush_text(&mut text, &mut tokens);
    tokens
}

/// 取属性（大小写不敏感，值为解码后文本）。
pub fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name || local_name(k) == name)
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities() {
        assert_eq!(decode_entities("a&amp;b&#65;&#x42;&nbsp;c"), "a&bAB\u{a0}c");
        assert_eq!(decode_entities("&unknown;"), "&unknown;");
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
}
