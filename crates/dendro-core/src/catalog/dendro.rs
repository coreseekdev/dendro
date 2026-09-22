//! dendro 内建的 SemanticsProvider 实现——包装现有 versioned catalog
//! 读取路径（RESOLVE_CACHE 复用、行为零变化），作为注入位为 None 时的
//! 默认供给。SQL 审计网关场景注入外部实现替代。

use super::provider::{ConstraintSet, SemanticsProvider, TableSemantics};
use crate::engine::Database;
use crate::error::{Result, SqlError};
use std::sync::{Arc, Weak};

pub struct DendroSemantics {
    db: Weak<Database>,
    /// provider 绑定的分支视图（多分支 = 多实例；Session 按会话分支取）
    branch: String,
}

impl DendroSemantics {
    pub fn new(db: &Arc<Database>, branch: &str) -> Self {
        Self {
            db: Arc::downgrade(db),
            branch: branch.to_string(),
        }
    }

    fn db(&self) -> Result<Arc<Database>> {
        self.db
            .upgrade()
            .ok_or_else(|| SqlError::internal("database dropped while resolving metadata"))
    }

    /// 当前分支的 catalog root（令牌源）
    fn current_root(&self) -> Result<Option<crate::format::hash::Hash>> {
        let db = self.db()?;
        let b = db.branch(&self.branch)?;
        let head = b.head.load_full();
        Ok(head.as_ref().as_ref().map(|c| c.root))
    }

    fn check_ver(&self, ver: &u64) -> Result<()> {
        let now = self
            .current_root()?
            .as_ref()
            .map(|r| r.as_u64())
            .unwrap_or(0);
        if *ver != now {
            return Err(SqlError::internal(format!(
                "stale catalog version {ver} != {now}; re-fetch head_version"
            )));
        }
        Ok(())
    }

    fn semantics_of(
        &self,
        schema: crate::versioned::TableSchema,
        entry: &crate::versioned::TableEntry,
    ) -> TableSemantics {
        TableSemantics {
            schema,
            constraints: ConstraintSet {
                unique_sets: entry.unique_sets.clone(),
                check_exprs: entry.check_exprs.clone(),
                foreign_keys: entry.foreign_keys.clone(),
            },
            extensions: entry.extensions.clone(),
        }
    }
}

impl SemanticsProvider for DendroSemantics {
    fn head_version(&self) -> Result<u64> {
        Ok(self
            .current_root()?
            .as_ref()
            .map(|r| r.as_u64())
            .unwrap_or(0))
    }

    fn resolve(&self, ver: &u64, table: &str) -> Result<Arc<TableSemantics>> {
        let db = self.db()?;
        self.check_ver(ver)?;
        let short = self.name_normalize(table);
        // 显式根版收口实现（RESOLVE_CACHE 命中 = Arc clone）——与 SQL
        // 层现路径同源，行为零变化
        let root = self.current_root()?;
        let rc =
            crate::sql::scan::resolve_table_at(&db, root.as_ref(), &short)?;
        Ok(Arc::new(self.semantics_of(rc.0.clone(), &rc.1)))
    }

    fn list(&self, ver: &u64) -> Result<Vec<(String, Arc<TableSemantics>)>> {
        let db = self.db()?;
        self.check_ver(ver)?;
        let root = self.current_root()?;
        let catalog = crate::versioned::Versioned::new(db.store.clone());
        let mut out = Vec::new();
        for (name, entry) in catalog.catalog_entries(root.as_ref())? {
            let schema = catalog.load_schema(&entry.schema_addr)?;
            out.push((name, Arc::new(self.semantics_of(schema, &entry))));
        }
        Ok(out)
    }

    fn view_text(&self, name: &str) -> Result<Option<Arc<str>>> {
        let db = self.db()?;
        let short = self.name_normalize(name);
        Ok(db
            .manifest()
            .manifest
            .views
            .get(&short)
            .map(|def| Arc::from(def.as_str())))
    }

    fn schema_epoch(&self) -> u64 {
        self.db().map(|db| db.schema_version()).unwrap_or(0)
    }

    fn owner_acl(&self, ver: &u64, table: &str) -> Option<super::provider::Governance> {
        let db = self.db().ok()?;
        self.check_ver(ver).ok()?;
        let short = self.name_normalize(table);
        let root = self.current_root().ok()?;
        let rc = crate::sql::scan::resolve_table_at(&db, root.as_ref(), &short).ok()?;
        Some((rc.1.owner.clone(), rc.1.acl.clone()))
    }

    fn indexes(&self, _ver: &u64, _table: &str) -> Vec<Arc<super::provider::IndexDef>> {
        Vec::new() // PR3 接入：TableEntry.indexes 落地后回填
    }
}
