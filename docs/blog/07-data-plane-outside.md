# 七：数据面在 VMM 之外：vhost-user 与 VFIO

上一篇的快路径已经很短：客户机写通知，eventfd 唤醒一个任务，任务写 irqfd。还有更短的形式，就是 VMM 完全不参与传输。

```text
vhost-user
guest notify -> KVM_IOEVENTFD -> kickfd -> 外部后端进程
外部后端 -> callfd -> KVM_IRQFD -> guest

VFIO
guest BAR  -> BAR mmap 或陷入的配置访问
guest DMA  -> IOMMU 映射 -> 硬件
硬件 IRQ   -> VFIO eventfd -> KVM_IRQFD -> guest
```

两条路径在稳定状态下都不进入 VMM 的用户态代码。随之而来的问题是 VMM 还负责什么。答案是职责从数据面转移到控制面和失效处理，后者更难写对。

## vhost-user：全局信息只有 VMM 有

外部后端不知道客户机有几块内存、队列在哪里、什么时候可以开始收发。这些信息只有 VMM 掌握，所以 VMM 保留以下工作：

- 建立连接与 feature 协商；
- `SET_MEM_TABLE`，把 RAM 的 backing fd 和 GPA 映射发送过去，[第五篇](05-guest-memory-consumers.md)中 backing 必须是文件的约束由此而来；
- `SET_VRING_*` 以及 kickfd 和 callfd，并把它们分别注册为 ioeventfd 和 irqfd；
- reset、错误处理，以及迁移相关的状态切换。

其中一个细节值得注意：kickfd 和 callfd 由 VMM 创建、注册进 KVM，然后交给后端使用。
这决定了停机和快照时由谁来断开通路。这个能力必须留在 VMM 一侧，否则一个卡死的后端会让 VM 无法停止。

## VFIO：通常没有后端任务

VFIO 设备一般不需要异步后端任务。能 mmap 的 BAR 直接映射给客户机，中断由内核 eventfd 直连 irqfd，DMA 经过 IOMMU。
vmm-main 上剩下的工作是一份短清单：hotplug、错误、reset、迁移状态切换，以及不能 mmap 的 BAR，后者仍需陷入用户态由 VMM 模拟。

这里有一个默认值需要明确：DAX 窗口和其他设备私有窗口默认不加入无关 VFIO 设备的 DMA domain。
把全部客户内存映射进 IOMMU 实现起来最省事，但允许一个直通设备 DMA 写入另一个设备的共享窗口。默认应取最小映射，扩大范围需要显式配置。

## 失效模型变宽

数据面移出之后，VMM 仍要判断这台 VM 是否健康，而它只能通过事件得知：

```rust
enum MachineEvent {
    VcpuExit { id: VcpuId, reason: VcpuExit },
    DeviceStopped { id: DeviceId },
    DeviceFailed { id: DeviceId, error: String },
    VhostUserDisconnected { id: DeviceId },
    VfioError { id: DeviceId, error: String },
}
```

`VhostUserDisconnected` 是进程内设备不会出现的情况：后端进程退出时，可能仍持有已取出但未完成的描述符。
客户机观察到的是请求不返回，这与设备返回错误是两种不同的故障。所以它必须作为事件进入 reducer（[第四篇](04-reducer-and-effects.md)），由状态机决定重连、降级还是停机，不能在设备代码中就地决定。

## 可迁移性是最贵的一项

进程内后端的 quiesce 只需要让任务停在安全点。外部后端不同：需要请求另一个进程冻结 vring 并交出 inflight 状态，而它可能不支持相应的协议版本。
VFIO 更严格：设备本身要支持 migration state，例如 `STOP_COPY`，恢复时目标设备还要兼容。

所以每个设备要显式声明属于哪一类：

| 声明            | 含义                                           |
| --------------- | ---------------------------------------------- |
| `Migratable`    | 状态可完整保存与恢复                           |
| `Restartable`   | 状态不保存，恢复时重建，例如重新连接、重新扫描 |
| `NonMigratable` | 拒绝在线快照                                   |

这三个标签是 `validate` 和 `reduce` 的输入，不只是文档。后端不支持所需迁移协议时，正确行为是拒绝快照请求，而不是保存一份无法恢复的数据。

下一篇：一个既不是内存也不是设备的东西 → [DAX 窗口](08-dax-window.md)
