//! 键位配置：动作 → 键位 的默认表 + 用户 config.toml 覆盖。
//!
//! 键位字符串格式（大小写敏感；Ctrl 用 `C-` 前缀）：
//!   普通字符：`j` `k` `/` `?` `;` `:` `0` `$` `w` `z` `e` `n` `N` `g` `G` `f` `b` `d` `u` `q` `x` `h` `l` `=` `1` ` `（空格写 `space`）
//!   功能键：`tab` `backtab` `up` `down` `left` `right` `pgup` `pgdn` `enter` `esc` `home` `end`
//!   Ctrl 组合：`C-d` `C-u` `C-left` `C-right` `C-up` `C-down`
//!
//! 配置文件：<config>/logviewer/config.toml，格式：
//!   [keys]
//!   scroll_down_line = "j"        # 单键或多个（逗号分隔）
//!   quit = ["q", "C-c"]
//! 未列出的动作保持默认；可动作可绑定多键。

use std::collections::HashMap;
use std::path::Path;

// ---------- 键位（KeySpec） ----------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeySpec {
    Char(char, bool), // (字符, 是否 Ctrl)
    Fun(&'static str, bool), // 功能键名, 是否 Ctrl
}

impl KeySpec {
    /// 把单个配置串解析成键位。失败返回 None（保持默认不动该键）。
    pub fn parse(s: &str) -> Option<KeySpec> {
        let (ctrl, rest) = if let Some(r) = s.strip_prefix("C-") {
            (true, r)
        } else {
            (false, s)
        };
        if rest.is_empty() {
            return None;
        }
        // 功能键名
        const FUNS: &[&str] = &[
            "tab", "backtab", "up", "down", "left", "right", "pgup", "pgdn", "enter", "esc",
            "home", "end", "space", "backspace", "delete", "f1", "f2", "f3", "f4", "f5",
            "f6", "f7", "f8", "f9", "f10", "f11", "f12",
        ];
        let lower = rest.to_ascii_lowercase();
        if let Some(f) = FUNS.iter().find(|f| **f == lower) {
            return Some(KeySpec::Fun(f, ctrl));
        }
        if rest.chars().count() == 1 && !ctrl {
            return Some(KeySpec::Char(rest.chars().next().unwrap(), false));
        }
        // Ctrl + 单字符
        if ctrl && rest.chars().count() == 1 {
            let c = rest.chars().next().unwrap();
            return Some(KeySpec::Char(c.to_ascii_lowercase(), true));
        }
        None
    }

    /// 由 crossterm KeyEvent 转成 KeySpec（用于查表）。
    pub fn from_key(key: &crossterm::event::KeyEvent) -> Option<KeySpec> {
        use crossterm::event::KeyCode;
        let ctrl = key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        Some(match key.code {
            KeyCode::Char(c) => KeySpec::Char(if ctrl { c.to_ascii_lowercase() } else { c }, ctrl),
            KeyCode::Tab => KeySpec::Fun("tab", ctrl),
            KeyCode::BackTab => KeySpec::Fun("backtab", ctrl),
            KeyCode::Up => KeySpec::Fun("up", ctrl),
            KeyCode::Down => KeySpec::Fun("down", ctrl),
            KeyCode::Left => KeySpec::Fun("left", ctrl),
            KeyCode::Right => KeySpec::Fun("right", ctrl),
            KeyCode::PageUp => KeySpec::Fun("pgup", ctrl),
            KeyCode::PageDown => KeySpec::Fun("pgdn", ctrl),
            KeyCode::Enter => KeySpec::Fun("enter", ctrl),
            KeyCode::Esc => KeySpec::Fun("esc", ctrl),
            KeyCode::Home => KeySpec::Fun("home", ctrl),
            KeyCode::End => KeySpec::Fun("end", ctrl),
            KeyCode::Backspace => KeySpec::Fun("backspace", ctrl),
            KeyCode::Delete => KeySpec::Fun("delete", ctrl),
            KeyCode::F(n) if (1..=12).contains(&n) => {
                const F: [&str; 12] = [
                    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11",
                    "f12",
                ];
                KeySpec::Fun(F[(n - 1) as usize], ctrl)
            }
            _ => return None,
        })
    }
}

// ---------- 动作 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Quit,
    FocusNext,
    FocusPrev,
    ScrollLineDown,
    ScrollLineUp,
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
    ToTop,
    ToBottom,
    SearchFwd,
    SearchBack,
    MatchNext,
    MatchPrev,
    ClearSearch,
    OpenMatchPane, // ';' → 目标选择
    GotoLine,      // ':' → 行号跳转
    Zoom,
    Export,
    Follow,
    HLeft,
    HRight,
    HHome,
    HEnd,
    Wrap,
    ClosePane,
    EnterJump,
    Balance,
    NudgeLeft,
    NudgeRight,
    NudgeUp,
    NudgeDown,
    History, // H
    Help,     // F1
    ToggleCase,
    ToggleWord,
}

