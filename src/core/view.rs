//! 文件视口状态：把大文件变成「带光标的可滚动行视图」。
//!
//! 模型：视口 = 顶行 top_row0 + 光标行 cursor_row0（须在视口内）。
//! 移动分两类（less 语义）：
//! - `move_cursor`：光标 ±n，触视口边缘时窗口跟随滚动；
//! - `scroll_window`：窗口整体平移（f/b/d/u/Page），光标相对屏幕位置不变。
//! 渲染时 `fill_to(need)` 从顶行行首（经稀疏索引定位）重建视口缓存。
//! 文本解码 M1 为 UTF-8 lossy（编码探测在 M3 引入，解码集中在 `decode`）。

use std::io;
use std::path::{Path, PathBuf};

use encoding_rs::{Encoding, GBK, UTF_8};

use crate::core::file::FileSource;
use crate::core::index::LineIndex;
use crate::core::lines::LineReader;

pub const DEFAULT_MAX_ROW: usize = 4096;

#[derive(Debug, Clone)]
pub struct ViewRow {
    /// 行首字节 offset
    pub offset: u64,
    /// 已解码文本（不含换行）
    pub text: String,
    /// 超长被截断（显示时给出提示标记）
    pub truncated: bool,
}

pub struct FileView {
    src: FileSource,
    path: PathBuf,
    index: LineIndex,
    /// 内容编码（探测：UTF-8 优先，失败回退 GBK）
    encoding: &'static Encoding,
    /// 视口首行行号（0-based）
    top_row0: u64,
    /// 光标行（0-based，须位于视口内；less 风格）
    cursor_row0: u64,
    /// 视口高度（最近一次 fill_to 的 need）
    vh: usize,
    /// 视口行缓存（自 top_row0 起连续；每次 fill_to 重建）
    rows: Vec<ViewRow>,
    max_row_bytes: usize,
    /// 最近一次 fill 后是否已读尽当前文件长度（tail 追加后可能再变）
    at_end: bool,
    /// 最近一次检测到的文件长度（用于 tail 增长判定）
    last_len: u64,
}

/// 探测文件文本编码：UTF-8 可严格解码则 UTF-8，否则回退 GBK（Windows 中文日志常见）。
fn detect_encoding(path: &Path) -> &'static Encoding {
    let mut head = [0u8; 4096];
    let n = std::fs::File::open(path)
        .ok()
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head).ok())
        .unwrap_or(0);
    let head = &head[..n];
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return UTF_8; // UTF-8 BOM
    }
    match std::str::from_utf8(head) {
        Ok(_) => UTF_8,
        // 仅"末尾不完整序列"（error_len == None，4096 字节截断在字符中间）仍视为 UTF-8；
        // 字节流中间出现非法序列才回退 GBK
        Err(e) if e.error_len().is_none() => UTF_8,
        Err(_) => GBK,
    }
}

