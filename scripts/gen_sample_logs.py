#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成多编码的中文日志样例，用于测试 LogViewer。

用法:
    python scripts/gen_sample_logs.py                    # 默认 100000 行到项目根目录
    python scripts/gen_sample_logs.py --lines 200000     # 指定行数
    python scripts/gen_sample_logs.py --out D:/tmp       # 指定输出目录

产物（每行含中文 + 可搜索关键字，与 sample.log 同构）:
    sample_utf8.log        UTF-8（无 BOM）
    sample_gbk.log         GBK（Windows 中文日志常见）
    sample_gb2312.log      GB2312
    sample_utf16le.log     UTF-16 LE（带 BOM FF FE）
    sample_utf16be.log     UTF-16 BE（带 BOM FE FF）

内容特征:
    - 每 5000 行出现 [test_str] for test
    - 每 997 行出现 [ERROR]（含中文“连接/超时/重试”，可测中文关键字搜索）
    - 第 70000 行是一条超长行（测自动换行 / 水平滚动）
"""
import argparse
import os
import sys


def make_rows(n: int):
    long_line = "ERROR long-line " + ("数据报文很长-" * 300) + " seq=70000"
    rows = []
    for i in range(n):
        if i == 70000:
            rows.append(long_line)
        elif i % 5000 == 0:
            rows.append(f"{i:06d} [WARN] [test_str] for test 心跳 seq={i}")
        elif i % 997 == 0:
            rows.append(f"{i:06d} [ERROR] 连接 10.0.0.{i % 250}:8080 超时 重试={i % 5}")
        else:
            rows.append(
                f"{i:06d} [INFO] 请求 id={i % 1000} 用时 {(i * 7) % 97}ms /api/items?page={i // 1000}"
            )
    return "\n".join(rows)


def write_enc(path: str, text: str, enc: str, bom: bytes = b""):
    body = text.encode(enc)
    with open(path, "wb") as f:
        f.write(bom)
        f.write(body)


def main():
    ap = argparse.ArgumentParser(description="Generate sample logs in multiple encodings")
    ap.add_argument("--lines", type=int, default=100000)
    ap.add_argument("--out", default=".")
    args = ap.parse_args()
    if args.lines < 3:
        print("lines 至少 3")
        sys.exit(2)
    out = args.out
    os.makedirs(out, exist_ok=True)
    text = make_rows(args.lines)
    targets = [
        ("sample_utf8.log", "utf-8", b""),
        ("sample_gbk.log", "gbk", b""),
        ("sample_gb2312.log", "gb2312", b""),
        ("sample_utf16le.log", "utf-16-le", b"\xff\xfe"),
        ("sample_utf16be.log", "utf-16-be", b"\xfe\xff"),
    ]
    for name, enc, bom in targets:
        p = os.path.join(out, name)
        write_enc(p, text, enc, bom)
        size = os.path.getsize(p)
        print(f"{name}: {size / 1e6:.2f} MB")


if __name__ == "__main__":
    main()
