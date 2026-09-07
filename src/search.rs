//! 内嵌搜索：不再依赖外部 rg 进程。
//!
//! - UTF-8 文件：用 grep-searcher + grep-regex（与 ripgrep 同源引擎，字节级流式搜索）；
//! - 非 UTF-8（GB18030 等，兼容 GBK / GB2312）：`TranscodingReader` 增量转码为 UTF-8
//!   后再走同一引擎，编码正确且仍享受引擎级速度与流式/取消；
//! - 取消：后台线程 + `AtomicBool` 标志（UI 侧设置后，引擎在下一个匹配处停止）。
//!
//! 对外接口保持与旧版一致：`RgSearch::start(..)` 返回带 channel 的句柄，
//! 通过 `RgMsg::{Match,Done,Error}` 流式上报匹配。

use std::io;
use std::io::Read as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};

#[derive(Debug, Clone)]
pub struct Match {
    /// 1-based 行号（与 rg 一致）
    pub line_no: u64,
    /// 该行文本（已按文件编码解码）
    pub text: String,
}

#[derive(Debug)]
pub enum RgMsg {
    Match(Match),
    /// 搜索结束（含被取消）。count 为已收到的匹配数。
    Done { count: u64, cancelled: bool },
    Error(String),
}

/// 大小写匹配模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    Smart,
    Insensitive,
    Sensitive,
}

impl Case {
    pub fn label(&self) -> &'static str {
        match self {
            Case::Smart => "智能",
            Case::Insensitive => "忽略",
            Case::Sensitive => "区分",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Case::Smart => Case::Insensitive,
            Case::Insensitive => Case::Sensitive,
            Case::Sensitive => Case::Smart,
        }
    }
}

/// 默认 rg 大小写规则（smart-case）的镜像：pattern 不含大写字母时忽略大小写。
/// 用于让可视区行内高亮与搜索一致。
pub fn smart_case_regex(pattern: &str) -> String {
    let has_upper = pattern.chars().any(|c| c.is_uppercase());
    if has_upper || pattern.starts_with("(?i)") {
        pattern.to_string()
    } else {
        format!("(?i){pattern}")
    }
}

/// 依据大小写模式与整词选项生成正则源码。
fn pattern_source(pattern: &str, case: Case, whole_word: bool) -> String {
    let base = match case {
        Case::Smart => smart_case_regex(pattern),
        Case::Insensitive => {
            if pattern.starts_with("(?i)") {
                pattern.to_string()
            } else {
                format!("(?i){pattern}")
            }
        }
        Case::Sensitive => pattern.to_string(),
    };
    if whole_word {
        format!(r"\b(?:{base})\b")
    } else {
        base
    }
}

/// 依选项编译行内高亮/过滤/GBK 回退用的正则。
pub fn compile_regex(pattern: &str, case: Case, whole_word: bool) -> Result<regex::Regex, String> {
    regex::Regex::new(&pattern_source(pattern, case, whole_word)).map_err(|e| e.to_string())
}

/// 把 Case 应用到 grep-regex 的匹配器。
fn matcher_for(pattern: &str, case: Case, word: bool) -> Result<grep_regex::RegexMatcher, String> {
    let mut b = RegexMatcherBuilder::new();
    match case {
        Case::Smart => {
            b.case_smart(true);
        }
        Case::Insensitive => {
            b.case_insensitive(true);
            b.case_smart(false);
        }
        Case::Sensitive => {
            b.case_insensitive(false);
            b.case_smart(false);
        }
    }
    b.word(word);
    b.build(pattern).map_err(|e| e.to_string())
}

pub struct RgSearch {
    rx: Receiver<RgMsg>,
    cancel: Arc<AtomicBool>,
}

/// 后台 sink：把命中的行经 channel 上报；被取消时返回 false 使引擎停止。
struct ReportSink {
    tx: Sender<RgMsg>,
    cancel: Arc<AtomicBool>,
    count: u64,
}

impl ReportSink {
    fn report(&mut self, line_no: u64, text: &[u8]) {
        self.count += 1;
        let mut text = String::from_utf8_lossy(text).into_owned();
        while text.ends_with('\n') || text.ends_with('\r') {
            text.pop();
        }
        let _ = self.tx.send(RgMsg::Match(Match { line_no, text }));
    }
}

impl Sink for ReportSink {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if let Some(n) = mat.line_number() {
            self.report(n, mat.bytes());
        }
        Ok(true)
    }
}

impl RgSearch {
    /// 启动一次内嵌搜索。encoding：探测到的编码小写标签（"utf-8"/"gbk"）。
    pub fn start(
        path: &str,
        pattern: &str,
        encoding: &str,
        case: Case,
        whole_word: bool,
    ) -> io::Result<RgSearch> {
        let (tx, rx): (Sender<RgMsg>, Receiver<RgMsg>) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        let path = path.to_string();
        let pat = pattern.to_string();
        let enc = encoding.to_string();
        std::thread::spawn(move || {
            run_search(&path, &pat, &enc, case, whole_word, thread_cancel, tx);
        });
        Ok(RgSearch { rx, cancel })
    }

