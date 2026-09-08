#!/usr/bin/env python3
# -*- coding: utf-8 -*-

'''
# 生成 5GB 的 UTF-8 文件
python gen_text_file.py -o large_utf8.txt -s 5G -e utf-8

# 生成 500MB 的 GBK 文件
python gen_text_file.py -o large_gbk.txt -s 500M -e gbk

# 生成 100MB 的 GB2312 文件
python gen_text_file.py -o large_gb2312.txt -s 100M -e gb2312

# 支持小单位，如 1024K 表示 1MB
python gen_text_file.py -o test.txt -s 1024K
'''

import argparse
import os
import sys

# 支持的编码列表
SUPPORTED_ENCODINGS = ['utf-8', 'gbk', 'gb2312']

def parse_size(size_str):
    """解析大小字符串，返回字节数，支持 B/K/M/G 后缀（大小写不敏感）"""
    size_str = size_str.strip().upper()
    if size_str.endswith('B'):
        size_str = size_str[:-1]
    if size_str.endswith('K'):
        return int(float(size_str[:-1]) * 1024)
    elif size_str.endswith('M'):
        return int(float(size_str[:-1]) * 1024 * 1024)
    elif size_str.endswith('G'):
        return int(float(size_str[:-1]) * 1024 * 1024 * 1024)
    else:
        return int(size_str)  # 默认视为字节

def generate_file(filename, size_bytes, encoding='utf-8'):
    """
    生成指定编码和字节数的文本文件。
    内容由一段包含中英文、数字、标点的固定样本重复填充。
    """
    # 样本字符串（包含 GB2312 支持的常用中文字符）
    sample_text = (
        "你好，世界！Hello, World! 1234567890\n"
        "这是一个测试文件，用于生成指定编码和大小的文本。\n"
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz\n"
        "中文标点：，。！？；：（）【】“”‘’\n"
        "数字和符号：!@#$%^&*()_+-=[]{}|;:'\",.<>/?\n"
    )
    try:
        sample_bytes = sample_text.encode(encoding)
    except UnicodeEncodeError as e:
        print(f"错误：样本字符无法用 {encoding} 编码。请修改样本字符串。")
        print(f"具体错误: {e}")
        return False

    sample_len = len(sample_bytes)
    if sample_len == 0:
        print("样本编码后为空，请检查编码。")
        return False

    # 分块写入，每块 1MB，以减少系统调用次数
    CHUNK_SIZE = 1024 * 1024
    # 计算整个样本重复次数，构造一个大的块，避免频繁 write
    # 我们可以每次写入一个由多个样本组成的大块，但要注意剩余大小
    # 更简单：直接用循环写入样本块，并显示进度

    print(f"开始生成文件：{filename}")
    print(f"目标大小：{size_bytes / (1024*1024):.2f} MB")
    print(f"编码：{encoding}")
    print("写入中...")

    with open(filename, 'wb') as f:
        written = 0
        # 先写入完整样本块（每次一个样本）
        while written < size_bytes:
            remaining = size_bytes - written
            if remaining >= sample_len:
                f.write(sample_bytes)
                written += sample_len
            else:
                # 最后一块不足样本长度，截断写入
                f.write(sample_bytes[:remaining])
                written += remaining

            # 每写入 100MB 显示一次进度
            if written % (100 * 1024 * 1024) < sample_len:
                progress = written / size_bytes * 100
                print(f"  进度：{progress:.1f}% ({written / (1024*1024):.1f} MB)", end='\r')
        print()  # 换行
    print("文件生成完成！")
    return True

def main():
    parser = argparse.ArgumentParser(
        description="生成指定编码和大小（最大 5GB 或更大）的文本文件。"
    )
    parser.add_argument(
        '-o', '--output',
        required=True,
        help="输出文件名"
    )
    parser.add_argument(
        '-s', '--size',
        required=True,
        help="文件大小，支持 B/K/M/G 后缀，例如 500M, 2G, 1024K (不区分大小写)"
    )
    parser.add_argument(
        '-e', '--encoding',
        default='utf-8',
        choices=SUPPORTED_ENCODINGS,
        help=f"文件编码，可选: {', '.join(SUPPORTED_ENCODINGS)}，默认 utf-8"
    )

    args = parser.parse_args()

    # 解析大小
    try:
        size_bytes = parse_size(args.size)
    except ValueError:
        print("错误：无法解析大小参数，请使用数字加 B/K/M/G 后缀，例如 500M, 2G")
        sys.exit(1)

    if size_bytes <= 0:
        print("错误：大小必须大于 0")
        sys.exit(1)

    # 检查磁盘空间（简单提示）
    # 不实际检查，因为可能无法获取剩余空间

    # 调用生成函数
    success = generate_file(args.output, size_bytes, args.encoding)
    sys.exit(0 if success else 1)

if __name__ == "__main__":
    main()
