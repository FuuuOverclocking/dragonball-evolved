# 六：设备是异步任务，而不是 epoll 订阅者

一个 virtio-block 设备需要等待的东西不少：队列通知 eventfd、限速器的定时器、后端 I/O 完成、驱动的激活、VMM 的停止请求。

常见做法是把这些描述符全部注册进一个进程级 epoll 集合，设备实现一个 `process()` 回调。Firecracker 采用这种结构，它带来三样东西看起来属于设备模型，实际上是 epoll 这种接口形式的要求。

## 一次唤醒要被翻译两次

epoll 只能告知“某个 fd 就绪了”。于是 `event-manager` 先用一张 `fd -> SubscriberId` 表把它翻译成订阅者对象，再调用 `process()`；
订阅者收到的仍然是一个不透明事件，所以它需要匹配打包在 epoll data word 高半部分的 `u32` 标签，才能确定是自己的哪一个描述符就绪，`PROCESS_QUEUE`、`PROCESS_RATE_LIMITER` 这类常量由此而来。

两次翻译存在的原因是等待的位置和处理的位置被分开了。在就绪本身有意义的位置等待，两次翻译都不需要：

```rust
select! {
    _ = queue_notify.read() => { /* 处理队列 */ }
    _ = time::sleep_until(refilled_at) => { /* 限速解除 */ }
    _ = stop.requested() => break,
}
```

`Arc<Mutex<dyn MutEventSubscriber>>` 也随之不需要，它是注册表与订阅者被共享才引入的。设备状态有唯一所有者之后，热路径不加锁。

## 注册的增删被当作了状态

这一项更隐蔽。Firecracker 用增删 epoll 注册来表示三种情况：设备未激活、被限速、串口接收 FIFO 已满。
这三种情况本质上都是“这个任务当前停在哪一行”：

| 情况      | epoll 写法                                                  | 异步任务写法               |
| --------- | ----------------------------------------------------------- | -------------------------- |
| 未激活    | 额外一根 `activate_evt`，用途是唤醒设备以便它修改自己的注册 | `await` 激活通道           |
| 被限速    | `timerfd` 加注册加标签                                      | `sleep_until(refilled_at)` |
| FIFO 已满 | 从 epoll 摘掉输入 fd，客户机读取后再加回                    | 停在写 FIFO 的那一行       |

`activate_evt` 最能说明问题：它存在不是因为激活握手需要一个描述符，而是因为订阅者只能在 `process()` 内部修改自己的注册，未激活的设备必须先有办法被唤醒一次。
注册不再是共享资源之后，这个理由消失，激活退化为一个普通 channel：

```rust
pub struct Activator { tx: mpsc::UnboundedSender<Activation> }
```

激活确实跨线程，客户机写 `DRIVER_OK` 是一次同步 mmio 退出，发生在 vCPU 线程上，但跨线程需要的只是一个 `Send` 的发送端，不是一根 eventfd。

## 快路径上没有 vCPU 退出

```text
guest notify -> KVM_IOEVENTFD -> AsyncEventFd
             -> local backend task -> used ring -> irqfd -> guest
```

客户机写队列通知寄存器，KVM 直接写 eventfd，vCPU 不退出；vmm-main 上的 backend task 被唤醒，读描述符链、执行 I/O、写 used ring，然后写 irqfd 注入中断。

分工也因此清楚：transport（配置空间、队列配置、中断线）由 vCPU 线程同步访问，放在自己的 `Arc<Mutex<...>>` 后面；backend 是 vmm-main 上的 local task，可以持有 `!Send` 状态。只有 backend 进入 LocalRuntime，transport 不进入。

## 这套结构带来的三条要求

**出现在 `select!` 里的 future，被丢弃时不能吞掉事件。** 这是 cancel safety。
`AsyncEventFd::read` 因此把消费通知的动作收窄到 `try_io` 中的一次同步 `read`：future 要么在这次 read 之前被丢弃，此时计数和 reactor 的就绪状态都未改变，要么已经取到值。写错的表现是偶发丢失中断，不易定位。

**vmm-main 只有一条线程。** 某个设备在其中执行同步文件 I/O，所有设备一起停顿。大块 I/O 要交给 worker。

**设备自行退出属于 VMM 级事件。** 停止是协作式的：`watch` 广播停止请求，设备在安全点退出，结果通过 `DeviceExit` 上报主循环，进行中的请求不会被中断在中途。

下一篇：如果数据面根本不在这个进程里 → [数据面在 VMM 之外](07-data-plane-outside.md)
