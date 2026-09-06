//! commit 对象（SPEC 03 §4）：root=catalog map 地址、parents、height、meta。
//! 编码：自研紧凑二进制（非 JSON，对象量大且字段定长）。

use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::objstore::cas::{Chunk, ChunkType};

#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    /// 数据根 = catalog prolly map 地址
    pub root: Hash,
    /// 父提交（多父 = merge）
    pub parents: Vec<Hash>,
    /// 祖先高度（单父+1，merge=max(parent)+1）
    pub height: u64,
    /// 提交时间 ms
    pub ts_ms: i64,
    /// 分支名（审计冗余）
    pub branch: String,
    /// 作者（Agent/用户标识）
    pub author: String,
    /// 说明
    pub message: String,
}

impl Commit {
    pub fn encode(&self) -> Chunk {
        let mut d = Vec::with_capacity(64);
        d.push(1u8); // version
        d.extend_from_slice(self.root.as_bytes());
        d.extend_from_slice(&(self.parents.len() as u16).to_le_bytes());
        for p in &self.parents {
            d.extend_from_slice(p.as_bytes());
        }
        d.extend_from_slice(&self.height.to_le_bytes());
        d.extend_from_slice(&self.ts_ms.to_le_bytes());
        put_str(&mut d, &self.branch);
        put_str(&mut d, &self.author);
        put_str(&mut d, &self.message);
        Chunk { ty: ChunkType::Commit, data: d }
    }

    pub fn decode(data: &[u8]) -> Result<Commit> {
        if data.is_empty() || data[0] != 1 {
            return Err(SqlError::internal("commit version"));
        }
        let mut r = &data[1..];
        let need = |r: &[u8], n: usize| -> Result<()> {
            if r.len() < n {
                Err(SqlError::internal("commit truncated"))
            } else {
                Ok(())
            }
        };
        need(r, 20)?;
        let mut root = [0u8; 20];
        root.copy_from_slice(&r[..20]);
        r = &r[20..];
        need(r, 2)?;
        let np = u16::from_le_bytes([r[0], r[1]]) as usize;
        r = &r[2..];
        need(r, np * 20)?;
        let mut parents = Vec::with_capacity(np);
        for i in 0..np {
            let mut p = [0u8; 20];
            p.copy_from_slice(&r[i * 20..i * 20 + 20]);
            parents.push(Hash::from_bytes(p));
        }
        r = &r[np * 20..];
        need(r, 16)?;
        let height = u64::from_le_bytes(r[..8].try_into().unwrap());
        let ts_ms = i64::from_le_bytes(r[8..16].try_into().unwrap());
        r = &r[16..];
        let (branch, r) = get_str(r)?;
        let (author, r) = get_str(r)?;
        let (message, r) = get_str(r)?;
        let _ = r;
        Ok(Commit { root: Hash::from_bytes(root), parents, height, ts_ms, branch, author, message })
    }

    pub fn addr(&self) -> Hash {
        self.encode().addr()
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn get_str(r: &[u8]) -> Result<(String, &[u8])> {
    if r.len() < 4 {
        return Err(SqlError::internal("commit str truncated"));
    }
    let n = u32::from_le_bytes(r[..4].try_into().unwrap()) as usize;
    if r.len() < 4 + n {
        return Err(SqlError::internal("commit str truncated"));
    }
    Ok((
        String::from_utf8(r[4..4 + n].to_vec()).map_err(|_| SqlError::internal("commit str utf8"))?,
        &r[4 + n..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let c = Commit {
            root: Hash::of(b"root"),
            parents: vec![Hash::of(b"p1"), Hash::of(b"p2")],
            height: 42,
            ts_ms: 1_700_000_000_123,
            branch: "agent42".into(),
            author: "claude".into(),
            message: "merge into main".into(),
        };
        let chunk = c.encode();
        assert_eq!(chunk.ty, ChunkType::Commit);
        let d = Commit::decode(&chunk.data).unwrap();
        assert_eq!(d, c);
        // 地址由 chunk(含类型前缀)决定，可从 base32 恢复
        let a = chunk.addr();
        assert_eq!(Hash::from_base32(&a.to_base32()).unwrap(), a);
    }
}
