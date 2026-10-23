// 图像工具冒烟：dart run example/image_smoke.dart <rrk_ffi 动态库> [图片路径]
//
// 验证 issue #8 的两个纯字节能力：
//   1. RrkImage.dimensions：只读图片头取尺寸（不解码像素）
//   2. RrkImage.encodePngFromBgra：BGRA 位图 → PNG 字节（逐行逆序通道）
// 未给图片路径时用内置合成 PNG 头与 4×3 渐变位图自检。
import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';
import 'package:rrk_reader/rrk_reader.dart';

void main(List<String> args) {
  if (args.isEmpty) {
    stderr.writeln('用法: dart run example/image_smoke.dart <库路径> [图片路径]');
    exit(2);
  }
  RrkEngine.init(args[0]);

  if (args.length >= 2) {
    final bytes = File(args[1]).readAsBytesSync();
    final size = RrkImage.dimensions(bytes);
    if (size == null) {
      print('尺寸: 无法识别（${bytes.length} 字节）');
    } else {
      print('尺寸: ${size.width}x${size.height} 格式=${size.format} mime=${size.mime}'
          '（源 ${bytes.length} 字节）');
    }
  }

  // 合成 PNG 头：IHDR 宽 640 高 480
  final png = <int>[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, //
    0, 0, 0, 13, ...'IHDR'.codeUnits,
    0, 0, 2, 128, 0, 0, 1, 224, 8, 6, 0, 0, 0];
  final head = RrkImage.dimensions(Uint8List.fromList(png));
  print('合成 PNG 头: ${head?.width}x${head?.height} (${head?.format})');

  // 4×3 渐变 BGRA → PNG
  const w = 4, h = 3;
  final bgra = Uint8List(w * h * 4);
  for (var y = 0; y < h; y++) {
    for (var x = 0; x < w; x++) {
      final i = (y * w + x) * 4;
      bgra[i] = 0x10 + x; // B
      bgra[i + 1] = 0x20 + y; // G
      bgra[i + 2] = 0x30; // R
      bgra[i + 3] = 0xFF; // A
    }
  }
  final encoded = RrkImage.encodePngFromBgra(bgra, width: w, height: h);
  print('BGRA→PNG: ${encoded.length} 字节，签名=${encoded.sublist(0, 8)}');

  // 编出来的 PNG 必须能被自己的头解析读回（自洽性）
  final back = RrkImage.dimensions(encoded);
  print('回读: ${back?.width}x${back?.height} (${back?.format})');
  if (back == null || back.width != w || back.height != h) {
    stderr.writeln('回读尺寸不符，编码器异常');
    exit(1);
  }

  // 零拷贝入口（宿主已有原生缓冲时）
  final ptr = malloc<Uint8>(bgra.length);
  try {
    ptr.asTypedList(bgra.length).setAll(0, bgra);
    final viaPtr = RrkImage.encodePngFromBgraPtr(ptr, bgra.length, width: w, height: h);
    print('零拷贝入口: ${viaPtr.length} 字节（与便捷入口一致=${viaPtr.length == encoded.length}）');
  } finally {
    malloc.free(ptr);
  }

  print('图像工具全链路 OK');
}
