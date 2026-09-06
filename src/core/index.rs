//! 稀疏行索引：为「按行号跳转」提供行首字节 offset。
//!
//! 内存模型：checkpoints[k] = 第 (k*interval) 行（0-based）的行首 offset。
//! 索引是 lazy 前缀：只解析到最近一次请求过的行号，避免为整个文件建立全量索引
//! （几 G / 上亿行的日志无法全量建索引）。
//!
//! 扩展用 `advance_nl`（纯换行计数，不产生行内容分配），首次深跳转需要线性扫过
//! 中间字节（依赖 OS page cache，通常亚秒级），之后 checkpoint 让回跳变 O(1) 附近。

use crate::core::file::FileSource;
use crate::core::lines::{LineReader, advance_nl};

pub const DEFAULT_INTERVAL: u64 = 1024;

pub struct LineIndex {
    interval: u64,
    /// checkpoints[k] = 第 (k*interval) 行（0-based）行首 offset；checkpoints[0] == 0
    checkpoints: Vec<u64>,
    /// 已确认的行数（[0, parsed_rows) 范围内的行首偏移均可由索引确定）
    parsed_rows: u64,
    /// 下一个待解析行的行首 offset
    scan_off: u64,
    /// 已扫描到文件尾（按最近一次读取为准）
    at_eof: bool,
}

impl LineIndex {
    pub fn new() -> Self {
        Self::with_interval(DEFAULT_INTERVAL)
    }

    pub fn with_interval(interval: u64) -> Self {
        Self {
            interval: interval.max(1),
            checkpoints: vec![0],
            parsed_rows: 0,
            scan_off: 0,
            at_eof: false,
        }
    }

    /// 已解析行数（0-based 计数；文件未到尾时为「已解析前缀」）。
    pub fn parsed_rows(&self) -> u64 {
        self.parsed_rows
    }

    pub fn at_eof(&self) -> bool {
        self.at_eof
    }

    /// 文件增长后调用：撤销 EOF 缓存，使扩展可继续推进（tail 场景）。
    pub fn invalidate_eof(&mut self) {
        self.at_eof = false;
    }

    /// 返回第 `row0` 行（0-based）的行首 offset；越界（超过文件尾）返回 `None`。
    pub fn row_offset(&mut self, src: &mut FileSource, row0: u64) -> Option<u64> {
        if row0 >= self.parsed_rows {
            self.extend(src, Some(row0));
        }
        if row0 >= self.parsed_rows {
            return None; // EOF 越界
        }
        let ck = (row0 / self.interval) as usize;
        let base = self.checkpoints[ck];
        let step = row0 % self.interval;
        advance_nl(src, base, step)
    }

    /// 扩展到文件尾（用于求总行数 / 跳转末尾）。
    pub fn extend_to_eof(&mut self, src: &mut FileSource) {
        self.extend(src, None);
    }

    /// 扩展索引，直到覆盖第 `up_to` 行（含）或遇到文件尾。
    fn extend(&mut self, src: &mut FileSource, up_to: Option<u64>) {
        while !self.at_eof {
            let cur = self.parsed_rows;
            if let Some(u) = up_to {
                if cur > u {
                    return;
                }
            }
            // 本次推进到 up_to 与下一个 checkpoint 边界中较近者
            let nxt_ck = ((cur / self.interval) + 1) * self.interval;
            let stop = match up_to {
                Some(u) => u.min(nxt_ck - 1),
                None => nxt_ck - 1,
            };
            let need = (stop + 1) - cur; // >= 1
            match advance_nl(src, self.scan_off, need) {
                Some(new_off) => {
                    self.parsed_rows = stop + 1;
                    self.scan_off = new_off;
                    if self.parsed_rows % self.interval == 0 {
                        self.checkpoints.push(new_off);
                    }
                }
                None => {
                    // 剩余不足 need 行：逐行收尾（残余必然不足一个 interval，无新 checkpoint）
                    let mut lr = LineReader::new(src, self.scan_off, 64);
                    while lr.next_row().is_some() {
                        self.parsed_rows += 1;
                    }
                    self.scan_off = lr.pos();
                    self.at_eof = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::file::test_tmp_path as tmp_path;
    use std::io::Write;

    fn content_lines(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..n {
            writeln!(v, "line-{i:05}").unwrap();
        }
        v
    }

    fn open_idx(content: &[u8], interval: u64) -> (std::path::PathBuf, FileSource, LineIndex) {
        let p = tmp_path("index");
        std::fs::write(&p, content).unwrap();
        let src = FileSource::open(&p).unwrap();
        (p, src, LineIndex::with_interval(interval))
    }

    #[test]
    fn row_offset_across_checkpoints() {
        let (p, mut src, mut idx) = open_idx(&content_lines(3000), 64);
        for row in [0u64, 1, 63, 64, 127, 128, 255, 256, 1000, 2000, 2999] {
            let off = idx.row_offset(&mut src, row).unwrap();
            let expect = row * 11; // 每行 "line-NNNNN\n" = 11 字节
            assert_eq!(off, expect, "row {row}");
        }
        // 越界
        assert!(idx.row_offset(&mut src, 3000).is_none());
        assert!(idx.row_offset(&mut src, 99999).is_none());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn checkpoints_lazy_growth() {
        let (p, mut src, mut idx) = open_idx(&content_lines(300), 64);
        assert_eq!(idx.parsed_rows(), 0);
        idx.row_offset(&mut src, 100).unwrap();
        assert_eq!(idx.parsed_rows(), 101);
        // 只有 0、64 两个 checkpoint 块之外… parsed 101 → checkpoints = [0, 64行行首]
        assert_eq!(idx.checkpoints.len(), 2);
        idx.extend_to_eof(&mut src);
        assert_eq!(idx.parsed_rows(), 300);
        assert_eq!(idx.checkpoints.len(), 5); // 64,128,192,256 边界
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn eof_without_trailing_newline() {
        let (p, mut src, mut idx) = open_idx(b"a\nb\nc", 4);
        idx.extend_to_eof(&mut src);
        assert_eq!(idx.parsed_rows(), 3);
        assert!(idx.at_eof());
        assert_eq!(idx.row_offset(&mut src, 2), Some(4));
        assert!(idx.row_offset(&mut src, 3).is_none());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn empty_file() {
        let (p, mut src, mut idx) = open_idx(b"", 4);
        assert!(idx.row_offset(&mut src, 0).is_none());
        idx.extend_to_eof(&mut src);
        assert_eq!(idx.parsed_rows(), 0);
        assert!(idx.at_eof());
        std::fs::remove_file(&p).ok();
    }
}
