# vmm-main 状态模型：思路

配套文件：`vmm-main-states.rs`（类型与它们之间的关系，伪代码 + 中文注释）。本文解释为什么这样建模，以及如何从当前实现一步步走过去。

范围：一个完整的 VMM 需要的全部生命周期能力——冷启动、暂停/恢复、virtio-mem、ACPI CPU 热插拔、ACPI 内存（DIMM）热插拔、快照与恢复、热迁移、热升级、进程内重启（E8）——以及它们与退出、故障隔离的关系。对当前代码，它同时是一份重构方案。

## 0. 结论先行

- 整机状态不是一个枚举，而是几个**彼此独立的维度**：准入（还收不收请求）、本进程与这台 guest 的关系（有没有机器、有没有运行权）、执行状态（Running / Paused / 正在迁移）、拓扑与在途变更、导出导入会话、退出事实。跨维度的约束用**嵌套**表达（例如 Precopy 只能出现在 `Exec::Running` 里），而不是把乘积摊平。
- 操作按"**在等什么**"分类，判据是"超时能否有一个安全的含义"：有界的独占执行；可能卡住的按作用域并行；等 guest 的根本不是操作，只是设备里的协议状态；只读导出可以中止；交出运行权不可逆。每一类在主循环里有固定的位置和固定的 Shutdown 行为。
- 三类东西要分清：**值**（Spec、Plan、Topology、ExitFacts：可比较、进快照）、**所有者**（fd、线程、映射：不可克隆，drop 不证明任何事）、**证明**（`CpusStopped`、`Cut`、`RunAuthority`、`TopologyFreeze`：只能由特定操作产生）。时序约束从注释变成参数。
- 退出原因从"覆盖"改为"**事实的并集**"，在终点一次性裁决。这顺带修掉当前 `prefer_exit` 的顺序依赖问题。

## 1. 出发点：当前实现哪里乱

以下都指 `crates/vmm/src/main_loop.rs` 与 `main_loop/controller.rs`，除非另注。

1. **状态散在三处。** `serve()` 里的 `controller`、`server: Option<VmmServer>`、`stopping: Option<Exit>` 共同构成状态。"stopping 有值 ⇒ server 为 None"只靠每个赋值点自觉维护。
2. **状态与完成没有对齐。** `Input` 为每种操作的完成各设一个变体，分派时必须列出 5 组 `unreachable!("completion from a different operation")`。`Busy` 的每个变体各存一份 `events` 和 `reply`，完成后补收事件还得再 match 一遍把 `events` 找出来。
3. **特判堆积。** 内存服务失效在循环入口和收到输入后各轮询一次；完成后用 `try_recv` 补收 final-poll 事件；Start 的 resume 失败在 stopping 时要特别并入退出原因；Resize 在关机时走专用的 `shutdown_resize`；`Terminate` 与 `StartTeardown` 各有一个直接 `return` 的分支。退出一共有四条路径。
4. **退出原因依赖到达顺序。** `prefer_exit` 用后来的原因按优先级覆盖先前的。先 Shutdown、再 KVM 级 reset、再因 CPU 已退出而失败，结果是 Error；reset 晚一点被观察到，结果就是 Ok。
5. **Resize 是全局 Busy。** 等待 virtio-mem 发布期间，所有写请求都得到 `InProgress`。这与 design.new-mem §10.3"卡住的变更局限在受影响的 Window，而不是把服务停在一个全局 Busy 里"不一致。
6. **时序约束写在注释里。** `MemoryHandle::stop` 的文档是 "Only after CPUs stop"；`capture()` 在内部逐项检查 quiesce 应答，而 `PausedMachine` 本身并不区分"已暂停"与"可捕获"。
7. **一个错误类型混了三种后果。** `StartError` 同时装着同步失败（Plan、Build）、可重试的失败（Resume，已清理）和终止性的失败（Teardown、BuildCleanup，资源可能仍被持有）。
8. **没有给扩展留位置。** vCPU 数是固定的 `u8`，快照里的 vCPU 是按位置排列的 `Vec`；没有"运行权"的概念，所以 `HandoverState` 只能回复 unsupported；没有拓扑的概念，所以热插拔无处安放。

