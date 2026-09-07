//! Manifest——元数据根（SPEC 01 §5）：单调版本 + Create 条件写的乐观提交。
//!
//! 内容：分支表(refs)、表目录(tables)、GC 水位。JSON 编码（v1），
//! 读最新版不依赖 LIST（缓存版本 +1 探测，超限退回 list）。

use super::{ObjError, ObjResult, ObjStore};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BranchHead {
    /// 最近 checkpoint 的 commit 地址（base32）；None=尚无 checkpoint
    #[serde(default)]
    pub commit: Option<String>,
    /// 已 durable 的 WAL 段号（高水位）
    pub wal_seg: u64,
    /// 父分支名
    #[serde(default)]
    pub parent: Option<String>,
    /// fork 点：父分支的 (commit, wal_seg)
    #[serde(default)]
    pub fork_commit: Option<String>,
    #[serde(default)]
    pub fork_wal_seg: u64,
    /// 写者 fence epoch（0=无）
    #[serde(default)]
    pub epoch: u64,
    /// 已物化进 commit 树的最高 txn seq（恢复时跳过 ≤ 此 seq 的帧）
    #[serde(default)]
    pub covered_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TableMeta {
    pub id: u32,
    /// 表 prolly map 根地址（base32）；None=空表
    #[serde(default)]
    pub root: Option<String>,
    /// schema chunk 地址
    pub schema: String,
    /// 主键列序号（v1 单列主键；多列时为列号列表）
    pub pk_cols: Vec<u16>,
    /// 列存投影代数（None=未物化）
    #[serde(default)]
    pub col_gen: Option<u64>,
    /// 列存投影包含的行数（pruning/对齐用）
    #[serde(default)]
    pub col_rows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u64,
    /// 乐观提交的不确定写消解凭证（SPEC 01 §3）：commit 时生成、写入 payload；
    /// PUT 结果不确定时 GET 反查该字段判定"是否其实已成功"
    #[serde(default)]
    pub writer_putid: Option<String>,
    pub format_version: u32,
    pub refs: BTreeMap<String, BranchHead>,
    pub tables: BTreeMap<String, TableMeta>,
    pub next_table_id: u32,
    /// 最近一次 GC 扫过的 manifest 版本
    pub gc_last_sweep_ver: u64,
    /// 全库创建时间戳 ms
    pub created_ms: i64,
}

impl Manifest {
    pub fn empty(version: u64, created_ms: i64) -> Self {
        let mut refs = BTreeMap::new();
        refs.insert("main".to_string(), BranchHead::default());
        Self {
            version,
            format_version: 1,
            writer_putid: None,
            refs,
            tables: BTreeMap::new(),
            next_table_id: 1,
            gc_last_sweep_ver: 0,
            created_ms,
        }
    }
}

pub struct ManifestStore {
    obj: Arc<dyn ObjStore>,
    cached: parking_lot::Mutex<u64>,
}

const MAX_PROBES: u64 = 32;

impl ManifestStore {
    pub fn new(obj: Arc<dyn ObjStore>) -> Self {
        Self { obj, cached: parking_lot::Mutex::new(0) }
    }

    fn path(ver: u64) -> String {
        format!("manifest/{ver:020}.json")
    }

    fn read_version(&self, ver: u64) -> ObjResult<Manifest> {
        let b = self.obj.get(&Self::path(ver))?;
        serde_json::from_slice(&b).map_err(|e| ObjError::Corrupt(format!("manifest {ver}: {e}")))
    }

    /// 初始化：库不存在时写入 version 1（已存在则报 Exists）
    pub fn init(&self, now_ms: i64) -> ObjResult<()> {
        let m = Manifest::empty(1, now_ms);
        self.obj
            .put_if_absent(&Self::path(1), serde_json::to_vec(&m).unwrap().into())?;
        *self.cached.lock() = 1;
        Ok(())
    }

    /// 读最新版本（探测优先，LIST 兜底）
    pub fn load_latest(&self) -> ObjResult<(u64, Manifest)> {
        let cached = *self.cached.lock();
        if cached > 0 {
            // 从 cached+1 探测
            for v in (cached + 1)..=(cached + MAX_PROBES) {
                match self.read_version(v) {
                    Ok(_) => continue, // 继续找更高
                    Err(ObjError::NotFound(_)) => {
                        // v-1 是最高（从 cached+1 到 v-1 至少有一个存在才走到这；
                        // 若 cached+1 就 NotFound，则 cached 是最高）
                        let top = if v == cached + 1 { cached } else { v - 1 };
                        let m = self.read_version(top)?;
                        *self.cached.lock() = top;
                        return Ok((top, m));
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        // 兜底：LIST manifest/ 前缀取最大
        let mut vers = Vec::new();
        for p in self.obj.list_prefix("manifest/")? {
            if let Some(stem) = p.strip_prefix("manifest/") {
                if let Some(num) = stem.strip_suffix(".json") {
                    if let Ok(v) = num.parse::<u64>() {
                        vers.push(v);
                    }
                }
            }
        }
        vers.sort_unstable();
        let top = vers.pop().ok_or_else(|| ObjError::NotFound("manifest/".into()))?;
        let m = self.read_version(top)?;
        *self.cached.lock() = top;
        Ok((top, m))
    }

    /// 乐观提交：cur=读取到的版本；成功返回新版本号；冲突返回 Err(Exists)。
    /// 不确定结果（网络超时）用 payload 内嵌 putid 反查消解。
    pub fn commit(&self, cur: u64, mut new: Manifest) -> ObjResult<u64> {
        new.version = cur + 1;
        let putid = format!("{}-{:x}", cur + 1, rand_u64());
        new.writer_putid = Some(putid.clone());
        let path = Self::path(cur + 1);
        match self
            .obj
            .put_if_absent(&path, serde_json::to_vec(&new).unwrap().into())
        {
            Ok(()) => {
                *self.cached.lock() = cur + 1;
                Ok(cur + 1)
            }
            Err(ObjError::Uncertain(_)) => {
                // 可能已成功：GET 反查 putid
                match self.read_version(cur + 1) {
                    Ok(m) if m.writer_putid.as_deref() == Some(putid.as_str()) => {
                        *self.cached.lock() = cur + 1;
                        Ok(cur + 1)
                    }
                    Ok(_) => Err(ObjError::Exists(path)), // 别人写赢了
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// GC：可清理的 manifest 版本（保留最近 K + 被引用的）
    pub fn retained(&self, latest: u64, keep_recent: u64) -> Vec<u64> {
        let lo = latest.saturating_sub(keep_recent);
        (1..=lo).collect()
    }
}

fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    let mut x = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 30;
    x.wrapping_mul(0xBF58_476D_1CE4_E5B9)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objstore::memory::MemoryObjStore;

    fn store() -> ManifestStore {
        let m: Arc<dyn ObjStore> = Arc::new(MemoryObjStore::new());
        let s = ManifestStore::new(m);
        s.init(1000).unwrap();
        s
    }

    #[test]
    fn optimistic_commit_chain() {
        let s = store();
        let (v0, mut m) = s.load_latest().unwrap();
        assert_eq!(v0, 1);
        m.refs.insert("agent42".into(), BranchHead { wal_seg: 3, ..Default::default() });
        let v1 = s.commit(v0, m.clone()).unwrap();
        assert_eq!(v1, 2);
        // 过期提交冲突
        assert!(matches!(s.commit(v0, m.clone()), Err(ObjError::Exists(_))));
        let (_, m2) = s.load_latest().unwrap();
        assert!(m2.refs.contains_key("agent42"));
    }
}