impl FileView {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::open_with(path, DEFAULT_MAX_ROW)
    }

    pub fn open_with<P: AsRef<Path>>(path: P, max_row_bytes: usize) -> io::Result<Self> {
        let src = FileSource::open(path.as_ref())?;
        let last_len = src.len();
        let encoding = detect_encoding(path.as_ref());
        Ok(Self {
            src,
            path: path.as_ref().to_path_buf(),
            index: LineIndex::new(),
            encoding,
            top_row0: 0,
            cursor_row0: 0,
            vh: 24,
            rows: Vec::new(),
            max_row_bytes: max_row_bytes.max(16),
            at_end: false,
            last_len,
        })
    }

    pub fn file_path(&self) -> &Path {
        &self.path
    }

    /// 行内容解码入口：按探测到的编码解码（UTF-8 lossy 或 GBK lossy）。
    fn decode_bytes(&self, bytes: &[u8]) -> String {
        if self.encoding == GBK {
            let (cow, _, _) = GBK.decode(bytes);
            cow.into_owned()
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        }
    }

    /// 给 rg 用的编码标签（小写）。
    pub fn encoding_label(&self) -> String {
        if self.encoding == GBK {
            "gbk".to_string()
        } else {
            "utf-8".to_string()
        }
    }

    pub fn file_len(&self) -> u64 {
        self.src.len()
    }

    /// 刷新并返回最新文件长度（tail 检测用）。
    pub fn poll_len(&mut self) -> u64 {
        self.refresh_tail();
        self.src.len()
    }

    pub fn max_row_bytes(&self) -> usize {
        self.max_row_bytes
    }

    /// 视口首行的 1-based 行号（空文件时为 1）。
    pub fn top_line1(&self) -> u64 {
        self.top_row0 + 1
    }

    /// 光标行的 1-based 行号。
    pub fn cursor_line1(&self) -> u64 {
        self.cursor_row0 + 1
    }

    pub fn rows(&self) -> &[ViewRow] {
        &self.rows
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 视口末行是否为文件当前末尾（供状态栏/tail 判定）。
    pub fn at_bottom(&self) -> bool {
        self.at_end
    }

    /// 探测 row0 行是否存在（越界返回 false）；不移动。
    fn row_exists(&mut self, row0: u64) -> bool {
        self.index.row_offset(&mut self.src, row0).is_some()
    }

    /// 光标下/上移动 delta 行（触视口边缘时窗口跟随，光标保持在视口内）。
    pub fn move_cursor(&mut self, delta: i64) {
        self.refresh_tail();
        if delta == 0 {
            return;
        }
        let new = (self.cursor_row0 as i64 + delta).max(0) as u64;
        let mut target = new;
        if !self.row_exists(new) {
            self.index.extend_to_eof(&mut self.src);
            let total = self.index.parsed_rows();
            if total == 0 {
                return;
            }
            target = total - 1;
        }
        self.cursor_row0 = target;
        let vh = self.vh.max(1) as u64;
        if self.cursor_row0 < self.top_row0 {
            self.top_row0 = self.cursor_row0;
        } else if self.cursor_row0 >= self.top_row0 + vh {
            self.top_row0 = self.cursor_row0 + 1 - vh;
        }
        self.rows.clear();
    }

    /// 窗口整体平移 delta 行（f/b/d/u/Page；光标相对屏幕位置不变，平移到边界自动钳制）。
    pub fn scroll_window(&mut self, delta: i64) {
        self.refresh_tail();
        if delta == 0 {
            return;
        }
        // 光标已在文件最后一行时，不再向下滚（与子窗一致：底部已显示）
        if delta > 0
            && self.index.at_eof()
            && self.cursor_row0 + 1 >= self.index.parsed_rows()
        {
            return;
        }
        let new_top = self.top_row0 as i64 + delta;
        let top = if new_top < 0 { 0 } else { new_top as u64 };
        if !self.row_exists(top) {
            // 底部方向越界：滚到能完整显示一行处
            self.index.extend_to_eof(&mut self.src);
            let total = self.index.parsed_rows();
            if total == 0 {
                return;
            }
            let vh = self.vh.max(1) as u64;
            self.top_row0 = if total > vh { total - vh } else { 0 };
            self.cursor_row0 = total - 1;
            self.rows.clear();
            return;
        }
        self.top_row0 = top;
        let vh = self.vh.max(1) as u64;
        let max_in_view = top + vh - 1;
        let cursor = (self.cursor_row0 as i64 + delta)
            .max(top as i64)
            .min(max_in_view as i64) as u64;
        self.cursor_row0 = cursor;
        self.rows.clear();
    }

    /// 跳到 1-based 行号 line1：光标落该行，视口使该行位于约 1/3 高度。
    /// 返回 false 表示越界/文件空。
    pub fn goto_line1(&mut self, line1: u64) -> bool {
        if line1 < 1 {
            return false;
        }
        let row0 = line1 - 1;
        if !self.row_exists(row0) {
            return false;
        }
        let vh = self.vh.max(1) as u64;
        self.top_row0 = row0.saturating_sub(vh / 3);
        self.cursor_row0 = row0;
        self.rows.clear();
        true
    }

    pub fn scroll_to_top(&mut self) {
        self.top_row0 = 0;
        self.cursor_row0 = 0;
        self.rows.clear();
    }

    /// 滚动到文件末尾（需要先扩展索引以得知总行数）。
    pub fn scroll_to_bottom(&mut self) {
        self.refresh_tail();
        self.index.extend_to_eof(&mut self.src);
        let total = self.index.parsed_rows();
        if total == 0 {
            self.top_row0 = 0;
            self.cursor_row0 = 0;
            self.rows.clear();
            return;
        }
        let vh = self.vh.max(1) as u64;
        self.cursor_row0 = total - 1;
        self.top_row0 = if total > vh { total - vh } else { 0 };
        self.rows.clear();
    }

    /// 当前文件总行数（未扫到文件尾时为 None）。
    pub fn total_rows(&mut self) -> Option<u64> {
        self.refresh_tail();
        if !self.index.at_eof() {
            self.index.extend_to_eof(&mut self.src);
        }
        if self.index.at_eof() {
            Some(self.index.parsed_rows())
        } else {
            None
        }
    }

    /// 已知总行数（仅在已扫描到过文件尾时 Some，不做新扫描；tail 增长后可能滞后）。
    pub fn known_total(&self) -> Option<u64> {
        if self.index.at_eof() {
            Some(self.index.parsed_rows())
        } else {
            None
        }
    }

    /// 检测 tail 增长：文件变长且索引此前已到 EOF 时，使其重新可扩展。
    fn refresh_tail(&mut self) {
        let len = self.src.refresh_len();
        if len != self.last_len {
            self.last_len = len;
            if self.index.at_eof() {
                self.index.invalidate_eof();
            }
        }
    }

    /// 确保光标在视口内（vh 变化或边界移动后调用）。
    fn ensure_cursor_visible(&mut self) {
        let vh = self.vh.max(1) as u64;
        if self.cursor_row0 < self.top_row0 {
            self.top_row0 = self.cursor_row0;
            self.rows.clear();
        } else if self.cursor_row0 >= self.top_row0 + vh {
            self.top_row0 = self.cursor_row0 + 1 - vh;
            self.rows.clear();
        }
    }

    /// 确保视口有至少 `need` 行（不足则从文件续读；文件尾则停）。
    /// 视口缓存每次重建，因此调用方应在 top 变化后调用本方法。
    pub fn fill_to(&mut self, need: usize) {
        self.refresh_tail();
        self.vh = need.max(1);
        self.ensure_cursor_visible();
        if self.rows.len() >= need && !self.rows.is_empty() {
            return;
        }
        self.rows.clear();
        let off = match self.index.row_offset(&mut self.src, self.top_row0) {
            Some(o) => o,
            None => {
                self.at_end = true;
                return;
            }
        };
        let maxb = self.max_row_bytes;
        let enc = self.encoding;
        let mut lr = LineReader::new(&mut self.src, off, maxb);
        while self.rows.len() < need {
            match lr.next_row() {
                Some((off2, bytes, trunc)) => {
                    let text = if enc == GBK {
                        let (cow, _, _) = GBK.decode(&bytes);
                        cow.into_owned()
                    } else {
                        String::from_utf8_lossy(&bytes).into_owned()
                    };
                    self.rows.push(ViewRow {
                        offset: off2,
                        text,
                        truncated: trunc,
                    });
                }
                None => break,
            }
        }
        self.at_end = self.rows.len() < need || lr.pos() >= self.src.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::file::test_tmp_path as tmp_path;
    use std::io::Write;

    fn make_file(n: usize) -> std::path::PathBuf {
        let p = tmp_path("view");
        let mut f = std::fs::File::create(&p).unwrap();
        for i in 0..n {
            writeln!(f, "row-{i:05}").unwrap();
        }
        p
    }

    /// 视口首行文本（第 0 行）
    fn top_text(v: &FileView) -> &str {
        &v.rows()[0].text
    }

    #[test]
    fn open_fill_and_rows() {
        let p = make_file(10);
        let mut v = FileView::open(&p).unwrap();
        v.fill_to(5);
        assert_eq!(v.row_count(), 5);
        assert_eq!(v.top_line1(), 1);
        assert_eq!(v.cursor_line1(), 1);
        assert_eq!(v.rows()[0].text, "row-00000");
        assert_eq!(v.rows()[4].text, "row-00004");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn cursor_and_window_move() {
        let p = make_file(100);
        let mut v = FileView::open(&p).unwrap();
        v.fill_to(10);
        // 光标移动：前 9 行内不滚动
        v.move_cursor(5);
        assert_eq!(v.cursor_line1(), 6);
        assert_eq!(v.top_line1(), 1);
        // 光标越过视口底部（10 行视口）：窗口跟一行
        v.move_cursor(4);
        assert_eq!(v.cursor_line1(), 10);
        v.move_cursor(1);
        assert_eq!(v.cursor_line1(), 11);
        assert_eq!(v.top_line1(), 2);
        v.fill_to(10);
        assert_eq!(v.rows()[0].text, "row-00001");
        // 向上回移：光标回到视口中部，窗口保持（光标未触顶不回滚）
        v.move_cursor(-2);
        assert_eq!(v.cursor_line1(), 9);
        assert_eq!(v.top_line1(), 2);
        v.fill_to(10);
        assert_eq!(v.rows()[0].text, "row-00001");
        // 光标继续上移触顶：窗口回滚
        v.move_cursor(-8);
        assert_eq!(v.cursor_line1(), 1);
        assert_eq!(v.top_line1(), 1);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn window_page_move() {
        let p = make_file(100);
        let mut v = FileView::open(&p).unwrap();
        v.fill_to(10);
        v.scroll_window(10);
        assert_eq!(v.top_line1(), 11);
        assert_eq!(v.cursor_line1(), 11);
        v.fill_to(10);
        assert_eq!(top_text(&v), "row-00010");
        v.scroll_window(-5);
        assert_eq!(v.top_line1(), 6);
        assert_eq!(v.cursor_line1(), 6);
        // 顶部边界
        v.scroll_window(-100);
        assert_eq!(v.top_line1(), 1);
        // 底部边界
        v.scroll_window(1000);
        v.fill_to(10);
        assert_eq!(v.cursor_line1(), 100);
        assert_eq!(v.rows().last().unwrap().text, "row-00099");
        assert!(v.at_bottom());
        // 光标已在最后一行：继续下滚不应移动窗口（子窗一致行为）
        let top_before = v.top_line1();
        v.scroll_window(10);
        assert_eq!(v.top_line1(), top_before);
        assert_eq!(v.cursor_line1(), 100);
        // 上滚仍可
        v.scroll_window(-10);
        assert_eq!(v.top_line1(), top_before - 10);
        std::fs::remove_file(&p).ok();
    }

    /// 回归：滚到底后先上滚再下滚，必须能回到末尾（旧模型 bug）
    #[test]
    fn bottom_then_up_then_down() {
        let p = make_file(5000);
        let mut v = FileView::open(&p).unwrap();
        v.scroll_to_bottom();
        v.fill_to(10);
        assert!(v.at_bottom());
        assert_eq!(v.cursor_line1(), 5000);
        assert_eq!(v.rows().last().unwrap().text, "row-04999");
        // 上滚 50 行
        v.scroll_window(-50);
        v.fill_to(10);
        assert!(v.cursor_line1() < 5000);
        // 再下滚必须能回到底部
        v.scroll_window(100);
        v.fill_to(10);
        assert_eq!(v.rows().last().unwrap().text, "row-04999");
        assert_eq!(v.cursor_line1(), 5000);
        assert!(v.at_bottom());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn goto_across_checkpoints() {
        let p = make_file(5000);
        let mut v = FileView::open(&p).unwrap();
        assert!(v.goto_line1(4096));
        assert_eq!(v.cursor_line1(), 4096);
        v.fill_to(20);
        assert!(v.rows().iter().any(|r| r.text == "row-04095"));
        assert!(v.goto_line1(5000));
        v.fill_to(20);
        assert_eq!(v.cursor_line1(), 5000);
        assert!(v.rows().iter().any(|r| r.text == "row-04999"));
        // 越界
        assert!(!v.goto_line1(5001));
        assert!(!v.goto_line1(0));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn scroll_to_bottom_and_total() {
        let p = make_file(5000);
        let mut v = FileView::open(&p).unwrap();
        v.scroll_to_bottom();
        assert_eq!(v.cursor_line1(), 5000);
        assert_eq!(v.total_rows(), Some(5000));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn tail_visible_after_growth() {
        let p = tmp_path("viewtail");
        {
            let mut f = std::fs::File::create(&p).unwrap();
            writeln!(f, "one").unwrap();
        }
        let mut v = FileView::open(&p).unwrap();
        v.fill_to(10);
        assert_eq!(v.row_count(), 1);
        assert!(v.at_bottom());
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            writeln!(f, "two").unwrap();
            writeln!(f, "three").unwrap();
        }
        v.scroll_to_bottom();
        v.fill_to(10);
        assert_eq!(v.cursor_line1(), 3);
        assert_eq!(v.rows().last().unwrap().text, "three");
        assert_eq!(v.total_rows(), Some(3));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn empty_file_view() {
        let p = tmp_path("viewempty");
        std::fs::write(&p, b"").unwrap();
        let mut v = FileView::open(&p).unwrap();
        v.fill_to(10);
        assert_eq!(v.row_count(), 0);
        assert_eq!(v.total_rows(), Some(0));
        assert!(!v.goto_line1(1));
        std::fs::remove_file(&p).ok();
    }
}
