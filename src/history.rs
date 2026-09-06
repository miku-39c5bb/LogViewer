//! 搜索历史持久化：关键字存到用户配置目录的 JSON 文件，跨文件、跨会话保留。

use std::io;
use std::path::PathBuf;

pub const HISTORY_CAP: usize = 300;
pub const DEFAULT_HISTORY_CAP: usize = HISTORY_CAP;

pub struct History {
    path: PathBuf,
    items: Vec<String>,
    /// 最多保留条数（可在 config.toml 的 [history].cap 配置）
    cap: usize,
}

impl History {
    /// 从 `path` 加载历史（文件不存在时为空历史），上限取默认值。
    pub fn load(path: PathBuf) -> Self {
        Self::load_cap(path, DEFAULT_HISTORY_CAP)
    }

    /// 从 `path` 加载历史并指定保留条数上限。
    pub fn load_cap(path: PathBuf, cap: usize) -> Self {
        let items = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();
        let mut cleaned: Vec<String> = Vec::with_capacity(items.len());
        for s in items {
            let s = s.trim();
            if s.is_empty() || cleaned.contains(&s.to_string()) {
                continue;
            }
            cleaned.push(s.to_string());
        }
        cleaned.truncate(cap.max(1));
        Self { path, items: cleaned, cap: cap.max(1) }
    }

    /// 从 config.toml 的 `[history].cap` 读取保留条数；缺失/非法时用默认。
    pub fn cap_from_toml(cfg: &std::path::Path) -> usize {
        let Ok(text) = std::fs::read_to_string(cfg) else {
            return DEFAULT_HISTORY_CAP;
        };
        let Ok(tbl) = text.parse::<toml::Table>() else {
            return DEFAULT_HISTORY_CAP;
        };
        tbl.get("history")
            .and_then(|v| v.get("cap"))
            .and_then(|v| v.as_integer())
            .map(|n| n.clamp(1, 100_000) as usize)
            .unwrap_or(DEFAULT_HISTORY_CAP)
    }

    /// 默认历史文件路径：<config>/logviewer/history.json
    pub fn default_path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("logviewer").join("history.json"))
    }

    /// 无持久化历史（找不到配置目录时的兜底，save 为空操作）。
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            items: Vec::new(),
            cap: DEFAULT_HISTORY_CAP,
        }
    }

    pub fn items(&self) -> &[String] {
        &self.items
    }

    /// 加入一条历史：去重后置于最前，超出上限截断。返回是否发生变化。
    pub fn push(&mut self, s: &str) -> bool {
        let s = s.trim();
        if s.is_empty() {
            return false;
        }
        if self.items.first().map(|x| x == s).unwrap_or(false) {
            return false;
        }
        self.items.retain(|x| x != s);
        self.items.insert(0, s.to_string());
        if self.items.len() > self.cap {
            self.items.truncate(self.cap);
        }
        true
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(&self.items)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        std::fs::write(&self.path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::file::test_tmp_path as tmp_path;

    #[test]
    fn push_dedupe_and_cap() {
        let p = tmp_path("hist");
        let mut h = History::load(p.clone());
        assert!(h.push("walog|JIIGAN"));
        assert!(h.push("error"));
        // 再次加入已存在的项：会把它提到最前，视为发生变化
        assert!(h.push("walog|JIIGAN"));
        assert_eq!(h.items(), &["walog|JIIGAN", "error"]);
        // 已在最前则无变化
        assert!(!h.push("walog|JIIGAN"));
        h.save().unwrap();
        let h2 = History::load(p.clone());
        assert_eq!(h2.items(), &["walog|JIIGAN", "error"]);
        std::fs::remove_file(&p).ok();
        let _ = std::fs::remove_dir(p.parent().unwrap());
    }

    #[test]
    fn load_cleans_empty_and_dupes() {
        let p = tmp_path("histclean");
        std::fs::write(
            &p,
            r#"["error", "", "  ", "error", "walog|JIIGAN", "error"]"#,
        )
        .unwrap();
        let h = History::load(p.clone());
        assert_eq!(h.items(), &["error", "walog|JIIGAN"]);
        std::fs::remove_file(&p).ok();
        let _ = std::fs::remove_dir(p.parent().unwrap());
    }

    #[test]
    fn cap_truncates() {
        let p = tmp_path("histcap");
        let mut h = History {
            path: p.clone(),
            items: Vec::new(),
            cap: HISTORY_CAP,
        };
        for i in 0..(HISTORY_CAP + 20) {
            h.push(&format!("kw{i}"));
        }
        assert_eq!(h.items.len(), HISTORY_CAP);
        assert_eq!(h.items[0], format!("kw{}", HISTORY_CAP + 19));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn custom_cap_from_toml() {
        let p = tmp_path("histcapcfg");
        std::fs::write(&p, "[history]\ncap = 5\n").unwrap();
        assert_eq!(History::cap_from_toml(&p), 5);
        std::fs::write(&p, "[history]\ncap = \"x\"\n").unwrap();
        assert_eq!(History::cap_from_toml(&p), DEFAULT_HISTORY_CAP);
        std::fs::write(&p, "").unwrap();
        assert_eq!(History::cap_from_toml(&p), DEFAULT_HISTORY_CAP);
        std::fs::remove_file(&p).ok();
    }
}
