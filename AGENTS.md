# Dragonball Evolved

Dragonball 开源版本的 Rust 重构。当前阶段：x86-64 + KVM + Linux。

参考实现（Firecracker、Cloud Hypervisor、QEMU、Linux）与规范在 `.references/`，由 `scripts/setup-references.sh` 准备，只读；查阅时以本地版本为准，不凭记忆。

## 代码品味

- 高可组合性、显式所有权；不喜欢万能 manager、全局状态，以及整个设备包在 `Arc<Mutex<dyn ...>>` 中。
- 功能核与副作用外壳分离，但不为函数式而函数式：不要求整个 VMM 或设备数据面都经过 reducer。
- 偏好不可变数据：状态转移返回新值；资源分配结果作为数据记录，恢复时重放而非重算。
- 用类型表达不变量：让非法状态不可表示；不同资源空间（GPA、PIO、GSI 等）用不同类型。
- 借鉴 capability-based security：权限由持有的值表达，而不是查表、全局状态或调用者自觉。
- 抽象应来自真实不变量和已经明确的扩展需求，而不是"以后也许需要"；不提前设计庞大的抽象框架。
- 每次扩展应保持系统可运行，而不是一次重写成最终架构。
- 不把 async 当成不阻塞或可安全取消的保证。

## 架构约束

- 设备 future 单一所有者、可为 `!Send`、运行在 `LocalRuntime`。
- 操作（API 枚举变体）使用动词命名，配置/值类型使用名词，例如 `AddDisk(BlockDevConfig)`。
- 局部故障就地隔离：控制面或单个设备失败不应终止 guest，除非继续运行会破坏正确性。
