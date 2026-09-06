# 九：快照的前提是确定谁还在写内存

“保存虚拟机”听起来像一次大规模拷贝：写出 RAM，写出寄存器，结束。
实际动手会发现难点在拷贝之前：必须先确定没有任何写者仍在修改要读取的数据。

客户内存的写者有四类：

| 写者                | 位置                                 |
| ------------------- | ------------------------------------ |
| vCPU                | 内核态 `KVM_RUN`，随时可能写         |
| 进程内 backend task | vmm-main，可能正持有已取出的描述符链 |
| vhost-user 后端     | 另一个进程，只能请求它停止           |
| VFIO 设备           | 硬件 DMA，用户态代码不在路径上       |

四类写者对应四种停止方式，这也是状态机里需要 `Quiescing` 的原因：停止本身需要时间，而这段时间内必须拒绝新请求。

## quiesce 完成的定义

三条，缺一条都不算完成：

1. 不再接受新请求；
2. in-flight I/O 已完成，或已序列化进设备状态；
3. queue index 和客户内存不再变化。

对应手段：

| 数据路径       | 停止方式                                              |
| -------------- | ----------------------------------------------------- |
| 进程内 backend | 任务停在安全点，保存 queue 与 in-flight 状态          |
| vhost-user     | suspend vring，读取 backend 与 inflight 迁移状态      |
| VFIO           | 进入设备 migration `STOP_COPY`，读取 migration stream |
| DAX            | 阻止 map 与 unmap，保存映射元数据或选择空窗口重建     |

第 2 条中“或已序列化”需要说明。一个已经从 avail ring 取出、尚未写入 used ring 的描述符链有两种合法处理：执行完成，或者作为 inflight 状态保存下来、恢复后重放。
不合法的是第三种，即丢弃它，客户机会一直等待这个请求。

## 快照是版本化的纯数据

```rust
struct MachineSnapshot {
    manifest: SnapshotManifest,   // format_version / arch / required_capabilities / topology_hash
    model: MachineModel,
    memory: MemorySnapshot,
    vm: KvmVmState,
    vcpus: Vec<VcpuState>,
    devices: BTreeMap<DeviceId, DeviceSnapshot>,
}
```

不序列化 `Arc`、fd、裸指针、mmap 地址（HVA）、线程和 task。这条规则展开就是一张对照表：

| 组件       | 保存                                       | 不保存                 |
| ---------- | ------------------------------------------ | ---------------------- |
| RAM        | region 元数据、全量页或脏页                | 当前 HVA               |
| KVM VM     | clock、irqchip、PIT、路由状态              | `VmFd`                 |
| vCPU       | regs、sregs、MSR、LAPIC、events、MP state  | `VcpuFd` 与线程        |
| 模拟设备   | feature、config、queue index、后端状态     | eventfd 与 task        |
| vhost-user | 协商的 feature、vring、inflight 与迁移状态 | socket、kickfd、callfd |
| VFIO       | migration blob 与兼容性标识                | device fd 与 DMA 映射  |
| DAX        | 窗口布局、映射元数据或重建策略             | 窗口页面导出与 HVA     |

[第四篇](04-reducer-and-effects.md)中 model 与运行时资源的分离在这里体现出作用：`MachineModel` 可以直接作为一个 section，不需要“从运行时对象中提取纯数据”的转换层，这类转换层容易与实际状态不一致。

## 格式规则中的三条

**section 独立版本化，遇到未知的必需 section 直接拒绝。** 版本兼容不能按尽力而为处理，要么能确定可以恢复，要么不恢复。

**machine state 规范化为 `Paused`。** 不保存 `Saving`、`Quiescing` 这类瞬态状态，否则恢复方拿到的是一台“正在保存”的虚拟机。

**manifest 最后原子发布。** 数据先写入临时目标并校验，最后才让 manifest 可见。否则进程在中途退出会留下一个看起来完整、实际截断的快照，而问题要到很久之后有人用它恢复时才暴露。

diff 快照还有一条：引用不可变的 base digest，而不是某个路径下的文件。路径的内容会被覆盖，digest 不会。

## 两种情况下不要执行

**增量快照需要完整的脏页信息。** KVM dirty log 覆盖 vCPU 的写，VFIO 的 DMA 写和部分后端的写不在其中。缺少设备侧 dirty tracking 时应退回全量。

**在线快照需要所有设备都能停止并交出状态。** 存在 `NonMigratable` 设备时拒绝在线快照，见[第七篇](07-data-plane-outside.md)的三个声明。

最后是一条工程约束，回到[第一篇](01-execution-contexts.md)的两条约束：数 GB 的 RAM 拷贝和迁移流 I/O 要交给专用 writer 或 worker。
放在 vmm-main 上执行的结果是快照期间整个控制面失去响应，包括用于取消快照的请求。

下一篇：这些步骤的顺序本身就是不变量 → [启动、停机与恢复的顺序](10-ordering.md)