这些问题有共同的根源：**数据结构没有先定下来**。流程里的每个特判，都是在弥补某个类型没表达出来的事实。

## 2. 建模原则

1. **先定数据，再定流程。** 每个类型回答三件事：拥有什么、由谁产生、能变成什么。流程只是类型之间的转换函数。
2. **值、所有者、证明三分。** 值可以随意复制、比较、进快照；所有者不能复制，drop 只请求停止；证明是零大小或借用的标记，只能由对应操作产生，用来把"只有在 X 之后才能 Y"写进函数签名。
3. **按维度拆分，用嵌套表达跨维度约束。** 维度之间大多数组合都合法，规则通常只与其中一个维度有关；少数跨维度约束（Precopy 只在 Running，StopCopy/Handover/Dump 只在 Paused，准入关闭 ⇔ Stopping）通过把一个维度嵌进另一个维度的变体来表达。
4. **单一权威。** 资源事实归机器（`Topology`：哪些 CPU、DIMM、设备存在，各自的地址）；协议状态归设备（热插控制器的待处理位、virtio-mem 的 requested_size、内存服务的 Population）。控制器只读设备状态的投影，从不写。这与 design.new-mem §9.2 是同一条原则。
5. **按"在等什么"分类操作。** 不是按快慢，而是按"超时能否有安全含义"。见 §4.3。
6. **能力即权限。** 能不能让 guest 执行指令，由是否持有 `RunAuthority` 表达；能不能停内存服务，由是否持有 `CpusStopped` 表达；能不能导出，由是否持有 `Cut` / `TopologyFreeze` 表达。不查表、不靠 bool。
7. **事实只增不改。** 机器报告的事实不可拒绝，只能折叠；退出原因是事实的并集，裁决与顺序无关。
8. **不为函数式而函数式。** 决策保持纯（消耗旧状态、返回 `Next`），执行留在外壳。不做全量 reducer + Effect 执行器（design.md E2 已否决）。

## 3. 数据结构全景

### 3.1 值：机器是什么

```text
MachineSpec ──plan(host)──▶ MachinePlan ──initial()──▶ Topology ──物化──▶ Parts（所有者）
意图，Absent 时可改          guest ABI，此后不变          资源事实，运行期可变
                                  │                          │
                                  └────────┬─────────────────┘
                                           ▼
           MachineSnapshot = Plan + Topology + 同一切面上各组件的状态（不含 RAM 内容、不含宿主端点）
```

- **Plan 决定上限，Topology 记录现状。** 可能的 CPU（MADT）、DIMM 槽数与热插 GPA 范围（DSDT/SRAT）、冷插设备与 grant 都在 Start 时写进 Plan，此后不变。热插拔只改 Topology。因此 ACPI 静态表只需在构建时生成一次，运行期变化只体现在热插控制器的寄存器里。
- **分配结果是数据。** 一条 DIMM 的 `placement` 记在 Topology 里；空闲空间由已记录的 placement 推导，不另存分配器状态。恢复、迁移目标端、进程内重启都重放这些记录，不重新分配。
- **复位保留拓扑。** 热插进来的 CPU 与 DIMM 在 guest 复位后仍然存在，所以 Topology 属于"这台机器"，而不属于"这次启动"。
- **等 guest 不是拓扑状态。** "已请求弹出、guest 尚未确认"是热插控制器的协议状态，随平台快照保存。Topology 只区分 Present 与 Dormant / Reclaiming 这类宿主资源事实。

### 3.2 所有者与证明：机器现在怎样

```text
           pause                       resume(&RunAuthority)
  Machine ───────▶ PausedMachine ─────────────────────────▶ Machine
     │  失败            │  失败                                ▲
     ▼                  ▼                                     │ plug_cpu / release_cpu（有界）
  FailedMachine ◀───────┘                              Machine┘
     │
     ▼ teardown（三种机器同一条）
  Ok(MemoryStore)  或  Err(PendingCleanup)
```

这部分基本沿用现有的 typestate，只加三样东西：

