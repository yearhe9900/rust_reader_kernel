# 金样测试字体（RRKGoldenSans.otf）

排版金样测试（`../golden_layout.rs`）专用的**确定性字体**：排版结果不依赖
宿主系统字体栈，跨平台逐位可复现，金样才能做跨版本回归真相。

## 出处与许可

- **上游**：[Noto Sans SC](https://github.com/notofonts/noto-cjk)（`Sans/SubsetOTF/SC/NotoSansSC-Regular.otf`）
- **许可**：SIL Open Font License 1.1（版权与许可条款保留在字体 name 表
  ID 0/13/14 中，随本目录分发）
- **修改**：按 `charset.txt`（金样测试源码字符集 + 缩进特殊空格 U+2002/U+3000）
  子集化（406 字符，73 KB），并按 OFL 1.1 第 3 条改名——修改版不得使用保留
  字体名 "Noto"，故字体族名改为 **RRK Golden Sans**（name ID 1/2/4/6/16/17
  与 CFF 内部命名同步改写，商标条目移除）

## 再生成（上游更新 / 测试文本扩充后）

```sh
# 1. 字符集（从金样测试源码提取 + 缩进空格）
python -c "
import sys
chars = set(open('crates/rrk-layout/tests/golden_layout.rs', encoding='utf-8').read())
chars |= {'\u2002', '\u3000'}
chars = {c for c in chars if c.isprintable() or c in ('\u2002', '\u3000')}
out = ''.join(sorted(chars))
open('crates/rrk-layout/tests/fonts/charset.txt', 'w', encoding='utf-8').write(out)
"

# 2. 子集化（需要 fonttools）
python -m fontTools.subset NotoSansSC-Regular.otf \
  --text-file=crates/rrk-layout/tests/fonts/charset.txt \
  --output-file=crates/rrk-layout/tests/fonts/RRKGoldenSans.otf \
  --no-hinting --name-IDs='*' --name-legacy --name-languages='*'

# 3. 改名（OFL 保留名条款；name ID 1/2/3/4/6/16/17 + CFF 内部命名 → RRK Golden Sans，
#    移除商标条目，保留版权/许可证条目）——见 git 历史中的改名脚本或按上文口径改写
```

## 注意

- 测试文本新增字符后必须重新子集化，否则缺字形会以 tofu 度量排版，
  金样会「看起来绿了但字形是错的」——`golden_layout.rs` 的族名断言只验证
  注册成功，缺字形不会自动报错，靠 charset 覆盖保证；
- 改字体 = 改排版真相：金样必须整体再生成并 review diff。
