//! 树 diff（SPEC 03 §6）：双游标并行走树，地址相同的子树整棵跳过。
//! 输出叶层变更流，供三方合并使用。

use super::node::EntryVal;
use super::NodeStore;
use crate::error::Result;
use crate::format::hash::Hash;
use std::sync::Arc;

/// 单 key 变更：old=None 表示新增；new=None 表示删除
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub key: Vec<u8>,
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

pub fn diff(store: Arc<NodeStore>, a: Option<&Hash>, b: Option<&Hash>) -> Result<Vec<Change>> {
    match (a, b) {
        (None, None) => Ok(vec![]),
        (None, Some(rb)) => {
            let mut out = Vec::new();
            let mut it = super::cursor::TreeIter::new(store, rb)?;
            while let Some((k, v)) = it.next_item()? {
                out.push(Change {
                    key: k,
                    old: None,
                    new: Some(v),
                });
            }
            Ok(out)
        }
        (Some(ra), None) => {
            let mut out = Vec::new();
            let mut it = super::cursor::TreeIter::new(store, ra)?;
            while let Some((k, v)) = it.next_item()? {
                out.push(Change {
                    key: k,
                    old: Some(v),
                    new: None,
                });
            }
            Ok(out)
        }
        (Some(ra), Some(rb)) => {
            if ra == rb {
                return Ok(vec![]);
            }
            let mut out = Vec::new();
            diff_nodes(&store, ra, rb, &mut out)?;
            Ok(out)
        }
    }
}

fn diff_nodes(store: &Arc<NodeStore>, a: &Hash, b: &Hash, out: &mut Vec<Change>) -> Result<()> {
    let na = store.get_node(a)?;
    let nb = store.get_node(b)?;
    if na.level() == nb.level() {
        if na.level() > 0 {
            // 同层内部节点：归并子树
            let mut ia = 0usize;
            let mut ib = 0usize;
            while ia < na.count() || ib < nb.count() {
                if ib >= nb.count() || (ia < na.count() && na.key(ia) < nb.key(ib)) {
                    // 仅 a 有：整棵删除
                    let child = match na.value(ia) {
                        EntryVal::Child(c, _) => c,
                        _ => unreachable!(),
                    };
                    emit_subtree(store, &child, None, out)?;
                    ia += 1;
                } else if ia >= na.count() || na.key(ia) > nb.key(ib) {
                    let child = match nb.value(ib) {
                        EntryVal::Child(c, _) => c,
                        _ => unreachable!(),
                    };
                    emit_subtree(store, &child, Some(true), out)?;
                    ib += 1;
                } else {
                    let ca = match na.value(ia) {
                        EntryVal::Child(c, _) => c,
                        _ => unreachable!(),
                    };
                    let cb = match nb.value(ib) {
                        EntryVal::Child(c, _) => c,
                        _ => unreachable!(),
                    };
                    if ca != cb {
                        diff_nodes(store, &ca, &cb, out)?;
                    }
                    ia += 1;
                    ib += 1;
                }
            }
        } else {
            // 同层叶：归并条目
            let mut ia = 0usize;
            let mut ib = 0usize;
            while ia < na.count() || ib < nb.count() {
                if ib >= nb.count() || (ia < na.count() && na.key(ia) < nb.key(ib)) {
                    out.push(Change {
                        key: na.key(ia),
                        old: Some(leaf_val(&na, ia)),
                        new: None,
                    });
                    ia += 1;
                } else if ia >= na.count() || na.key(ia) > nb.key(ib) {
                    out.push(Change {
                        key: nb.key(ib),
                        old: None,
                        new: Some(leaf_val(&nb, ib)),
                    });
                    ib += 1;
                } else {
                    let va = leaf_val(&na, ia);
                    let vb = leaf_val(&nb, ib);
                    if va != vb {
                        out.push(Change {
                            key: na.key(ia),
                            old: Some(va),
                            new: Some(vb),
                        });
                    }
                    ia += 1;
                    ib += 1;
                }
            }
        }
    } else {
        // 层不同：浅者为深者的"单条目视图"展开
        if na.level() < nb.level() {
            diff_shallow(store, &nb, 0, &na, out)?;
        } else {
            diff_shallow(store, &na, 0, &nb, out)?;
        }
    }
    Ok(())
}

/// na 是单节点（更低层），nb 是高层子树：枚举 nb 全部叶条目，与 na 逐条对比
fn diff_shallow(
    store: &Arc<NodeStore>,
    deep: &super::node::Node,
    _di: usize,
    shallow: &super::node::Node,
    out: &mut Vec<Change>,
) -> Result<()> {
    // 把 shallow 单节点当作一棵树与 deep 子树做 diff：等价于
    // diff(shallow_entries, deep_entries) —— 借助迭代器归并
    let mut deep_items: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    collect_items(store, deep, &mut deep_items)?;
    let mut shallow_items: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for i in 0..shallow.count() {
        shallow_items.push((shallow.key(i), leaf_val(shallow, i)));
    }
    merge_item_lists(shallow_items, deep_items, out);
    Ok(())
}

/// 展开整棵子树的叶条目；side=Some(true) 表示只存在于 b（新增），None 表示只存在于 a（删除）
fn emit_subtree(
    store: &Arc<NodeStore>,
    addr: &Hash,
    new_side: Option<bool>,
    out: &mut Vec<Change>,
) -> Result<()> {
    let mut items = Vec::new();
    collect_items(store, &store.get_node(addr)?, &mut items)?;
    for (k, v) in items {
        match new_side {
            Some(true) => out.push(Change {
                key: k,
                old: None,
                new: Some(v),
            }),
            Some(false) | None => out.push(Change {
                key: k,
                old: Some(v),
                new: None,
            }),
        }
    }
    Ok(())
}

pub fn collect_items(
    store: &Arc<NodeStore>,
    node: &super::node::Node,
    out: &mut Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<()> {
    if node.level() == 0 {
        for i in 0..node.count() {
            out.push((node.key(i), leaf_val(node, i)));
        }
        return Ok(());
    }
    for i in 0..node.count() {
        if let EntryVal::Child(c, _) = node.value(i) {
            collect_items(store, &store.get_node(&c)?, out)?;
        }
    }
    Ok(())
}

fn leaf_val(n: &super::node::Node, i: usize) -> Vec<u8> {
    match n.value(i) {
        EntryVal::Item(v) => v,
        _ => unreachable!(),
    }
}

/// 两个有序条目表归并出 Change 流（shallow=a 视角）
fn merge_item_lists(a: Vec<(Vec<u8>, Vec<u8>)>, b: Vec<(Vec<u8>, Vec<u8>)>, out: &mut Vec<Change>) {
    let mut ia = 0usize;
    let mut ib = 0usize;
    while ia < a.len() || ib < b.len() {
        if ib >= b.len() || (ia < a.len() && a[ia].0 < b[ib].0) {
            out.push(Change {
                key: a[ia].0.clone(),
                old: Some(a[ia].1.clone()),
                new: None,
            });
            ia += 1;
        } else if ia >= a.len() || a[ia].0 > b[ib].0 {
            out.push(Change {
                key: b[ib].0.clone(),
                old: None,
                new: Some(b[ib].1.clone()),
            });
            ib += 1;
        } else {
            if a[ia].1 != b[ib].1 {
                out.push(Change {
                    key: a[ia].0.clone(),
                    old: Some(a[ia].1.clone()),
                    new: Some(b[ib].1.clone()),
                });
            }
            ia += 1;
            ib += 1;
        }
    }
}
