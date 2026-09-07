//! 主题：颜色与光标形状配置（config.toml 的 [theme] 段）。

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Hidden,
    Block,
    Underline,
    Bar,
}

/// 渲染一行用到的可配颜色。
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub cursor_line_bg: Color,   // 光标/选中所在行背景
    pub cursor_marker: Color,    // 行首 ▶ 标记
    pub focus_border: Color,     // 焦点窗边框
    pub inactive_border: Color,  // 非焦点窗边框
    pub panel_border: Color,     // 帮助/历史/选择面板边框
    pub line_number: Color,      // 行号与 │
    pub keyword: Color,          // 高亮关键字
    pub cursor_normal: CursorShape, // 普通模式系统光标
    pub cursor_input: CursorShape,  // 命令输入模式系统光标
}

impl Theme {
    pub fn default() -> Self {
        Theme {
            cursor_line_bg: Color::Blue,
            cursor_marker: Color::Cyan,
            focus_border: Color::Cyan,
            inactive_border: Color::DarkGray,
            panel_border: Color::Magenta,
            line_number: Color::DarkGray,
            keyword: Color::Yellow,
            cursor_normal: CursorShape::Hidden,
            cursor_input: CursorShape::Hidden,
        }
    }

    /// 从已解析的 toml 表合并（[theme] 段）。
    pub fn from_toml(tbl: &toml::Table) -> Self {
        let mut t = Theme::default();
        let Some(s) = tbl.get("theme").and_then(|v| v.as_table()) else {
            return t;
        };
        let mut set_color = |f: &mut Color, name: &str| {
            if let Some(v) = s.get(name).and_then(|x| x.as_str()) {
                if let Some(c) = parse_color(v) {
                    *f = c;
                }
            }
        };
        set_color(&mut t.cursor_line_bg, "cursor_line_bg");
        set_color(&mut t.cursor_marker, "cursor_marker");
        set_color(&mut t.focus_border, "focus_border");
        set_color(&mut t.inactive_border, "inactive_border");
        set_color(&mut t.panel_border, "panel_border");
        set_color(&mut t.line_number, "line_number");
        set_color(&mut t.keyword, "keyword");
        if let Some(v) = s.get("cursor_normal").and_then(|x| x.as_str()) {
            if let Some(c) = parse_shape(v) {
                t.cursor_normal = c;
            }
        }
        if let Some(v) = s.get("cursor_input").and_then(|x| x.as_str()) {
            if let Some(c) = parse_shape(v) {
                t.cursor_input = c;
            }
        }
        t
    }

    /// config.toml 模板中的 [theme] 文本。
    pub fn template_text() -> &'static str {
        "[theme]
# 光标/选中行背景：black red green yellow blue magenta cyan gray dark_gray light_red ... white
# 或 \"#rrggbb\"
cursor_line_bg = \"blue\"
cursor_marker = \"cyan\"       # 行首 ▶ 光标标记
focus_border = \"cyan\"        # 焦点窗边框
inactive_border = \"dark_gray\" # 非焦点窗边框
panel_border = \"magenta\"     # 帮助/历史/目标选择面板边框
line_number = \"dark_gray\"    # 行号与分隔线
keyword = \"yellow\"           # 匹配关键字
cursor_normal = \"hidden\"     # 普通模式系统光标: hidden|block|underline|bar
cursor_input = \"hidden\"         # 命令输入(搜索/行号/历史)时系统光标
"
    }
}

pub fn parse_color(s: &str) -> Option<Color> {
    use ratatui::style::Color as C;
    let s = s.trim().to_ascii_lowercase();
    let simple = match s.as_str() {
        "black" => Some(C::Black),
        "red" => Some(C::Red),
        "green" => Some(C::Green),
        "yellow" => Some(C::Yellow),
        "blue" => Some(C::Blue),
        "magenta" => Some(C::Magenta),
        "cyan" => Some(C::Cyan),
        "gray" | "grey" => Some(C::Gray),
        "dark_gray" | "darkgrey" => Some(C::DarkGray),
        "light_red" => Some(C::LightRed),
        "light_green" => Some(C::LightGreen),
        "light_yellow" => Some(C::LightYellow),
        "light_blue" => Some(C::LightBlue),
        "light_magenta" => Some(C::LightMagenta),
        "light_cyan" => Some(C::LightCyan),
        "white" => Some(C::White),
        "reset" | "default" => Some(C::Reset),
        _ => None,
    };
    if simple.is_some() {
        return simple;
    }
    // #rrggbb
    let h = s.strip_prefix('#').or_else(|| s.strip_prefix("0x"))?;
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(C::Rgb(r, g, b))
}

pub fn parse_shape(s: &str) -> Option<CursorShape> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "hidden" => CursorShape::Hidden,
        "block" => CursorShape::Block,
        "underline" => CursorShape::Underline,
        "bar" => CursorShape::Bar,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_colors() {
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("dark_gray"), Some(Color::DarkGray));
        assert_eq!(parse_color("#FF0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn shapes() {
        assert_eq!(parse_shape("bar"), Some(CursorShape::Bar));
        assert_eq!(parse_shape("hidden"), Some(CursorShape::Hidden));
        assert_eq!(parse_shape("x"), None);
    }

    #[test]
    fn toml_override() {
        let text = "[theme]\ncursor_line_bg=\"red\"\nkeyword=\"#00FF00\"\ncursor_normal=\"bar\"\n";
        let tbl: toml::Table = text.parse().unwrap();
        let t = Theme::from_toml(&tbl);
        assert_eq!(t.cursor_line_bg, Color::Red);
        assert_eq!(t.keyword, Color::Rgb(0, 255, 0));
        assert_eq!(t.cursor_normal, CursorShape::Bar);
        assert_eq!(t.cursor_input, CursorShape::Bar); // 未覆盖保持默认
    }
}
