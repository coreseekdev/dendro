//! 目录抽象层：SQL 前端的元数据供给接口 + dendro 内建实现。

pub mod dendro;
pub mod provider;

pub use dendro::DendroSemantics;
pub use provider::{
    ConstraintSet, ExtensionDef, Governance, IndexDef, SemanticsProvider, TableSemantics,
};
