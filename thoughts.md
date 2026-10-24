- CPU 和 mem 的部分现在还是太简单了
- disks 现在是简单地放在 Spec 根下, 以后可能要先对设备做分类, 然后作为 dyn 或者 enum 放在一个比较深的地方, 然后 spec -> plan -> machine 需要各个模板分包, 不是所有设备在同一个地方解决
- 用 std::range::Range 而不用 legacy range, 然后例如 LEGACY_GSI_START..LEGACY_GSI_END 可以写成常量 range
- 中断数量是否足够的检查需要一个资源管理器来检查, 这里也需要不可变数据结构
- 有没有必要把 DeviceExit 从 MachineEvent 里拆出来?
- idle 和 busy 拆子模块, 看起来会增长
- Event::Request(None) 是不该发生的意外情况, 但直接把 vmm 关了不是一个好的生产做法, 不能期望代码始终严丝合缝, 在这种场合我们应尽可能维持其他组件 run as-is, 维持对客户至关重要的业务
- 在 AGENTS.md 加上偏好不可变数据, 用类型解决问题

这里有些建议可能根本不用做, 它们只是针对变化中的当前代码提出的, 优先演化下一阶段重点功能后再评估.