- `CpusParked` / `CpusStopped`：把 pause 的顺序（CPU → 设备 → 内存服务）与 teardown 的前提（vCPU 线程结束后才能停内存服务）写进签名。
- `Cut<'m>`：`PausedMachine` 的全部应答都是 Replied，且没有在途或卡住的作用域变更。快照、stop-copy、热升级都只接受 `Cut`。"已暂停"与"可捕获"从此是两个类型。
- `release_local(parts, &Relinquishment)`：交出运行权之后的本地释放，不对共享 backing 做破坏性操作。

### 3.3 整机状态的组成

```text
Vmm ─┬─ Serving  { admission: Open(server) | Detached, guest: Guest }
     └─ Stopping { leaving: Leaving, exit: ExitFacts }

Guest ─┬─ Absent(MachineSpec)
       ├─ Importing(Restore | Receive | Adopt)        没有运行权
       ├─ Standby { machine, topology, views, facts } 有机器、没有运行权
       ├─ Owned   { authority, exec, topology, changes, views, facts }
       └─ Held    { machine, fault, suspended authority, … }   可选的故障策略

Exec ─┬─ Running { machine, export: Option<LiveExport> }   Precopy
      ├─ Paused  { machine, export: Option<CutExport> }    Dump / StopCopy / Handover
      └─ Moving(Move { kind, op: Bounded<Landed> })        至多一个

Changes { active: Scope → Change, stalled: Scope → StalledChange, frozen_by }

Leaving ─ Settling(Guest) → TearingDown{teardown ∥ changes} | Releasing{release_local} → Done{retained}
```

几个关系值得单独说：

- **`facts` 挂在整机上，不挂在操作上。** 现在每个 Busy 变体各存一份 `events`；模型里它在 `Owned` / `Standby` / `Held` / `Leaving` 上，操作只拥有机器本身。
- **`Exec` 与 `Changes` 并列。** 有界迁移拿走机器，作用域变更不拿；所以一个 virtio-mem 发布卡住时，机器仍然可以暂停或 teardown。
- **`Owned<X>` 的 Exec 槽是类型参数。** 外壳构造迁移期间，`Owned<Vacated>` 表示"机器已交给操作、Exec 尚未填回"，而不是一个暂时不成立的不变量。
- **`Stopping` 没有 server 字段。** 准入关闭不再是约定。

## 4. 各维度

### 4.1 准入：Serving 与 Stopping

`Admission::Detached` 取代了 `Starting.detached` 标记：Start 失败时，控制器直接看准入判断"还有没有人能重试"。没有运行权（Absent、Standby）又没有客户端时，没人能再推进这台机器，于是退出；有运行权时 guest 继续运行。

进入 Stopping 时外壳只做一件事：丢弃 `VmmServer`。之后由 `Leaving` 的阶段驱动：

1. **Settling**：中止导出会话，等 `Exec::Moving` 落地（有界）。
2. **TearingDown**：teardown 与仍在途的作用域变更**并行**推进。卡住的发布往往要靠 teardown 释放内存才能结算，而它的回复不能丢——这是现有 `shutdown_resize` 的推广。
3. **Releasing**：交出运行权的进程只做本地释放。
4. **Done**：尚未证明可释放的 `PendingCleanup` 一直保留到线程返回。

当前的 `teardown()`、`poll_teardown()`、`shutdown_resize()`、`Next::StartTeardown` 四条路径合并到这一处。

### 4.2 Guest：本进程与这台 guest 的关系

| 变体 | 有机器 | 有运行权 | 典型来源 |
|---|---|---|---|
| Absent | 否 | 否 | 进程启动；Start 失败且已清理 |
| Importing | 正在建 | 否 | RestoreSnapshot、ReceiveMigration、AdoptState |
| Standby | 是（parked） | 否 | 迁移 / 升级目标端就绪，等授予 |
| Owned | 是 | 是 | Start、本地恢复、接受授予 |
| Held | 是 | 扣押 | 故障策略为 Hold 时的致命故障 |

本地快照恢复直接得到运行权（`Origin::Restored`），因为本地快照可以被恢复多次，得到的是彼此独立的克隆而不是脑裂；但它们身份相同，需要更新 VMGenID（依赖 A5）。迁移和升级的目标端必须等授予，因为源端可能还在运行同一台 guest。

