//! `dendro backup`——一致性点物理备份（M-1 最低交付）。
//!
//! 原理：dendro 的存储是 **append-only**（对象一经写入不可变），因此
//! "先拷数据对象、**最后**拷 manifest/ 前缀"即得到一个一致性点快照：
//! 快照内的 manifest 所引用的全部对象必然已拷入（它们在 manifest 之前
//! 落盘），manifest 之后的新提交不被包含——快照 = 拷贝完成时捕获到的
//! 最高 manifest 版本的一致状态。
//!
//! 实现说明：直接递归拷贝文件树（本地→本地语义）。不能用
//! `ObjStore::list_prefix`——本地实现是**非递归**的（只列直接子文件），
//! 会漏掉 `objects/{x}/{hash}.chunk` 这类多层路径（回归
//! backup_restore_roundtrip 曾因此红）。
//!
//! 运行手册（不停机）：直接对生产库运行本命令；需要更长 PITR 窗口时对
//! 源库以 `--gc-retention-ms -1` 暂停 GC。恢复 = 把备份目录整体作为
//! `--data` 打开。
//!
//! 范围诚实声明：v1 全量拷贝（含历史，体积 ≈ 全库），增量/精简拷贝为
//! M-1 后续。

use dendro_core::objstore::ObjError;
use std::path::Path;

/// 元数据前缀（最后拷，决定一致性点）
const META_PREFIX: &str = "manifest/";

pub fn backup_dir(src_root: &Path, dst_root: &Path) -> Result<(usize, u64), ObjError> {
    if !src_root.join(META_PREFIX).is_dir() {
        return Err(ObjError::NotFound(
            "source has no manifest objects — not a dendro data root?".into(),
        ));
    }
    // ① 数据对象（除 manifest 外全部前缀；fence 亦备份——副本打开需要它）
    let (mut objects, mut bytes) = copy_tree_rooted(src_root, dst_root, src_root, META_PREFIX)?;
    // ② manifest 最后拷：拷入的最高版本即快照一致性点
    let (n, b) = copy_tree_rooted(
        &src_root.join(META_PREFIX),
        &dst_root.join(META_PREFIX),
        &src_root.join(META_PREFIX),
        "",
    )?;
    objects += n;
    bytes += b;
    if n == 0 {
        return Err(ObjError::NotFound(
            "source has no manifest objects — not a dendro data root?".into(),
        ));
    }
    Ok((objects, bytes))
}

/// 递归拷贝 `cur` 下全部文件到 `dst_root/<相对 root 的路径>`；
/// `skip`（相对 root 的前缀，如 "manifest/"）用于数据阶段跳过 manifest；
/// `""` = 不跳过。返回 (文件数, 字节数)。目标已存在同路径文件则跳过
/// （append-only ⇒ 同路径同内容，幂等）。
fn copy_tree_rooted(
    cur: &Path,
    dst_root: &Path,
    root: &Path,
    skip: &str,
) -> Result<(usize, u64), ObjError> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let rd = std::fs::read_dir(cur)
        .map_err(|e| ObjError::Io(format!("read_dir {}: {e}", cur.display())))?;
    for entry in rd {
        let entry = entry.map_err(|e| ObjError::Io(format!("readdir: {e}")))?;
        let p = entry.path();
        let rel = p
            .strip_prefix(root)
            .map_err(|e| ObjError::Io(format!("rel {}: {e}", p.display())))?;
        if !skip.is_empty() {
            // skip 以路径段语义判断（如 "manifest/" 匹配 rel 首段）
            if rel.starts_with(Path::new(skip.trim_end_matches('/'))) {
                continue;
            }
        }
        if p.is_dir() {
            let (n, b) = copy_tree_rooted(&p, dst_root, root, skip)?;
            files += n;
            bytes += b;
            continue;
        }
        let target = dst_root.join(rel);
        files += 1; // 计数含幂等跳过者（调用方以"是否见过对象"判断，非拷贝数）
        if let Ok(dst_meta) = std::fs::metadata(&target) {
            // 幂等 + 守卫（第五轮 P1）：append-only ⇒ 同路径同内容；但上次
            // 中断可能留下**截断文件**（旧版直接 fs::copy 非原子）——尺寸
            // 不符即重拷修复，绝不让撕裂文件固化。
            if dst_meta.len() == p.metadata().map_err(|e| ObjError::Io(e.to_string()))?.len() {
                continue;
            }
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ObjError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        // **原子写**：临时文件 + rename——备份进程中断/并发运行都不会在
        // 最终路径留下截断对象（manifest 恰是一致性点本身，必须原子）
        let tmp = target.with_extension("dendro-bak-tmp");
        let n = std::fs::copy(&p, &tmp)
            .map_err(|e| ObjError::Io(format!("copy {}: {e}", p.display())))?;
        std::fs::rename(&tmp, &target)
            .map_err(|e| ObjError::Io(format!("rename {}: {e}", target.display())))?;
        bytes += n;
    }
    Ok((files, bytes))
}
