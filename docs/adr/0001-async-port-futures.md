# ADR 0001：异步 port 使用显式 boxed future

状态：已实施；范围：原始重构计划 3.3 的异步 trait 实现方式。

原方案推荐 async-trait 以支持对象安全和 fake 注入。当前实现采用显式 `Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>`，仍支持动态分发和确定性 fake，不引入额外宏依赖。此选择不改变端口职责、第三方隔离及错误类型要求。

代价是实现需要 `Box::pin(async move { ... })`，可读性需要在后续接口增加时复核；并未消除分配，也不宣称性能优于 async-trait。若签名维护成本明显增长，可局部改用 async-trait，保留相同契约测试。

其余未完成接口、身份 newtype、测试支持 crate、覆盖率目标和配置迁移仍按原方案推进，不能以此 ADR 视为取消。