### 4.3 操作的五类

| 类别 | 在等什么 | 到期 / 中止的含义 | 位置 | Shutdown 时 |
|---|---|---|---|---|
| Bounded | vCPU、设备、服务的应答 | 失败封闭：Failed / Pending | `Exec::Moving`，独占 | 等它落地 |
| Stallable | 宿主内部依赖：租约、mlock、发布 | 没有安全含义，只能重试 | `Changes`，按作用域 | 与 teardown 并行 |
| 等 guest | `_EJ0`、online | 没有安全含义 | 设备协议状态（数据） | 直接 teardown |
| Abortable | 对端、磁盘、网络 | 只丢弃会话 | `Exec` 中的 export | 先中止 |
| 交出运行权 | —— | 不可逆 | 消耗 `RunAuthority` | —— |

由此推出几条具体规则：

- **CPU 集合的变化是有界的 Exec 迁移。** 它和 pause / resume 操作同一组 vCPU，必须互斥；线程应答有期限，超时则整机只能 teardown。KVM 只有 `KVM_CREATE_VCPU` 没有销毁接口，所以拔出后的 CPU 是 `Dormant`（线程结束、fd 保留），再插入时复用。
- **DIMM 物化与回收、virtio-mem 发布是作用域变更。** 同一作用域至多一个，不同作用域互不阻塞。回收不能因超时而释放（design.new-mem §11 的撤销证据），卡住时变成 `StalledChange`，凭 `OperationId` 重试。
- **请求弹出只是一个信号。** 写热插控制器的协议位、发中断，同步完成，不占任何槽。guest 可能永远不弹出；这段等待不阻塞 Pause、快照或 Shutdown。
- **所有 CPU 迁移共用一个完成类型 `Landed`。** 操作在尾部按结果回复请求方，然后交出落点；控制器只需把它装回 `Exec`。`(状态, 完成)` 的交叉匹配与 `unreachable!` 一起消失。
- **`Landed` 的变体就是后果的分类。** `Vacant`（已清理，可重试）、`Failed`（只能 teardown）、`Stuck`（清理未完成，保留到终止）。`StartError` 里混在一起的三种后果在这里各归各位；Plan / Build 的同步失败根本不进入迁移。

### 4.4 运行权与会话

快照、迁移、热升级共享同一个骨架：**切面 → 只读导出 →（迁移和升级）交出运行权**。

- 切面是 `Cut`；导出会话冻结拓扑（`TopologyFreeze`，只在没有在途或卡住的作用域变更时发放）。冻结期间 guest 仍可能自行弹出（Linux 经 sysfs eject 触发 `_EJ0`）：这只记录在设备协议状态里，回收推迟到会话结束，随状态交给目标端。
- `LiveExport::Precopy` 只能在 `Running` 里；`CutExport::{Dump, StopCopy, Handover}` 只能在 `Paused` 里。这是类型保证的。
- `grant(RunAuthority) -> (Grant, Relinquishment)`：消耗运行权。进程内，交出之后再 Resume 在类型上不可表示；跨进程，类型无能为力，只能靠协议：源端先停 vCPU 再授予，授予一旦发出，即使确认丢失也不再运行（`GrantDelivery::Unknown`），由编排者核对对端。
- **热升级不能交接 KVM fd。** `kvm_main.c` 在 `kvm->mm != current->mm` 时让 VM / vCPU / 设备 ioctl 一律返回 `-EIO`。所以升级 = 捕获状态 + 交接 memfd 与设备后端 fd + 新进程重建 VM。前提是内存后端为 Shared memfd；匿名私有内存只能复制，也就退化成本机迁移。
- `Handover` 的 bundle 交出之后，旧进程不得再对共享 backing 做破坏性操作（discard、截断）；中止时收回 bundle、留在 Paused，原先的 Resume 仍然合法，因为新进程处在 Standby，没有运行权。

### 4.5 事实与退出

