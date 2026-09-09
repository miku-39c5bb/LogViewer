//! M2 v2.0 多窗格 TUI。
//!
//! 窗格模型（用户定稿语义）：
//! - 文件窗固定为 1 号（index 0），不可覆盖、不可关闭（关闭文件窗 = 退出程序）；
//! - 其余为「匹配列表窗」（模式 b 的结果）。每个窗格常显编号（标题 [n]）。
//! - 光标所在窗格 = 搜索作用域：文件窗搜索 → rg 全文；匹配窗搜索 → 对当前列表内存过滤。
//! - `;` 把当前窗格的匹配上下文生成结果窗，先进入目标选择：N 新建 / 数字覆盖 / Esc 取消。
//! - 匹配窗回车 → 文件窗跳转到对应行（上下文）；文件窗保留 n/N 逐条跳（模式 a）。

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use regex::Regex;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::view::FileView;
use crate::history::History;
use crate::search::{self, Match, RgMsg, RgSearch};

/// 返回使「累计显示宽度 ≥ n 列」的第一个字符起点字节偏移（跳过前 n 列，整字符）。
fn byte_after_cols(s: &str, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let mut col = 0usize;
    for (bi, ch) in s.char_indices() {
        let w = ch.width().unwrap_or(1);
        if col + w > n {
            return bi;
        }
        col += w;
        if col >= n {
            return bi + ch.len_utf8();
        }
    }
    s.len()
}

/// 超长文本只保留开头若干列 + "..."（用于状态栏关键字等避免挤掉后续信息）。
fn clip_str(s: &str, max_cols: usize) -> String {
    if max_cols <= 3 {
        return if s.is_empty() { String::new() } else { ".".repeat(max_cols) };
    }
    if UnicodeWidthStr::width(s) <= max_cols {
        return s.to_string();
    }
    let keep = max_cols.saturating_sub(3);
    let cut = byte_after_cols(s, keep);
    format!("{}...", &s[..cut])
}

/// 取 s 中跳过 skip 列后、长度不超过 max 列的片段（整字符边界）。
fn slice_cols(s: &str, skip: usize, max: usize) -> &str {
    if s.is_empty() || max == 0 {
        return "";
    }
    let start = byte_after_cols(s, skip);
    let rest = &s[start..];
    let end = byte_after_cols(rest, max);
    &rest[..end]
}

