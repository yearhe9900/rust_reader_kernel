//! 布局缓存 payload 接入 rrk-cache 的集成测试（三要素 key + 落盘 + 命中免排版）。

use rrk_model::{
    decode_pages, encode_pages, style_hash, viewport_hash, ChapterContent, StyleSpec, Viewport,
};

#[test]
fn layout_payload_survives_disk_cache() {
    // 造一个最小布局产物（用真实 Layouter 走一遍成本高，这里直接用模型层样例）
    use rrk_model::{Block, Inline, LayoutPage, LineEntry, SemanticPos, StyleFlags, TextRun, TextRange};
    let pages = vec![LayoutPage {
        index: 0,
        chapter: 0,
        start: SemanticPos { chapter: 0, char_offset: 0 },
        end: SemanticPos { chapter: 0, char_offset: 42 },
        lines: vec![LineEntry { start: 0, end: 42, height: 28.0, para_start: true }],
        runs: vec![TextRun {
            line: 0,
            x: 0.0,
            baseline: 22.0,
            font_size: 18.0,
            bold: false,
            italic: false,
            width: 42.0,
            range: (0, 42),
        }],
        images: vec![],
        height: 842.0,
    }];
    let _ = (
        ChapterContent { id: 0, text: "".into(), blocks: vec![], link_ranges: vec![] },
        Block::Divider,
        Inline::Text { range: TextRange::new(0, 0), style: StyleFlags::default() },
    );

    let style = StyleSpec::default();
    let vp = Viewport { width: 390.0, height: 842.0, dpr: 3.0 };
    let key = rrk_cache::CacheKey::new(
        0xABCDEF,
        style_hash(&style),
        viewport_hash(&vp),
        rrk_cache::CacheKind::Layout,
    )
    .with_part(0);

    let dir = std::env::temp_dir().join(format!("rrk-layout-cache-{}", std::process::id()));
    let store = rrk_cache::DiskStore::open(&dir).unwrap();
    store.save(&key, &encode_pages(&pages)).unwrap();

    // 命中 → 解码 → 与原产物逐字段一致（免排版口径）
    let loaded = store.load(&key).expect("缓存命中");
    let back = decode_pages(&loaded).expect("解码");
    assert_eq!(pages, back);

    // 换字号 → 三要素 key 变 → 未命中（精确失效口径）
    let mut style2 = style.clone();
    style2.font_size = 20.0;
    let key2 = rrk_cache::CacheKey::new(
        0xABCDEF,
        style_hash(&style2),
        viewport_hash(&vp),
        rrk_cache::CacheKind::Layout,
    );
    assert!(store.load(&key2).is_none());

    let _ = std::fs::remove_dir_all(&dir);
}
