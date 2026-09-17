//! 编码选举纪律锁定（pgrust 列存调研 P0）：
//! 1. 字节确定性——同输入两次编码字节恒等（内容寻址前提）
//! 2. ≥10% 门槛——不可压缩列降级 RAW（防负收益）
//! 3. round-trip——debug 构建下 write_cbf 内联验证（本文件即运行面）

use arrow::array::{Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use std::sync::Arc;

fn batches_compressed() -> Vec<RecordBatch> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("tag", DataType::Utf8, false),
    ]));
    let n = 200_000;
    let ids: Int64Array = (0..n as i64).collect();
    let tag_v: Vec<String> = (0..n).map(|i| format!("tag-{}", i % 64)).collect();
    let tags = StringArray::from(tag_v);
    vec![RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(tags)]).unwrap()]
}

fn batches_incompressible() -> Vec<RecordBatch> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("rand", DataType::Utf8, false),
    ]));
    let n = 100_000;
    // 均匀随机印刷 ASCII（12 字符/串，95 字符字母表）：双字符组合
    // 9025 种远超 FSST 256 符号表——符号化无从收益（对照：hex 的
    // 16²=256 组合恰满表可 2× 压缩；CJK 的 UTF-8 导向字节有固定
    // 前缀模式——都不构成不可压缩样本）
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let ids: Int64Array = (0..n as i64).collect();
    let rand_v: Vec<String> = (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (0..12)
                .map(|i| (0x21 + ((x >> (i * 5)) % 94) as u8) as char)
                .collect()
        })
        .collect();
    let rands = StringArray::from(rand_v);
    vec![RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(rands)]).unwrap()]
}

/// 同输入两次编码 → 字节恒等（同选举 ⇒ 同字节法则的直接锁定；
/// debug 构建下同时行使 round-trip 验证——不对称即报错）
#[test]
fn deterministic_election_byte_equal() {
    let b = batches_compressed();
    let a1 = dendro_columnar::write_cbf(&b, 0, None).unwrap();
    let a2 = dendro_columnar::write_cbf(&b, 0, None).unwrap();
    assert_eq!(a1, a2, "同输入必须字节恒等（内容寻址前提）");
}

/// 不可压缩列（高熵 hex 串）→ ≥10% 门槛降级 RAW；可压缩列照常编码
#[test]
fn ten_percent_gate_demotes_incompressible() {
    let b = batches_incompressible();
    let data = dendro_columnar::write_cbf(&b, 0, None).unwrap();
    let footer = dendro_columnar::read_footer(&data).unwrap();
    let rand_codec = footer.rgs[0].cols[1].blocks[0].codec;
    assert_eq!(
        rand_codec,
        dendro_columnar::CodecId::Raw,
        "高熵列必须被门槛降级 RAW（got {rand_codec:?}）"
    );
    // 回读等价（语义不因降级改变）
    let (schema, rbs) = dendro_columnar::read_cbf(&data).unwrap();
    assert_eq!(rbs.len(), 1);
    assert_eq!(rbs[0].num_rows(), 100_000);
    assert_eq!(schema.fields().len(), 2);
}

/// 可压缩列不受门槛误伤（低基数 tag 应选出非 RAW codec）
#[test]
fn gate_does_not_hurt_compressible() {
    let b = batches_compressed();
    let data = dendro_columnar::write_cbf(&b, 0, None).unwrap();
    let footer = dendro_columnar::read_footer(&data).unwrap();
    let tag_codec = footer.rgs[0].cols[1].blocks[0].codec;
    assert_ne!(
        tag_codec,
        dendro_columnar::CodecId::Raw,
        "低基数文本列不应被门槛误降级（got {tag_codec:?}）"
    );
}
