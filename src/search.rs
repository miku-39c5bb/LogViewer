//! 内嵌搜索：不再依赖外部 rg 进程。
//!
//! - UTF-8 文件：用 grep-searcher + grep-regex（与 ripgrep 同源引擎，字节级多线程/流式搜索）；
//! - GBK 等非 UTF-8 文件：复用 core 的行读取器，按行解码后再用 regex 匹配（引擎一致，速度较慢）；
//! - 取消：后台线程 + `AtomicBool` 标志（UI 侧设置后，引擎在下一个匹配/行处停止）。
//!
//! 对外接口保持与旧版一致：`RgSearch::start(..)` 返回带 channel 的句柄，
//! 通过 `RgMsg::{Match,Done,Error}` 流式上报匹配。

use std::io;
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

fn run_search(
    path: &str,
    pattern: &str,
    encoding: &str,
    case: Case,
    word: bool,
    cancel: Arc<AtomicBool>,
    tx: Sender<RgMsg>,
) {
    if encoding.eq_ignore_ascii_case("utf-8") {
        match matcher_for(pattern, case, word) {
            Ok(m) => {
                let mut sink = ReportSink {
                    tx: tx.clone(),
                    cancel: cancel.clone(),
                    count: 0,
                };
                let mut searcher = SearcherBuilder::new().line_number(true).build();
                if let Err(e) = searcher.search_path(m, path, &mut sink) {
                    let _ = tx.send(RgMsg::Error(e.to_string()));
                }
                let _ = tx.send(RgMsg::Done {
                    count: sink.count,
                    cancelled: cancel.load(Ordering::Relaxed),
                });
            }
            Err(e) => {
                let _ = tx.send(RgMsg::Error(e));
                let _ = tx.send(RgMsg::Done {
                    count: 0,
                    cancelled: cancel.load(Ordering::Relaxed),
                });
            }
        }
    } else {
        // 非 UTF-8（GBK 等）：复用行读取器，逐行解码后 regex 匹配
        run_decoded_fallback(path, pattern, case, word, cancel, tx);
    }
}

fn run_decoded_fallback(
    path: &str,
    pattern: &str,
    case: Case,
    word: bool,
    cancel: Arc<AtomicBool>,
    tx: Sender<RgMsg>,
) {
    use crate::core::file::FileSource;
    use crate::core::lines::LineReader;

    let re = match compile_regex(pattern, case, word) {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(RgMsg::Error(e));
            let _ = tx.send(RgMsg::Done {
                count: 0,
                cancelled: cancel.load(Ordering::Relaxed),
            });
            return;
        }
    };
    let mut src = match FileSource::open(path) {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(RgMsg::Error(e.to_string()));
            let _ = tx.send(RgMsg::Done {
                count: 0,
                cancelled: cancel.load(Ordering::Relaxed),
            });
            return;
        }
    };
    let mut count: u64 = 0;
    let mut line_no: u64 = 0;
    let mut lr = LineReader::new(&mut src, 0, 1 << 20);
    while let Some((_off, bytes, _trunc)) = lr.next_row() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        line_no += 1;
        let text = encoding_rs::GBK.decode(&bytes).0.into_owned();
        if re.is_match(&text) {
            count += 1;
            let _ = tx.send(RgMsg::Match(Match {
                line_no,
                text,
            }));
        }
    }
    let _ = tx.send(RgMsg::Done {
        count,
        cancelled: cancel.load(Ordering::Relaxed),
    });
}

/// 保留的按行读取辅助占位（兼容旧引用）。
#[allow(unused)]
fn _unused() {}

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

    fn tmpfile(content: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "lv_search_{}_{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, content).unwrap();
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
