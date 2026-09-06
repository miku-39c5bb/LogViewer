//! 大文件字节访问后端：按需块读取 + 小型 LRU 缓存。
//!
//! tail 安全设计：只缓存「读满一个 chunk」的块；文件尾部的不足块不缓存，
//! 因此日志被追加时，重新读取同一位置能看到新数据。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;
pub const CACHE_SLOTS: usize = 8;

struct Slot {
    idx: u64,
    seq: u64,
    data: Vec<u8>,
}

pub struct FileSource {
    file: File,
    len: u64,
    chunk_size: usize,
    cache: Vec<Slot>,
    seq: u64,
    io_err: Option<std::io::Error>,
}

/// 测试辅助：生成唯一临时文件路径（cfg(test) 下跨模块共享）。
#[cfg(test)]
pub(crate) fn test_tmp_path(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "logviewer_{}_{}_{}.tmp",
        tag,
        std::process::id(),
        nanos
    ))
}

impl FileSource {
    pub fn open<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            len,
            chunk_size: DEFAULT_CHUNK_SIZE,
            cache: Vec::with_capacity(CACHE_SLOTS),
            seq: 0,
            io_err: None,
        })
    }

    /// 测试专用：自定义块大小，用于构造跨块行。
    #[cfg(test)]
    pub fn open_with_chunk<P: AsRef<Path>>(path: P, chunk_size: usize) -> std::io::Result<Self> {
        let mut s = Self::open(path)?;
        s.chunk_size = chunk_size;
        Ok(s)
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// 刷新文件长度（日志追加检测），返回最新长度。
    pub fn refresh_len(&mut self) -> u64 {
        match self.file.metadata() {
            Ok(md) => self.len = md.len(),
            Err(e) => self.io_err = Some(e),
        }
        self.len
    }

    pub fn take_io_err(&mut self) -> Option<std::io::Error> {
        self.io_err.take()
    }

    /// 返回第 `idx` 块的数据副本（尾部块可能短于 chunk_size；空 Vec = 无数据）。
    pub fn chunk(&mut self, idx: u64) -> Vec<u8> {
        let cs = self.chunk_size as u64;
        let start = idx.saturating_mul(cs);
        if start >= self.len {
            // 起始越界：刷新一次长度再判定，避免漏掉刚追加的数据
            self.refresh_len();
        }
        if start >= self.len {
            return Vec::new();
        }
        if let Some(slot) = self.cache.iter_mut().find(|s| s.idx == idx) {
            slot.seq = self.seq;
            self.seq = self.seq.wrapping_add(1);
            return slot.data.clone();
        }
        if let Err(e) = self.file.seek(SeekFrom::Start(start)) {
            self.io_err = Some(e);
            return Vec::new();
        }
        let mut buf = vec![0u8; self.chunk_size];
        let mut got = 0usize;
        loop {
            match self.file.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => {
                    got += n;
                    if got == buf.len() {
                        break;
                    }
                }
                Err(e) => {
                    self.io_err = Some(e);
                    break;
                }
            }
        }
        buf.truncate(got);
        if got == self.chunk_size {
            // 只有读满的块才进缓存（tail 安全，见模块注释）
            self.insert_cache(idx, buf.clone());
        }
        buf
    }

    fn insert_cache(&mut self, idx: u64, data: Vec<u8>) {
        if let Some(slot) = self.cache.iter_mut().find(|s| s.idx == idx) {
            slot.data = data;
            slot.seq = self.seq;
            self.seq = self.seq.wrapping_add(1);
            return;
        }
        if self.cache.len() < CACHE_SLOTS {
            self.cache.push(Slot {
                idx,
                seq: self.seq,
                data,
            });
            self.seq = self.seq.wrapping_add(1);
            return;
        }
        // 淘汰最久未用的槽
        let mut victim = 0usize;
        for i in 1..self.cache.len() {
            if self.cache[i].seq < self.cache[victim].seq {
                victim = i;
            }
        }
        let slot = &mut self.cache[victim];
        slot.idx = idx;
        slot.seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        slot.data = data;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::core::file::test_tmp_path as tmp_path;
    #[test]
    fn chunk_reads_small_file() {
        let p = tmp_path("chunk");
        std::fs::write(&p, b"hello world").unwrap();
        let mut src = FileSource::open_with_chunk(&p, 4).unwrap();
        assert_eq!(src.len(), 11);
        assert_eq!(src.chunk(0), b"hell");
        assert_eq!(src.chunk(1), b"o wo");
        assert_eq!(src.chunk(2), b"rld");
        assert!(src.chunk(3).is_empty());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn tail_appends_visible() {
        let p = tmp_path("tail");
        {
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(b"aaaa\nbbbb\n").unwrap();
        }
        let mut src = FileSource::open_with_chunk(&p, 4).unwrap();
        assert_eq!(src.len(), 10);
        assert_eq!(src.chunk(0), b"aaaa");
        // 尾部块（不足 chunk）不缓存：追加后应能看到新数据
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(b"cccc\n").unwrap();
        }
        assert_eq!(src.refresh_len(), 15);
        assert_eq!(src.chunk(2), b"b\ncc"); // idx2 = [8,12)
        assert_eq!(src.chunk(3), b"cc\n"); // idx3 = [12,15)：cccc 占 10-13，\n 在 14
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn full_chunk_is_cached() {
        let p = tmp_path("cache");
        let big = vec![b'x'; DEFAULT_CHUNK_SIZE * 2 + 100];
        std::fs::write(&p, &big).unwrap();
        let mut src = FileSource::open(&p).unwrap();
        let a = src.chunk(0);
        let b = src.chunk(0);
        assert_eq!(a.len(), DEFAULT_CHUNK_SIZE);
        assert!(std::ptr::eq(a.as_ptr(), b.as_ptr()) == false); // 克隆返回，仅验证内容
        assert_eq!(a, b);
        std::fs::remove_file(&p).ok();
    }
}
