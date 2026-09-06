//! 内容寻址 chunk 存储（SPEC 03 §1）：SHA-512/160 哈希寻址，批量并行写入。

use super::{ObjResult, ObjStore};
use crate::format::hash::Hash;
use bytes::Bytes;
use std::sync::Arc;

/// chunk 类型标签（data[0]）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkType {
    Node = 1,
    Commit = 2,
    Schema = 3,
}

pub struct Chunk {
    pub ty: ChunkType,
    pub data: Vec<u8>, // 不含类型标签前缀
}

impl Chunk {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() + 1);
        out.push(self.ty as u8);
        out.extend_from_slice(&self.data);
        out
    }
    pub fn addr(&self) -> Hash {
        Hash::of(&self.encode())
    }
}

pub struct CasStore {
    obj: Arc<dyn ObjStore>,
}

impl CasStore {
    pub fn new(obj: Arc<dyn ObjStore>) -> Self {
        Self { obj }
    }

    pub fn chunk_path(h: &Hash) -> String {
        let s = h.to_base32();
        format!("objects/{}/{}.chunk", &s[..1], &s[1..])
    }

    pub fn get(&self, h: &Hash) -> ObjResult<(ChunkType, Bytes)> {
        let b = self.obj.get(&Self::chunk_path(h))?;
        if b.is_empty() {
            return Err(super::ObjError::Corrupt("empty chunk".into()));
        }
        let ty = match b[0] {
            1 => ChunkType::Node,
            2 => ChunkType::Commit,
            3 => ChunkType::Schema,
            other => return Err(super::ObjError::Corrupt(format!("bad chunk tag {other}"))),
        };
        Ok((ty, b.slice(1..)))
    }

    pub fn has(&self, h: &Hash) -> bool {
        self.obj.head(&Self::chunk_path(h)).ok().flatten().is_some()
    }

    /// 批量写入：批内去重 + 命中缓存跳过 + 并行上传（std::thread::scope）。
    /// 返回实际上传的 chunk 数。
    pub fn put_batch(&self, chunks: &[Chunk], session_cache: &mut std::collections::HashSet<Hash>) -> ObjResult<usize> {
        let mut jobs: Vec<(&Chunk, Hash)> = Vec::with_capacity(chunks.len());
        let mut seen = std::collections::HashSet::new();
        for c in chunks {
            let h = c.addr();
            if seen.insert(h) && !session_cache.contains(&h) && !self.has(&h) {
                jobs.push((c, h));
            }
        }
        // 并行上传（并发 8；对象存储 PUT 天然并行）
        const PAR: usize = 8;
        let obj = &self.obj;
        let err: std::sync::Mutex<Option<super::ObjError>> = std::sync::Mutex::new(None);
        let result: std::result::Result<(), super::ObjError> = std::thread::scope(|s| {
            for group in jobs.chunks(PAR) {
                for (c, h) in group {
                    let err = &err;
                    s.spawn(move || {
                        let data = c.encode();
                        if let Err(e) = obj.put(&Self::chunk_path(h), Bytes::from(data)) {
                            let mut g = err.lock().unwrap();
                            if g.is_none() {
                                *g = Some(e);
                            }
                        }
                    });
                }
                // scope 结束时 join；每组结束后检查错误，尽早失败
                if err.lock().unwrap().is_some() {
                    return Err(super::ObjError::Transient("put batch failed".into()));
                }
            }
            Ok(())
        });
        result.map_err(|_| err.lock().unwrap().take().expect("error"))?;
        for (_, h) in &jobs {
            session_cache.insert(*h);
        }
        Ok(jobs.len())
    }
}