**事实来源**挂在整机上：vCPU 线程与平台设备、设备监督者、每个内存服务的 `finished` 观察者（`RamWatch`）、热插控制器。内存服务失效由与 `MemoryHandle` 同源的观察者报告——身份来自能力，不来自任务名——取代现在的两处轮询；Stop 路径凭 `CpusStopped` 解除观察者，此后服务结束是预期的。

**顺序规则只有一条**：应用任何落点、变更结算、会话进展之前，先把全部已就绪的事实折叠进状态；请求排在最后。于是"操作在最后一次 poll 中入队的事件"不再是特例。

**退出原因是事实的并集。** `ExitFacts` 逐字段记录：请求过的停止、复位来源、每个 CPU 的结束方式、失去的 RAM 依赖、不安全的 resume、迁移中的 CPU 故障、清理失败、运行权交接。`join` 是并集，交换、结合、幂等；`verdict` 在终点一次裁决：

1. 清理失败 → Error（资源可能仍被持有，压过一切）。
2. CPU 故障、失去 RAM 依赖、不安全的 resume → Error。
3. CPU 超时、panic、join 失败 → Error。
4. `TransitionExited(cpu)`：同一 CPU 有结束事实则已被解释，否则 Error。
5. 其余 → Ok，原因按 handoff > reset > 请求报告。

第 4 条要求 CPU 结束事实带序号：现在的 `MachineEvent::GuestReset` 不区分来源，模型拆成 `Fact::CpuEnded { cpu, ending }` 与 `Fact::GuestReset(I8042)`。

## 5. 转换总表

### 5.1 Guest 层

| 从 | 触发 | 到 | 回复时机 |
|---|---|---|---|
| Absent | Configure | Absent(新 spec) | 立即 |
| Absent | Start | 外壳同步 plan + build → `Owned{Moving(Starting)}`；同步失败留在 Absent | Landed |
| Absent | RestoreSnapshot | `Importing(Restore)` → `Owned{Paused}`（Restored） | Landed |
| Absent | ReceiveMigration / AdoptState | `Importing` → `Standby` | Landed |
| Standby | AcceptGrant | `Owned{Paused}`（Granted） | 立即 |
| Standby | 对端放弃 | Stopping（本地 teardown，guest 从未在此运行） | —— |
| Absent / Standby | Detached | Stopping | —— |
| Owned | 复位事实 | `Moving(Rebooting)`（ResetPolicy::Reboot）或 Stopping（Exit，现状） | —— |
| Owned | 致命事实 / `ChangeOutcome::Fatal` | `Held`（FaultPolicy::Hold）或 Stopping（Exit，现状） | —— |
| 任意 Serving | Shutdown | `Stopping{Settling}` | Accepted |

### 5.2 Exec 层（Owned 之内）

| 从 | 触发 | 到 | 回复时机 |
|---|---|---|---|
| Running | Pause | `Moving(Pausing)` → Paused | Landed |
| Paused | Resume | `Moving(Resuming)` → Running | Landed |
| Running | PlugCpu | `Moving(PluggingCpu)` → Running；超时 → Failed → Stopping | Landed |
| Running | UnplugCpu | 信号：设热插控制器协议位；Exec 不变 | Published |
| Running / Paused | 事实 `CpuEjected` | `Moving(ReleasingCpu)` → 原状态；Topology 记为 Dormant | —— |
| Running | ResizeMemory / PlugMemory | `Changes += Scope::Window / Dimm` | Published |
| Running | UnplugMemory | 信号 | Published |
| Running / Paused | 事实 `DimmEjected` | Topology 记为 Reclaiming；`Changes += ReclaimDimm` | —— |
| Paused | SaveSnapshot | `cut` → `Moving(Capturing)` → `Paused{Dump}` → Paused | Completed |
| Paused | HandoverState | `cut` → `Moving(Capturing)` → `Paused{Handover}` | 回复 bundle |
| Paused{Handover} | CommitHandover | grant → `Stopping{Releasing}`（Upgraded） | 回复 Grant |
| Paused{Handover} | AbortHandover | Paused | 立即 |
| Running | SendMigration | 冻结 → `Running{Precopy}` →（收敛）Pausing → `cut` → Capturing → `Paused{StopCopy}` →（对端就绪）grant → `Stopping{Releasing}`（Migrated） | Completed |
| Running{Precopy} / Paused{StopCopy} | CancelMigration / 对端失败 | 中止会话；StopCopy 时先 Resume → Running | Completed（Err） |
| Moving | 任何写请求 | 拒绝 `InProgress` | 立即 |
| 有同作用域变更 | 同作用域请求 | 拒绝 `ScopeBusy` | 立即 |
| 冻结中 | 宿主侧拓扑请求 | 拒绝 `TopologyFrozen` | 立即 |

