//! 窗格布局：递归二分树（split tree）。
//!
//! - `Split::H` 左右分割（a 在左），`Split::V` 上下分割（a 在上）；
//! - 叶子持窗格 id（1-based 编号派生自存储顺序）；
//! - 支持：按焦点 id 分割出新窗（方位 h/j/k/l → 左/下/上/右 由调用方换算为 Dir+a/b 顺序）、
//!   关闭合并、与父分割相邻边界的 ratio 调整、均衡。
//!
//! 布局树不持有窗格内容；内容仍在 App 的 `panes` 存储中，本模块只负责形状。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// 左右分割
    H,
    /// 上下分割
    V,
}

#[derive(Debug, Clone)]
pub enum Node {
    Leaf { id: u64 },
    Split {
        dir: Dir,
        /// a 所占比例（0..=1）
        ratio: f64,
        a: Box<Node>,
        b: Box<Node>,
    },
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: Node,
}

impl Layout {
    pub fn new(id: u64) -> Self {
        Layout {
            root: Node::Leaf { id },
        }
    }

    pub fn leaf_ids(&self) -> Vec<u64> {
        let mut out = Vec::new();
        Self::collect(&self.root, &mut out);
        out
    }

    fn collect(n: &Node, out: &mut Vec<u64>) {
        match n {
            Node::Leaf { id } => out.push(*id),
            Node::Split { a, b, .. } => {
                Self::collect(a, out);
                Self::collect(b, out);
            }
        }
    }

    /// 在含 `focus` 的叶子处，沿 `dir` 把它分割成 [原窗, 新窗]，新窗为 b 侧。
    pub fn split_after(&mut self, focus: u64, dir: Dir, new_id: u64) -> bool {
        let mut found = false;
        Self::split_impl(&mut self.root, focus, dir, new_id, &mut found);
        found
    }

    fn split_impl(
        n: &mut Node,
        focus: u64,
        dir: Dir,
        new_id: u64,
        found: &mut bool,
    ) {
        if *found {
            return;
        }
        match n {
            Node::Leaf { id } if *id == focus => {
                *n = Node::Split {
                    dir,
                    ratio: 0.5,
                    a: Box::new(Node::Leaf { id: focus }),
                    b: Box::new(Node::Leaf { id: new_id }),
                };
                *found = true;
            }
            Node::Leaf { .. } => {}
            Node::Split { a, b, .. } => {
                Self::split_impl(a, focus, dir, new_id, found);
                Self::split_impl(b, focus, dir, new_id, found);
            }
        }
    }

    /// 在含 `focus` 的叶子处，沿 `dir` 分割：new_first 时新窗在 a（左/上），否则在 b（右/下）。
    pub fn split_relative(&mut self, focus: u64, dir: Dir, new_id: u64, new_first: bool) -> bool {
        let mut found = false;
        Self::split_rel_impl(
            &mut self.root,
            focus,
            dir,
            new_id,
            new_first,
            &mut found,
        );
        found
    }

    fn split_rel_impl(
        n: &mut Node,
        focus: u64,
        dir: Dir,
        new_id: u64,
        new_first: bool,
        found: &mut bool,
    ) {
        if *found {
            return;
        }
        match n {
            Node::Leaf { id } if *id == focus => {
                let (a, b) = if new_first {
                    (
                        Box::new(Node::Leaf { id: new_id }),
                        Box::new(Node::Leaf { id: focus }),
                    )
                } else {
                    (
                        Box::new(Node::Leaf { id: focus }),
                        Box::new(Node::Leaf { id: new_id }),
                    )
                };
                *n = Node::Split {
                    dir,
                    ratio: 0.5,
                    a,
                    b,
                };
                *found = true;
            }
            Node::Leaf { .. } => {}
            Node::Split { a, b, .. } => {
                Self::split_rel_impl(a, focus, dir, new_id, new_first, found);
                Self::split_rel_impl(b, focus, dir, new_id, new_first, found);
            }
        }
    }

