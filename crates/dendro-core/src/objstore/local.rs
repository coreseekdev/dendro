//! 本地目录对象存储：目录布局与对象存储语义一致（开发/基准默认后端）。
//! put = 临时文件 + rename（原子）；put_if_absent = create_new（条件写）。

use super::{HeadInfo, ObjError, ObjResult, ObjStore};
use bytes::Bytes;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct LocalObjStore {
    root: PathBuf,
    /// 写后 fsync（WAL/manifest 正确性需要；基准可关）
    pub sync_writes: std::sync::atomic::AtomicBool,
}

impl LocalObjStore {
    pub fn open(root: impl Into<PathBuf>) -> ObjResult<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            sync_writes: std::sync::atomic::AtomicBool::new(true),
        })
    }

    fn full(&self, path: &str) -> ObjResult<PathBuf> {
        super::validate_path(path)?;
        Ok(self.root.join(path))
    }

    fn write_file(&self, path: &Path, data: &[u8], exclusive: bool) -> ObjResult<()> {
        self.write_file_impl(path, data, exclusive, true)
    }

    fn write_file_no_sync(&self, path: &Path, data: &[u8], exclusive: bool) -> ObjResult<()> {
        self.write_file_impl(path, data, exclusive, false)
    }

    fn write_file_impl(
        &self,
        path: &Path,
        data: &[u8],
        exclusive: bool,
        sync: bool,
    ) -> ObjResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        if exclusive {
            opts.create_new(true);
        }
        let mut f = opts.open(path).map_err(|e| map_io(e, path))?;
        f.write_all(data).map_err(|e| map_io(e, path))?;
        if sync && self.sync_writes.load(std::sync::atomic::Ordering::Relaxed) {
            f.sync_all().map_err(|e| map_io(e, path))?;
        }
        Ok(())
    }
}

/// 唯一临时路径（PID + 进程级原子序号——同进程多线程写同内容寻址
/// 块不再共享 tmp：原 PID-only 名在并发 put_batch 的 PAR=8 线程 ×
/// 多个 CAS 写者交叉时同名碰撞 → O_TRUNC 互踩 / rename ENOENT
///（58030 主因——评审实证））
fn unique_tmp(p: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    p.with_extension(format!("tmp{}.{}", std::process::id(), n))
}

/// 原子发布 rename + 同内容幂等（并发写同地址：胜者已 rename、
/// 败者 tmp 仍在 → 正常 rename；败者已被清扫（crash 恢复）→ 目标
/// 存在即视为成功——内容寻址保证同地址 = 同字节）
fn rename_published(tmp: &Path, p: &Path) -> ObjResult<()> {
    match fs::rename(tmp, p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // tmp 不在（被并发胜者消费或清扫）——目标存在即幂等成功
            if p.exists() {
                Ok(())
            } else {
                Err(map_io(e, p))
            }
        }
        Err(e) => Err(map_io(e, p)),
    }
}

fn map_io(e: std::io::Error, p: &Path) -> ObjError {
    match e.kind() {
        std::io::ErrorKind::AlreadyExists => ObjError::Exists(p.display().to_string()),
        std::io::ErrorKind::NotFound => ObjError::NotFound(p.display().to_string()),
        _ => ObjError::Io(format!("{}: {e}", p.display())),
    }
}

