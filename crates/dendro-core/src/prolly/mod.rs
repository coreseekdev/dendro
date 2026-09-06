//! prolly tree（SPEC 03 §2）：内容寻址的概率 B-tree。
//!
//! 边界确定性 ⇒ 同内容子树同地址 ⇒ 结构共享/零成本分支/chunk 级 diff。
//! 参数沿用 dolt 实证值：min 512 / target 4096 / max 16384 / weibull K=4。

pub mod chunker;
pub mod cursor;
pub mod diff;
pub mod node;
pub mod splitter;
pub mod store;

pub use chunker::{Chunker, Mutation};
pub use node::Node;
pub use store::NodeStore;
