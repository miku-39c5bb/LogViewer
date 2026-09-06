//! 大文件内核：按需块读、行定位、视口缓存（与 UI 无关，可独立测试）。

pub mod file;
pub mod index;
pub mod lines;
pub mod view;
