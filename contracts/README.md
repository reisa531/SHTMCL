# Contract fixtures

`contracts/samples/` 是 v1 信封的 JSON 样例，由以下两处消费：

- `crates/domain/tests/contract_samples.rs`：反序列化并检查不变量。
- `scripts/check-contract.mjs`：无依赖的样例/版本一致性检查。

样例不是业务数据集，不包含账户、令牌、实例内容或在线状态。修改字段时同步更新
`contracts/domain-v1.md`、Rust 类型、TypeScript 镜像和样例。
