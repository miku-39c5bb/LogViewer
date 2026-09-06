# LogViewer

一个运行在终端里的日志分析工具，支持 Windows 和 Linux。

## 编译

### 依赖
- Rust 工具链（stable）。
- `rg`（ripgrep）需在 `PATH` 中（程序以外部进程调用它）。

### 构建

```bash
cargo build --release
# 运行全部单元测试（内核 / 布局 / 键位 / 历史等）
cargo test
```

产物：`target/release/logviewer`。

---

## 使用

```bash
logviewer /path/to/big.log
```

### 键位一览（默认值，均可在 config.toml 中修改；F1 查看当前实际键位）

| 类别 | 键 | 作用 |
|---|---|---|
| 滚动 | `j`/`k` `↑`/`↓` | 光标逐行移动（触边自动滚屏） |
| | `d`/`u` | 下半屏 / 上半屏 |
| | `f`/`b` `PgDn`/`PgUp` | 下翻 / 上翻一屏 |
| | `g` / `G` | 到文件首 / 文件尾 |
| | `←` `→` 或 `h` `l` | 水平滚动（长行） |
| | `0` / `$` | 到行首 / 到行尾 |
| | `w` | 自动换行开关（续行带 `↪` 对齐） |
| 搜索 | `/` `?` | 向下 / 向上搜索（输入后 Enter 执行） |
| | `n` `N` | 下一处 / 上一处匹配（**从当前光标行起算**） |
| | `c` | 清除搜索 / 高亮 |
| | `C` | 大小写模式循环：智能 → 忽略 → 区分 |
| | `W` | 整词匹配开关 |
| | `H` | 历史关键字模糊选择器（Enter 填入可改，再 Enter 执行） |
| 结果窗 | `;` | 把当前窗格的匹配输出为结果窗（随后 h/j/k/l 选方位、数字覆盖、Esc 取消） |
| | `Enter` | 结果窗选中行 → 主窗跳原文上下文 |
| | `x` | 关闭结果窗（主窗不可关，关闭=退出） |
| | `z` | zoom 当前窗到全屏 / 还原（标题带 `[ZOOM]`） |
| | `Ctrl+←→↑↓` | 调整窗格大小（→/↑ 变大，←/↓ 变小；`=` 均衡） |
| 其它 | `:` | 输入行号跳转（1-based，指原始文件行号） |
| | `F` | tail 跟随（日志增长自动滚到底） |
| | `e` | 导出当前（搜索/结果）内容到文件 |
| | `q` | 退出 |
| | `F1` | 键位帮助（显示配置后的实际键位） |

> 数字键 `1`-`9`：直接把焦点切到对应编号窗格（编号 = 屏幕几何顺序，主窗为 1）。

### 配置文件

首次运行自动生成：

- Windows：`%APPDATA%\logviewer\config.toml`、`history.json`
- Linux：`~/.config/logviewer/config.toml`、`history.json`

`config.toml` 示例：

```toml
[keys]
scroll_line_down = ["j", "down", "C-n"]   # 一个动作可绑多个键
quit = ["q", "C-c"]
wrap = "w"
help = "f1"
toggle_case = "C"
toggle_word = "W"
```

键写法：普通字符原样（`j` `/` `?` `:` `0` `$`…）；功能键小写名（`tab` `up` `down`
`left` `right` `pgup` `pgdn` `enter` `esc` `home` `end` `f1`…`f12`）；Ctrl 用 `C-` 前缀
（`C-d` `C-left`…）。改动后重启生效。

### 搜索历史

成功执行的搜索关键字自动持久化到 `history.json`（跨文件、跨会话、去重）。
搜索输入时按 `↑`/`↓` 按 less 习惯逐条浏览历史；或按 `H` 用模糊匹配快速挑选。

---

## 内部结构与测试

```
src/
  core/   大文件内核：块读 + 稀疏行索引 + 光标视口（tail 感知、编码解码）
  layout.rs  split 树布局（H/V 分割、zoom、resize、均衡）
  app.rs   TUI 状态机、窗格、搜索、渲染
  search.rs rg 子进程流解析 + 正则编译（大小写/整词）
  keymap.rs 动作→键位默认表 + config.toml 加载
  history.rs 搜索历史 JSON 持久化
```

`cargo test` 覆盖内核、布局树、键位、历史与搜索解析逻辑。
