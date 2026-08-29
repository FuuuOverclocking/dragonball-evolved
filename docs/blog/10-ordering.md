# 十：启动、停机与恢复失败的顺序

前面九篇讨论结构：所有权、线程归属、哪些代码是纯函数。这一篇讨论顺序：同一组操作，顺序不同，结果可以从正常工作变成破坏宿主内存。

三段序列，每段有一个不变量。

## 启动：客户机开始执行之前，路径必须建好

```text
分配 RAM 与设备窗口 -> 创建 VM 与 irqchip -> 注册 KVM slot
-> 分配 MMIO/PIO/PCI/GSI -> 每个设备：transport 与 backend、ioeventfd/irqfd
-> 创建 VcpuFd[] -> 启动 local 与 control 任务 -> 启动 vCPU 线程
```

不变量：内存和 transport 在 vCPU 启动前就绪，设备数据路径先于客户机执行建立。

违反它的写法看起来合理，即先启动 vCPU，设备随后挂载。后果是客户机的第一次队列通知可能落在尚未注册 ioeventfd 的地址上：
KVM 找不到对应的 ioeventfd，会把它转成一次 MMIO 退出，交给一个还不存在的 transport。表现是启动阶段偶发挂起，是否触发取决于客户机内核探测设备的时机。

vCPU 线程的启动放在最后一步，因为它是这条序列里唯一一旦开始就不再受 VMM 控制的动作。

## 停机：先停写者，再断通路，最后释放内存

```text
Shutdown
-> vCPU：immediate_exit + 定向信号 -> 退出 KVM_RUN -> join
-> 停设备数据面：vhost-user 停止、DAX 清除映射、VFIO mask/reset/unmap DMA
-> 注销 ioeventfd、irqfd 与 device slot
-> drop devices 与 address space -> drop VmFd -> drop MachineMemory
```

不变量：任何还能写客户内存的组件仍在运行时，都不能释放它可以写入的内存。

这条的后果比其他几条严重。客户机崩溃只影响客户机；一个已经停止但 DMA 仍在进行的 VFIO 设备写入已经 `munmap` 的地址，破坏的是宿主，而且写入的很可能是这段虚拟地址被重新分配后的新使用者。这类问题在测试中不会出现。

顺序中有两个细节：

- vCPU 必须最先停止，它是客户机继续发起新 I/O 的唯一来源。先停设备的话，客户机会继续向队列中提交请求。
- `immediate_exit`、定向信号、join 三者缺一不可：只设置标志，线程仍阻塞在 `KVM_RUN` 中；只发信号，`EINTR` 之后循环会重新进入 `KVM_RUN`；不 join，无法确认线程已经退出。这也是[第一篇](01-execution-contexts.md)中 vCPU 不使用线程池的原因。

关机回复在这条链的末端发出，所以它表示已经关完，调用方可以直接退出进程（[第二篇](02-main-loop-state.md)）。

## 恢复：校验先于任何副作用

```text
读 manifest -> 校验版本、架构、能力、拓扑 -> RestorePlan
-> 在原 GPA 重建 RAM 与设备窗口 -> 载入全量或 base 加脏页
-> 创建 VM、irqchip、KVM slot -> 恢复 clock、irqchip、PIT、路由
-> 重建 transport 与资源布局 -> DAX 预留窗口、vhost-user 重连、VFIO 进入 RESUMING
-> 应用 VcpuState[] -> 启动任务 -> 恢复外部数据面 -> 启动 vCPU 线程
```

不变量一：校验先于副作用。manifest 与宿主能力不匹配时，此时应该还没有创建 VM、没有连接任何后端、没有占用任何 slot。

不变量二在失败路径上：恢复失败时只销毁已经 materialize 的运行时资源，原始 `MachineSnapshot` 保持不变，可以重新规划或再次恢复。
这依赖[第四篇](04-reducer-and-effects.md)的分离：快照是不可变纯数据，materialize 的产物是另一组对象。如果恢复过程边解析边修改快照对象，一次失败就会让重试不可能。

顺序上恢复与启动同构：内存和布局在前，外部依赖居中，vCPU 线程最后。恢复就是一次以快照为输入的启动。

## 十篇之间的关系

前九篇的每条边界，最终都服务于这三段序列能被写对：

- 执行上下文分离（1），使 vCPU 可以被精确停止；
- 协调状态放在栈帧、reply 有确定的清算位置（2、3），使“已经关完”这个语义可以表达；
- reducer 与 effect 分离（4），使失败产生事件，而不是留下一台建了一半的机器；
- 内存集中分配（5），使释放顺序有明确的负责方；
- 设备是异步任务（6），使“停在安全点”是一个 `await`，而不是一组注册表操作；
- 数据面在进程外（7、8），使 quiesce 必须显式，可迁移性必须声明；
- 快照是纯数据（9），使恢复可以重试。

反过来也成立：如果一个 VMM 的关机路径需要靠 sleep 来等待，原因通常不在关机代码里，而在前面某一个决定上。

回到[系列索引](README.md)。结论形式的汇总见[目标架构](../vmm-architecture.md)。