查询类请求（GetInstanceInfo、GetTopology、UpdateLogger）在任何 Serving 状态都立即回答；`Moving` 时报告出发状态，与现在一致。

## 6. 场景走查

**① 请求拔出 CPU，guest 一直不弹出，然后 Shutdown。**
UnplugCpu 只设协议位并发中断，回复 Published，Exec 仍是 Running。guest 不理会：没有任何操作在等待，Pause、快照照常可用（协议位随平台快照保存）。Shutdown → Settling 无事可等 → TearingDown → Ok。现在的设计若把这段等待放进 Busy，Shutdown 会被 guest 卡住。

**② DIMM 回收卡住，然后 Shutdown。**
guest `_EJ0` → Topology 记为 Reclaiming，`ReclaimDimm` 进 Changes。某个设备租约迟迟不还，变更卡住，作用域 `Dimm(n)` 保持占用，其他作用域与 Pause 不受影响。Shutdown → TearingDown 同时推进 teardown 与这个变更；teardown 停掉设备后租约释放，回收结算。若 teardown 自身失败，`PendingCleanup` 保留到线程返回，裁决为 Error。

**③ Precopy 中途对端失败。**
`Running{Precopy}` 期间对端断开 → 会话进展事实 → 中止（只丢弃会话与冻结令牌）→ `Running{export: None}`，回复 SendMigration 错误。guest 从未停过；运行权从未离开。

**④ 热升级：成功与回滚。**
旧进程 Pause → HandoverState：`cut` → Capturing → `Paused{Handover}`，回复 bundle（快照 + memfd + 后端 fd）。新进程 AdoptState → 重建 VM（不能复用 KVM fd）→ Standby。编排者在旧进程上 CommitHandover：grant → 旧进程 Releasing（release_local，不 discard）→ 退出 Upgraded；把 Grant 交给新进程 AcceptGrant → `Owned{Paused}` → Resume。若新进程重建失败，编排者在旧进程上 AbortHandover → Paused → Resume；新进程从未拥有运行权，不可能已经运行过。

**⑤ Pausing 中 Shutdown 先到，随后 KVM 级 reset（当前实现的问题）。**
事实依次折叠：`asked = {Shutdown}`、`cpu_ended[0] = Reset`、`transitions = [Exited(0) during Pausing]`。裁决第 4 条：CPU 0 的退出由它自己的 Reset 解释 → Ok。无论三者以什么顺序到达，结果都一样。

**⑥ 暂停时有一个 virtio-mem 发布卡住，用户要求快照。**
Pause 照常落地：mem worker 有未完成的 Population 变更，对 quiesce 的应答超时，记进 `QuiesceAcks`。SaveSnapshot 调用 `cut` → `NotCuttable`（应答不是 Replied，且 `Changes` 非空）→ 立即拒绝，说明原因。用户可以 Resume 后等变更结算，或凭 OperationId 重试。

## 7. 重构路线

每一步都保持系统可运行、可单独验证；前三步不改变对外行为（第 1 步修复一个缺陷）。