/// 把长文本按 max 列软换行成多段（整字符边界）。
fn wrap_cols(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while UnicodeWidthStr::width(rest) > max {
        let cut = byte_after_cols(rest, max).max(1);
        if cut >= rest.len() {
            out.push(rest.to_string());
            return out;
        }
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

// ---------- 模式与命令输入 ----------

pub enum Mode {
    Normal,
    /// 搜索命令输入：/ 向前（文件窗）或搜索（匹配窗）；? 反方向
    Cmd(CmdState),
    /// 目标选择：结果窗放哪（hjkl 方位 / 数字覆盖 / Esc 取消）
    PickTarget(PickState),
    /// 行号跳转输入（: 123 Enter）
    Goto(GotoState),
    /// 历史模糊选择器（H 键打开）
    History(HistoryState),
    /// 帮助面板（F1）
    Help,
    /// 视觉选择（v / V / Ctrl-V；y 复制，Esc 退出）
    Visual(VisualState),
}

/// 视觉选择状态（作用于主文件窗）。
pub struct VisualState {
    kind: VisualKind,
    /// 起始行（1-based）
    anchor_row: u64,
    /// 起始列（字符下标，含）
    anchor_col: usize,
    /// 当前列（字符下标，含）
    cur_col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VisualKind {
    Char,
    Line,
    Block,
}

/// 目标选择模式下待放置的匹配上下文。
pub struct PickState {
    pub rows: Vec<Match>,
    pub tag: String,
}

/// 行号跳转输入（`:` 命令）：输入 1-based 行号后 Enter。
pub struct GotoState {
    buf: String,
    cursor: usize,
}

/// 历史选择器：输入过滤词（fzf 风格子序列匹配），↑↓ 选择，Enter 填入命令输入框（可再编辑）。
pub struct HistoryState {
    filter: String,
    /// 通过过滤的历史下标（指向 History.items()，最近在前）
    list: Vec<usize>,
    sel: usize,
}

impl Mode {
    fn is_cmd(&self) -> bool {
        matches!(self, Mode::Cmd(_))
    }
    fn is_pick(&self) -> bool {
        matches!(self, Mode::PickTarget(_))
    }
    fn is_goto(&self) -> bool {
        matches!(self, Mode::Goto(_))
    }
    fn is_history(&self) -> bool {
        matches!(self, Mode::History(_))
    }
    fn is_help(&self) -> bool {
        matches!(self, Mode::Help)
    }
    fn is_visual(&self) -> bool {
        matches!(self, Mode::Visual(_))
    }
}

pub struct CmdState {
    forward: bool,
    buf: String,
    /// 光标位置（字节索引，保证落在 char 边界）
    cursor: usize,
    /// 进入命令模式前的输入（历史回退时恢复）
    initial: String,
    /// 历史候选（最近在前）
    hist: Vec<String>,
    hist_pos: Option<usize>,
}

/// 文件窗的活动搜索（模式 a：rg 匹配流 + n/N 逐条跳 + 行内高亮）。
/// 匹配参照点是光标行（less 语义）：n/N 从光标行向后/向前找下一处匹配。
pub struct ActiveSearch {
    runner: RgSearch,
    pattern: String,
    forward: bool,
    /// 已收到的匹配（行号升序，来自搜索引擎；文本保留供结果窗与状态显示）
    matches: Vec<Match>,
    done: bool,
    /// 搜索提交后的首次定位尚未执行（等收到首批匹配后跳一次）
    pending_first: bool,
}

// ---------- 窗格内容 ----------

pub enum Content {
    File(FileContent),
    Matches(MatchesContent),
}

pub struct FileContent {
    view: FileView,
    search: Option<ActiveSearch>,
    /// 行内高亮正则（与 search.pattern 同步，smart-case）
    hl: Option<Regex>,
    /// 内容区可视高度（渲染时刷新）
    inner_h: usize,
    /// 水平滚动列数
    hscroll: usize,
    /// 自动换行显示（软 wrap）
    wrap: bool,
}

pub struct MatchesContent {
    /// 本窗内容（不破坏；窗内搜索只是加高亮与游标，不改列表）
    rows: Vec<Match>,
    /// 窗内搜索状态（`/` 或 `?` 在该窗内触发，行为与主窗一致）
    search: Option<PaneSearch>,
    /// 来源描述（标题显示）
    tag: String,
    /// 选中行（在 rows 中的下标）
    sel: usize,
    /// 列表顶部可见行（rows 下标）
    top: usize,
    inner_h: usize,
    /// 水平滚动列数
    hscroll: usize,
    /// 自动换行显示
    wrap: bool,
}

/// 匹配窗内的一次搜索：不破坏列表，仅记录「rows 中哪些行匹配」；
/// n/N 每次以当前选中行（光标）为参照定位下一处，与主窗完全一致。
pub struct PaneSearch {
    query: String,
    /// 搜索方向：/ 为 true（n 向列表下方移动）
    forward: bool,
    /// rows 中匹配 query 的下标（升序）
    match_idx: Vec<usize>,
}

pub struct Pane {
    /// 稳定标识（布局树叶子引用）
    id: u64,
    content: Content,
}

impl Pane {
    fn file(id: u64, path: &str, utf16_limit: u64) -> io::Result<Self> {
        Ok(Pane {
            id,
            content: Content::File(FileContent {
                view: FileView::open_with_limits(
                    path,
                    crate::core::view::DEFAULT_MAX_ROW,
                    Some(utf16_limit),
                )?,
                search: None,
                hl: None,
                inner_h: 24,
                hscroll: 0,
                wrap: false,
            }),
        })
    }

    fn matches(id: u64, rows: Vec<Match>, tag: String) -> Self {
        Pane {
            id,
            content: Content::Matches(MatchesContent {
                rows,
                search: None,
                tag,
                sel: 0,
                top: 0,
                inner_h: 24,
                hscroll: 0,
                wrap: false,
            }),
        }
    }
}

impl FileContent {
    /// 将 1-based 行号作为光标定位到本窗视口约 1/3 高度。
    fn center_on(&mut self, line1: u64) -> bool {
        let h = self.inner_h.max(10);
        if !self.view.goto_line1(line1) {
            return false;
        }
        self.view.fill_to(h);
        true
    }

    /// 已扫描到文件尾时的总行数（否则 None，用于状态栏百分比）。
    fn known_total(&self) -> Option<u64> {
        self.view.known_total()
    }
}

// ---------- 应用状态 ----------

/// 搜索选项（大小写/整词），显示于状态栏。
#[derive(Clone, Copy)]
pub struct SearchPrefs {
    pub case: crate::search::Case,
    pub word: bool,
}

/// 耗时跳转目标（分片执行，可 Ctrl+C / Esc 取消）
#[derive(Clone, Copy)]
enum JumpKind {
    Goto(u64), // 1-based 行号
    Bottom,
}

struct Jump {
    kind: JumpKind,
    back: (u64, u64),
    start: std::time::Instant,
}

pub struct App {
    panes: Vec<Pane>,
    /// 布局树（叶子 id 顺序与 panes 顺序一致 = 几何顺序）
    layout: crate::layout::Layout,
    next_id: u64,
    /// 焦点窗在 panes 中的下标
    focus: usize,
    /// 放大的窗格 id（None = 普通分屏）
    zoom: Option<u64>,
    /// 文件窗 tail 跟随（F 键切换，less -F 语义）
    follow: bool,
    /// 可配置键位表（config.toml）
    keys: crate::keymap::Keymap,
    /// 主题（颜色 / 光标形状）
    theme: crate::theme::Theme,
    /// 搜索选项
    opts: SearchPrefs,
    /// 耗时跳转任务（分片）
    pending_jump: Option<Jump>,
    /// 一次性英文提示（按键即消失），如耗时/取消信息
    trans: Option<String>,
    /// 最近一次 rg 搜索的启动时刻
    search_start: Option<std::time::Instant>,
    /// 焦点窗最近可视列数（$ 行尾用）
    last_cols: usize,
    history: History,
    mode: Mode,
    msg: String,
    quit: bool,
}

// ---------- 构造与主循环 ----------

impl App {
    pub fn open(path: &str) -> io::Result<Self> {
        let (history, cfg) = match History::default_path() {
            Some(hp) => {
                let cfg_path = hp.with_file_name("config.toml");
                if !cfg_path.exists() {
                    if let Some(dir) = cfg_path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let tpl = format!(
                        "{}\n{}\n# 搜索历史最多保留条数（1-100000）\n[history]\ncap = {}\n\n# UTF-16 文件全量内存转码的源文件大小上限(MB，超出会拒绝打开并提示)\n[misc]\nutf16_buffer_mb = 512\n",
                        crate::keymap::Keymap::default_template(),
                        crate::theme::Theme::template_text(),
                        crate::history::DEFAULT_HISTORY_CAP
                    );
                    let _ = std::fs::write(&cfg_path, tpl);
                }
                let cap = crate::history::History::cap_from_toml(&cfg_path);
                (History::load_cap(hp, cap), Some(cfg_path))
            }
            None => (History::in_memory(), None),
        };
        let keys = crate::keymap::Keymap::load_or_default(cfg.as_deref());
        let theme = match &cfg {
            Some(p) => std::fs::read_to_string(p)
                .ok()
                .and_then(|t| t.parse::<toml::Table>().ok())
                .map(|t| crate::theme::Theme::from_toml(&t))
                .unwrap_or_else(crate::theme::Theme::default),
            None => crate::theme::Theme::default(),
        };
        // [misc].utf16_buffer_mb：UTF-16 内存转码上限（MB）
        let utf16_limit = cfg
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| t.parse::<toml::Table>().ok())
            .and_then(|t| {
                t.get("misc")
                    .and_then(|v| v.get("utf16_buffer_mb"))
                    .and_then(|v| v.as_integer())
            })
            .map(|mb| (mb.clamp(16, 16 * 1024) as u64) << 20)
            .unwrap_or(crate::core::view::DEFAULT_UTF16_INMEM_LIMIT);
        let mut pane = Pane::file(1, path, utf16_limit)?;
        // 打开即预填首屏，避免空窗闪烁
        if let Content::File(fc) = &mut pane.content {
            fc.view.fill_to(24);
        }
        Ok(Self {
            panes: vec![pane],
            layout: crate::layout::Layout::new(1),
            next_id: 2,
            focus: 0,
            zoom: None,
            follow: false,
            keys,
            theme,
            opts: SearchPrefs {
                case: crate::search::Case::Smart,
                word: false,
            },
            pending_jump: None,
            trans: None,
            search_start: None,
            last_cols: 100,
            history,
            mode: Mode::Normal,
            msg: String::new(),
            quit: false,
        })
    }

    pub fn run(&mut self) -> io::Result<()> {
        let mut term = ratatui::init();
        let result = self.event_loop(&mut term);
        // 取消仍在跑的后台搜索
        if let Content::File(fc) = &mut self.panes[0].content {
            if let Some(a) = fc.search.take() {
                a.runner.cancel();
            }
        }
        ratatui::restore();
        result
    }

    fn event_loop(&mut self, term: &mut ratatui::DefaultTerminal) -> io::Result<()> {
        loop {
            term.draw(|f| self.render(f))?;
            self.sync_cursor_shape();
            if self.quit {
                return Ok(());
            }
            if event::poll(Duration::from_millis(150))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            self.on_tick();
        }
    }

    /// 按主题配置设置系统光标形状（输入模式与普通模式不同）。
    fn sync_cursor_shape(&mut self) {
        use crate::theme::CursorShape;
        use crossterm::cursor::{self, SetCursorStyle};
        use crossterm::QueueableCommand;
        use std::io::Write;
        let input = self.mode.is_cmd() || self.mode.is_goto() || self.mode.is_history();
        let shape = if input {
            self.theme.cursor_input
        } else {
            self.theme.cursor_normal
        };
        let mut out = std::io::stdout();
        match shape {
            CursorShape::Hidden => {
                let _ = out.queue(cursor::Hide);
            }
            CursorShape::Block => {
                let _ = out.queue(cursor::Show);
                let _ = out.queue(SetCursorStyle::BlinkingBlock);
            }
            CursorShape::Underline => {
                let _ = out.queue(cursor::Show);
                let _ = out.queue(SetCursorStyle::BlinkingUnderScore);
            }
            CursorShape::Bar => {
                let _ = out.queue(cursor::Show);
                let _ = out.queue(SetCursorStyle::BlinkingBar);
            }
        }
        let _ = out.flush();
    }

    fn on_tick(&mut self) {
        self.harvest_file_search();
        self.process_jump();
        self.follow_poll();
    }

    fn set_msg(&mut self, s: impl Into<String>) {
        self.msg = s.into();
    }
}

// ---------- 文件窗搜索（模式 a）----------

impl App {
    /// 每帧收割文件窗后台 rg 消息（文件窗固定 index 0）。
    fn harvest_file_search(&mut self) {
        let (note, first_jump) = {
            let Content::File(fc) = &mut self.panes[0].content else {
                return;
            };
            let Some(a) = fc.search.as_mut() else {
                return;
            };
            let mut note: Option<String> = None;
            loop {
                match a.runner.rx().try_recv() {
                    Ok(RgMsg::Match(m)) => a.matches.push(m),
                    Ok(RgMsg::Error(e)) => note = Some(format!("rg: {e}")),
                    Ok(RgMsg::Done { count, cancelled }) => {
                        a.done = true;
                        let dur = self.search_start.take().map(|s| s.elapsed());
                        let dur_txt = dur.map(|d| format!(" ({})", ms_text(d))).unwrap_or_default();
                        note = if cancelled {
                            Some(format!("search cancelled{dur_txt}"))
                        } else if count == 0 {
                            Some(format!("search done: no matches{dur_txt}"))
                        } else {
                            Some(format!("search done{dur_txt}"))
                        };
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        a.done = true;
                        break;
                    }
                }
            }
            // 首次定位等待搜索完成（结果集完整）后才执行：
            // 反向搜索的“最后一个匹配”与正向的边界都依赖完整结果，流式部分结果会导致跳错。
            let fj = a.pending_first && a.done && !a.matches.is_empty();
            if fj {
                a.pending_first = false;
            }
            (note, fj)
        };
        if let Some(n) = note {
            self.set_trans(n);
        }
        if first_jump {
            self.init_first_jump();
        }
    }

    /// 在已收匹配中定位「光标行附近的一处」。
    /// n/N 定位：从光标行严格之外找下一处。back=true 向后（行号严格 > 光标），false 向前（严格 < 光标）。
    fn locate_from_cursor(fc: &FileContent, back: bool) -> Option<(usize, u64)> {
        let a = fc.search.as_ref()?;
        if a.matches.is_empty() {
            return None;
        }
        let cursor = fc.view.cursor_line1();
        let m = &a.matches;
        let pos = m.binary_search_by(|x| x.line_no.cmp(&cursor));
        if back {
            // 光标恰在某匹配行时，跳过它找下一个
            let i = match pos {
                Ok(i) => i + 1,
                Err(i) => i,
            };
            if i < m.len() {
                Some((i, m[i].line_no))
            } else {
                None
            }
        } else {
            let i = match pos {
                Ok(i) => i,     // cursor 恰为第 i 个匹配：上一个为 i-1
                Err(i) => i,    // 前 i 个匹配 < cursor：上一个为 i-1
            };
            if i > 0 {
                Some((i - 1, m[i - 1].line_no))
            } else {
                None
            }
        }
    }

    /// 搜索提交后的首次定位（含当前行）：/ 找 >= 光标的首个匹配；? 找 <= 光标的最后一个匹配。
    fn init_first_jump(&mut self) {
        let target = {
            let Content::File(fc) = &self.panes[0].content else {
                return;
            };
            let Some(a) = fc.search.as_ref() else {
                return;
            };
            if a.matches.is_empty() {
                return;
            }
            let cursor = fc.view.cursor_line1();
            let m = &a.matches;
            let pos = m.binary_search_by(|x| x.line_no.cmp(&cursor));
            if a.forward {
                // 首个 >= 光标
                match pos {
                    Ok(i) | Err(i) if i < m.len() => Some((i, m[i].line_no)),
                    _ => None,
                }
            } else {
                // 最后一个 <= 光标
                match pos {
                    Ok(i) => Some((i, m[i].line_no)),
                    Err(0) => None,
                    Err(i) => Some((i - 1, m[i - 1].line_no)),
                }
            }
        };
        if let Some((_idx, line)) = target {
            if self.jump_file_to_line(line) {
                self.set_msg(format!("匹配 {}", line));
            }
        }
    }

    /// 文件窗滚到某行（光标定位，视口 1/3 处）。
    fn jump_file_to_line(&mut self, line1: u64) -> bool {
        let Content::File(fc) = &mut self.panes[0].content else {
            return false;
        };
        fc.center_on(line1)
    }

    /// 文件窗 n/N：从光标行向后/向前找下一处匹配（less 语义，每次都以当前光标为参照）。
    fn file_step_match(&mut self, reverse: bool) {
        enum Act {
            Jump(u64),
            Msg(String),
        }
        let act = {
            let Content::File(fc) = &self.panes[0].content else {
                return;
            };
            let Some(a) = fc.search.as_ref() else {
                return;
            };
            if a.matches.is_empty() {
                Act::Msg(if a.done {
                    "无匹配".to_string()
                } else {
                    "搜索中…".to_string()
                })
            } else {
                // 同向搜索方向沿 a.forward；N 反向
                let back = a.forward != reverse;
                match Self::locate_from_cursor(fc, back) {
                    Some((_i, line)) => Act::Jump(line),
                    None => Act::Msg(if !a.done {
                        "搜索尚未完成，稍候再按 n/N".to_string()
                    } else if back {
                        "已是最后一个匹配".to_string()
                    } else {
                        "已是第一个匹配".to_string()
                    }),
                }
            }
        };
        match act {
            Act::Msg(m) => self.set_msg(m),
            Act::Jump(line) => {
                if self.jump_file_to_line(line) {
                    self.set_msg(format!("匹配 {}", line));
                }
            }
        }
    }
}

// ---------- 窗格生成 / 覆盖 / 关闭（模式 b 结果窗）----------

impl App {
    fn pane_title(&self, i: usize) -> String {
        match &self.panes[i].content {
            Content::File(fc) => {
                let fname = fc
                    .view
                    .file_path()
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("[{}] file: {fname}", i + 1)
            }
            Content::Matches(mc) => {
                let n = mc.rows.len().to_string();
                format!("[{}] match: {} ({n})", i + 1, mc.tag)
            }
        }
    }

    /// 当前焦点窗格的「匹配上下文」，用于模式 b 生成结果窗。
    fn focus_match_source(&self) -> Option<(Vec<Match>, String)> {
        match &self.panes[self.focus].content {
            Content::File(fc) => {
                let a = fc.search.as_ref()?;
                if a.matches.is_empty() {
                    return None;
                }
                let tag = format!("{}{}", if a.forward { "/" } else { "?" }, a.pattern);
                Some((a.matches.clone(), tag))
            }
            Content::Matches(mc) => {
                if mc.rows.is_empty() {
                    return None;
                }
                // 窗内搜索激活时输出匹配子集；否则输出整个列表
                match &mc.search {
                    Some(s) if !s.match_idx.is_empty() => {
                        let rows: Vec<Match> =
                            s.match_idx.iter().map(|&i| mc.rows[i].clone()).collect();
                        Some((rows, format!("/{}", s.query)))
                    }
                    _ => Some((mc.rows.clone(), mc.tag.clone())),
                }
            }
        }
    }

    /// 由「匹配上下文」进入目标选择。N 新建 / 数字覆盖 / Esc 取消。
    fn enter_pick(&mut self) {
        let Some((rows, tag)) = self.focus_match_source() else {
            self.set_msg("当前窗格没有可列出的匹配（先搜索并等待完成）");
            return;
        };
        if self.mode.is_pick() {
            return;
        }
        self.set_msg(format!("{} 条匹配 → 放置位置:", rows.len()));
        self.mode = Mode::PickTarget(PickState { rows, tag });
    }

    /// 覆盖数字目标（idx = number-1）。文件窗(1 号)不可覆盖。
    fn pick_overwrite(&mut self, ps: PickState, number: u64) {
        let idx = (number as usize).saturating_sub(1);
        if idx >= self.panes.len() {
            self.set_msg(format!("没有编号 {number} 的窗格"));
            return;
        }
        if idx == 0 {
            self.set_msg("原始文件窗不可覆盖");
            return;
        }
        // 覆盖 = 该窗格内容替换为新匹配列表（编号不变）
        self.panes[idx].content = Content::Matches(MatchesContent {
            rows: ps.rows,
            search: None,
            tag: ps.tag,
            sel: 0,
            top: 0,
            inner_h: 24,
            hscroll: 0,
            wrap: false,
        });
        self.focus = idx;
        self.set_msg(format!("已覆盖到 {number} 号窗格"));
    }

    /// 以指定方位新建结果窗：new_first=true 新窗在焦点窗左/上，否则右/下。
    fn new_matches_pane(
        &mut self,
        rows: Vec<Match>,
        tag: String,
        dir: crate::layout::Dir,
        mut new_first: bool,
    ) {
        let n = rows.len();
        // 文件窗(id=1)保持位于 panes[0]：不能在其左/上插入
        if self.panes[self.focus].id == 1 {
            new_first = false;
        }
        let id = self.next_id;
        self.next_id += 1;
        let fid = self.panes[self.focus].id;
        let insert_at = if new_first { self.focus } else { self.focus + 1 };
        let new_pane = Pane::matches(id, rows, tag);
        self.panes.insert(insert_at, new_pane);
        self.layout.split_relative(fid, dir, id, new_first);
        self.focus = insert_at;
        self.zoom = None;
        self.set_msg(format!("结果窗已创建：{n} 条匹配"));
    }

    /// 关闭焦点搜索窗（文件窗不可关闭）。
    fn close_focus_pane(&mut self) {
        if self.focus == 0 {
            self.set_msg("文件窗不可关闭（关闭即退出程序）");
            return;
        }
        let id = self.panes[self.focus].id;
        self.layout.close(id);
        self.panes.remove(self.focus);
        if self.zoom == Some(id) {
            self.zoom = None;
        }
        if self.focus >= self.panes.len() {
            self.focus = self.panes.len() - 1;
        }
        self.set_msg("已关闭结果窗");
    }
}

// ---------- 耗时操作（分片跳转 / processing / 取消）----------

/// 人类可读毫秒格式（保留 ms 精度）。
fn ms_text(d: std::time::Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        format!("{:.0} ms", ms)
    }
}

impl App {
    /// 发起分片耗时跳转（G / :行号）。
    fn start_jump(&mut self, kind: JumpKind) {
        let Content::File(fc) = &mut self.panes[0].content else {
            return;
        };
        if self.pending_jump.is_some() {
            return;
        }
        let back = fc.view.snapshot_pos();
        self.pending_jump = Some(Jump {
            kind,
            back,
            start: std::time::Instant::now(),
        });
    }

    /// 每帧推进跳转分片；完成时执行定位并显示耗时。
    fn process_jump(&mut self) {
        let target1 = match self.pending_jump.as_ref() {
            Some(j) => match j.kind {
                JumpKind::Goto(l) => Some(l),
                JumpKind::Bottom => None,
            },
            None => return,
        };
        // 每帧最多推进 4000 个 checkpoint 块（约 4M 行），期间事件循环可响应取消键
        let done = {
            let Content::File(fc) = &mut self.panes[0].content else {
                return;
            };
            fc.view.extend_toward(target1, 4000)
        };
        if !done {
            return;
        }
        let (kind, start) = {
            let j = self.pending_jump.as_ref().unwrap();
            (j.kind, j.start)
        };
        self.pending_jump = None;
        let dur = start.elapsed();
        let ok = {
            let Content::File(fc) = &mut self.panes[0].content else {
                return;
            };
            match kind {
                JumpKind::Bottom => {
                    fc.view.scroll_to_bottom();
                    fc.view.fill_to(fc.inner_h);
                    true
                }
                JumpKind::Goto(line1) => {
                    let ok = fc.view.goto_line1(line1);
                    if ok {
                        fc.view.fill_to(fc.inner_h);
                    }
                    ok
                }
            }
        };
        let label = match kind {
            JumpKind::Goto(l) => format!("goto line {l}"),
            JumpKind::Bottom => "goto end of file".to_string(),
        };
        if ok {
            self.set_trans(format!("{label} done ({})", ms_text(dur)));
        } else {
            self.set_trans(format!("{label} failed: line out of range"));
        }
    }

    /// 取消当前耗时跳转并恢复原位。
    fn cancel_jump(&mut self, why: &str) {
        if let Some(j) = self.pending_jump.take() {
            let Content::File(fc) = &mut self.panes[0].content else {
                return;
            };
            fc.view.restore_pos(j.back.0, j.back.1);
            fc.view.fill_to(fc.inner_h);
            self.set_trans(why.to_string());
        }
    }

    /// 设置一次性英文提示（任意按键后消失）。
    fn set_trans(&mut self, s: String) {
        self.trans = Some(s);
    }

    /// 退出或取消：有耗时跳转 → 取消它；有进行中的搜索 → 取消搜索；否则退出。
    fn try_quit(&mut self) {
        if self.pending_jump.is_some() {
            self.cancel_jump("cancelled: jump");
            return;
        }
        let searching = {
            let Content::File(fc) = &self.panes[0].content else {
                return;
            };
            fc.search.as_ref().map(|a| !a.done).unwrap_or(false)
        };
        if searching {
            if let Content::File(fc) = &mut self.panes[0].content {
                if let Some(a) = fc.search.take() {
                    a.runner.cancel();
                }
                fc.hl = None;
            }
            self.set_trans("cancelled: search".to_string());
            return;
        }
        self.quit = true;
    }
}

/// 按字符下标截取字符串 [from,to_excl)（下标越界 → 截到字符串末尾）。
fn slice_chars(s: &str, from: usize, to_excl: usize) -> String {
    let mut start = s.len();
    let mut end = s.len();
    let mut i = 0usize;
    for (bi, ch) in s.char_indices() {
        if i == from {
            start = bi;
        }
        if i == to_excl {
            end = bi;
            break;
        }
        i += 1;
    }
    if to_excl > 0 && i <= to_excl && to_excl >= from && end == s.len() {
        end = s.len();
    }
    if from > i {
        return String::new();
    }
    s[start..end.min(s.len())].to_string()
}

impl App {
    fn start_visual(&mut self, kind: crate::app::VisualKind) {
        if self.focus != 0 {
            self.set_msg("视觉选择仅支持主文件窗");
            return;
        }
        let row = {
            let Content::File(fc) = &self.panes[0].content else {
                return;
            };
            fc.view.cursor_line1()
        };
        // 水平滚动归零，保证字符级渲染从行首可见；自动换行保持用户当前设置不变
        if let Content::File(fc) = &mut self.panes[0].content {
            fc.hscroll = 0;
        }
        self.mode = Mode::Visual(VisualState {
            kind,
            anchor_row: row,
            anchor_col: 0,
            cur_col: 0,
        });
    }

    /// 当前光标行文本的字符数（用于列移动 clamp）
    fn visual_cur_len(&mut self) -> usize {
        let row = {
            let Content::File(fc) = &self.panes[0].content else {
                return 0;
            };
            fc.view.cursor_line1()
        };
        self.main_read_line(row).map(|t| t.chars().count()).unwrap_or(0)
    }

    fn visual_move_row(&mut self, dir: i64) {
        if let Content::File(fc) = &mut self.panes[0].content {
            fc.view.move_cursor(dir);
        }
        let len = self.visual_cur_len();
        if let Mode::Visual(v) = &mut self.mode {
            v.cur_col = v.cur_col.min(len);
        }
    }

    fn visual_move_col(&mut self, delta: i64) {
        let len = self.visual_cur_len();
        if let Mode::Visual(v) = &mut self.mode {
            let c = v.cur_col as i64 + delta;
            v.cur_col = c.clamp(0, len as i64) as usize;
        }
    }

    /// 选区覆盖的行范围（含端点，1-based）
    fn visual_row_span(&self) -> Option<(u64, u64)> {
        let Mode::Visual(v) = &self.mode else {
            return None;
        };
        let cur = {
            let Content::File(fc) = &self.panes[0].content else {
                return None;
            };
            fc.view.cursor_line1()
        };
        Some((v.anchor_row.min(cur), v.anchor_row.max(cur)))
    }

    fn on_visual_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Char('y') => self.visual_yank(),
            KeyCode::Left | KeyCode::Char('h') => self.visual_move_col(-1),
            KeyCode::Right | KeyCode::Char('l') => self.visual_move_col(1),
            KeyCode::Up | KeyCode::Char('k') => self.visual_move_row(-1),
            KeyCode::Down | KeyCode::Char('j') => self.visual_move_row(1),
            _ => {}
        }
    }

    /// 复制选区到系统剪贴板并退出视觉模式。
    fn visual_yank(&mut self) {
        let (kind, a_row, a_col, b_col) = match &self.mode {
            Mode::Visual(v) => (v.kind, v.anchor_row, v.anchor_col, v.cur_col),
            _ => return,
        };
        let cur_row = {
            let Content::File(fc) = &self.panes[0].content else {
                return;
            };
            fc.view.cursor_line1()
        };
        let r1 = a_row.min(cur_row);
        let r2 = a_row.max(cur_row);
        let mut lines: Vec<String> = Vec::new();
        for r in r1..=r2 {
            lines.push(self.main_read_line(r).unwrap_or_default());
        }
        let out: String = match kind {
            crate::app::VisualKind::Line => {
                let mut s = lines.join("\n");
                s.push('\n');
                s
            }
            crate::app::VisualKind::Block => {
                let (c1, c2) = (a_col.min(b_col), a_col.max(b_col));
                lines
                    .iter()
                    .map(|l| slice_chars(l, c1, c2 + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            crate::app::VisualKind::Char => {
                let (c1, c2) = (a_col.min(b_col), a_col.max(b_col));
                if r1 == r2 {
                    slice_chars(&lines[0], c1, c2 + 1)
                } else {
                    let mut s = slice_chars(&lines[0], c1, usize::MAX);
                    for l in &lines[1..lines.len() - 1] {
                        s.push('\n');
                        s.push_str(l);
                    }
                    s.push('\n');
                    s.push_str(&slice_chars(
                        lines.last().unwrap_or(&String::new()),
                        0,
                        c2 + 1,
                    ));
                    s
                }
            }
        };
        let copied = match arboard::Clipboard::new() {
            Ok(mut cb) => cb.set_text(out.clone()).is_ok(),
            Err(_) => false,
        };
        self.mode = Mode::Normal;
        if copied {
            self.set_msg(format!(
                "已复制 {} 行 / {} 字符到系统剪贴板",
                r2 - r1 + 1,
                out.chars().count()
            ));
        } else {
            self.set_msg("复制到系统剪贴板失败");
        }
    }
}

// ---------- 按键分发 ----------

impl App {
    fn on_key(&mut self, key: KeyEvent) {
        // 任意按键清除一次性英文提示（耗时/取消消息）
        self.trans = None;
        if self.mode.is_cmd() {
            self.on_cmd_key(key);
        } else if self.mode.is_pick() {
            self.on_pick_key(key);
        } else if self.mode.is_goto() {
            self.on_goto_key(key);
        } else if self.mode.is_history() {
            self.on_history_key(key);
        } else if self.mode.is_help() {
            self.on_help_key(key);
        } else if self.mode.is_visual() {
            self.on_visual_key(key);
        } else {
            self.on_normal_key(key);
        }
    }

    fn on_pick_key(&mut self, key: KeyEvent) {
        let take = |mode: &mut Mode| match std::mem::replace(mode, Mode::Normal) {
            Mode::PickTarget(ps) => Some(ps),
            _ => None,
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.set_msg("");
            }
            // 方位创建：h=左 j=下 k=上 l=右（文件窗只允许 j/l）
            KeyCode::Char('h') => {
                if let Some(ps) = take(&mut self.mode) {
                    self.new_matches_pane(ps.rows, ps.tag, crate::layout::Dir::H, true);
                }
            }
            KeyCode::Char('k') => {
                if let Some(ps) = take(&mut self.mode) {
                    self.new_matches_pane(ps.rows, ps.tag, crate::layout::Dir::V, true);
                }
            }
            KeyCode::Char('j') => {
                if let Some(ps) = take(&mut self.mode) {
                    self.new_matches_pane(ps.rows, ps.tag, crate::layout::Dir::V, false);
                }
            }
            KeyCode::Char('l') | KeyCode::Char('n') | KeyCode::Char('N') => {
                if let Some(ps) = take(&mut self.mode) {
                    self.new_matches_pane(ps.rows, ps.tag, crate::layout::Dir::H, false);
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                if let Some(ps) = take(&mut self.mode) {
                    self.pick_overwrite(ps, c.to_digit(10).unwrap() as u64);
                }
            }
            _ => {}
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        // Esc：取消耗时跳转（无则忽略）
        if key.code == KeyCode::Esc {
            self.cancel_jump("cancelled: jump");
            return;
        }
        // 视觉选择进入（仅主文件窗）
        if self.focus == 0 {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('v') if ctrl => {
                    self.start_visual(crate::app::VisualKind::Block);
                    return;
                }
                KeyCode::Char('V') => {
                    self.start_visual(crate::app::VisualKind::Line);
                    return;
                }
                KeyCode::Char('v') => {
                    self.start_visual(crate::app::VisualKind::Char);
                    return;
                }
                _ => {}
            }
        }
        // 数字键 1-9：直接把焦点切到对应编号窗格（编号 = 几何顺序，动态不进入配置表）
        if let KeyCode::Char(d) = key.code {
            if d.is_ascii_digit() && d != '0' {
                let n = d.to_digit(10).unwrap() as usize;
                if n <= self.panes.len() {
                    self.focus = n - 1;
                    self.zoom = None;
                }
                return;
            }
        }
        // 其余按键经可配置键位表派发
        if let Some(spec) = crate::keymap::KeySpec::from_key(&key) {
            if let Some(act) = self.keys.action(&spec) {
                self.dispatch_normal(act);
            }
        }
    }

    fn dispatch_normal(&mut self, act: crate::keymap::Action) {
        use crate::keymap::Action as A;
        match act {
            A::Quit => self.try_quit(),
            A::FocusNext => {
                if self.panes.len() > 1 {
                    self.focus = (self.focus + 1) % self.panes.len();
                    self.zoom = None;
                }
            }
            A::FocusPrev => {
                if self.panes.len() > 1 {
                    self.focus = (self.focus + self.panes.len() - 1) % self.panes.len();
                    self.zoom = None;
                }
            }
            A::ScrollLineDown => self.pane_move(1),
            A::ScrollLineUp => self.pane_move(-1),
            A::HalfDown => self.pane_page(1, true),
            A::HalfUp => self.pane_page(-1, true),
            A::PageDown => self.pane_page(1, false),
            A::PageUp => self.pane_page(-1, false),
            A::ToTop => self.pane_home(),
            A::ToBottom => {
                if self.focus == 0 {
                    self.start_jump(JumpKind::Bottom);
                } else {
                    self.pane_end();
                }
            }
            A::SearchFwd => self.start_cmd(true),
            A::SearchBack => self.start_cmd(false),
            A::MatchNext => {
                if self.focus == 0 {
                    self.file_step_match(false);
                } else {
                    self.matches_step(false);
                }
            }
            A::MatchPrev => {
                if self.focus == 0 {
                    self.file_step_match(true);
                } else {
                    self.matches_step(true);
                }
            }
            A::ClearSearch => {
                if self.focus == 0 {
                    self.file_cancel_search();
                } else {
                    self.matches_clear_search();
                }
            }
            A::OpenMatchPane => self.enter_pick(),
            A::GotoLine => self.start_goto(),
            A::Zoom => self.zoom_toggle(),
            A::Export => self.export_focus(),
            A::Follow => self.toggle_follow(),
            A::HLeft => self.pane_hscroll(-8),
            A::HRight => self.pane_hscroll(8),
            A::HHome => self.pane_hscroll(0),
            A::HEnd => self.pane_goto_eol(),
            A::Wrap => self.pane_toggle_wrap(),
            A::ClosePane => self.close_focus_pane(),
            A::EnterJump => {
                if self.focus > 0 {
                    self.matches_jump_to_file();
                }
            }
            A::Balance => self.layout.balance(),
            A::NudgeLeft => self.nudge_resize(KeyDir::Left),
            A::NudgeRight => self.nudge_resize(KeyDir::Right),
            A::NudgeUp => self.nudge_resize(KeyDir::Up),
            A::NudgeDown => self.nudge_resize(KeyDir::Down),
            A::History => self.start_history(),
            A::Help => self.mode = Mode::Help,
            A::ToggleCase => {
                self.opts.case = self.opts.case.next();
                self.set_msg(format!(
                    "大小写匹配：{}（{}）",
                    self.opts.case.label(),
                    match self.opts.case {
                        crate::search::Case::Smart => "全小写关键字自动忽略大小写",
                        crate::search::Case::Insensitive => "忽略大小写",
                        crate::search::Case::Sensitive => "区分大小写",
                    }
                ));
            }
            A::ToggleWord => {
                self.opts.word = !self.opts.word;
                self.set_msg(if self.opts.word {
                    "整词匹配：开"
                } else {
                    "整词匹配：关（子串/正则匹配）"
                });
            }
        }
    }

    fn file_cancel_search(&mut self) {
        let Content::File(fc) = &mut self.panes[0].content else {
            return;
        };
        if let Some(a) = fc.search.take() {
            a.runner.cancel();
        }
        fc.hl = None;
        self.msg.clear();
    }

    /// 焦点窗格的按行移动（文件窗=光标移动；匹配窗=移动选中行）
    fn pane_move(&mut self, dir: i64) {
        match &mut self.panes[self.focus].content {
            Content::File(fc) => {
                if dir < 0 {
                    self.follow = false; // 向上查看时退出 tail 跟随
                }
                fc.view.move_cursor(dir);
            }
            Content::Matches(mc) => {
                let n = mc.rows.len();
                if n == 0 {
                    return;
                }
                let target = if dir < 0 {
                    mc.sel.saturating_sub(1)
                } else {
                    (mc.sel + 1).min(n - 1)
                };
                mc.sel = target;
                mc.keep_visible();
            }
        }
    }

    fn pane_page(&mut self, dir: i64, half: bool) {
        match &mut self.panes[self.focus].content {
            Content::File(fc) => {
                let h = fc.inner_h.max(2);
                let n = if half {
                    (h / 2).max(1) as i64
                } else {
                    (h.saturating_sub(2)).max(1) as i64
                };
                if dir < 0 {
                    self.follow = false; // 向上翻页退出 tail 跟随
                }
                fc.view.scroll_window(dir * n);
            }
            Content::Matches(mc) => {
                let n = if half {
                    (mc.inner_h / 2).max(1)
                } else {
                    mc.inner_h.saturating_sub(2).max(1)
                };
                let len = mc.rows.len();
                if len == 0 {
                    return;
                }
                if dir < 0 {
                    mc.sel = mc.sel.saturating_sub(n);
                } else {
                    mc.sel = (mc.sel + n).min(len - 1);
                }
                mc.keep_visible();
            }
        }
    }

    fn pane_home(&mut self) {
        match &mut self.panes[self.focus].content {
            Content::File(fc) => fc.view.scroll_to_top(),
            Content::Matches(mc) => {
                mc.sel = 0;
                mc.top = 0;
            }
        }
    }

    fn pane_end(&mut self) {
        match &mut self.panes[self.focus].content {
            Content::File(fc) => fc.view.scroll_to_bottom(),
            Content::Matches(mc) => {
                if !mc.rows.is_empty() {
                    mc.sel = mc.rows.len() - 1;
                    mc.keep_visible();
                }
            }
        }
    }

    /// 用方向键调整焦点窗尺寸：左右键调宽（最近的 H 祖先分割）、上下键调高（最近 V 祖先）。
    fn nudge_resize(&mut self, dir: KeyDir) {
        if self.panes.len() <= 1 {
            return;
        }
        let fid = self.panes[self.focus].id;
        let horizontal = matches!(dir, KeyDir::Left | KeyDir::Right);
        // 用户约定：→/↑ = 变大，←/↓ = 变小
        let grow = matches!(dir, KeyDir::Right | KeyDir::Up);
        if self.layout.adjust_axis(fid, horizontal, grow) {
            self.set_msg(if horizontal {
                "已调整宽度（= 均衡）"
            } else {
                "已调整高度（= 均衡）"
            });
        } else {
            self.set_msg(if horizontal {
                "无左右相邻窗格可调整"
            } else {
                "无上下相邻窗格可调整"
            });
        }
    }

    /// zoom 当前窗格到全屏 / 还原。
    fn zoom_toggle(&mut self) {
        let id = self.panes[self.focus].id;
        self.zoom = if self.zoom == Some(id) { None } else { Some(id) };
    }

    /// 按行号读取主文件窗对应行原文（文本按需读取的统一入口）。
    fn main_read_line(&mut self, line1: u64) -> Option<String> {
        let Content::File(fc) = &mut self.panes[0].content else {
            return None;
        };
        fc.view.read_line_text(line1)
    }

    /// 把焦点窗格当前内容导出为文件（主窗=搜索结果；匹配窗=列表；文本按行读取，不驻留）。
    fn export_focus(&mut self) {
        let lines: Vec<u64> = match &self.panes[self.focus].content {
            Content::File(fc) => match &fc.search {
                Some(a) => a.matches.iter().map(|m| m.line_no).collect(),
                None => Vec::new(),
            },
            Content::Matches(mc) => mc.rows.iter().map(|m| m.line_no).collect(),
        };
        if lines.is_empty() {
            self.set_msg("没有可导出的内容（先搜索）");
            return;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let name = format!("logviewer_export_{nanos}.txt");
        let mut out = String::new();
        for line in &lines {
            let text = self.main_read_line(*line).unwrap_or_default();
            out.push_str(&format!("{line}: {text}\n"));
        }
        match std::fs::write(&name, out) {
            Ok(()) => {
                let n = lines.len();
                self.set_msg(format!("已导出 {n} 行到 {name}（当前目录）"));
            }
            Err(e) => self.set_msg(format!("导出失败: {e}")),
        }
    }

    /// F：主文件窗 tail 跟随开关（less -F 语义；日志增长自动滚到底）。
    fn toggle_follow(&mut self) {
        self.follow = !self.follow;
        if self.follow {
            let Content::File(fc) = &mut self.panes[0].content else {
                return;
            };
            fc.view.scroll_to_bottom();
            fc.view.fill_to(fc.inner_h);
            self.set_msg("tail 跟随中（F 退出，向上滚动也会退出）");
        } else {
            self.set_msg("");
        }
    }

    fn follow_poll(&mut self) {
        if !self.follow {
            return;
        }
        let Content::File(fc) = &mut self.panes[0].content else {
            return;
        };
        let before = fc.view.file_len();
        if fc.view.poll_len() != before {
            fc.view.scroll_to_bottom();
            fc.view.fill_to(fc.inner_h);
        }
    }
}

impl App {
    /// 焦点窗水平滚动（←→ / h / l / Home）。
    fn pane_hscroll(&mut self, delta: i64) {
        let f = |wrap: bool, h: &mut usize| {
            if wrap {
                return;
            }
            if delta == 0 {
                *h = 0;
            } else {
                *h = (*h as i64 + delta).max(0) as usize;
            }
        };
        match &mut self.panes[self.focus].content {
            Content::File(fc) => f(fc.wrap, &mut fc.hscroll),
            Content::Matches(mc) => f(mc.wrap, &mut mc.hscroll),
        }
    }

    /// `$`：跳到当前（光标/选中）行行尾（水平滚动到末尾可见）。
    fn pane_goto_eol(&mut self) {
        let cols = self.last_cols.max(10);
        if self.focus == 0 {
            let Content::File(fc) = &mut self.panes[0].content else {
                return;
            };
            if fc.wrap {
                return;
            }
            let rows = fc.view.rows();
            let cur = fc.view.cursor_line1();
            let top = fc.view.top_line1();
            if cur >= top {
                let i = (cur - top) as usize;
                if let Some(r) = rows.get(i) {
                    let w = UnicodeWidthStr::width(r.text.as_str());
                    fc.hscroll = w.saturating_sub(cols.saturating_sub(8));
                }
            }
            return;
        }
        // 匹配窗：行文本按需从主文件窗读取
        let target = {
            let Content::Matches(mc) = &self.panes[self.focus].content else {
                return;
            };
            if mc.wrap {
                return;
            }
            mc.rows.get(mc.sel).map(|m| m.line_no)
        };
        let Some(no) = target else {
            return;
        };
        let w = self
            .main_read_line(no)
            .map(|t| UnicodeWidthStr::width(t.as_str()))
            .unwrap_or(0);
        if let Content::Matches(mc) = &mut self.panes[self.focus].content {
            mc.hscroll = w.saturating_sub(cols.saturating_sub(8));
        }
    }

    /// 焦点窗换行显示开关（w）。
    fn pane_toggle_wrap(&mut self) {
        let on = match &mut self.panes[self.focus].content {
            Content::File(fc) => {
                fc.wrap = !fc.wrap;
                if fc.wrap {
                    fc.hscroll = 0;
                }
                fc.wrap
            }
            Content::Matches(mc) => {
                mc.wrap = !mc.wrap;
                if mc.wrap {
                    mc.hscroll = 0;
                }
                mc.wrap
            }
        };
        self.set_msg(if on {
            "自动换行：开（超出部分显示在下方，续行带 ↪）"
        } else {
            "自动换行：关（长行用 ←/→ 或 h/l 水平滚动）"
        });
    }
}

/// resize 按键方向
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyDir {
    Left,
    Right,
    Up,
    Down,
}

impl MatchesContent {
    /// 滚动列表使选中行可见
    fn keep_visible(&mut self) {
        if self.rows.is_empty() {
            self.sel = 0;
            self.top = 0;
            return;
        }
        let h = self.inner_h.max(1);
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + h {
            self.top = self.sel + 1 - h;
        }
    }

    fn jump_to_file(&self) -> Option<u64> {
        self.rows.get(self.sel).map(|m| m.line_no)
    }

    /// 在当前列表中执行窗内搜索（不破坏列表；行文本按需从主视图读取）。
    /// 返回 Ok(匹配数)；正则非法返回 Err。超大列表给出护栏错误。
    fn do_search(
        &mut self,
        view: &mut FileView,
        pattern: &str,
        forward: bool,
        opts: &SearchPrefs,
    ) -> Result<usize, String> {
        if self.rows.len() > 20_000 {
            return Err("列表过大（>2 万行），窗内过滤略过；请回主窗用同一关键字搜索".to_string());
        }
        let re = crate::search::compile_regex(pattern, opts.case, opts.word)?;
        let mut match_idx: Vec<usize> = Vec::new();
        for (i, m) in self.rows.iter().enumerate() {
            let t = view.read_line_text(m.line_no).unwrap_or_default();
            if re.is_match(&t) {
                match_idx.push(i);
            }
        }
        if match_idx.is_empty() {
            self.search = None;
            return Ok(0);
        }
        // 首次定位（含当前行，与主窗一致）：/ 定位到 sel 之后（含自身）首个匹配；? 之前最近（含自身）
        let p = match_idx.binary_search(&self.sel);
        let len = match_idx.len();
        let first = if forward {
            match p {
                Ok(k) => Some(k),
                Err(k) if k < len => Some(k),
                _ => None,
            }
        } else {
            match p {
                Ok(k) => Some(k),
                Err(0) => None,
                Err(k) => Some(k - 1),
            }
        };
        self.search = Some(PaneSearch {
            query: pattern.to_string(),
            forward,
            match_idx: match_idx.clone(),
        });
        if let Some(k) = first {
            self.sel = match_idx[k];
            self.keep_visible();
        }
        Ok(len)
    }

    /// n/N：以当前选中行（光标）为参照，沿搜索方向找下一/上一处匹配（严格不含当前行）。
    /// 返回提示消息（None = 成功移动）。
    fn step_match(&mut self, reverse: bool) -> Option<String> {
        // 在借用外计算目标，避免多次借用 self
        let action = {
            let s = self.search.as_ref()?;
            let len = s.match_idx.len();
            // k = 插入点：sel 之前（含恰匹配时的自身下标）有 k 个匹配
            let (is_on, k) = match s.match_idx.binary_search(&self.sel) {
                Ok(k) => (true, k),
                Err(k) => (false, k),
            };
            // go_back = 与搜索方向一致（沿 forward）
            let go_back = s.forward != reverse;
            let target = if go_back {
                // 下一个匹配：严格大于 sel
                let t = if is_on { k + 1 } else { k };
                if t < len {
                    Some(t)
                } else {
                    None
                }
            } else {
                // 上一个匹配：严格小于 sel
                let t = if is_on { k } else { k }.checked_sub(1)?;
                Some(t)
            };
            match target {
                Some(t) => Some((t, s.match_idx[t])),
                None => None,
            }
        };
        match action {
            Some((_t, idx)) => {
                self.sel = idx;
                self.keep_visible();
                None
            }
            None => {
                let forward = self.search.as_ref().map(|s| s.forward).unwrap_or(true);
                let go_back = forward != reverse;
                Some(if go_back {
                    "已是最后一个匹配".to_string()
                } else {
                    "已是第一个匹配".to_string()
                })
            }
        }
    }

    fn clear_search(&mut self) {
        self.search = None;
    }

    /// 光标（选中行）恰为匹配行时的序号（用于状态栏）。
    fn search_pos(&self) -> Option<(usize, usize)> {
        let s = self.search.as_ref()?;
        let k = s.match_idx.binary_search(&self.sel).ok()?;
        Some((k + 1, s.match_idx.len()))
    }
}

impl App {
    /// 匹配窗选中行回车 → 文件窗跳上下文
    fn matches_jump_to_file(&mut self) {
        let line = {
            let Content::Matches(mc) = &self.panes[self.focus].content else {
                return;
            };
            match mc.jump_to_file() {
                Some(l) => l,
                None => return,
            }
        };
        if self.jump_file_to_line(line) {
            self.set_msg(format!("跳转到 {} 行", line));
        }
    }

    /// 匹配窗执行窗内搜索（作用域 = 当前窗格；行为与主窗一致）
    fn matches_do_search(&mut self, pattern: &str, forward: bool) {
        if self.focus == 0 {
            return;
        }
        let opts = self.opts;
        let result = {
            let (head, rest) = self.panes.split_at_mut(1);
            match (
                &mut head[0].content,
                rest.get_mut(self.focus - 1).map(|p| &mut p.content),
            ) {
                (Content::File(pf), Some(Content::Matches(mc))) => {
                    mc.do_search(&mut pf.view, pattern, forward, &opts)
                }
                _ => Err("该窗格不支持此搜索".to_string()),
            }
        };
        match result {
            Ok(0) => self.set_msg(format!("窗内无匹配: {pattern}")),
            Ok(n) => self.set_msg(format!("窗内搜索: {pattern}  → {n} 处匹配")),
            Err(e) => self.set_msg(format!("无效正则: {e}")),
        }
    }

    fn matches_clear_search(&mut self) {
        if let Content::Matches(mc) = &mut self.panes[self.focus].content {
            mc.clear_search();
        }
    }

    /// 匹配窗 n/N
    fn matches_step(&mut self, reverse: bool) {
        let note = {
            let Content::Matches(mc) = &mut self.panes[self.focus].content else {
                return;
            };
            if mc.search.is_none() {
                Some("该窗格还没有搜索（按 / 或 ? 开始）".to_string())
            } else {
                mc.step_match(reverse)
            }
        };
        if let Some(n) = note {
            self.set_msg(n);
        }
    }
}

// ---------- 命令输入 ----------

impl App {
    fn start_cmd(&mut self, forward: bool) {
        let hist = self.history.items().to_vec();
        self.mode = Mode::Cmd(CmdState {
            forward,
            buf: String::new(),
            cursor: 0,
            initial: String::new(),
            hist,
            hist_pos: None,
        });
    }

    fn commit_cmd(&mut self) {
        let (forward, pattern) = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Cmd(cs) => (cs.forward, cs.buf),
            m => {
                self.mode = m;
                return;
            }
        };
        let pattern = pattern.trim().to_string();
        if pattern.is_empty() {
            return;
        }
        if self.history.push(&pattern) {
            let _ = self.history.save();
        }
        if self.focus == 0 {
            self.file_start_search(&pattern, forward);
        } else {
            self.matches_do_search(&pattern, forward);
        }
    }

    /// 帮助面板按键：Esc / Enter / 空格 / q / F1 关闭返回。
    fn on_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc
            | KeyCode::Enter
            | KeyCode::Char(' ')
            | KeyCode::Char('q')
            | KeyCode::F(1) => self.mode = Mode::Normal,
            _ => {}
        }
    }

    /// 行号跳转（`:` 命令）输入开始。
    fn start_goto(&mut self) {
        self.mode = Mode::Goto(GotoState {
            buf: String::new(),
            cursor: 0,
        });
    }

    /// H：打开历史模糊选择器。
    fn start_history(&mut self) {
        if self.history.items().is_empty() {
            self.set_msg("还没有搜索历史");
            return;
        }
        let st = HistoryState {
            filter: String::new(),
            list: (0..self.history.items().len()).collect(),
            sel: 0,
        };
        self.mode = Mode::History(st);
    }

    /// 依当前 filter 重建候选列表。
    fn history_rebuild(st: &mut HistoryState, items: &[String]) {
        let q = st.filter.trim();
        let list: Vec<usize> = if q.is_empty() {
            (0..items.len()).collect()
        } else {
            items
                .iter()
                .enumerate()
                .filter(|(_, it)| fuzzy_subseq(q, it))
                .map(|(i, _)| i)
                .collect()
        };
        st.list = list;
        if st.sel >= st.list.len() {
            st.sel = st.list.len().saturating_sub(1);
        }
    }

    fn on_history_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                if let Mode::History(st) = &mut self.mode {
                    st.filter.pop();
                    Self::history_rebuild(st, self.history.items());
                }
            }
            KeyCode::Up => {
                if let Mode::History(st) = &mut self.mode {
                    if st.sel > 0 {
                        st.sel -= 1;
                    }
                }
            }
            KeyCode::Down => {
                if let Mode::History(st) = &mut self.mode {
                    if st.sel + 1 < st.list.len() {
                        st.sel += 1;
                    }
                }
            }
            KeyCode::Enter => {
                // 选中历史 → 填入命令输入框（可再编辑），再次 Enter 才执行
                let picked = {
                    let Mode::History(st) = &mut self.mode else {
                        return;
                    };
                    st.list.get(st.sel).map(|&i| self.history.items()[i].clone())
                };
                self.mode = Mode::Normal;
                if let Some(item) = picked {
                    let hist = self.history.items().to_vec();
                    let cursor = item.len();
                    self.mode = Mode::Cmd(CmdState {
                        forward: true,
                        buf: item,
                        cursor,
                        initial: String::new(),
                        hist,
                        hist_pos: None,
                    });
                    self.set_msg("按 Enter 执行该搜索，或先修改再 Enter");
                }
            }
            KeyCode::Char(c) => {
                if let Mode::History(st) = &mut self.mode {
                    st.filter.push(c);
                    Self::history_rebuild(st, self.history.items());
                }
            }
            _ => {}
        }
    }

    fn on_goto_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => self.goto_execute(),
            KeyCode::Backspace => {
                if let Mode::Goto(gs) = &mut self.mode {
                    if gs.cursor > 0 {
                        gs.buf.pop();
                        gs.cursor = gs.buf.len();
                    }
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Mode::Goto(gs) = &mut self.mode {
                    gs.buf.push(c);
                    gs.cursor = gs.buf.len();
                }
            }
            _ => {}
        }
    }

    /// 执行 `:` 行号跳转（1-based，均指原始日志文件行号）：
    /// 主窗 → 直接跳该行；子窗 → 定位到列表中该行（不存在则提示）。
    fn goto_execute(&mut self) {
        let text = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Goto(gs) => gs.buf,
            m => {
                self.mode = m;
                return;
            }
        };
        let line1: u64 = match text.trim().parse() {
            Ok(v) if v >= 1 => v,
            _ => {
                self.set_msg("无效行号");
                return;
            }
        };
        if self.focus == 0 {
            // 大文件行号跳转耗时，走分片 job（可 Ctrl+C / Esc 取消，状态栏显示 processing）
            self.start_jump(JumpKind::Goto(line1));
        } else {
            // 子窗：在列表中定位 line_no == line1 的行
            let target = {
                let Content::Matches(mc) = &self.panes[self.focus].content else {
                    return;
                };
                mc.rows
                    .binary_search_by(|m| m.line_no.cmp(&line1))
                    .ok()
            };
            match target {
                Some(i) => {
                    let Content::Matches(mc) = &mut self.panes[self.focus].content else {
                        return;
                    };
                    mc.sel = i;
                    mc.keep_visible();
                    self.set_msg(format!("列表内定位到 {line1} 行，按 Enter 跳转主窗"));
                }
                None => {
                    self.set_msg(format!("{line1} 行不在当前结果列表中"));
                }
            }
        }
    }

    /// 文件窗：启动 rg 搜索（模式 a）
    fn file_start_search(&mut self, pattern: &str, forward: bool) {
        let (case, word) = (self.opts.case, self.opts.word);
        // 预检正则（按当前大小写/整词选项，供行内高亮）
        let hl = match search::compile_regex(pattern, case, word) {
            Ok(r) => Some(r),
            Err(e) => {
                self.set_msg(format!("无效正则: {e}"));
                return;
            }
        };
        let (path, enc, cancel_old) = {
            let Content::File(fc) = &mut self.panes[0].content else {
                return;
            };
            let path = fc.view.file_path().to_string_lossy().into_owned();
            let enc = fc.view.encoding_label();
            let cancel_old = fc.search.take();
            (path, enc, cancel_old)
        };
        if let Some(old) = cancel_old {
            old.runner.cancel();
        }
        let path2 = path.clone();
        let pat2 = pattern.to_string();
        self.search_start = Some(std::time::Instant::now());
        match RgSearch::start(&path2, &pat2, &enc, case, word) {
            Ok(runner) => {
                if let Content::File(fc) = &mut self.panes[0].content {
                    fc.hl = hl;
                    fc.search = Some(ActiveSearch {
                        runner,
                        pattern: pat2,
                        forward,
                        matches: Vec::new(),
                        done: false,
                        pending_first: true,
                    });
                }
                self.set_msg(format!("搜索: {pattern} …"));
            }
            Err(e) => {
                self.set_msg(format!("搜索启动失败: {e}"));
            }
        }
    }

    fn on_cmd_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Char('c') if ctrl => self.mode = Mode::Normal,
            KeyCode::Enter => self.commit_cmd(),
            KeyCode::Backspace => self.cmd_edit(|s| {
                if s.cursor > 0 {
                    if let Some((idx, _)) = s.buf[..s.cursor].char_indices().last() {
                        s.buf.drain(idx..s.cursor);
                        s.cursor = idx;
                    }
                }
            }),
            KeyCode::Delete => self.cmd_edit(|s| {
                if s.cursor < s.buf.len() {
                    let end = s.cursor + s.buf[s.cursor..].chars().next().unwrap().len_utf8();
                    s.buf.drain(s.cursor..end);
                }
            }),
            KeyCode::Left => self.cmd_move_cursor(-1),
            KeyCode::Right => self.cmd_move_cursor(1),
            KeyCode::Home => self.cmd_edit(|s| s.cursor = 0),
            KeyCode::End => self.cmd_edit(|s| s.cursor = s.buf.len()),
            KeyCode::Char('u') if ctrl => self.cmd_edit(|s| {
                s.buf.clear();
                s.cursor = 0;
            }),
            KeyCode::Up => self.cmd_hist(-1),
            KeyCode::Down => self.cmd_hist(1),
            KeyCode::Char(c) => self.cmd_edit(move |s| {
                s.buf.insert(s.cursor, c);
                s.cursor += c.len_utf8();
            }),
            _ => {}
        }
    }

    fn cmd_edit(&mut self, f: impl FnOnce(&mut CmdState)) {
        if let Mode::Cmd(cs) = &mut self.mode {
            f(cs);
        }
    }

    fn cmd_move_cursor(&mut self, dir: i64) {
        self.cmd_edit(|cs| {
            if dir < 0 {
                if let Some((idx, _)) = cs.buf[..cs.cursor].char_indices().last() {
                    cs.cursor = idx;
                }
            } else if cs.cursor < cs.buf.len() {
                cs.cursor += cs.buf[cs.cursor..].chars().next().unwrap().len_utf8();
            }
        });
    }

    /// ↑ / ↓ 浏览历史候选（less 语义）。
    ///
    /// 历史列表 hist[0] 为最近一次搜索（已去重，链表头 = 最近）。遍历模型：
    /// - ↑：第一次跳到最近的 hist[0]；继续 ↑ 依次更旧（下标 +1），到最旧即停；
    /// - ↓：从历史逐步回到较新（下标 -1），下标 0 再按 ↓ 回到"进入命令模式时的当前输入"，到此后 ↓ 不再有动作；
    /// - 相同关键字只保留最近一次（见 History::push 去重置顶）。
    fn cmd_hist(&mut self, dir: i64) {
        let Mode::Cmd(cs) = &mut self.mode else {
            return;
        };
        if cs.hist.is_empty() {
            return;
        }
        if dir < 0 {
            // ↑：向更旧
            match cs.hist_pos {
                None => {
                    cs.initial = cs.buf.clone();
                    cs.hist_pos = Some(0);
                    cs.buf = cs.hist[0].clone();
                }
                Some(p) if p + 1 < cs.hist.len() => {
                    cs.hist_pos = Some(p + 1);
                    cs.buf = cs.hist[p + 1].clone();
                }
                Some(_) => {} // 已到最旧
            }
        } else {
            // ↓：向较新，直到回到当前输入
            match cs.hist_pos {
                None => {}
                Some(0) => {
                    cs.hist_pos = None;
                    cs.buf = cs.initial.clone();
                }
                Some(p) => {
                    cs.hist_pos = Some(p - 1);
                    cs.buf = cs.hist[p - 1].clone();
                }
            }
        }
        if let Mode::Cmd(cs) = &mut self.mode {
            cs.cursor = cs.buf.len();
        }
    }
}

