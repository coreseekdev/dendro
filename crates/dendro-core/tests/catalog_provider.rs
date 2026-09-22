//! 元数据 Provider 接口差分测试（PR1 验收）：
//! 1. 内建 DendroSemantics 与 SQL 层现路径（resolve_table）语义一致；
//! 2. 静态 mock provider（SQL 审计网关形态：零存储）语义面等价；
//! 3. 注入位三级优先（Session 覆写 > Database 注入 > 内建）；
//! 4. 名字规约统一。

use dendro_core::catalog::{DendroSemantics, SemanticsProvider, TableSemantics};
use dendro_core::types::ColType;
use dendro_core::{Database, DbOptions, StoreConfig};
use std::collections::BTreeMap;
use std::sync::Arc;

fn db_with_t() -> Arc<Database> {
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        mem_sample_interval_ms: 0,
        ..Default::default()
    })
    .unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)")
        .unwrap();
    s.exec("INSERT INTO t VALUES (1, 10, 'a'), (2, 20, 'b')").unwrap();
    db
}

/// 静态 mock（网关形态：手写元数据，零存储依赖）
struct StaticMock {
    epoch: std::sync::atomic::AtomicU64,
}

impl SemanticsProvider for StaticMock {
    fn head_version(&self) -> dendro_core::error::Result<u64> {
        Ok(1)
    }
    fn resolve(
        &self,
        ver: &u64,
        table: &str,
    ) -> dendro_core::error::Result<Arc<TableSemantics>> {
        if *ver != 1 {
            return Err(dendro_core::error::SqlError::internal("stale"));
        }
        let short = self.name_normalize(table);
        if short != "t" {
            return Err(dendro_core::error::SqlError::undefined_table(short));
        }
        Ok(Arc::new(TableSemantics {
            schema: dendro_core::versioned::TableSchema {
                name: "t".into(),
                columns: vec![
                    cd("id", ColType::Int64),
                    cd("v", ColType::Int64),
                    cd("tag", ColType::Utf8),
                ],
                pk: vec![0],
            },
            constraints: Default::default(),
            extensions: vec![],
        }))
    }
    fn list(
        &self,
        ver: &u64,
    ) -> dendro_core::error::Result<Vec<(String, Arc<TableSemantics>)>> {
        Ok(vec![("t".into(), self.resolve(ver, "t")?)])
    }
    fn schema_epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn cd(name: &str, ty: ColType) -> dendro_core::versioned::ColumnDef {
    dendro_core::versioned::ColumnDef {
        name: name.into(),
        ty,
        nullable: true,
    }
}

#[test]
fn builtin_matches_sql_path() {
    let db = db_with_t();
    let p = DendroSemantics::new(&db, "main");
    let ver = p.head_version().unwrap();
    let ts = p.resolve(&ver, "t").unwrap();
    assert_eq!(ts.schema.name, "t");
    assert_eq!(ts.schema.columns.len(), 3);
    assert_eq!(ts.schema.pk, vec![0]);
    // 名字规约：带 schema 前缀 / @branch 后缀 → 短名
    let ts2 = p.resolve(&ver, "public.t").unwrap();
    assert_eq!(ts2.schema.name, "t");
    // list
    let l = p.list(&ver).unwrap();
    assert!(l.iter().any(|(n, _)| n == "t"));
    // view_text：无视图 = None
    assert!(p.view_text("nope").unwrap().is_none());
    // schema_epoch：随 DDL 推进
    let e1 = p.schema_epoch();
    let mut s = db.new_session();
    s.exec("CREATE TABLE u (x BIGINT PRIMARY KEY)").unwrap();
    assert!(p.schema_epoch() > e1);
}

#[test]
fn stale_version_rejected() {
    let db = db_with_t();
    let p = DendroSemantics::new(&db, "main");
    // DDL 推进根后，旧令牌必须被拒（调用方重取）
    let v1 = p.head_version().unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE w (y BIGINT PRIMARY KEY)").unwrap();
    let err = p.resolve(&v1, "t").unwrap_err();
    assert!(err.message.contains("stale"), "{err:?}");
}

#[test]
fn mock_provider_semantics_equivalent() {
    let db = db_with_t();
    let builtin = DendroSemantics::new(&db, "main");
    let mock = StaticMock {
        epoch: std::sync::atomic::AtomicU64::new(7),
    };
    let vb = builtin.head_version().unwrap();
    let vm = mock.head_version().unwrap();
    let tb = builtin.resolve(&vb, "t").unwrap();
    let tm = mock.resolve(&vm, "t").unwrap();
    // 语义面等价：列名/类型/pk
    assert_eq!(tb.schema.name, tm.schema.name);
    assert_eq!(tb.schema.pk, tm.schema.pk);
    assert_eq!(tb.schema.columns.len(), tm.schema.columns.len());
    for (a, b) in tb.schema.columns.iter().zip(tm.schema.columns.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.ty, b.ty);
    }
    assert_eq!(mock.schema_epoch(), 7);
}

#[test]
fn injection_precedence() {
    let db = db_with_t();
    // 内建（无注入）
    let mut s = db.new_session();
    let _ = &s; // 覆写位默认空（经 semantics_provider 回落内建验证）
    assert_eq!(s.semantics_provider().head_version().unwrap() > 0, true);
    // Session 覆写 > Database 注入
    let mock: Arc<dyn SemanticsProvider> = Arc::new(StaticMock {
        epoch: std::sync::atomic::AtomicU64::new(42),
    });
    s.set_catalog_provider_override(mock);
    assert_eq!(s.semantics_provider().schema_epoch(), 42);
    // Database 注入 > 内建
    let s2 = db.new_session();
    db.set_catalog_provider(Arc::new(StaticMock {
        epoch: std::sync::atomic::AtomicU64::new(99),
    }));
    assert_eq!(s2.semantics_provider().schema_epoch(), 99);
    // 已持覆写的会话不受 Database 注入影响
    assert_eq!(s.semantics_provider().schema_epoch(), 42);
}

#[test]
fn builtin_governance_and_views() {
    let db = db_with_t();
    let mut s = db.new_session();
    s.exec("CREATE VIEW tv AS SELECT id FROM t").unwrap();
    let p = DendroSemantics::new(&db, "main");
    let ver = p.head_version().unwrap();
    // 建表者（超户）owner
    let gov = p.owner_acl(&ver, "t").expect("governance");
    assert_eq!(gov.0, "dendro");
    // 视图文本
    let vt = p.view_text("tv").unwrap().expect("view");
    assert!(vt.to_uppercase().contains("SELECT"));
}

#[test]
fn views_manifest_shape() {
    // view_text 走 manifest.views（BTreeMap<String,String>）的口径锁定
    let db = db_with_t();
    let m = db.manifest();
    let _: &BTreeMap<String, String> = &m.manifest.views;
}
