//! 内存对象存储（测试/原型）。

use super::{HeadInfo, ObjResult, ObjStore};
use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::BTreeMap;

#[derive(Default)]
pub struct MemoryObjStore {
    m: RwLock<BTreeMap<String, Bytes>>,
}

impl MemoryObjStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.m.read().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// 测试辅助：已存对象字节数
    pub fn total_bytes(&self) -> u64 {
        self.m.read().values().map(|b| b.len() as u64).sum()
    }
}

impl ObjStore for MemoryObjStore {
    fn get(&self, path: &str) -> ObjResult<Bytes> {
        self.m
            .read()
            .get(path)
            .cloned()
            .ok_or_else(|| super::ObjError::NotFound(path.into()))
    }
    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes> {
        let b = self.get(path)?;
        let s = (off as usize).min(b.len());
        let e = (s + len).min(b.len());
        Ok(b.slice(s..e))
    }
    fn put(&self, path: &str, data: Bytes) -> ObjResult<()> {
        super::validate_path(path)?;
        self.m.write().insert(path.into(), data);
        Ok(())
    }
    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()> {
        super::validate_path(path)?;
        let mut m = self.m.write();
        if m.contains_key(path) {
            return Err(super::ObjError::Exists(path.into()));
        }
        m.insert(path.into(), data);
        Ok(())
    }
    fn delete(&self, path: &str) -> ObjResult<()> {
        self.m.write().remove(path);
        Ok(())
    }
    fn head(&self, path: &str) -> ObjResult<Option<HeadInfo>> {
        Ok(self
            .m
            .read()
            .get(path)
            .map(|b| HeadInfo { len: b.len() as u64 }))
    }
    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>> {
        let m = self.m.read();
        Ok(m.range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, _)| k.clone())
            .collect())
    }
    fn copy(&self, from: &str, to: &str) -> ObjResult<()> {
        let b = self.get(from)?;
        self.put(to, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditional_put() {
        let s = MemoryObjStore::new();
        s.put_if_absent("a/b", Bytes::from_static(b"x")).unwrap();
        assert!(matches!(
            s.put_if_absent("a/b", Bytes::from_static(b"y")),
            Err(super::super::ObjError::Exists(_))
        ));
        assert_eq!(s.get("a/b").unwrap().as_ref(), b"x");
        assert_eq!(s.list_prefix("a/").unwrap().len(), 1);
    }
}