impl ObjStore for LocalObjStore {
    fn get(&self, path: &str) -> ObjResult<Bytes> {
        let p = self.full(path)?;
        let mut f = fs::File::open(&p).map_err(|e| map_io(e, &p))?;
        let mut buf = Vec::with_capacity(f.metadata().map(|m| m.len() as usize).unwrap_or(0));
        f.read_to_end(&mut buf).map_err(|e| map_io(e, &p))?;
        Ok(Bytes::from(buf))
    }

    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes> {
        use std::io::Seek;
        let p = self.full(path)?;
        let mut f = fs::File::open(&p).map_err(|e| map_io(e, &p))?;
        let flen = f.metadata().map_err(|e| map_io(e, &p))?.len();
        if off >= flen {
            return Ok(Bytes::new());
        }
        let len = (len as u64).min(flen - off) as usize;
        f.seek(std::io::SeekFrom::Start(off))
            .map_err(|e| map_io(e, &p))?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf).map_err(|e| map_io(e, &p))?;
        Ok(Bytes::from(buf))
    }

    fn put(&self, path: &str, data: Bytes) -> ObjResult<()> {
        let p = self.full(path)?;
        let tmp = unique_tmp(&p);
        self.write_file(&tmp, &data, false)?;
        rename_published(&tmp, &p)?;
        Ok(())
    }

    fn put_no_sync(&self, path: &str, data: Bytes) -> ObjResult<()> {
        let p = self.full(path)?;
        let tmp = unique_tmp(&p);
        self.write_file_no_sync(&tmp, &data, false)?;
        rename_published(&tmp, &p)?;
        Ok(())
    }

    /// 批末一次 syncfs：覆盖本文件系统全部脏页/元数据（含本批数据文件
    /// 与 rename 目录项）。一个 syscall 替代批内逐文件 fsync——大
    /// checkpoint（万级 chunk）的吞吐关键；崩溃面与逐文件等价
    ///（syncfs 返回 = 全部落盘）
    fn sync_batch(&self) -> ObjResult<()> {
        let f = fs::File::open(&self.root).map_err(|e| map_io(e, &self.root))?;
        let r = unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&f)) };
        if r != 0 {
            return Err(map_io(std::io::Error::last_os_error(), &self.root));
        }
        Ok(())
    }

    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()> {
        let p = self.full(path)?;
        match self.write_file(&p, &data, true) {
            Ok(()) => Ok(()),
            Err(ObjError::Exists(_)) => Err(ObjError::Exists(path.into())),
            Err(e) => Err(e),
        }
    }

    fn append_at(&self, path: &str, offset: u64, data: &[u8]) -> ObjResult<()> {
        let p = self.full(path)?;
        let parent_existed = p.parent().map(|d| d.exists()).unwrap_or(true);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| map_io(e, &p))?;
        }
        let existed = p.exists();
        let f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&p)
            .map_err(|e| map_io(e, &p))?;
        // 预分配尺寸内的写入不改文件尺寸 ⇒ fdatasync 纯数据刷盘（无元数据
        // 日志）。未预分配时 pwrite 扩展尺寸 → fdatasync 退化为 fsync 级
        // 别（正确性不变，仅慢）。
        use std::io::{Seek, SeekFrom, Write};
        let mut f = f;
        f.seek(SeekFrom::Start(offset)).map_err(|e| map_io(e, &p))?;
        f.write_all(data).map_err(|e| map_io(e, &p))?;
        f.sync_data().map_err(|e| map_io(e, &p))?;
        if !existed {
            // 目录项持久化链（审计 R5-2）：文件已 fdatasync，但目录项/新建
            // 的 epoch 目录本身可能未落盘——掉电后整个目录（含已落盘段）
            // 消失 = 已 ack 提交丢失。父目录 + （新建时的）祖父目录都要
            // fsync；失败**上抛**（目录项不持久 = ack 不安全 → 毒化）。
            if let Some(parent) = p.parent() {
                let d = fs::File::open(parent).map_err(|e| map_io(e, &p))?;
                d.sync_all().map_err(|e| map_io(e, &p))?;
                if !parent_existed {
                    if let Some(gp) = parent.parent() {
                        let gd = fs::File::open(gp).map_err(|e| map_io(e, &p))?;
                        gd.sync_all().map_err(|e| map_io(e, &p))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// fallocate 全量预分配（空间 + 尺寸一步到位；后续 append_at 不改尺寸）
    fn preallocate(&self, path: &str, len: u64) -> ObjResult<()> {
        let p = self.full(path)?;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| map_io(e, &p))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&p)
                .map_err(|e| map_io(e, &p))?;
            let ret = unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, len as i64) };
            if ret != 0 {
                return Err(ObjError::Io(std::io::Error::last_os_error().to_string()));
            }
            // fallocate 改变尺寸到 len——数据一致性由 append_at 的偏移写保证
            f.sync_all().map_err(|e| map_io(e, &p))?;
        }
        #[cfg(not(unix))]
        {
            let _ = (p, len);
        }
        Ok(())
    }

    /// 缩到实际使用尺寸（封段回收预分配空间；失败仅空间浪费）
    fn resize(&self, path: &str, len: u64) -> ObjResult<()> {
        let p = self.full(path)?;
        let f = fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .map_err(|e| map_io(e, &p))?;
        f.set_len(len).map_err(|e| map_io(e, &p))?;
        Ok(())
    }

    fn supports_append(&self) -> bool {
        true
    }

    fn delete(&self, path: &str) -> ObjResult<()> {
        let p = self.full(path)?;
        match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(map_io(e, &p)),
        }
    }

    fn head(&self, path: &str) -> ObjResult<Option<HeadInfo>> {
        let p = self.full(path)?;
        match fs::metadata(&p) {
            Ok(m) if m.is_file() => Ok(Some(HeadInfo { len: m.len() })),
            Ok(_) => Ok(None),
            Err(_) => Ok(None),
        }
    }

    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>> {
        super::validate_path(prefix)?;
        let dir_path = self.root.join(prefix);
        let mut out = Vec::new();
        if !dir_path.exists() {
            return Ok(out);
        }
        let dir = fs::read_dir(&dir_path).map_err(|e| map_io(e, &dir_path))?;
        for e in dir {
            let entry = e.map_err(|e| map_io(e, &dir_path))?;
            let p = entry.path();
            if entry.file_type().map_err(|e| map_io(e, &p))?.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                out.push(format!("{prefix}{name}"));
            }
        }
        out.sort();
        Ok(out)
    }

    fn copy(&self, from: &str, to: &str) -> ObjResult<()> {
        let a = self.full(from)?;
        let b = self.full(to)?;
        if let Some(parent) = b.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&a, &b).map_err(|e| map_io(e, &a))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_conditional() {
        let dir = std::env::temp_dir().join(format!("dendro-test-{}", std::process::id()));
        let s = LocalObjStore::open(&dir).unwrap();
        s.put("wal/00000000000000000001.wal", Bytes::from_static(b"abc"))
            .unwrap();
        assert_eq!(
            s.get("wal/00000000000000000001.wal").unwrap().as_ref(),
            b"abc"
        );
        assert_eq!(
            s.get_range("wal/00000000000000000001.wal", 1, 2)
                .unwrap()
                .as_ref(),
            b"bc"
        );
        s.put_if_absent("objects/ab/cd.chunk", Bytes::from_static(b"z"))
            .unwrap();
        assert!(matches!(
            s.put_if_absent("objects/ab/cd.chunk", Bytes::from_static(b"z2")),
            Err(ObjError::Exists(_))
        ));
        assert_eq!(s.head("objects/ab/cd.chunk").unwrap().unwrap().len, 1);
        assert!(s.head("nope").unwrap().is_none());
        assert_eq!(s.list_prefix("wal/").unwrap().len(), 1);
        s.delete("wal/00000000000000000001.wal").unwrap();
        assert!(s.head("wal/00000000000000000001.wal").unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bad_paths() {
        let dir = std::env::temp_dir().join(format!("dendro-test-bad-{}", std::process::id()));
        let s = LocalObjStore::open(&dir).unwrap();
        assert!(s.put("../escape", Bytes::new()).is_err());
        assert!(s.put("/abs", Bytes::new()).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tmp_collision_tests {
    use super::*;
    use crate::objstore::memory::MemoryObjStore;
    use crate::objstore::ObjStore;

    /// 回归：并发写同内容寻址地址——唯一 tmp + 幂等 rename 后
    /// 全部成功（原 PID-only tmp 下部分线程 rename ENOENT → 58030）
    #[test]
    fn concurrent_same_address_writes_all_succeed() {
        let dir = std::env::temp_dir().join(format!("dendro_tmp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = super::LocalObjStore::open(&dir).unwrap();
        let data = b"same content-addressed chunk".to_vec();
        let results: Vec<_> = std::thread::scope(|s| {
            (0..16)
                .map(|_| {
                    let st = &store;
                    let d = data.clone();
                    s.spawn(move || st.put("objects/a/test.chunk", d.into()))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap().is_ok())
                .collect()
        });
        assert!(
            results.iter().all(|&ok| ok),
            "全部并发写同地址必须成功（58030 回归）：{results:?}"
        );
        // 内容正确（唯一字节）
        let got = store.get("objects/a/test.chunk").unwrap();
        assert_eq!(&got[..], &data[..]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 幂等 rename：tmp 不在但目标存在 → Ok（并发胜者已发布同内容）
    #[test]
    fn rename_published_idempotent() {
        let dir = std::env::temp_dir().join(format!("dendro_tmp_idem_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.chunk");
        std::fs::write(&target, b"content").unwrap();
        let ghost = dir.join("ghost.tmp"); // 不存在
        assert!(rename_published(&ghost, &target).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 确保没有残留 tmp 文件（唯一名写入者清理自身 tmp）
    #[test]
    fn no_stale_tmp_files() {
        let _ = MemoryObjStore::new(); // 抑制 unused 警告
        let dir = std::env::temp_dir().join(format!("dendro_tmp_stale_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = super::LocalObjStore::open(&dir).unwrap();
        for i in 0..10 {
            store
                .put(&format!("obj/{i}"), format!("data-{i}").into_bytes().into())
                .unwrap();
        }
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "无残留 tmp：{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