    pub fn rx(&self) -> &Receiver<RgMsg> {
        &self.rx
    }

    /// 请求取消搜索（后台线程在下一个匹配/行处停止，随后发出 Done{cancelled:true}）。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}


/// 流式转码 Reader：把非 UTF-8 文件字节流增量解码为 UTF-8 流（保行结构），喂给引擎。
struct TranscodingReader {
    file: std::fs::File,
    dec: encoding_rs::Decoder,
    src_buf: Vec<u8>,
    pending: Vec<u8>,
    pend_pos: usize,
    finished: bool,
}

impl TranscodingReader {
    fn new(file: std::fs::File, enc: &'static encoding_rs::Encoding) -> Self {
        TranscodingReader {
            file,
            dec: enc.new_decoder(),
            src_buf: vec![0u8; 16 * 1024],
            pending: Vec::new(),
            pend_pos: 0,
            finished: false,
        }
    }

    fn drain_tail(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        // 以 last=true 通知解码器输入流结束，处理截断在尾部的半字符
        let mut buf = [0u8; 256];
        loop {
            let (_, _, u, _) = self.dec.decode_to_utf8(&[], &mut buf, true);
            if u == 0 {
                break;
            }
            self.pending.extend_from_slice(&buf[..u]);
        }
    }
}

impl std::io::Read for TranscodingReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pend_pos < self.pending.len() {
                let n = (self.pending.len() - self.pend_pos).min(out.len());
                out[..n].copy_from_slice(&self.pending[self.pend_pos..self.pend_pos + n]);
                self.pend_pos += n;
                return Ok(n);
            }
            // 已输出区段清空
            self.pending.clear();
            self.pend_pos = 0;
            if self.finished {
                return Ok(0);
            }
            let n = self.file.read(&mut self.src_buf)?;
            if n == 0 {
                self.drain_tail();
                continue;
            }
            // 增量解码整块输入（GB18030/GBK 到 UTF-8 放大倍数 < 4，一次 scratch 足够）
            let mut scratch = vec![0u8; self.src_buf.len() * 4 + 64];
            let mut consumed = 0usize;
            while consumed < n {
                let (_, c, u, _) = self
                    .dec
                    .decode_to_utf8(&self.src_buf[consumed..n], &mut scratch, false);
                if c == 0 {
                    if u == 0 {
                        break; // 防御：无进展
                    }
                    scratch.resize(scratch.len() * 2, 0);
                    continue;
                }
                consumed += c;
                self.pending.extend_from_slice(&scratch[..u]);
            }
        }
    }
}

