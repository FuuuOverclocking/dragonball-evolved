# 三：请求为什么自带回复通道

`VmmRequest` 现在只有两个变体，将来会有几十个：配置启动源、挂载设备、热插拔、pause 与 resume、创建快照、恢复快照、查询状态、查询指标。
在这个规模上，请求枚举的形状决定了所有调用点的写法。两个决定影响最大。

## 决定一：回复通道由请求携带

常规做法是一进一出两个枚举，请求用 `VmmAction`，响应用 `VmmData`。Firecracker 采用这种做法，代价是 `VmmData` 每个查询一个变体，任何只关心一件事的调用者都要 match 全部响应，其余分支写 `_ => unreachable!()`。

那一行 `unreachable!()` 是把“这个请求不会返回那种响应”从类型检查换成了人工保证。响应变体增加时，人工保证不会自动更新。

改成每个变体携带 `Reply<T>`：

```rust
pub enum VmmRequest {
    Shutdown(Reply<()>),
    Status(Reply<VmmStatus>),
}
```

`client.request(VmmRequest::Status)` 的返回类型就是 `VmmStatus`，其他类型回不来。集成测试里那行 `let status: VmmStatus = ...` 是被编译器检查的，不是断言出来的。

客户端也不需要为每个请求写一个方法，它接受的是变体的构造器：

```rust
pub fn request<T>(&self, request: impl FnOnce(Reply<T>) -> VmmRequest) -> Result<T>
```

无参变体直接传函数名 `VmmRequest::Status`，带参变体传闭包 `|reply| VmmRequest::Something(config, reply)`。
阻塞的 `request`、`request_timeout` 和 `request_async` 三个入口共用同一套请求枚举，所以同步嵌入者和异步嵌入者不需要两份 API：`Answer<T>` 就是 oneshot 的接收端，本身支持阻塞、超时和 await 三种取值方式。

## 决定二：失败按请求划分

没有 crate 级的错误枚举。会失败的请求在自己的回复类型里写明：`Reply<Result<T, ThatRequestsError>>`；不会失败的请求不要求调用者处理不会发生的错误。
Firecracker 把所有失败归并到一个 `VmmActionError`，每个调用者因此要拆解一个远大于自身需要的错误空间。

## 代价：请求不再是纯数据

它持有一个 channel，所以既不能 `Clone`，也不能序列化。这是有意的取舍。

线格式，以及 TUI、GUI、HTTP schema 需要反射的信息，都应放在变体携带的参数类型上，那些是普通的 `#[derive(Debug, Clone, PartialEq)]` 数据结构；传递请求的通道不参与序列化。
参数与通道分开之后，给 API 增加一个字段和给 API 增加一种传输方式是两件互不影响的事。

`Reply<T>` 是 `Send` 的。在单线程 LocalRuntime 上这一点有用：处理函数如果需要较长时间，可以把 `Reply` 移入一个 task，由它回答，主循环不必等待（[第一篇](01-execution-contexts.md)的两条约束）。
另一侧，`Reply::send` 忽略接收端已丢弃的情况，调用方不再等待不属于 VMM 需要处理的错误。

## 少了一根 eventfd

跨线程投递请求的常见组合是 std channel 加 eventfd：channel 存数据，eventfd 负责唤醒 epoll。
这里只用 channel。`UnboundedSender::send` 是同步、无锁的入队操作，并直接唤醒 vmm 的 task，唤醒本身不需要描述符。

保留那根 eventfd 的实际后果是：同一个队列有两处状态需要一起推理，channel 里的内容和 fd 的计数，而只有 channel 能回答“现在有没有待处理请求”。
去掉之后，关机时也就不存在“计数还剩 1 但队列已空”这类需要单独处理的情况。

下一篇：请求进入之后，由谁决定状态如何变化 → [reducer 与 effect](04-reducer-and-effects.md)
