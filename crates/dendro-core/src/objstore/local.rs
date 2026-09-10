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
        if self.sync_writes.load(std::sync::atomic::Ordering::Relaxed) {
            f.sync_all().map_err(|e| map_io(e, path))?;
        }
        Ok(())
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
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        self.write_file(&tmp, &data, false)?;
        fs::rename(&tmp, &p).map_err(|e| map_io(e, &p))?;
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