// ---------- Keymap ----------

pub struct Keymap {
    map: HashMap<KeySpec, Action>,
}

/// 默认键位表（动作名 → 键串列表）。
fn default_bindings() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("quit", &["q", "C-c"]),
        ("focus_next", &["tab"]),
        ("focus_prev", &["backtab"]),
        ("scroll_line_down", &["j", "down", "C-n"]),
        ("scroll_line_up", &["k", "up", "C-p"]),
        ("half_down", &["d", "C-d"]),
        ("half_up", &["u", "C-u"]),
        ("page_down", &["f", "pgdn"]),
        ("page_up", &["b", "pgup"]),
        ("to_top", &["g"]),
        ("to_bottom", &["G"]),
        ("search_fwd", &["/"]),
        ("search_back", &["?"]),
        ("match_next", &["n"]),
        ("match_prev", &["N"]),
        ("clear_search", &["c", "R"]),
        ("open_match_pane", &[";"]),
        ("goto_line", &[":"]),
        ("zoom", &["z"]),
        ("export", &["e"]),
        ("follow", &["F"]),
        ("h_left", &["h", "left"]),
        ("h_right", &["l", "right"]),
        ("h_home", &["0", "home"]),
        ("h_end", &["$"]),
        ("wrap", &["w"]),
        ("close_pane", &["x"]),
        ("enter_jump", &["enter"]),
        ("balance", &["="]),
        ("nudge_right", &["C-right"]),
        ("nudge_left", &["C-left"]),
        ("nudge_up", &["C-up"]),
        ("nudge_down", &["C-down"]),
        ("history", &["H"]),
        ("help", &["f1"]),
        ("toggle_case", &["C"]),
        ("toggle_word", &["W"]),
    ]
}

impl Keymap {
    pub fn default() -> Self {
        let mut map = HashMap::new();
        for (name, keys) in default_bindings() {
            let Some(act) = action_by_name(name) else {
                continue;
            };
            for k in *keys {
                if let Some(spec) = KeySpec::parse(k) {
                    map.insert(spec, act);
                }
            }
        }
        Keymap { map }
    }

    /// 加载用户配置（可缺省）。覆盖同名动作绑定；未知键忽略。
    pub fn load_or_default(cfg: Option<&Path>) -> Self {
        let mut km = Keymap::default();
        let Some(path) = cfg else {
            return km;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return km;
        };
        let Ok(tbl) = text.parse::<toml::Table>() else {
            return km;
        };
        let Some(keys) = tbl.get("keys").and_then(|v| v.as_table()) else {
            return km;
        };
        for (name, v) in keys {
            let Some(act) = action_by_name(name) else {
                continue;
            };
            // 支持字符串或字符串数组
            let list: Vec<String> = match v {
                toml::Value::String(s) => vec![s.clone()],
                toml::Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect(),
                _ => continue,
            };
            // 覆盖：先移除该动作旧键，再插入新键
            km.map.retain(|_, a| *a != act);
            for k in list {
                if let Some(spec) = KeySpec::parse(&k) {
                    km.map.insert(spec, act);
                }
            }
        }
        km
    }

    pub fn action(&self, spec: &KeySpec) -> Option<Action> {
        self.map.get(spec).copied()
    }

    /// 以实际生效键位列出帮助（按默认顺序；含用户覆盖）。返回 (动作, 键显示串)。
    pub fn help_entries(&self) -> Vec<(Action, String)> {
        let mut out = Vec::new();
        for (name, _defaults) in default_bindings() {
            let Some(act) = action_by_name(name) else {
                continue;
            };
            let mut keys: Vec<String> = self
                .map
                .iter()
                .filter(|(_, a)| **a == act)
                .map(|(spec, _)| Self::format_spec(spec))
                .collect();
            keys.sort();
            out.push((act, keys.join("  /  ")));
        }
        out
    }

    /// 键位显示为可读字符串（帮助用）。
    pub fn format_spec(spec: &KeySpec) -> String {
        match spec {
            KeySpec::Char(c, false) => c.to_string(),
            KeySpec::Char(c, true) => format!("C-{c}"),
            KeySpec::Fun(f, false) => f.to_uppercase(),
            KeySpec::Fun(f, true) => format!("C-{}", f.to_uppercase()),
        }
    }