    /// 把 id 叶子放在含 focus 叶子的父分割的 b 侧：返回实际使用的 (dir, 是否插入)。
    /// 调用方需先用 `leaf_dir` 决定；这里仅做插入语义（新窗与 focus 并列同父）。
    pub fn split_with(&mut self, focus: u64, dir: Dir, new_id: u64) -> bool {
        self.split_relative(focus, dir, new_id, false)
    }

    /// 删除 id 叶子；若父分割仅剩一个子则合并（父被 sibling 替换）。返回是否删除。
    pub fn close(&mut self, id: u64) -> bool {
        let mut removed = false;
        let root = std::mem::replace(&mut self.root, Node::Leaf { id: u64::MAX });
        self.root = Self::close_impl(root, id, &mut removed);
        removed
    }

    fn close_impl(n: Node, id: u64, removed: &mut bool) -> Node {
        match n {
            Node::Leaf { id: lid } if lid == id => {
                *removed = true;
                Node::Leaf { id: u64::MAX } // 哨兵：由父处理
            }
            Node::Leaf { .. } => n,
            Node::Split { dir, ratio, a, b } => {
                let a2 = Self::close_impl(*a, id, removed);
                let b2 = Self::close_impl(*b, id, removed);
                match (a2, b2) {
                    (Node::Leaf { id: u64::MAX }, b) => b,
                    (a, Node::Leaf { id: u64::MAX }) => a,
                    (a, b) => Node::Split {
                        dir,
                        ratio,
                        a: Box::new(a),
                        b: Box::new(b),
                    },
                }
            }
        }
    }

    /// 调整含 focus 叶子的父分割比例（delta 应用到 a 侧）。
    /// 仅当父分割是 id 叶子所在的最近分割时生效（调整的是它朝 b 的那条边）。
    pub fn adjust(&mut self, focus: u64, delta: f64) {
        Self::adjust_impl(&mut self.root, focus, delta);
    }

    /// 沿轴调整：horizontal=true 找最近的 H（左右）分割调宽，否则找最近的 V（上下）分割调高。
    /// grow=true 使 focus 变大。返回是否找到可调分割。
    pub fn adjust_axis(&mut self, focus: u64, horizontal: bool, grow: bool) -> bool {
        Self::axis_impl(&mut self.root, focus, horizontal, grow)
    }

    fn axis_impl(n: &mut Node, focus: u64, horizontal: bool, grow: bool) -> bool {
        match n {
            Node::Leaf { .. } => false,
            Node::Split { dir, ratio, a, b } => {
                let in_a = contains_leaf(a, focus);
                let contains = in_a || contains_leaf(b, focus);
                if !contains {
                    return false;
                }
                if (*dir == Dir::H) == horizontal {
                    // 本分割轴匹配：调整使 focus 变大/变小
                    let step = 0.05;
                    let delta = if grow { step } else { -step };
                    let delta = if in_a { delta } else { -delta };
                    *ratio = (*ratio + delta).clamp(0.05, 0.95);
                    true
                } else if in_a {
                    Self::axis_impl(a, focus, horizontal, grow)
                } else {
                    Self::axis_impl(b, focus, horizontal, grow)
                }
            }
        }
    }

    fn adjust_impl(n: &mut Node, focus: u64, delta: f64) {
        match n {
            Node::Leaf { .. } => {}
            Node::Split { a, b, ratio, .. } => {
                if contains_leaf(a, focus) {
                    // focus 在 a：把 a/b 边界往 b 移（a 变大）
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                } else if contains_leaf(b, focus) {
                    // focus 在 b：a 变小
                    *ratio = (*ratio - delta).clamp(0.1, 0.9);
                } else {
                    return;
                }
            }
        }
    }

    /// 全部 ratio 归 0.5（= 键）。
    pub fn balance(&mut self) {
        Self::balance_impl(&mut self.root);
    }

    fn balance_impl(n: &mut Node) {
        match n {
            Node::Leaf { .. } => {}
            Node::Split { ratio, a, b, .. } => {
                *ratio = 0.5;
                Self::balance_impl(a);
                Self::balance_impl(b);
            }
        }
    }

    /// 布局：返回 (id, rect) 列表，顺序与 leaf_ids 一致。
    pub fn layout(&self, area: RectLike) -> Vec<(u64, RectLike)> {
        let mut out = Vec::new();
        self.layout_impl(&self.root, area, &mut out);
        out
    }

