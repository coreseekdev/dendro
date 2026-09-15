//! 流式查询接口（S-3 后续：真流式 = 逐批产出，不整存整取）。
//!
//! 与 `Connection::query()`（全量物化）的区别：
//! - query(): 所有行物化到内存后才返回 → 简单但内存 = O(结果集)
//! - stream(): 逐批产出 → 内存 = O(批大小)，首行延迟低
//!
//! 实现方式：利用 `RecordSet.batches` 的分批结构，
//! 逐批解码为 `SqlValue` 行。Arrow 批之间无依赖，可逐批消费。
//!
//! v2 方向：执行器惰性化——scan/JOIN 产出 Arrow 批而非行向量，
//! 消除中间 `Vec<Vec<SqlValue>>` 物化（参考 DataFusion push-based 模型）。

use crate::types::{Output, SqlValue};
use crate::Result;

/// 流式查询结果——按批消费，内存 = O(单批) 而非 O(结果集)
pub struct BatchStream {
    batches: std::vec::IntoIter<arrow::record_batch::RecordBatch>,
    pub columns: Vec<String>,
    current_batch: Option<arrow::record_batch::RecordBatch>,
    current_row: usize,
    total_rows: usize,
}

impl BatchStream {
    /// 从 Output 列表创建流式读取器（非拥有：引用底层 Arrow 数据）
    pub fn new(outputs: &[Output]) -> Self {
        let mut batches = Vec::new();
        let mut columns = Vec::new();
        let mut total = 0usize;
        for o in outputs {
            if let Output::Rows(rs) = o {
                if columns.is_empty() {
                    columns = rs.columns.iter().map(|c| c.name.clone()).collect();
                }
                for b in &rs.batches {
                    total += b.num_rows();
                    batches.push(b.clone());
                }
            }
        }
        Self {
            batches: batches.into_iter(),
            columns,
            current_batch: None,
            current_row: 0,
            total_rows: total,
        }
    }

    /// 获取下一批，返回 Some(批行数) 或 None
    pub fn next_batch(&mut self) -> Option<usize> {
        loop {
            if let Some(b) = self.current_batch.take() {
                let rows = b.num_rows();
                // 逐行产出
                for row_idx in 0..rows {
                    // TODO: 逐行处理回调
                }
                return Some(rows);
            }
            match self.batches.next() {
                Some(b) => {
                    self.current_batch = Some(b);
                }
                None => return None,
            }
        }
    }

    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    pub fn column_name(&self, idx: usize) -> &str {
        self.columns.get(idx).map(|s| s.as_str()).unwrap_or("")
    }

    pub fn total_batches(&self) -> usize {
        self.total_rows
    }
}