    /// 动作的中文标签（帮助用）。
    pub fn action_label(action: Action) -> &'static str {
        use Action as A;
        match action {
            A::Quit => "退出程序",
            A::FocusNext => "下一窗格 (Tab)",
            A::FocusPrev => "上一窗格",
            A::ScrollLineDown => "下移一行",
            A::ScrollLineUp => "上移一行",
            A::HalfDown => "下半屏",
            A::HalfUp => "上半屏",
            A::PageDown => "下翻一屏",
            A::PageUp => "上翻一屏",
            A::ToTop => "到文件首",
            A::ToBottom => "到文件尾",
            A::SearchFwd => "向下搜索 / 窗内搜索",
            A::SearchBack => "向上搜索",
            A::MatchNext => "下一个匹配",
            A::MatchPrev => "上一个匹配",
            A::ClearSearch => "清除搜索/高亮",
            A::OpenMatchPane => "生成结果窗（目标选择）",
            A::GotoLine => "跳转行号",
            A::Zoom => "放大/还原当前窗",
            A::Export => "导出当前内容到文件",
            A::Follow => "tail 跟随",
            A::HLeft => "水平左移",
            A::HRight => "水平右移",
            A::HHome => "到行首",
            A::HEnd => "到行尾",
            A::Wrap => "自动换行开关",
            A::ClosePane => "关闭结果窗",
            A::EnterJump => "结果窗跳到文件上下文",
            A::Balance => "窗格均衡",
            A::NudgeLeft => "缩小宽度",
            A::NudgeRight => "增大宽度",
            A::NudgeUp => "增大高度",
            A::NudgeDown => "缩小高度",
            A::History => "历史关键字模糊选择",
            A::Help => "帮助",
            A::ToggleCase => "大小写模式循环（智能/忽略/区分）",
            A::ToggleWord => "整词匹配开关",
        }
    }

    /// 以 TOML 文本导出默认表（便于用户生成模板）。
    pub fn default_template() -> String {
        let mut s = String::from(
            "# logviewer 键位配置\n# 动作 = 键 或 键列表；未列出的动作保持默认。\n# 键格式见 src/keymap.rs 顶部注释。\n[keys]\n",
        );
        for (name, keys) in default_bindings() {
            let joined = keys
                .iter()
                .map(|k| format!("\"{k}\""))
                .collect::<Vec<_>>()
                .join(", ");
            s.push_str(&format!("{name} = [{joined}]\n"));
        }
        s
    }
}

pub fn action_by_name(name: &str) -> Option<Action> {
    Some(match name {
        "quit" => Action::Quit,
        "focus_next" => Action::FocusNext,
        "focus_prev" => Action::FocusPrev,
        "scroll_line_down" => Action::ScrollLineDown,
        "scroll_line_up" => Action::ScrollLineUp,
        "half_down" => Action::HalfDown,
        "half_up" => Action::HalfUp,
        "page_down" => Action::PageDown,
        "page_up" => Action::PageUp,
        "to_top" => Action::ToTop,
        "to_bottom" => Action::ToBottom,
        "search_fwd" => Action::SearchFwd,
        "search_back" => Action::SearchBack,
        "match_next" => Action::MatchNext,
        "match_prev" => Action::MatchPrev,
        "clear_search" => Action::ClearSearch,
        "open_match_pane" => Action::OpenMatchPane,
        "goto_line" => Action::GotoLine,
        "zoom" => Action::Zoom,
        "export" => Action::Export,
        "follow" => Action::Follow,
        "h_left" => Action::HLeft,
        "h_right" => Action::HRight,
        "h_home" => Action::HHome,
        "h_end" => Action::HEnd,
        "wrap" => Action::Wrap,
        "close_pane" => Action::ClosePane,
        "enter_jump" => Action::EnterJump,
        "balance" => Action::Balance,
        "nudge_right" => Action::NudgeRight,
        "nudge_left" => Action::NudgeLeft,
        "nudge_up" => Action::NudgeUp,
        "nudge_down" => Action::NudgeDown,
        "history" => Action::History,
        "help" => Action::Help,
        "toggle_case" => Action::ToggleCase,
        "toggle_word" => Action::ToggleWord,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_specs() {
        assert_eq!(KeySpec::parse("j"), Some(KeySpec::Char('j', false)));
        assert_eq!(KeySpec::parse("C-d"), Some(KeySpec::Char('d', true)));
        assert_eq!(KeySpec::parse("pgdn"), Some(KeySpec::Fun("pgdn", false)));
        assert_eq!(KeySpec::parse("C-left"), Some(KeySpec::Fun("left", true)));
        assert_eq!(KeySpec::parse("/"), Some(KeySpec::Char('/', false)));
        assert_eq!(KeySpec::parse(""), None);
    }

    #[test]
    fn defaults_cover_actions() {
        let km = Keymap::default();
        for name in [
            "quit",
            "scroll_line_down",
            "scroll_line_up",
            "search_fwd",
            "open_match_pane",
            "wrap",
            "history",
        ] {
            let act = action_by_name(name).unwrap();
            assert!(
                km.map.values().any(|a| *a == act),
                "{name} 应有默认键"
            );
        }
    }

    #[test]
    fn template_is_valid_toml() {
        let t = Keymap::default_template();
        assert!(t.parse::<toml::Table>().is_ok());
    }
}