    fn layout_impl(&self, n: &Node, area: RectLike, out: &mut Vec<(u64, RectLike)>) {
        match n {
            Node::Leaf { id } => out.push((*id, area)),
            Node::Split { dir, ratio, a, b } => {
                match dir {
                    Dir::H => {
                        let w = (area.w as f64 * ratio).round() as u16;
                        let w = w.max(1);
                        let a_area = RectLike {
                            x: area.x,
                            y: area.y,
                            w,
                            h: area.h,
                        };
                        let b_area = RectLike {
                            x: area.x + w,
                            y: area.y,
                            w: area.w.saturating_sub(w),
                            h: area.h,
                        };
                        self.layout_impl(a, a_area, out);
                        self.layout_impl(b, b_area, out);
                    }
                    Dir::V => {
                        let h = (area.h as f64 * ratio).round() as u16;
                        let h = h.max(1);
                        let a_area = RectLike {
                            x: area.x,
                            y: area.y,
                            w: area.w,
                            h,
                        };
                        let b_area = RectLike {
                            x: area.x,
                            y: area.y + h,
                            w: area.w,
                            h: area.h.saturating_sub(h),
                        };
                        self.layout_impl(a, a_area, out);
                        self.layout_impl(b, b_area, out);
                    }
                }
            }
        }
    }

    /// focus 叶子最近的父分割方向（无分割返回 None）。
    pub fn parent_dir(&self, focus: u64) -> Option<Dir> {
        self.parent_dir_impl(&self.root, focus)
    }

    /// focus 在最近父分割中的位置：(dir, 是否位于 a 侧)。
    pub fn side(&self, focus: u64) -> Option<(Dir, bool)> {
        self.side_impl(&self.root, focus)
    }

    fn side_impl(&self, n: &Node, focus: u64) -> Option<(Dir, bool)> {
        match n {
            Node::Leaf { .. } => None,
            Node::Split { dir, a, b, .. } => {
                if let Node::Leaf { id } = a.as_ref() {
                    if *id == focus {
                        return Some((*dir, true));
                    }
                }
                if let Node::Leaf { id } = b.as_ref() {
                    if *id == focus {
                        return Some((*dir, false));
                    }
                }
                self.side_impl(a, focus).or_else(|| self.side_impl(b, focus))
            }
        }
    }

    fn parent_dir_impl(&self, n: &Node, focus: u64) -> Option<Dir> {
        match n {
            Node::Leaf { .. } => None,
            Node::Split { dir, a, b, .. } => {
                // focus 直接是 a 或 b 的叶子 → 本分割即其最近父
                if let Node::Leaf { id } = a.as_ref() {
                    if *id == focus {
                        return Some(*dir);
                    }
                }
                if let Node::Leaf { id } = b.as_ref() {
                    if *id == focus {
                        return Some(*dir);
                    }
                }
                self.parent_dir_impl(a, focus).or_else(|| self.parent_dir_impl(b, focus))
            }
        }
    }
}

fn contains_leaf(n: &Node, id: u64) -> bool {
    match n {
        Node::Leaf { id: lid } => *lid == id,
        Node::Split { a, b, .. } => contains_leaf(a, id) || contains_leaf(b, id),
    }
}

