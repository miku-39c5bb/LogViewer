//! rg 集成：以外部子进程方式执行搜索，后台线程流式解析 stdout，经 channel 送到 UI。
//!
//! 一次搜索 = 一个「匹配流」。行号按 1-based（与 rg 输出一致）。UI 侧可边收边跳转
//! （模式 a），也可完整消费后填充结果窗（模式 b）。

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Match {
    /// 1-based 行号（与 rg 一致）
    pub line_no: u64,
    /// 该行文本（rg 输出原样，编码问题在 M3 统一处理）
    pub text: String,
}

#[derive(Debug)]
pub enum RgMsg {
    Match(Match),
    /// 搜索结束（含被取消）。count 为已收到的匹配数。
    Done { count: u64, cancelled: bool },
    Error(String),
}

pub struct RgSearch {
    rx: Receiver<RgMsg>,
    child: Arc<Mutex<Option<Child>>>,
}

fn parse_line(line: &str) -> Option<Match> {
    let c = line.find(':')?;
    let (no, text) = line.split_at(c);
    let line_no: u64 = no.trim().parse().ok()?;
    Some(Match {
        line_no,
        text: text[1..].to_string(),
    })
}

impl RgSearch {
    /// 启动一次搜索。pattern 为 rg 正则（-e 传参）。encoding 为文本编码小写标签。
    /// case 决定大小写 flag；whole_word 时加 -w（整词匹配）。
    pub fn start(
        path: &str,
        pattern: &str,
        encoding: &str,
        case: Case,
        whole_word: bool,
    ) -> std::io::Result<RgSearch> {
        let mut cmd = Command::new("rg");
        // --text: 强制按文本处理
        cmd.arg("--text");
        match case {
            Case::Smart => {}
            Case::Insensitive => {
                cmd.arg("-i");
            }
            Case::Sensitive => {
                cmd.arg("-s");
            }
        }
        if whole_word {
            cmd.arg("-w");
        }
        if !encoding.eq_ignore_ascii_case("utf-8") {
            cmd.arg("--encoding").arg(encoding);
        }
        cmd.args(["--line-number", "--no-heading", "--color", "never"])
            .arg("-e")
            .arg(pattern)
            .arg(path);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "无法获取 rg stdout")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "无法获取 rg stderr")
        })?;
        let child = Arc::new(Mutex::new(Some(child)));
        let (tx, rx): (Sender<RgMsg>, Receiver<RgMsg>) = channel();
        let worker = child.clone();
        std::thread::spawn(move || {
            let mut count: u64 = 0;
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if let Some(m) = parse_line(&l) {
                            count += 1;
                            if tx.send(RgMsg::Match(m)).is_err() {
                                break; // UI 已放弃
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(RgMsg::Error(e.to_string()));
                        break;
                    }
                }
            }
            // reader 已被 lines() 消费并在循环结束时自动关闭（stdout EOF）
            // 收尾读 stderr，避免管道阻塞
            let err_text = BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
                .collect::<Vec<_>>()
                .join("\n");
            let cancelled = match worker.lock() {
                Ok(mut guard) => match guard.as_mut() {
                    Some(c) => {
                        let st = c.wait();
                        match st {
                            Ok(s) => !s.success(),
                            Err(_) => true,
                        }
                    }
                    None => true,
                },
                Err(_) => true,
            };
            if !err_text.is_empty() && !cancelled {
                let _ = tx.send(RgMsg::Error(err_text));
            }
            let _ = tx.send(RgMsg::Done { count, cancelled });
        });
        Ok(RgSearch { rx, child })
    }

    pub fn rx(&self) -> &Receiver<RgMsg> {
        &self.rx
    }

    /// 取消搜索（kill 子进程）。线程侧随后会发出 Done{cancelled:true}。
    pub fn cancel(&self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(c) = guard.as_mut() {
                let _ = c.kill();
            }
        }
    }
}

/// 大小写匹配模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    /// 智能（rg smart-case：全小写忽略大小写；含大写则敏感）
    Smart,
    /// 忽略大小写
    Insensitive,
    /// 区分大小写
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
/// 用于让可视区行内高亮与 rg 行为一致。
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

/// 依选项编译行内高亮/过滤正则。
pub fn compile_regex(pattern: &str, case: Case, whole_word: bool) -> Result<regex::Regex, String> {
    regex::Regex::new(&pattern_source(pattern, case, whole_word)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_line() {
        let m = parse_line("42:hello world").unwrap();
        assert_eq!(m.line_no, 42);
        assert_eq!(m.text, "hello world");
    }

    #[test]
    fn parse_skips_warning_lines() {
        assert!(parse_line("binary file matches (found X 1 times)").is_none());
        assert!(parse_line("  12:  text").is_some());
    }

    #[test]
    fn smart_case() {
        assert!(smart_case_regex("hello").starts_with("(?i)"));
        assert_eq!(smart_case_regex("Hello"), "Hello");
        assert_eq!(smart_case_regex("(?i)xyz"), "(?i)xyz"); // 已有大写无关 flag
    }
}
