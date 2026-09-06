//! 行级扫描工具：在 `FileSource` 字节流上完成行定位与逐行读取。
//!
//! 行 = 以 `\n` 结尾的字节序列；`\r\n` 的 `\r` 在取行内容时剔除。
//! 所有函数把「当前已读取到的字节长度」作为文件尾判断依据，读取不足块时不做缓存，
//! 因此文件增长后再次调用即可读到新数据（tail 场景）。

use crate::core::file::FileSource;

/// 从 `from`（应为某行行首）前进 `count` 行（即越过 `count` 个 `\n`），返回新行行首。
/// `count == 0` 原样返回 `Some(from)`。若在数够之前遇到文件尾，返回 `None`。
pub fn advance_nl(src: &mut FileSource, from: u64, count: u64) -> Option<u64> {
    if count == 0 {
        return Some(from);
    }
    let cs = src.chunk_size() as u64;
    let mut need = count;
    let mut off = from;
    loop {
        let chunk_start = (off / cs) * cs;
        let data = src.chunk(off / cs);
        if data.is_empty() {
            return None;
        }
        let rel = (off - chunk_start) as usize;
        for (i, &b) in data.iter().enumerate().skip(rel) {
            if b == b'\n' {
                need -= 1;
                if need == 0 {
                    return Some(chunk_start + i as u64 + 1);
                }
            }
        }
        off = chunk_start + cs;
    }
}

/// 返回 `off`（应为某行行首）的上一行行首。
/// `off == 0` 时返回 `None`（已在文件首行）。若 `0..off` 内没有换行符，返回 `Some(0)`。
pub fn prev_line_start(src: &mut FileSource, off: u64) -> Option<u64> {
    if off == 0 {
        return None;
    }
    let cs = src.chunk_size() as u64;
    let mut cur = off - 1;
    loop {
        let chunk_start = (cur / cs) * cs;
        let data = src.chunk(cur / cs);
        if data.is_empty() {
            return None; // 防御：正常情况下不会发生
        }
        let rel = (cur - chunk_start) as usize;
        for j in (0..=rel).rev() {
            if data[j] == b'\n' {
                let j_abs = chunk_start + j as u64;
                if j_abs + 1 == off {
                    // 紧贴 off 的换行符属于上一行的行尾，需继续向前找
                    continue;
                }
                return Some(j_abs + 1);
            }
        }
        if chunk_start == 0 {
            return Some(0);
        }
        cur = chunk_start - 1;
    }
}

fn append_limited(out: &mut Vec<u8>, seg: &[u8], max: usize, truncated: &mut bool) {
    if *truncated {
        return;
    }
    let room = max.saturating_sub(out.len());
    if seg.len() <= room {
        out.extend_from_slice(seg);
    } else {
        out.extend_from_slice(&seg[..room]);
        *truncated = true;
    }
}

/// 流式逐行读取器：从任意字节位置开始，逐行取出内容，自动跨块。
pub struct LineReader<'a> {
    src: &'a mut FileSource,
    pos: u64,
    eof: bool,
    max_row: usize,
}

impl<'a> LineReader<'a> {
    pub fn new(src: &'a mut FileSource, from: u64, max_row: usize) -> Self {
        Self {
            src,
            pos: from,
            eof: false,
            max_row,
        }
    }

    /// 当前扫描位置（下一待读字节的文件偏移）。
    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// 是否已到达文件尾（以最近一次读取为准；文件增长后需新建 reader 才能看到新数据）。
    pub fn at_eof(&self) -> bool {
        self.eof
    }