/// 简易矩形（避免依赖 ratatui 便于纯测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RectLike {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids_of(v: &[(u64, RectLike)]) -> Vec<u64> {
        v.iter().map(|x| x.0).collect()
    }

    #[test]
    fn single_leaf() {
        let l = Layout::new(1);
        assert_eq!(l.leaf_ids(), vec![1]);
        let r = l.layout(RectLike { x: 0, y: 0, w: 100, h: 40 });
        assert_eq!(r, vec![(1, RectLike { x: 0, y: 0, w: 100, h: 40 })]);
    }

    #[test]
    fn split_h_and_v() {
        let mut l = Layout::new(1);
        assert!(l.split_after(1, Dir::H, 2));
        assert_eq!(l.leaf_ids(), vec![1, 2]);
        // 再把 2 向下分割出 3
        assert!(l.split_after(2, Dir::V, 3));
        assert_eq!(l.leaf_ids(), vec![1, 2, 3]);
        let rects = l.layout(RectLike { x: 0, y: 0, w: 100, h: 100 });
        assert_eq!(ids_of(&rects), vec![1, 2, 3]);
        // 1 在左侧约占一半
        let r1 = rects.iter().find(|(i, _)| *i == 1).unwrap().1;
        assert!(r1.w <= 60 && r1.w >= 40);
        // 2 与 3 上下分占 2 所在区域
        let r2 = rects.iter().find(|(i, _)| *i == 2).unwrap().1;
        let r3 = rects.iter().find(|(i, _)| *i == 3).unwrap().1;
        assert_eq!(r2.x, r3.x);
        assert_eq!(r2.w, r3.w);
        assert!(r2.y < r3.y);
    }

    #[test]
    fn close_merges_parent() {
        let mut l = Layout::new(1);
        l.split_after(1, Dir::H, 2);
        l.split_after(2, Dir::V, 3);
        assert!(l.close(3));
        assert_eq!(l.leaf_ids(), vec![1, 2]);
        // 再关 2 → 只剩 1，树不再分裂
        assert!(l.close(2));
        assert_eq!(l.leaf_ids(), vec![1]);
        assert!(matches!(l.root, Node::Leaf { id: 1 }));
    }

    #[test]
    fn adjust_ratio_side() {
        let mut l = Layout::new(1);
        l.split_after(1, Dir::H, 2);
        // focus=1 在 a：+ 让它变大
        l.adjust(1, 0.2);
        if let Node::Split { ratio, a, b, .. } = &l.root {
            assert!((ratio - 0.7).abs() < 1e-9);
            assert!(contains_leaf(a, 1));
            assert!(contains_leaf(b, 2));
        } else {
            panic!("expected split");
        }
        // focus=2 在 b：+ 让它变大 => a 变小
        l.adjust(2, 0.2);
        if let Node::Split { ratio, .. } = &l.root {
            assert!((ratio - 0.5).abs() < 1e-9);
        }
    }

    #[test]
    fn adjust_axis_ancestor_split() {
        // 复现用户场景：H[1, V[2,3]]，focus=3（右下角）
        let mut l = Layout::new(1);
        l.split_relative(1, Dir::H, 2, false); // 1 | 2
        l.split_relative(2, Dir::V, 3, false); // 2 上、3 下
        // 3 调宽：应作用到最近 H 祖先（root）；3 在其 b 侧，变宽 → 压缩 a(1)
        let mut l2 = l.clone();
        assert!(l2.adjust_axis(3, true, true));
        if let Node::Split { ratio, a, b, .. } = &l2.root {
            assert!(contains_leaf(a, 1));
            assert!(contains_leaf(b, 3));
            assert!((ratio - 0.45).abs() < 1e-9);
        } else {
            panic!("root should stay H");
        }
        // 3 调高：应作用到最近 V 父（2/3 之间），ratio(a=2) 减小使 b(3) 变大
        let mut l3 = l.clone();
        assert!(l3.adjust_axis(3, false, true));
        let b = match &l3.root {
            Node::Split { b, .. } => b,
            _ => panic!("root should stay H"),
        };
        match b.as_ref() {
            Node::Split {
                dir: Dir::V,
                ratio,
                b,
                ..
            } => {
                assert!(contains_leaf(b, 3));
                assert!((ratio - 0.45).abs() < 1e-9);
            }
            _ => panic!("expected V split under root b"),
        }
        // 单窗无分割可调
        let mut lone = Layout::new(7);
        assert!(!lone.adjust_axis(7, true, true));
    }

    #[test]
    fn parent_dir_reports() {
        let mut l = Layout::new(1);
        l.split_after(1, Dir::H, 2);
        l.split_after(2, Dir::V, 3);
        assert_eq!(l.parent_dir(2), Some(Dir::V));
        assert_eq!(l.parent_dir(1), Some(Dir::H));
        assert!(l.parent_dir(3).is_some());
    }
}