| 步 | 内容 | 验证 |
|---|---|---|
| R1 | `ExitFacts` + `verdict` 取代 `Exit` + `prefer_exit`；vCPU 事件带序号 | 纯函数性质测试：任意排列同一组事实，裁决相同；补"Shutdown → reset → Exited"用例 |
| R2 | `Vmm::{Serving, Stopping}` + `Admission`；`Leaving` 吸收四条退出路径 | 现有 main_loop 测试全部通过 |
| R3 | events 移到 `Owned.facts`；`Move` + `Landed` 取代 `Busy::{Starting, Pausing, Resuming}` 与对应的 Input 变体；"先折叠事实"取代 final-poll 补收与 Start 特判 | 删除 `unreachable!` 组合；`events_queued_in_a_failed_starts_final_poll_are_not_lost` 仍通过 |
| R4 | `RamWatch` → `Fact::RamDependencyLost`，删除两处轮询；`MemoryHandle::stop(&CpusStopped)` | 内存服务失效相关测试；编译期保证 stop 的前提 |
| R5 | `Busy::Resizing` → `Changes{Scope::Window}`；`shutdown_resize` 并入 `Leaving::TearingDown` | 现有 resize / 关机竞态测试；新增"发布卡住时 Pause 仍能落地" |
| R6 | `QuiesceAcks` + `Cut`；SaveSnapshot / RestoreSnapshot 请求接到同进程 rebuild | snapshot.rs 测试改为经过 `Cut` |
| R7 | `RunAuthority`：Start / Restore 铸造，`resume` 需要它 | 纯类型变化，无行为变化 |
| R8 | A5 ACPI → `CpuPlan` / `DimmPlan` / `Topology` / 热插控制器 → CPU 热插拔 → DIMM | 依赖 A5，单独立项 |
| R9 | 会话：Dump（G6 序列化 + ObjectContents）→ Handover / Adopt → Precopy / StopCopy / Receive（DirtyEpoch） | 依次立项 |
| R10 | 策略：ResetPolicy::Reboot（E8）、FaultPolicy::Hold | 各自立项 |

R1–R5 是"整理现有代码"，R6 以后是"为新能力留位置"。R1 可以立刻做，它独立且修复一个真实的顺序依赖问题。

## 8. 刻意没有做的事

- **扁平大枚举**（Running、RunningPrecopy、PausedDumping、PausedHandover……）：维度乘积爆炸，而大部分规则只关心一个维度。只有确实跨维度的约束才用嵌套表达。
- **全量 reducer + Effect 执行器**：design.md E2 已否决。决策保持纯，构造 future、同步 build、丢弃 server 留在外壳。
- **把等 guest 放进 Busy**：Shutdown 会被 guest 卡住。
- **给弹出或回收加超时然后释放**：违背撤销证据（design.new-mem §11）。超时只能用于"失败封闭"的场合。
- **热升级交接 KVM fd**：内核拒绝跨进程使用。
- **用 bool 或全局表表示运行权**：违背 capability 原则；进程内应当由类型保证。
- **把 Topology 和设备协议状态合成一份**：会出现两个权威；协议状态必须随设备快照，否则恢复后 guest 读到的寄存器与宿主记录不一致。
- **为每种热插设备设计通用框架**：模型只给出 CPU、DIMM、virtio-mem、设备四种具体形状；`Scope` 与 `ScopedEffect` 在出现第二个真实需求时再扩展。

## 9. 未决问题

1. guest 一直不弹出时：ACPI 没有标准的"撤回弹出请求"，是否以 `_OST` 失败为撤回依据？如何在 GetTopology 中报告？
2. Paused 时是否允许作用域变更。现在 ResizeMemory 在 Paused 时拒绝（`NotRunning`）；guest 触发的 DIMM 回收在 Paused 时开始似乎无害，因为它不产生 guest 可见 I/O。
3. Precopy 期间用户发 Pause：拒绝（需先 Cancel），还是视为提前进入 stop-copy？
4. 复位后协议状态的规范化：待处理的弹出请求是否保留？需要对照 QEMU 与 Linux 的实际行为再定。
5. Held 的诊断能力边界：`HeldMachine::Paused` 可以诊断性捕获，`Failed` 不行；是否需要"只导出、不可恢复"的快照标记？
6. Standby 时编排者失联：是否在 Detached 之外再设超时退出？
7. `ExitReason::HandedOffUnconfirmed` 的退出码约定，以及编排者如何核对对端。
8. `Scope` 的粒度：同一 DMA 域内的两个 Window 是否必须视为同一作用域（design.new-mem §10.3 末段：无法分解出独立范围时必须扩大隔离范围）。