// ---------- 布局 ----------

impl App {
    /// 可见窗格（id, 矩形）列表：zoom 时只含放大窗占满区域；否则按布局树。
    fn visible_panes(&self, area: Rect) -> Vec<(usize, Rect)> {
        let mut out = Vec::new();
        let pairs: Vec<(u64, crate::layout::RectLike)> = if let Some(zid) = self.zoom {
            self.layout
                .leaf_ids()
                .into_iter()
                .filter(|id| *id == zid)
                .map(|id| (id, crate::layout::RectLike { x: area.x, y: area.y, w: area.width, h: area.height }))
                .collect()
        } else {
            self.layout.layout(crate::layout::RectLike {
                x: area.x,
                y: area.y,
                w: area.width,
                h: area.height,
            })
        };
        for (id, r) in pairs {
            if let Some(idx) = self.panes.iter().position(|p| p.id == id) {
                out.push((
                    idx,
                    Rect::new(r.x, r.y, r.w.max(1), r.h.max(1)),
                ));
            }
        }
        out
    }
}

// ---------- 渲染 ----------

impl App {
    /// 文件窗 wrap：非文件尾时光标行首段保持可见（视觉 2/3 附近，不落在屏外）。
    fn wrap_cursor_fit(fc: &mut FileContent, cols: usize) {
        if !fc.wrap {
            return;
        }
        let need = fc.inner_h.max(4);
        let target = (need * 2) / 3;
        for _ in 0..6 {
            fc.view.fill_to(fc.inner_h);
            let (prefix, seg_cur, idx) = {
                let rows = fc.view.rows();
                if rows.is_empty() {
                    return;
                }
                let top = fc.view.top_line1();
                let cur = fc.view.cursor_line1();
                let idx = (cur - top) as usize;
                if idx >= rows.len() || idx == 0 {
                    return; // 光标位于首行，必然可见
                }
                let ln_w = (top + rows.len() as u64 - 1).to_string().len().max(4);
                let budget = cols.saturating_sub(1 + ln_w + 1).max(4);
                let mut prefix = 0usize;
                for r in rows.iter().take(idx) {
                    let t = r.text.as_str();
                    prefix += if UnicodeWidthStr::width(t) <= budget {
                        1
                    } else {
                        wrap_cols(t, budget).len()
                    };
                }
                let t = rows[idx].text.as_str();
                let seg_cur = if UnicodeWidthStr::width(t) <= budget {
                    1
                } else {
                    wrap_cols(t, budget).len()
                };
                (prefix, seg_cur, idx)
            };
            let want = if seg_cur <= need {
                (need - seg_cur).min(target)
            } else {
                0
            };
            if prefix <= want {
                return;
            }
            let avg = (prefix / idx).max(1);
            let drop = ((prefix - want) as f64 / avg as f64).ceil() as u64;
            let t = (fc.view.top_line1() - 1) + drop;
            fc.view.set_top(t);
        }
    }

