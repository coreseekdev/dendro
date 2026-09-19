//! 文本 IR（dendro.ir，spec 09）：v1 标量方言（printer/parser/verifier
//! 同文件，防漂移）。逻辑方言随执行层管线化落地。

pub mod canonical;
pub mod plan;
pub mod text;
pub mod unparse;
