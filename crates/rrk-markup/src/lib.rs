//! rrk-markup：EPUB / MOBI 共用的标记处理层。
//!
//! - [`markup`]：最小 XML/HTML 标签扫描器（容错，不建 DOM）；
//! - [`clean`]：XHTML → `Block` 清洗管线（标签白名单 + CSS 最小集，方案 §3.3-4）。

pub mod clean;
pub mod markup;

pub use clean::{clean_xhtml, CleanResult};
pub use markup::{attr, decode_entities, decode_entities_cow, local_name, tokenize, Token};