    /// 子窗（匹配列表）wrap：非末尾时选中行保持可见（文本按需从主视图读取）。
    fn wrap_list_cursor_fit(mc: &mut MatchesContent, view: &mut FileView, cols: usize) {
        if !mc.wrap || mc.rows.is_empty() {
            return;
        }
        let last = mc.rows.len() - 1;
        if mc.sel == last || mc.sel <= mc.top {
            return;
        }
        let need = mc.inner_h.max(4);
        let target = (need * 2) / 3;
        let ln_w = mc.rows[last].line_no.to_string().len().max(4);
        let budget = cols.saturating_sub(1 + ln_w + 1).max(4);
        let seg_of = |t: &str| -> usize {
            if UnicodeWidthStr::width(t) <= budget {
                1
            } else {
                wrap_cols(t, budget).len()
            }
        };
        for _ in 0..6 {
            if mc.sel <= mc.top {
                return;
            }
            let idx = mc.sel - mc.top;
            let mut prefix = 0usize;
            for k in mc.top..mc.sel {
                let no = mc.rows[k].line_no;
                let t = view.read_line_text(no).unwrap_or_default();
                prefix += seg_of(&t);
            }
            let no_cur = mc.rows[mc.sel].line_no;
            let cur_text = view.read_line_text(no_cur).unwrap_or_default();
            let seg_cur = seg_of(&cur_text);
            let want = if seg_cur <= need {
                (need - seg_cur).min(target)
            } else {
                0
            };
            if prefix <= want {
                return;
            }
            let avg = (prefix / idx).max(1);
            let drop = ((prefix - want) as f64 / avg as f64).ceil() as usize;
            mc.top = (mc.top + drop).min(mc.sel);
        }
    }

