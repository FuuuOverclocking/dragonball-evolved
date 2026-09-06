# 二：主循环的协调状态放在哪里

vmm-main 上有一个循环，在关机之前不会返回。它需要持有的东西包括：请求通道、机器本体、尚未回答的 reply、退出状态。
默认做法是把它们放进一个 `Vmm` struct，逻辑都写成它的方法。这个做法有两个问题。

## 一、`select!` 的分支不能借用 `self`

主循环要同时等待三类输入：客户请求、vCPU 退出、设备退出，写出来就是 `select!`。
如果每个分支直接处理事件，分支体需要 `&mut self`，而此时另外两个分支的 future 正借用着 `self.requests`、`self.vcpu_exits`、`self.devices`，编译不通过。

常见的绕法有几种：把字段逐个 `take()` 出来、给每个字段加 `Arc<Mutex>`、把 receiver 从 struct 里移出来做局部变量。
它们都在回避同一个问题：判断“发生了什么”和决定“怎么处理”被写在了同一个表达式里。分开就没有冲突：

```rust
let event = select! {
    biased;
    request = self.requests.next() => Event::Request(request),
    Some(exit) = self.devices.next_exit() => Event::DeviceExit(exit),
};

match self.handle(event).await { ... }
```

`select!` 只负责产生一个 `Event`，所有借用在它求值结束时释放，`handle` 拿到完整的 `&mut self`。
另一个效果是这份分支列表不随请求变体增长：`VmmRequest` 从 2 个变体增加到 50 个，这里仍然是三行。

## 二、这个 struct 没有需要共同维护的不变量

`Vmm` 的字段之间没有约束关系，它只是若干局部变量的集合。给它一个名字、再写一批 `&mut self` 方法，实际效果是把这些变量的作用域从一个栈帧扩大到整个类型，任何方法都能修改它们。

所以目标形态是把协调状态放回栈帧：

```rust
async fn run_vmm(mut requests: VmmServer, kvm: Arc<KvmContext>) -> VmmExitStatus {
    let mut machine: Option<Machine> = None;
    let mut pending = HashMap::<OperationId, PendingReply>::new();
    // loop { next_loop_input -> dispatch/reduce -> LoopControl }
}
```

判据是：只有具备独立不变量、独立生命周期或者可测试接口的概念才定义类型。
`Machine` 满足，它聚合内存、KVM、设备，有真实的所有权和析构顺序；`MachineModel` 满足，它是可比较、可序列化的纯数据；“主循环的几个局部变量”不满足。

## 中央 epoll 不再需要

Firecracker 的主循环是 `loop { event_manager.run(); 检查 shutdown_exit_code }`。它必须存在，因为所有设备描述符都注册在一个进程级 epoll 集合里，需要有人轮询。

这里没有这个集合（[第六篇](06-device-as-task.md)展开）。设备各自等待自己的描述符，主循环里剩下的只有属于 VMM 整体的事件。
它的职责因此压缩为三项：选择输入、维护 pending reply、提交 effect。状态决策不在这里，在 reducer（[第四篇](04-reducer-and-effects.md)）。

## 退出路径上的两条规则

**关机回复表示“已经关完”，不是“已经收到”。** 所以 `Step::Stop` 把 `Reply<()>` 一路带出循环，等设备全部 quiesce 之后才 `send(())`。
调用方收到回复即可安全地删除 socket、退出进程，不需要额外判断。

**`run_vmm` 返回之前，所有 pending reply 必须被完成或置为失败。** 否则等待方得到的是“channel 被丢弃”，这个错误不包含任何原因。
`Reply` 不进入 model、reducer 和 snapshot，只按 `OperationId` 存在栈上，就是为了让这项清算有一个确定的位置。

还有一类容易遗漏的输入：所有客户端都已消失。这不是错误，没有人能再向 VMM 下命令，属于正常退出，但值得记一条 warn。
对应的单元测试 `the_loop_exits_once_every_client_is_gone` 只能写在 crate 内部，因为 `VmmClient` 同时持有发送端和线程句柄，集成测试无法既断开通道又还能 join。

下一篇：请求本身的形状，以及它为什么自带回复通道 → [请求自带回复通道](03-typed-requests.md)
