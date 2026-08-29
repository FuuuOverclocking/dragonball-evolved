# 四：状态转换与副作用的分界

VMM 里最难查的缺陷集中在状态转换上：pause 执行到一半时收到 snapshot 请求、restore 失败后资源建了一半、shutdown 时某个后端仍在写客户内存。

这些缺陷难查的原因相同：判断“能不能转换”的逻辑和执行 ioctl 的代码写在一起，复现一次需要一台开了虚拟化的机器、一块磁盘和一个特定版本的内核。

要把它们分开，就要给纯函数划一条边界。但不必划过头：`KVM_RUN`、`mmap`、线程、socket 本身就是副作用，硬写成纯函数只会多一层没有实际作用的封装。
所以采用 functional core / imperative shell：决策是纯函数，执行不是。

## 两份状态：模型与聚合

```rust
struct Machine {
    model: MachineModel,          // 纯数据
    memory: Arc<MachineMemory>,   // 运行时资源
    vm: Arc<KvmVm>,
    vcpus: VcpuManager,
    // ...
}

struct MachineModel {
    state: MachineState,
    config: ValidatedMachineConfig,
    topology: DeviceTopology,
    resources: ResourceLayout,
}
```

`MachineModel` 可比较、可验证、可序列化，它就是快照里的 machine-model section。`Machine` 是领域聚合对象，比 model 多出来的部分全是 fd、mmap 和线程句柄。
分开之后，状态机测试只需要构造 `MachineModel`，不需要 `/dev/kvm`。

## 决策写成函数

```rust
fn validate(config: MachineConfig, caps: &HostCapabilities) -> Result<ValidatedMachineConfig>;
fn plan(config: &ValidatedMachineConfig, inv: &ResourceInventory) -> Result<MachinePlan>;
fn reduce(model: &MachineModel, input: MachineInput) -> Result<Transition>;
fn query_status(model: &MachineModel) -> VmmStatus;

struct Transition { next: MachineModel, effects: Vec<Effect> }
```

`reduce` 的输入是命令和事件的并集，即 `MachineInput::Command | Event`。
这一点容易写错：如果只让命令进入 reducer，那么设备失败、vhost-user 断开、vCPU 退出这些事件就会在别的地方修改状态，“状态由谁决定”重新变成需要全局搜索才能回答的问题。

`Effect` 只描述要做什么，不持有运行时对象：

```rust
enum Effect {
    StartVcpus, StopVcpus, QuiesceDevices,
    AttachDevice(DevicePlan), RegisterIrq(IrqPlan), MapDax(DaxMapPlan),
    ChangeVfioMigrationState(VfioMigrationState),
}
```

于是 reducer 的返回值可以直接断言：给定这个 model 和这个输入，应当产生哪几个 effect。ioctl、fd、mmap、线程、socket 只出现在 executor 里。

effect 执行失败不通过 reducer 的返回值传播，而是产生一个新事件再次进入 reducer，由 reducer 决定回滚、停止还是降级。
这样错误处理和正常转换走同一条路径，不会出现一套只在失败时运行、因此从未被测试过的代码。

## 状态表里为什么有瞬态状态

```text
Created -> Materializing -> Paused <-> Running
Paused/Running -> Quiescing -> Paused        (pause / snapshot)
Paused -> Saving -> Paused|Running
Created -> Restoring -> Paused
Running|Paused -> Stopping -> Stopped
Materializing|Quiescing|Saving|Restoring -> Failed
```

`Quiescing`、`Saving`、`Restoring` 是瞬态状态。它们存在的原因是“停下来”本身需要时间，而这段时间内必须拒绝新请求。
它们不进入快照：快照中的 machine state 一律规范化为 `Paused`，否则会保存出一台处于“正在保存”状态的虚拟机，恢复时无法处理。

`Failed` 的含义要限定得窄：它不表示出错，而表示一致性已经无法保证，即回滚失败或者不变量被破坏。
能够干净回滚的失败应当退回 `Created` 或 `Paused`，并把错误作为请求的 reply 返回。

## 两条边界

**数据面不经过 reducer。** virtqueue 处理、DMA、DAX 缺页各走自己的快路径，不接触状态机。reducer 的输入频率是每秒几次的量级，不是每秒十万次。

**布局在运行后保持稳定。** `DeviceId`、GPA、BAR、GSI、KVM slot 在 materialize 之后不再变化。
这是快照的前提：`topology_hash` 的比较和恢复时“在原 GPA 重建窗口”都依赖它。

下一篇：内存是第一个必须明确“谁分配、谁使用”的资源 → [客户内存的三个使用者](05-guest-memory-consumers.md)