    /// 子窗（匹配列表）wrap 模式：选中行位于列表末尾时，把 top 调整到视觉行贴住列表尾部。
    fn wrap_list_bottom_fit(mc: &mut MatchesContent, view: &mut FileView, cols: usize) {
        if !mc.wrap || mc.rows.is_empty() {
            return;
        }
        let last = mc.rows.len() - 1;
        if mc.sel != last {
            return; // 不在列表末尾
        }
        // 大列表逐行读原文统计太慢：仅对 ≤1500 行的列表做精确贴底
        if mc.rows.len() > 1500 {
            return;
        }
        let ln_w = mc.rows[last].line_no.to_string().len().max(4);
        let budget = cols.saturating_sub(1 + ln_w + 1).max(4);
        let need = mc.inner_h.max(4);
        let mut segs: Vec<usize> = Vec::with_capacity(mc.rows.len());
        for m in mc.rows.iter() {
            let t = view.read_line_text(m.line_no).unwrap_or_default();
            segs.push(if UnicodeWidthStr::width(t.as_str()) <= budget {
                1
            } else {
                wrap_cols(t.as_str(), budget).len()
            });
        }
        let mut top = mc.top.min(last);
        for _ in 0..12 {
            let vs: usize = segs[top..].iter().sum();
            if (vs as i64 - need as i64).abs() <= 1 {
                break;
            }
            let n = last - top + 1; // 剩余逻辑行数（至少 1）
            let avg = (vs as f64 / n.max(1) as f64).max(1.0);
            if vs > need {
                let drop = ((vs - need) as f64 / avg).ceil() as usize;
                let nt = top + drop;
                if nt >= last {
                    top = last;
                    break;
                }
                top = nt;
            } else if top > 0 {
                let add = ((need - vs) as f64 / avg).ceil() as usize;
                top = top.saturating_sub(add);
            } else {
                break;
            }
        }
        mc.top = top;
    }

