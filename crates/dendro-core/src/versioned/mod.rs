//! 版本层（SPEC 03 §4-6）：commit 对象、catalog（库级/表级 prolly map）、三方合并。

pub mod commit;
pub mod merge;

use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::format::row::{encode_key, encode_row};
use crate::objstore::cas::{Chunk, ChunkType};
use crate::prolly::{Chunker, NodeStore};
use crate::types::{ColType, SqlValue};
use std::collections::HashSet;
use std::sync::Arc;

/// 表 schema（作为 chunk 存储）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// 主键列下标（v1 支持多列；引擎层常用单列）
    pub pk: Vec<u16>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColType,
    pub nullable: bool,
}

impl TableSchema {
    pub fn pk_types(&self) -> Vec<ColType> {
        self.pk
            .iter()
            .map(|&i| self.columns[i as usize].ty)
            .collect()
    }
    pub fn col_index(&self, name: &str) -> Option<usize> {
        let low = name.to_ascii_lowercase();
        self.columns
            .iter()
            .position(|c| c.name.to_ascii_lowercase() == low)
    }
    pub fn to_chunk(&self) -> Chunk {
        Chunk {
            ty: ChunkType::Schema,
            data: serde_json::to_vec(self).unwrap(),
        }
    }
    pub fn addr_of(&self) -> Hash {
        self.to_chunk().addr()
    }
}

/// 表内一行的键（主键元组 → 保序字节）
pub fn row_key(schema: &TableSchema, pk_vals: &[SqlValue]) -> Result<Vec<u8>> {
    let want = schema.pk.len();
    if pk_vals.len() != want {
        return Err(SqlError::internal(format!(
            "pk arity {}/{}",
            pk_vals.len(),
            want
        )));
    }
    Ok(encode_key(pk_vals))
}

/// 行 → 叶值字节（列顺序 = schema 列顺序）
pub fn row_value(schema: &TableSchema, vals: &[SqlValue]) -> Result<Vec<u8>> {
    if vals.len() != schema.columns.len() {
        return Err(SqlError::internal(format!(
            "row arity {}/{}",
            vals.len(),
            schema.columns.len()
        )));
    }
    Ok(encode_row(vals))
}

/// catalog 条目（catalog map 的值）：表名 → TableEntry 字节
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TableEntry {
    pub id: u32,
    pub name: String,
    pub schema_addr: String,        // schema chunk base32
    pub table_root: Option<String>, // 表 prolly map 根（None=空表）
    pub row_count: u64,
    /// 列存投影段列表（增量物化；末尾=最新；SPEC 05 §6 v2）
    #[serde(default)]
    pub col_segments: Vec<ColSegment>,
    /// 已删除行的 pk（base32 编码键），扫描时抑制；超阈值触发全量重建
    #[serde(default)]
    pub col_deletes: Vec<String>,
    /// 列存投影行数估计（段行数和，advisory）
    #[serde(default)]
    pub col_rows: u64,
}

/// 一个列存投影段（不可变 CBF 对象）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ColSegment {
    /// 对象路径 col/{table}/{hash12}.cbf
    pub path: String,
    /// 段内行数
    pub rows: u64,
    /// pk order 域定点 min/max（段级剪枝；与 CBF footer 同口径）
    pub pk_min: u64,
    pub pk_max: u64,
}

pub fn encode_table_entry(e: &TableEntry) -> Vec<u8> {
    serde_json::to_vec(e).unwrap()
}

pub fn decode_table_entry(b: &[u8]) -> Result<TableEntry> {
    serde_json::from_slice(b).map_err(|e| SqlError::internal(format!("table entry: {e}")))
}

/// 库级操作集：catalog map（表名→TableEntry）本身是 prolly map
pub struct Versioned {
    pub store: Arc<NodeStore>,
}

impl Versioned {
    pub fn new(store: Arc<NodeStore>) -> Self {
        Self { store }
    }

    pub fn load_schema_with_entry(&self, entry: &TableEntry) -> Result<TableSchema> {
        self.load_schema(&entry.schema_addr)
    }

    pub fn load_schema(&self, addr: &str) -> Result<TableSchema> {
        let h = Hash::from_base32(addr).ok_or_else(|| SqlError::internal("bad schema addr"))?;
        let (_ty, data) = self.store.cas().get(&h)?;
        serde_json::from_slice(&data).map_err(|e| SqlError::internal(format!("schema: {e}")))
    }

    /// 读 catalog：表名 → TableEntry
    pub fn catalog_entries(
        &self,
        catalog_root: Option<&Hash>,
    ) -> Result<Vec<(String, TableEntry)>> {
        let mut out = Vec::new();
        if let Some(r) = catalog_root {
            for (k, v) in crate::prolly::cursor::range_scan(self.store.clone(), r, None, None)? {
                out.push((
                    String::from_utf8_lossy(&k).to_string(),
                    decode_table_entry(&v)?,
                ));
            }
        }
        Ok(out)
    }

    pub fn catalog_lookup(
        &self,
        catalog_root: Option<&Hash>,
        table: &str,
    ) -> Result<Option<TableEntry>> {
        if let Some(r) = catalog_root {
            let key = table.as_bytes().to_vec();
            if let Some(v) = crate::prolly::cursor::lookup(&self.store, r, &key)? {
                return Ok(Some(decode_table_entry(&v)?));
            }
        }
        Ok(None)
    }

    /// 应用 catalog 变更（upsert/delete 表条目），返回新 catalog 根
    pub fn apply_catalog(
        &self,
        catalog_root: Option<&Hash>,
        changes: Vec<(String, Option<TableEntry>)>,
        session: &mut HashSet<Hash>,
    ) -> Result<Option<Hash>> {
        let muts: Vec<(Vec<u8>, crate::prolly::chunker::Mutation)> = changes
            .into_iter()
            .map(|(name, e)| {
                let m = match e {
                    Some(t) => crate::prolly::chunker::Mutation::Put(encode_table_entry(&t)),
                    None => crate::prolly::chunker::Mutation::Delete,
                };
                (name.into_bytes(), m)
            })
            .collect();
        let mut ck = Chunker::new(&self.store, session);
        match ck.apply(catalog_root, &muts)? {
            Some(r) => Ok(Some(r)),
            None => {
                // 目录清空：以"空叶节点"为根（合法的空 prolly map）
                let node = crate::prolly::node::Node::build(0, &[]);
                let mut dummy: HashSet<Hash> = HashSet::new();
                Ok(Some(self.store.put_node(node, &mut dummy)?))
            }
        }
    }

    /// 应用表行变更
    pub fn apply_table_mutations(
        &self,
        table_root: Option<&Hash>,
        muts: Vec<(Vec<u8>, crate::prolly::chunker::Mutation)>,
        session: &mut HashSet<Hash>,
    ) -> Result<(Option<Hash>, usize)> {
        let mut ck = Chunker::new(&self.store, session);
        let new_root = ck.apply(table_root, &muts)?;
        Ok((new_root, ck.dirty))
    }
}

pub use commit::Commit;