    /// 读取下一行，返回 `(行首 offset, 行内容字节[不含 \n 与行尾 \r], 是否截断)`。
    /// 内容超过 `max_row` 字节时截断（截断仍会消费完整行）。文件尾返回 `None`。
    pub fn next_row(&mut self) -> Option<(u64, Vec<u8>, bool)> {
        if self.eof {
            return None;
        }
        let start = self.pos;
        let cs = self.src.chunk_size() as u64;
        let mut content: Vec<u8> = Vec::new();
        let mut truncated = false;
        loop {
            let chunk_start = (self.pos / cs) * cs;
            let data = self.src.chunk(self.pos / cs);
            if data.is_empty() {
                self.eof = true;
                if content.is_empty() {
                    return None; // 该位置无任何数据
                }
                // 文件尾部无换行的最后一行
                return Some((start, content, truncated));
            }
            let rel = (self.pos - chunk_start) as usize;
            match data[rel..].iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let seg = &data[rel..rel + i];
                    append_limited(&mut content, seg, self.max_row, &mut truncated);
                    self.pos = chunk_start + rel as u64 + i as u64 + 1;
                    if content.last() == Some(&b'\r') {
                        content.pop(); // 剔除 CRLF 的 \r
                    }
                    return Some((start, content, truncated));
                }
                None => {
                    // 本块内无换行
                    if rel < data.len() {
                        let seg = &data[rel..];
                        append_limited(&mut content, seg, self.max_row, &mut truncated);
                    }
                    if data.len() < self.src.chunk_size() {
                        // 尾部块（读不满一块）：之后无更多数据（文件增长后新 reader 重读可见）
                        self.eof = true;
                        if content.is_empty() {
                            return None;
                        }
                        // 文件尾部无换行的最后一行
                        return Some((start, content, truncated));
                    }
                    // 满块仍无换行（超长行）：前进到下一块继续
                    self.pos = chunk_start + cs;
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

    fn with_src(content: &[u8], chunk: usize) -> (std::path::PathBuf, FileSource) {
        let p = tmp_path("lines");
        {
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(content).unwrap();
        }
        let src = FileSource::open_with_chunk(&p, chunk).unwrap();
        (p, src)
    }

    fn rows(src: &mut FileSource, from: u64, max: usize) -> Vec<(u64, String)> {
        let mut lr = LineReader::new(src, from, max);
        let mut out = Vec::new();
        while let Some((off, bytes, _t)) = lr.next_row() {
            out.push((off, String::from_utf8_lossy(&bytes).into_owned()));
        }
        out
    }

    #[test]
    fn simple_rows_and_offsets() {
        let (p, mut src) = with_src(b"alpha\nbeta\ngamma", 64);
        let r = rows(&mut src, 0, 4096);
        assert_eq!(
            r,
            vec![
                (0, "alpha".into()),
                (6, "beta".into()),
                (11, "gamma".into())
            ]
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn crlf_stripped() {
        let (p, mut src) = with_src(b"a\r\nb\r\nc", 64);
        let r = rows(&mut src, 0, 4096);
        assert_eq!(r, vec![(0, "a".into()), (3, "b".into()), (6, "c".into())]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn rows_across_chunk_boundary() {
        // chunk=4，行内容会跨块；行首偏移: 0 / 9(=8+1) / 20(=9+10+1)
        let (p, mut src) = with_src(b"12345678\nabcdefghij\nok", 4);
        let r = rows(&mut src, 0, 4096);
        assert_eq!(
            r,
            vec![(0, "12345678".into()), (9, "abcdefghij".into()), (20, "ok".into())]
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn empty_lines_and_empty_file() {
        let (p, mut src) = with_src(b"\n\nab\n", 4);
        let r = rows(&mut src, 0, 4096);
        assert_eq!(
            r,
            vec![(0, "".into()), (1, "".into()), (2, "ab".into())]
        );
        std::fs::remove_file(&p).ok();

        let (p2, mut src2) = with_src(b"", 4);
        assert!(src2.chunk(0).is_empty());
        assert_eq!(rows(&mut src2, 0, 4096), Vec::<(u64, String)>::new());
        std::fs::remove_file(&p2).ok();
    }

    #[test]
    fn long_row_truncated() {
        let (p, mut src) = with_src(b"abcdef\nxyz", 64);
        let mut lr = LineReader::new(&mut src, 0, 3);
        let (off, bytes, trunc) = lr.next_row().unwrap();
        assert_eq!(off, 0);
        assert_eq!(bytes, b"abc");
        assert!(trunc);
        // 完整行已被消费：下一行是 xyz
        let (off2, bytes2, _) = lr.next_row().unwrap();
        assert_eq!(off2, 7);
        assert_eq!(bytes2, b"xyz");
        assert!(lr.next_row().is_none());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn advance_and_prev() {
        let (p, mut src) = with_src(b"a\nbb\nccc\ndddd\n", 64);
        // 行首偏移: 0, 2, 5, 9
        assert_eq!(advance_nl(&mut src, 0, 0), Some(0));
        assert_eq!(advance_nl(&mut src, 0, 1), Some(2));
        assert_eq!(advance_nl(&mut src, 0, 3), Some(9));
        assert_eq!(advance_nl(&mut src, 0, 4), Some(14)); // 越过最后一个 \n 到 EOF 位置
        assert_eq!(advance_nl(&mut src, 0, 5), None); // EOF 前不够

        assert_eq!(prev_line_start(&mut src, 14), Some(9));
        assert_eq!(prev_line_start(&mut src, 9), Some(5));
        assert_eq!(prev_line_start(&mut src, 5), Some(2));
        assert_eq!(prev_line_start(&mut src, 2), Some(0));
        assert_eq!(prev_line_start(&mut src, 0), None);
        // 单行无换行
        let (p2, mut src2) = with_src(b"only", 64);
        assert_eq!(prev_line_start(&mut src2, 0), None);
        std::fs::remove_file(&p).ok();
        std::fs::remove_file(&p2).ok();
    }

    #[test]
    fn prev_skips_touching_newline() {
        // B 行行首 = 4，其紧贴前一个字符是 A 行的 \n(3)，不应把 4 当 B 的上一行行首
        let (p, mut src) = with_src(b"A\nB\nC\n", 64);
        assert_eq!(prev_line_start(&mut src, 4), Some(2));
        assert_eq!(prev_line_start(&mut src, 2), Some(0));
        std::fs::remove_file(&p).ok();
    }
}