fn run_search(
    path: &str,
    pattern: &str,
    encoding: &str,
    case: Case,
    word: bool,
    cancel: Arc<AtomicBool>,
    tx: Sender<RgMsg>,
) {
    let enc = encoding_rs::Encoding::for_label(encoding.as_bytes())
        .unwrap_or(encoding_rs::UTF_8);
    let m = match matcher_for(pattern, case, word) {
        Ok(m) => m,
        Err(e) => {
            let _ = tx.send(RgMsg::Error(e));
            let _ = tx.send(RgMsg::Done {
                count: 0,
                cancelled: cancel.load(Ordering::Relaxed),
            });
            return;
        }
    };
    let mut sink = ReportSink {
        tx: tx.clone(),
        cancel: cancel.clone(),
        count: 0,
    };
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let res = if enc == encoding_rs::UTF_8 {
        searcher.search_path(m, path, &mut sink)
    } else {
        // 非 UTF-8（GB18030 等，兼容 GBK/GB2312）：流式转码为 UTF-8 后走引擎
        match std::fs::File::open(path) {
            Ok(f) => {
                let rdr = TranscodingReader::new(f, enc);
                searcher.search_reader(m, rdr, &mut sink)
            }
            Err(e) => Err(e),
        }
    };
    if let Err(e) = res {
        let _ = tx.send(RgMsg::Error(e.to_string()));
    }
    let _ = tx.send(RgMsg::Done {
        count: sink.count,
        cancelled: cancel.load(Ordering::Relaxed),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_case() {
        assert!(smart_case_regex("hello").starts_with("(?i)"));
        assert_eq!(smart_case_regex("Hello"), "Hello");
        assert_eq!(smart_case_regex("(?i)xyz"), "(?i)xyz");
    }

    #[test]
    fn compile_respects_case_and_word() {
        assert!(compile_regex("hello", Case::Insensitive, false).unwrap().is_match("HELLO"));
        assert!(!compile_regex("hello", Case::Sensitive, false).unwrap().is_match("HELLO"));
        let re = compile_regex("log", Case::Insensitive, true).unwrap();
        assert!(re.is_match("a log here"));
        assert!(!re.is_match("logging"));
    }

    #[test]
    fn case_cycle() {
        assert_eq!(Case::Smart.next(), Case::Insensitive);
        assert_eq!(Case::Insensitive.next(), Case::Sensitive);
        assert_eq!(Case::Sensitive.next(), Case::Smart);
        assert_eq!(Case::Sensitive.label(), "区分");
    }

    #[test]
    fn end_to_end_gb18030() {
        // GB18030 编码（兼容 GBK/GB2312 中文日志）：验证转码 Reader + 引擎路径
        let text = "甲行\n第二行 hello\n丙\n";
        let (bytes, _, _) = encoding_rs::GB18030.encode(text);
        let p = tmpfile2(&bytes);
        // 中文关键字（必须经转码后字节才能对上）
        let s = RgSearch::start(
            p.to_str().unwrap(),
            "行",
            "gb18030",
            Case::Sensitive,
            false,
        )
        .unwrap();
        let (hits, done, _) = collect(&s);
        assert!(done);
        assert_eq!(
            hits.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec![1, 2],
            "hits={hits:?}"
        );
        // ASCII 关键字同样工作，行文本解码正确
        let s = RgSearch::start(
            p.to_str().unwrap(),
            "hello",
            "gb18030",
            Case::Sensitive,
            false,
        )
        .unwrap();
        let (hits, done, _) = collect(&s);
        assert!(done);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 2);
        assert_eq!(hits[0].1, "第二行 hello", "hits={hits:?}");
        std::fs::remove_file(&p).ok();
    }

    fn tmpfile(content: &str) -> std::path::PathBuf {
        tmpfile2(content.as_bytes())
    }

    fn tmpfile2(bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "lv_search_{}_{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn collect(
        s: &RgSearch,
    ) -> (Vec<(u64, String)>, bool, u64) {
        let mut hits = Vec::new();
        let mut done = false;
        let mut count = 0;
        while let Ok(msg) = s.rx().recv() {
            match msg {
                RgMsg::Match(m) => hits.push((m.line_no, m.text)),
                RgMsg::Done { count: c, cancelled } => {
                    done = true;
                    count = c;
                    let _ = cancelled;
                    break;
                }
                RgMsg::Error(_) => break,
            }
        }
        (hits, done, count)
    }

    #[test]
    fn end_to_end_utf16le_search() {
        // UTF-16LE（带 BOM）：转码 Reader + 引擎路径
        let text = "甲行\n第二行 hello\n丙\n";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let p = tmpfile2(&bytes);
        let s = RgSearch::start(
            p.to_str().unwrap(),
            "行",
            "utf-16le",
            Case::Sensitive,
            false,
        )
        .unwrap();
        let (hits, done, _) = collect(&s);
        assert!(done);
        assert_eq!(
            hits.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec![1, 2],
            "hits={hits:?}"
        );
        let s = RgSearch::start(
            p.to_str().unwrap(),
            "hello",
            "utf-16le",
            Case::Sensitive,
            false,
        )
        .unwrap();
        let (hits, done, _) = collect(&s);
        assert!(done);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 2);
        assert_eq!(hits[0].1, "第二行 hello", "hits={hits:?}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn end_to_end_inline_search() {
        let p = tmpfile("first line\nhello world\nanother hello here\nLOG it\n");
        // 大小写敏感 / hello
        let s = RgSearch::start(
            p.to_str().unwrap(),
            "hello",
            "utf-8",
            Case::Sensitive,
            false,
        )
        .unwrap();
        let (hits, done, count) = collect(&s);
        assert!(done);
        assert_eq!(count, 2, "hits={hits:?}");
        assert_eq!(hits.len(), 2, "hits={hits:?}");
        assert_eq!(hits[0].0, 2, "hits={hits:?}");
        assert_eq!(hits[0].1, "hello world", "hits={hits:?}");
        assert_eq!(hits[1].0, 3, "hits={hits:?}");

        // 忽略大小写 / LOG 命中第 4 行
        let s = RgSearch::start(
            p.to_str().unwrap(),
            "log",
            "utf-8",
            Case::Insensitive,
            false,
        )
        .unwrap();
        let (hits, done, _) = collect(&s);
        assert!(done);
        assert!(hits.iter().any(|(n, t)| *n == 4 && t == "LOG it"));

        // 整词 log：应命中行 4（LOG）而不再命中行 2/3 的 hello（无独立 log 词？有）验证 -w 生效
        let s = RgSearch::start(
            p.to_str().unwrap(),
            "log",
            "utf-8",
            Case::Insensitive,
            true,
        )
        .unwrap();
        let (hits, done, _) = collect(&s);
        assert!(done);
        assert_eq!(hits.iter().map(|x| x.0).collect::<Vec<_>>(), vec![4]);

        std::fs::remove_file(&p).ok();
    }
}