    /// wrap 模式下，光标位于文件尾部时把视口顶行调整到「视觉行恰好贴住文件尾」。
    fn wrap_tail_fit(fc: &mut FileContent, cols: usize) {
        if !fc.wrap {
            return;
        }
        let Some(total) = fc.view.known_total() else {
            return;
        };
        if fc.view.cursor_line1() < total {
            return; // 不在文件尾
        }
        let need = fc.inner_h.max(4);
        for _ in 0..12 {
            fc.view.fill_to(fc.inner_h);
            let (vs, n) = {
                let rows = fc.view.rows();
                if rows.is_empty() {
                    return;
                }
                let top = fc.view.top_line1();
                let last = top + rows.len() as u64 - 1;
                let ln_w = last.to_string().len().max(4);
                let budget = cols.saturating_sub(1 + ln_w + 1).max(4);
                let mut vs = 0usize;
                for r in rows.iter() {
                    let t = r.text.as_str();
                    vs += if UnicodeWidthStr::width(t) <= budget {
                        1
                    } else {
                        wrap_cols(t, budget).len()
                    };
                }
                (vs, rows.len())
            };
            if n == 0 {
                return;
            }
            if (vs as i64 - need as i64).abs() <= 1 {
                return;
            }
            let avg = (vs as f64 / n as f64).max(1.0);
            let cur_row = (fc.view.cursor_line1() - 1) as i64;
            let cur_top = fc.view.top_line1() as i64 - 1;
            if vs > need {
                let drop = ((vs - need) as f64 / avg).ceil() as i64;
                let t = (cur_top + drop).min(cur_row).max(0) as u64;
                fc.view.set_top(t);
            } else if cur_top > 0 {
                let add = ((need - vs) as f64 / avg).ceil() as i64;
                let t = (cur_top - add).max(0) as u64;
                fc.view.set_top(t);
            } else {
                return;
            }
        }
        fc.view.fill_to(fc.inner_h);
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.height < 3 {
            return;
        }
        let bottom_h = if self.mode.is_cmd() || self.mode.is_pick() || self.mode.is_goto() {
            1
        } else {
            0
        };
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(bottom_h),
        ])
        .split(area);
        let content_area = chunks[0];
        if self.mode.is_history() {
            self.render_history(frame, content_area);
            self.render_status(frame, chunks[1]);
            return;
        }
        if self.mode.is_help() {
            self.render_help(frame, content_area);
            self.render_status(frame, chunks[1]);
            return;
        }
        // 1) 布局 + 预填充内容：先文件窗，再匹配窗（经主视图读取行文本）
        let visible = self.visible_panes(content_area);
        for (idx, rect) in visible.iter() {
            if *idx != 0 {
                continue;
            }
            let inner_h = rect.height.saturating_sub(2).max(1) as usize;
            if let Content::File(fc) = &mut self.panes[0].content {
                fc.inner_h = inner_h;
                fc.view.fill_to(inner_h);
                if fc.wrap {
                    let cols = rect.width.saturating_sub(2).max(1) as usize;
                    let tail = fc
                        .view
                        .known_total()
                        .map(|t| fc.view.cursor_line1() >= t)
                        .unwrap_or(false);
                    if tail {
                        Self::wrap_tail_fit(fc, cols);
                    } else {
                        Self::wrap_cursor_fit(fc, cols);
                    }
                }
            }
        }
        for (idx, rect) in visible.iter() {
            if *idx == 0 {
                continue;
            }
            let inner_h = rect.height.saturating_sub(2).max(1) as usize;
            let cols = rect.width.saturating_sub(2).max(1) as usize;
            let (head, rest) = self.panes.split_at_mut(1);
            let Content::File(pf) = &mut head[0].content else {
                continue;
            };
            let rel = *idx - 1;
            let Content::Matches(mc) = &mut rest[rel].content else {
                continue;
            };
            mc.inner_h = inner_h;
            mc.keep_visible();
            if mc.wrap {
                if mc.rows.is_empty() || mc.sel == mc.rows.len() - 1 {
                    Self::wrap_list_bottom_fit(mc, &mut pf.view, cols);
                } else {
                    Self::wrap_list_cursor_fit(mc, &mut pf.view, cols);
                }
            }
        }
        // 2) 收割（可能触发跳转重建视口，再次填充）
        self.harvest_file_search();
        for (idx, _rect) in visible.iter() {
            if *idx == 0 {
                if let Content::File(fc) = &mut self.panes[0].content {
                    fc.view.fill_to(fc.inner_h);
                }
            }
        }
        // 3) 绘制各窗格
        for (idx, rect) in visible.iter() {
            self.render_pane(frame, *idx, *rect);
        }
        self.render_status(frame, chunks[1]);
        if self.mode.is_cmd() {
            self.render_cmdline(frame, chunks[2]);
        } else if self.mode.is_pick() {
            self.render_pick(frame, chunks[2]);
        } else if self.mode.is_goto() {
            self.render_goto(frame, chunks[2]);
        }
    }

    fn render_pane(&mut self, frame: &mut Frame, idx: usize, rect: Rect) {
        if idx == self.focus {
            self.last_cols = rect.width.saturating_sub(2).max(1) as usize;
        }
        let focused = idx == self.focus;
        let mut title = self.pane_title(idx);
        if self.zoom == Some(self.panes[idx].id) {
            title = format!("[ZOOM] {title}");
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if focused {
                Style::default()
                    .fg(self.theme.focus_border)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(self.theme.inactive_border)
            });
        let cols = rect.width.saturating_sub(2).max(1);
        let content = if idx == 0 {
            let fc = match &self.panes[0].content {
                Content::File(fc) => fc,
                _ => return,
            };
            self.file_lines(fc, cols)
        } else {
            let (head, rest) = self.panes.split_at_mut(1);
            match (
                &mut head[0].content,
                rest.get_mut(idx - 1).map(|p| &mut p.content),
            ) {
                (Content::File(pf), Some(Content::Matches(mc))) => {
                    Self::build_matches_lines(&mut pf.view, mc, cols, &self.theme, self.opts)
                }
                _ => Vec::new(),
            }
        };
        frame.render_widget(Paragraph::new(content).block(block), rect);
    }

    /// 把一行内容排成可视行（支持水平滚动或软换行），返回 span 列表与续行。
    /// budget_first/budget_cont 为内容可用列数（不含前缀）。
    fn push_row_lines(
        out: &mut Vec<Line<'static>>,
        no_text: String,
        no_width: usize,
        cur: bool,
        text: &str,
        hscroll: usize,
        wrap: bool,
        budget_first: usize,
        hl: Option<&Regex>,
        theme: &crate::theme::Theme,
        kw: Color,
        sel_row: bool,
        cursor_marker: bool,
    ) {
        let marker = || -> Span<'static> {
            Span::styled(
                if cursor_marker { "▶" } else { " " },
                Style::new().fg(theme.cursor_marker).add_modifier(Modifier::BOLD),
            )
        };
        let no_span = || -> Span<'static> {
            Span::styled(
                format!("{no_text:>no_width$}│"),
                Style::new()
                    .fg(theme.line_number)
                    .add_modifier(Modifier::DIM),
            )
        };
        let ln_dim = || -> Span<'static> {
            Span::styled(
                " ".repeat(no_width),
                Style::new().fg(theme.line_number),
            )
        };
        if wrap {
            let mut segs: Vec<&str> = Vec::new();
            let mut rest = text;
            if UnicodeWidthStr::width(rest) > budget_first {
                let cut = byte_after_cols(rest, budget_first).max(1);
                if cut < rest.len() {
                    segs.push(&rest[..cut]);
                    rest = &rest[cut..];
                }
            }
            while UnicodeWidthStr::width(rest) > budget_first {
                let cut = byte_after_cols(rest, budget_first).max(1);
                if cut >= rest.len() {
                    break;
                }
                segs.push(&rest[..cut]);
                rest = &rest[cut..];
            }
            if !rest.is_empty() {
                segs.push(rest);
            }
            if segs.is_empty() {
                segs.push(""); // 空行也需占一行显示
            }
            for (k, seg) in segs.into_iter().enumerate() {
                let mut spans = Vec::new();
                if k == 0 {
                    spans.push(marker());
                    spans.push(no_span());
                } else {
                    // 续行：↪ 顶最左 + 空行号区 + │ 延续竖线，文本与首行内容左对齐
                    spans.push(Span::styled(
                        "↪",
                        Style::new()
                            .fg(theme.line_number)
                            .add_modifier(Modifier::DIM),
                    ));
                    spans.push(ln_dim());
                    spans.push(Span::styled(
                        "│",
                        Style::new()
                            .fg(theme.line_number)
                            .add_modifier(Modifier::DIM),
                    ));
                }
                append_highlighted(&mut spans, seg, hl, cur, sel_row, theme, kw);
                out.push(Line::from(spans));
            }
        } else {
            let seg = slice_cols(text, hscroll, budget_first);
            let mut spans = vec![marker(), no_span()];
            append_highlighted(&mut spans, seg, hl, cur, sel_row, theme, kw);
            out.push(Line::from(spans));
        }
    }

    fn file_lines(&self, fc: &FileContent, cols: u16) -> Vec<Line<'static>> {
        let top = fc.view.top_line1();
        let rows = fc.view.rows();
        let cursor_line1 = fc.view.cursor_line1();
        // 行内匹配高亮色由 [theme].keyword 配置
        let kw = self.theme.keyword;
        // 视觉选择选区（整行高亮）
        let sel_span = self.visual_row_span();
        let ln_w = top.saturating_add(rows.len() as u64).to_string().len().max(4);
        let cols = cols as usize;
        let budget1 = cols.saturating_sub(1 + ln_w + 1).max(4);
        let mut lines = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let line1 = top + i as u64;
            let is_cursor = cursor_line1 == line1;
            // 视觉选择模式：列级选中高亮（光标行无整行底色，选中字符段灰底）
            if let Mode::Visual(v) = &self.mode {
                let text = row.text.as_str();
                let nchars = text.chars().count();
                // 本行选中的字符区间 [lo, hi)（含端点换算为 hi = last+1）
                let r1 = v.anchor_row.min(cursor_line1);
                let r2 = v.anchor_row.max(cursor_line1);
                let lo_hi = if line1 < r1 || line1 > r2 {
                    None
                } else if v.kind == crate::app::VisualKind::Line {
                    Some((0usize, nchars))
                } else if v.kind == crate::app::VisualKind::Block {
                    let (c1, c2) = (v.anchor_col.min(v.cur_col), v.anchor_col.max(v.cur_col));
                    Some((c1.min(nchars), (c2 + 1).min(nchars)))
                } else {
                    // Char 模式：单行取两端列；多行首/末行取各自端点列，中间行全行
                    if r1 == r2 {
                        let (c1, c2) = (v.anchor_col.min(v.cur_col), v.anchor_col.max(v.cur_col));
                        Some((c1.min(nchars), (c2 + 1).min(nchars)))
                    } else if line1 == r1 {
                        let edge = if v.anchor_row == r1 {
                            v.anchor_col
                        } else {
                            v.cur_col
                        };
                        Some((edge.min(nchars), nchars))
                    } else if line1 == r2 {
                        let edge = if v.anchor_row == r2 {
                            v.anchor_col
                        } else {
                            v.cur_col
                        };
                        Some((0, (edge + 1).min(nchars)))
                    } else {
                        Some((0, nchars))
                    }
                };
                let (lo, hi) = match lo_hi {
                    Some((a, b)) => (a, b),
                    None => (0, 0),
                };
                let empty_sel = match lo_hi {
                    Some((a, b)) => a >= b,
                    None => true,
                };
                // 光标字符列（蓝格）；光标在行尾后时用尾部空格块
                let cur_cell = if is_cursor { Some(v.cur_col) } else { None };
                let dim_ln = || -> Span<'static> {
                    Span::styled(
                        "│",
                        Style::new()
                            .fg(self.theme.line_number)
                            .add_modifier(Modifier::DIM),
                    )
                };
                let segs: Vec<String> = if fc.wrap {
                    if UnicodeWidthStr::width(text) <= budget1 {
                        vec![text.to_string()]
                    } else {
                        wrap_cols(text, budget1)
                    }
                } else {
                    vec![text.to_string()]
                };
                let cur_st = Style::new()
                    .bg(self.theme.cursor_line_bg)
                    .fg(self.theme.cursor_line_text);
                let sel_st = Style::new().bg(Color::DarkGray).fg(Color::White);
                let plain_st = Style::new().fg(self.theme.text);
                let mut gi = 0usize; // 全局字符下标（跨段累计）
                for (k, seg) in segs.iter().enumerate() {
                    let mut spans: Vec<Span> = Vec::new();
                    if k == 0 {
                        spans.push(Span::styled(
                            if is_cursor { "▶" } else { " " },
                            Style::new()
                                .fg(self.theme.cursor_marker)
                                .add_modifier(Modifier::BOLD),
                        ));
                        spans.push(Span::styled(
                            format!("{line1:>ln_w$}"),
                            Style::new()
                                .fg(self.theme.line_number)
                                .add_modifier(Modifier::DIM),
                        ));
                    } else {
                        spans.push(Span::styled(
                            "↪",
                            Style::new()
                                .fg(self.theme.line_number)
                                .add_modifier(Modifier::DIM),
                        ));
                        spans.push(Span::styled(
                            " ".repeat(ln_w),
                            Style::new().fg(self.theme.line_number),
                        ));
                    }
                    spans.push(dim_ln());
                    for ch in seg.chars() {
                        let st = if Some(gi) == cur_cell {
                            cur_st.clone()
                        } else if !empty_sel && gi >= lo && gi < hi {
                            sel_st.clone()
                        } else {
                            plain_st.clone()
                        };
                        spans.push(Span::styled(ch.to_string(), st));
                        gi += 1;
                    }
                    // 光标位于行尾后：在本行最后一段尾部追加空格光标块
                    if is_cursor
                        && cur_cell.unwrap_or(0) >= nchars
                        && k + 1 == segs.len()
                    {
                        spans.push(Span::styled(" ", cur_st.clone()));
                    }
                    lines.push(Line::from(spans));
                }
                continue;
            }
            // 普通模式：整行光标蓝底 + 关键字高亮
            let cur_arg = is_cursor;
            Self::push_row_lines(
                &mut lines,
                line1.to_string(),
                ln_w,
                cur_arg,
                &row.text,
                fc.hscroll,
                fc.wrap,
                budget1,
                fc.hl.as_ref(),
                &self.theme,
                kw,
                false,
                is_cursor,
            );
        }
        lines
    }

    /// 结果窗渲染：按视口行号从主视图逐行读取原文后排版（文本不驻留）。
    fn build_matches_lines(
        view: &mut FileView,
        mc: &MatchesContent,
        cols: u16,
        theme: &crate::theme::Theme,
        opts: SearchPrefs,
    ) -> Vec<Line<'static>> {
        if mc.rows.is_empty() {
            return vec![Line::from(Span::styled(
                "(空)",
                Style::new().fg(Color::DarkGray),
            ))];
        }
        let ln_w = mc
            .rows
            .last()
            .map(|m| m.line_no.to_string().len())
            .unwrap_or(6)
            .max(4);
        let hl_re = match &mc.search {
            Some(s) => crate::search::compile_regex(&s.query, opts.case, opts.word).ok(),
            None => None,
        };
        let end = (mc.top + mc.inner_h).min(mc.rows.len());
        let cols = cols as usize;
        let budget1 = cols.saturating_sub(1 + ln_w + 1).max(4);
        let kw = theme.keyword;
        let mut lines = Vec::new();
        for ri in mc.top..end {
            let no = mc.rows[ri].line_no;
            let text = view.read_line_text(no).unwrap_or_default();
            let cur = ri == mc.sel;
            Self::push_row_lines(
                &mut lines,
                no.to_string(),
                ln_w,
                cur,
                &text,
                mc.hscroll,
                mc.wrap,
                budget1,
                hl_re.as_ref(),
                theme,
                kw,
                false,
                cur,
            );
        }
        lines
    }

    fn render_status(&mut self, frame: &mut Frame, area: Rect) {
        let mut text = String::new();
        if self.pending_jump.is_some() {
            text.push_str("[processing...]  ");
        }
        // 关键字截断上限：随窗口列宽动态（窗口越窄/字体越大列数越少 → 上限越小）
        let kw_lim = (area.width as usize)
            .saturating_mul(18)
            .saturating_div(100)
            .clamp(14, 90);
        match &self.panes[self.focus].content {
            Content::File(fc) => {
                let rows_n = fc.view.row_count();
                let top = fc.view.top_line1();
                let last = if rows_n == 0 {
                    top
                } else {
                    top + rows_n as u64 - 1
                };
                text.push_str(&format!("行 {top}-{last}"));
                // 百分比（光标所在行 / 已知总行数，less 风格）
                if let Some(total) = fc.known_total() {
                    let cur = fc.view.cursor_line1();
                    let pct = if total > 0 {
                        ((cur - 1) * 100 / total).min(100)
                    } else {
                        100
                    };
                    text.push_str(&format!("/{total}  ({pct}%)"));
                } else {
                    text.push_str("/?");
                }
                if let Some(a) = &fc.search {
                    let dir = if a.forward { "/" } else { "?" };
                    let total = if a.done {
                        a.matches.len().to_string()
                    } else {
                        format!("≥{}", a.matches.len())
                    };
                    // 光标行若是匹配行则显示序号
                    let cur = fc.view.cursor_line1();
                    let seq = match a.matches.binary_search_by(|m| m.line_no.cmp(&cur)) {
                        Ok(i) => format!("{}/{}", i + 1, total),
                        Err(_) => total,
                    };
                    text.push_str(&format!("  {dir}{}  匹配 {seq}", clip_str(&a.pattern, kw_lim)));
                }
            }
            Content::Matches(mc) => {
                text.push_str(&format!(
                    "{} 共 {} 行  选中原始行 {}",
                    clip_str(&mc.tag, kw_lim),
                    mc.rows.len(),
                    mc.rows.get(mc.sel).map(|m| m.line_no).unwrap_or(0),
                ));
                if let Some((k, n)) = mc.search_pos() {
                    let q = mc.search.as_ref().unwrap();
                    text.push_str(&format!("  匹配 {k}/{n}  {}", clip_str(&q.query, kw_lim)));
                }
            }
        }
        // 搜索选项状态（大小写 / 整词）
        text.push_str(&format!(
            "   [大小写: {}] [整词: {}]",
            self.opts.case.label(),
            if self.opts.word { "开" } else { "关" }
        ));
        if let Some(t) = &self.trans {
            text.push_str(&format!("   [ {t} ]"));
        } else if !self.msg.is_empty() {
            text.push_str(&format!("   {msg}", msg = self.msg));
        }
        let st = if self.theme.status_fg.is_some() || self.theme.status_bg.is_some() {
            let mut s = Style::new();
            if let Some(f) = self.theme.status_fg {
                s = s.fg(f);
            }
            if let Some(b) = self.theme.status_bg {
                s = s.bg(b);
            }
            s
        } else {
            Style::new().add_modifier(Modifier::REVERSED)
        };
        let line = Line::from(Span::styled(text, st));
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_cmdline(&mut self, frame: &mut Frame, area: Rect) {
        let Mode::Cmd(cs) = &self.mode else {
            return;
        };
        let prompt = if cs.forward { "/" } else { "?" };
        // 输入的关键字与提示符同色（/ 用 prompt_fwd，? 用 prompt_back）
        let col = if cs.forward {
            self.theme.prompt_fwd
        } else {
            self.theme.prompt_back
        };
        let mut spans = vec![Span::styled(prompt, Style::new().fg(col))];
        // 可视输入区宽度（扣除提示符占的 1 列）
        let vis = (area.width as usize).saturating_sub(1);
        let text = cs.buf.as_str();
        let cut = cs.cursor.min(cs.buf.len());
        let c_w = UnicodeWidthStr::width(&text[..cut]);
        let cur_style = Style::new()
            .fg(col)
            .add_modifier(Modifier::REVERSED);
        let dim = || -> Span<'static> {
            Span::styled(
                "...",
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            )
        };
        if vis > 0 {
            let tw = vis;
            let t_w = UnicodeWidthStr::width(text);
            // 光标右侧是否还有内容（决定是否显示右侧省略号）
            let right_need = c_w < t_w;
            // 滚动与省略号预算互相影响：迭代两次收敛
            let mut left_ell = false;
            let mut avail = tw;
            let mut scroll = 0usize;
            for _ in 0..2 {
                avail = tw
                    .saturating_sub(3 * ((left_ell as usize) + (right_need as usize)))
                    .max(1);
                scroll = if c_w < avail { 0 } else { c_w - avail + 1 };
                left_ell = scroll > 0;
            }
            let right_ell = right_need;
            // 从 scroll 列开始切 avail 列文本
            let startb = byte_after_cols(text, scroll);
            let seg = &text[startb..];
            let endb = byte_after_cols(seg, avail);
            let shown = &seg[..endb.min(seg.len())];
            if left_ell {
                spans.push(dim());
            }
            // 光标在可用文本区中的列（锚定最右列）
            let cr = c_w.saturating_sub(scroll);
            let cbb = byte_after_cols(shown, cr).min(shown.len());
            let (before, rest) = shown.split_at(cbb);
            spans.push(Span::styled(before, Style::new().fg(col)));
            match rest.chars().next() {
                Some(ch) => {
                    let (ch, after) = rest.split_at(ch.len_utf8());
                    spans.push(Span::styled(ch.to_string(), cur_style));
                    spans.push(Span::styled(after, Style::new().fg(col)));
                }
                None => {
                    spans.push(Span::styled(" ", cur_style));
                }
            }
            if right_ell {
                spans.push(dim());
            }
        } else {
            // 极窄终端：至少显示提示符与光标
            spans.push(Span::styled(" ", cur_style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_history(&mut self, frame: &mut Frame, area: Rect) {
        let Mode::History(st) = &self.mode else {
            return;
        };
        let items = self.history.items().clone();
        let filter = st.filter.clone();
        let list = st.list.clone();
        let sel = st.sel;
        let rows_h = area.height.saturating_sub(2) as usize;
        let mut lines: Vec<Line> = Vec::new();
        // 顶部：过滤输入行
        lines.push(Line::from(vec![
            Span::styled(
                "历史搜索  ",
                Style::new().fg(self.theme.panel_border).add_modifier(Modifier::BOLD),
            ),
            Span::styled(filter.clone(), Style::new().fg(Color::White)),
            Span::styled(
                if filter.is_empty() {
                    "  (输入过滤  ↑↓选择  Enter=填入可编辑  再次 Enter 执行  Esc取消)"
                } else {
                    "  (↑↓选择  Enter=填入  再次 Enter 执行  Esc取消)"
                },
                Style::new().fg(Color::DarkGray),
            ),
        ]));
        lines.push(Line::from(""));
        if list.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("无匹配历史：{filter}"),
                Style::new().fg(Color::Red),
            )));
        } else {
            let shown = rows_h.saturating_sub(2).min(list.len());
            for k in 0..shown {
                let idx = list[k];
                let cur = k == sel;
                let text = &items[idx];
                let mut spans = vec![Span::styled(
                    if cur { "▶ " } else { "  " },
                    Style::new().fg(self.theme.cursor_marker),
                )];
                spans.push(Span::styled(
                    format!("{idx:>3}  "),
                    Style::new().fg(Color::DarkGray),
                ));
                let st2 = if cur {
                    Style::new()
                        .bg(self.theme.cursor_line_bg)
                        .fg(Color::White)
                } else {
                    Style::new().fg(Color::Gray)
                };
                spans.push(Span::styled(text.clone(), st2));
                lines.push(Line::from(spans));
            }
            if list.len() > shown {
                lines.push(Line::from(Span::styled(
                    format!("… 共 {} 条", list.len()),
                    Style::new().fg(Color::DarkGray),
                )));
            }
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" 历史关键字（模糊匹配） ")
            .border_style(Style::default().fg(self.theme.panel_border));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_help(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled(
                "logviewer 帮助（键位来自当前配置 config.toml，改动后重启生效）",
                Style::new()
                    .fg(self.theme.focus_border)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("   Esc/q/空格/F1 关闭", Style::new().fg(Color::DarkGray)),
        ]));
        lines.push(Line::from(""));
        let max_rows = area.height.saturating_sub(2) as usize;
        for (act, keys) in self.keys.help_entries() {
            if lines.len() >= max_rows {
                break;
            }
            let label = crate::keymap::Keymap::action_label(act);
            lines.push(Line::from(vec![
                Span::styled(label, Style::new().fg(Color::White)),
                Span::styled("    ", Style::new().fg(Color::DarkGray)),
                Span::styled(keys, Style::new().fg(self.theme.keyword)),
            ]));
        }
        if lines.len() < max_rows {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "提示：结果窗与主窗行为一致（/ 搜索、n/N 跳转、Enter 回主窗）；h/j/k/l 为结果窗放置方位；配置键位字符串格式见 config.toml 注释。",
                Style::new().fg(Color::DarkGray),
            )));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" 键位帮助 ")
            .border_style(Style::default().fg(self.theme.panel_border));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_pick(&mut self, frame: &mut Frame, area: Rect) {
        let n = self.panes.len();
        let line = Line::from(Span::styled(
            format!(
                "结果放入 →  h/j/k/l = 新窗在焦点窗左/下/上/右（Enter 默认右侧） | 1-{n} 覆盖（1 为文件窗） | Esc 取消"
            ),
            Style::new()
                .fg(self.theme.panel_border)
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_goto(&mut self, frame: &mut Frame, area: Rect) {
        let Mode::Goto(gs) = &self.mode else {
            return;
        };
        let mut spans = vec![Span::styled(
            ":",
            Style::new().fg(self.theme.cursor_marker),
        )];
        spans.push(Span::styled(&gs.buf, Style::new().fg(Color::White)));
        let cur_style = Style::new().add_modifier(Modifier::REVERSED);
        if gs.cursor >= gs.buf.len() {
            spans.push(Span::styled(" ", cur_style));
        }
        spans.push(Span::styled(
            "  (输入行号 Enter 跳转，Esc 取消)",
            Style::new().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

// ---------- 行内高亮 ----------

/// 历史模糊匹配：query 的字符按顺序出现在 text 中（忽略大小写）。
fn fuzzy_subseq(query: &str, text: &str) -> bool {
    let mut it = text.chars();
    for q in query.chars() {
        match it.find(|c| c.eq_ignore_ascii_case(&q)) {
            Some(_) => continue,
            None => return false,
        }
    }
    true
}

/// 将一行文本按高亮正则拆成带色 span。
/// 行底色：光标行 → 主题光标行底色；选区行(sel_row) → 灰色底；均无 → 无底色。
fn append_highlighted(
    spans: &mut Vec<Span>,
    text: &str,
    hl: Option<&Regex>,
    cur: bool,
    sel_row: bool,
    theme: &crate::theme::Theme,
    kw: Color,
) {
    let base = if cur {
        theme.cursor_line_text
    } else if sel_row {
        Color::White
    } else {
        theme.text
    };
    let bg = if cur {
        Some(theme.cursor_line_bg)
    } else if sel_row {
        Some(Color::DarkGray)
    } else {
        None
    };
    let mk = |s: String, fg: Color, bold: bool| -> Span {
        let mut st = Style::new().fg(fg);
        if let Some(b) = bg {
            st = st.bg(b);
        }
        if bold {
            st = st.add_modifier(Modifier::BOLD);
        }
        Span::styled(s, st)
    };
    match hl {
        Some(re) => {
            let mut pos = 0;
            for m in re.find_iter(text) {
                let (s, e) = (m.start(), m.end());
                if s > pos {
                    spans.push(mk(text[pos..s].to_string(), base, false));
                }
                spans.push(mk(text[s..e].to_string(), kw, true));
                pos = e;
            }
            if pos < text.len() {
                spans.push(mk(text[pos..].to_string(), base, false));
            }
        }
        None => {
            spans.push(mk(text.to_string(), base, false));
        }
    }
}
