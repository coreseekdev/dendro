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
    /// 本 epoch WAL 的 GC 起始段号（之前的段已墓碑回收；0=视作 1。GC 定案）
    #[serde(default)]
    pub wal_first_seg: u64,
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
    /// GC 墓碑：已无新 manifest 引用、等待保留窗口过期的对象（GC 定案，docs/design/GC定案.md）
    #[serde(default)]
    pub tombstones: Vec<Tombstone>,
}

/// 一条待回收对象。登记与"新 manifest 停止引用"在同一版本原子发布——
/// 崩溃窗口内旧 manifest 仍引用的对象绝不会被删除；保留窗口覆盖滞后读者。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tombstone {
    pub path: String,
    pub at_ms: i64,
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
            tombstones: Vec::new(),
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

    /// 读最新版本（探测加速；**遇任何空洞回落 LIST**）。
    /// 空洞来源：GC 删除 `latest-16` 以下的版本对象。若在空洞处"就近停下"，
    /// 停滞写者会把提交写进已删版本号的空洞（put_if_absent 在不存在路径上
    /// 成功）→ 影子谱系，其 ack 的分支头注定被 GC 删除（P1-C）。
    /// 故 NotFound 一律以 LIST 的结果为准——LIST 频率 = 空洞命中率 + 探测
    /// 窗口用尽，GC 节奏下可忽略。
    pub fn load_latest(&self) -> ObjResult<(u64, Manifest)> {
        let cached = *self.cached.lock();
        if cached > 0 {
            for v in (cached + 1)..=(cached + MAX_PROBES) {
                match self.read_version(v) {
                    Ok(_) => continue, // 连续存在，继续找更高
                    Err(ObjError::NotFound(_)) => return self.load_latest_via_list(),
                    Err(e) => return Err(e),
                }
            }
            // 探测窗口内全部存在 → 可能还有更高版本，LIST 兜底
            return self.load_latest_via_list();
        }
        self.load_latest_via_list()
    }

    /// LIST manifest/ 前缀取最大版本（权威路径；天然兼容空洞）
    fn load_latest_via_list(&self) -> ObjResult<(u64, Manifest)> {
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

    /// GC：可清理的 manifest 版本——只按数量保留最近 `keep_recent` 个
    /// （旧版本无引用语义；`load_latest` 的 LIST 路径天然兼容由此产生的空洞）
    pub fn retained(&self, latest: u64, keep_recent: u64) -> Vec<u64> {
        let lo = latest.saturating_sub(keep_recent);
        (1..=lo).collect()
    }

    /// GC：删除旧 manifest 版本对象（load_latest 的探测/LIST 双路径兼容空洞）。
    /// 返回实际删除数；个别删除失败仅计数（下轮重试）。
    pub fn delete_versions(&self, vers: &[u64]) -> usize {
        let mut n = 0;
        for v in vers {
            if self.obj.delete(&Self::path(*v)).is_ok() {
                n += 1;
            }
        }
        n
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

    fn store() -> (ManifestStore, Arc<dyn ObjStore>) {
        let m: Arc<dyn ObjStore> = Arc::new(MemoryObjStore::new());
        let s = ManifestStore::new(m.clone());
        s.init(1000).unwrap();
        (s, m)
    }

    #[test]
    fn optimistic_commit_chain() {
        let (s, _obj) = store();
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

    #[test]
    fn load_latest_falls_back_to_list_on_gc_holes() {
        // P1-C 回归：GC 删除中间版本对象后，**停滞写者**（cached 停在 v1）
        // 的探测在 cached+1 处撞洞——必须回落 LIST 拿到真最新版本，
        // 否则其提交会写进已删版本号的空洞形成影子谱系。
        let (s, obj) = store();
        let mut m = s.load_latest().unwrap().1;
        for _ in 0..9 {
            m.refs.insert("b".into(), BranchHead { wal_seg: m.version, ..Default::default() });
            m = s.read_version(s.commit(m.version, m.clone()).unwrap()).unwrap();
        }
        let latest = s.load_latest().unwrap().0;
        assert_eq!(latest, 10);
        // 模拟另一实例 GC：删除 2..=9（保留 1 与 10）→ 版本号空间出现空洞
        for v in 2..=9 {
            obj.delete(&format!("manifest/{v:020}.json")).unwrap();
        }
        // 停滞写者：新的 ManifestStore，cached 强制停在 1（同模块可访问私有字段）
        let stalled = ManifestStore::new(obj.clone());
        *stalled.cached.lock() = 1;
        let (top, m10) = stalled.load_latest().unwrap();
        assert_eq!(top, 10, "撞洞必须回落 LIST 而非把 cached 当最新");
        assert_eq!(m10.version, 10);
        // 停滞写者从真最新版本继续提交：不产生影子谱系
        let v = stalled.commit(10, m10).unwrap();
        assert_eq!(v, 11);
        assert!(stalled.load_latest().unwrap().0 >= 11);
    }
}
