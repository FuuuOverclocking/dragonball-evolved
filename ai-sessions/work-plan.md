# 工作计划

> 状态：执行中。2026-10-31 已确认的决定见 §5，进度见 §6。基于分支 `memory` @ `d970b2e`。
> 本文是后续工作的入口。`ai-sessions/` 下的其余文档都是历史输入，只读参考。
> 阶段编号用 `P0–P7`，避免与 design.md 的 `M1–M6 / L / S / R1–R5`、design.mem.md 的 `M0–M5` 混淆（对应关系见 §2.4）。

## 0. 结论

- **定位**：`crates/vmm` 是 POC，是探索 VMM 更好写法的实验场。它的产出是三样东西：
  1. 已验证的事实（附录 A）；
  2. 可复用的测试设施；
  3. 纯 arch 模块。

  代码结构本身不是资产。
- **方式**：
  - 旧代码移到 `crates/vmm.old/`，作只读参考，不参与构建；
  - 在新的 `crates/vmm` 中自底向上重写，涉及的其他 crate 原地修改；
  - 从 vmm.old 移植的代码，按新的类型与错误约定改写，不整文件复制后修补。
- **代价**：P0–P3 期间无法通过可执行文件启动 guest，需要 Start 的顶层测试暂时 ignore，P4 恢复；P5 追平 vmm.old 的全部能力。
- **内存**：采纳 design.new-mem.md 的事实分层与所有权划分作为方向，但“有消费者才落地”。P2 只做：
  - 存储与放置分离；
  - 宿主政策与 guest 布局分离；
  - 唯一 CPU 注册权；
  - 固定视图。

  租约、外部 DMA、StalledChange、dirty tracking 推迟到有消费者时再做。
- **顺序**：P0 起步 → P1 基础类型 → P2 内存 S1 → P3 机器核心 → P4 生命周期与接线（恢复可启动）→ P5 virtio-blk 与快照（追平）→ P6 virtio-mem → P7 快照持久化 → backlog。

## 1. 文档盘点

| 文件 | 性质 | 状态 | 怎么用 |
| --- | --- | --- | --- |
| `design.md` | 旧总设计（首阶段 + 生命周期 / 快照 / 资源 / 内存 M0–M2） | 与 `d970b2e` 基本一致 | 可参考，不必遵从。§5 vCPU 语义、§7 CPUID/MSR/寄存器表、§8.8 capture/rebuild 顺序、§13 已知限制、附录 A 内核选项仍有用 |
| `design.mem.md` | 旧内存设计 M0–M5 | M0–M2 已实现；M3 起未开始 | M0–M2 的所有权语义，以及 M3 的 virtio-mem 纯核、副作用顺序和 API 字段可沿用；区域模型与视图传播被新设计取代 |
| `design.new-mem.md` | 新内存设计第 5 轮整理稿 | 未定案；只依据参考项目，未读当前代码 | 方向与反例来源。§15.3 的分期是 P2 / P6 / P7 的依据 |
| `design.new-mem.draft.md` | 新设计的逐轮原始稿 | 已被整理稿取代 | 只在追查否决历史时查阅 |
| `memory.rs` | 新设计的伪定义附件，不编译 | 同上 | 线程归属、消耗关系与失败归属的对照表（§10） |
| `thinking.md` | review 后的两条意见（Range、页大小） | 已纳入 P1 | — |
| `memory-plan.html` | design.mem.md 的交互版（M0 前） | 过时 | 不再维护 |
| `new-memory-review.html` | design.new-mem.md 的评审页 | 与整理稿同步 | 评审用 |
| `vmm-structure.html` | `e849215` 时 crates/vmm 的结构图 | 描述的是 vmm.old | 理解旧代码 |

整理方式：上述文件冻结，不再修改。每个阶段开始前写一页阶段设计，内容包括决定、被拒绝的替代方案和验收标准。阶段结束时回到本文更新 §6。

## 2. 代码进展（vmm.old）

### 2.1 已实现

依据 design.md §14 与 design.mem.md §10。

- **启动**：PVH 启动（含 initrd modlist）、MP table、CPUID 归一化、1–254 个 vCPU。
- **设备**：16550A 串口与 stdio 控制台、i8042 复位、virtio-mmio v2 transport、virtio-blk（io 线程，同步 pread/pwrite）。
- **生命周期**：`Controller::{Idle, Busy}`，支持 Start / Pause / Resume / Shutdown；Busy 时可通过 cancel 取消操作；局部故障就地隔离。
- **快照**：同进程 capture / teardown / rebuild。快照为纯数据，不持有 RAM；块设备通过协作 quiesce 导出 `QueueState`；恢复时补发未确认的中断。
- **资源账本**：值类型的 `AddressPool` / `IdPool`，plan 记录 grant，恢复时 `replay` 重放。
- **内存**：
  - 匿名与 memfd 两种后端；
  - `GuestRam` 不可克隆；`KvmVm` 拥有 RAM，在全部 vCPU fd 关闭后交还；
  - 设备内存窗口；
  - 不可克隆的 `MemSlots` / `SlotPermit` / `Binding`：唯一凭据，失败交回，Drop 删除失败时泄漏映射。
- **API**：REST API 与配置文件自动启动，OpenAPI / JSON Schema 导出。
- **验证**：曾在真实 Linux 6.6.102 上启动到 systemd 登录，使用双 vCPU 与 ext4 根盘。

规模：`src` 约 10.0k 行，`tests` 约 2.0k 行。

### 2.2 测试状态（2026-10-31，移动前）

- 不依赖 KVM 的测试全部通过：api、api_macros、api-schema、hyper-uds、logger、logger-backend；vmm lib 87 项；`tests/block.rs` 17 项；`tests/serial.rs` 5 项。
- 依赖 `/dev/kvm` 的测试在本机**全部失败**，原因是环境：当前用户 `fuu` 不在 `kvm` 组，打开 `/dev/kvm` 返回 EACCES。这不是代码回归，但本次**无法复核** KVM 路径与真实 Linux 冒烟，见 P0。

### 2.3 问题清单

这是重写的输入。来源包括 thinking.md、design.md §13，以及本次阅读代码。路径相对于 `crates/vmm.old/src`。

| # | 区域 | 问题 | 阶段 |
| --- | --- | --- | --- |
| 1 | 区间 | `GpaRange` 包装 `std::range::Range<u64>`，不能表达止于类型上限的区间（例如 PIO 端口 `0xffff`、u64 顶端）。PIO 与 MMIO 总线都用无类型的 `Range<u64>`（`machine/bus.rs:37`、`arch/x86_64/layout.rs:30-37`） | P1 |
| 2 | 页大小 | 一个 `layout::PAGE_SIZE = 4096` 同时充当 guest e820 对齐、宿主 mmap 对齐与 KVM memslot 对齐（`machine/memory.rs:45/101`、`machine/vm.rs:82`、`machine/build.rs:199`）；后端页大小（hugetlb）无处表达 | P1 |
| 3 | 内存 | 一段 RAM 一个映射；`MemoryBacking` 是二值枚举，并且在 `spec.memory_backing` 与 `plan.memory.backing` 中重复记录；`GuestRam::map(plan, runtime_ram)` 的第二个参数只为测试存在 | P2 |
| 4 | 设备控制 | 两套 quiesce 机制：外部 gate 冻结 future，以及块设备自己的 channel。`DeviceRole::HostOnly` 实际包含 guest 可见的块设备。gate 无法导出状态 | P3 |
| 5 | Machine | 字段是 `pub` 的，还有 `Option<Vcpus>`。“已暂停”没有类型表示，capture 靠运行时检查。plan 内嵌整个 spec。cmdline 与 grant 的一致性无人校验。`Restore.devices` 按位置对应 | P3 / P5 |
| 6 | 生命周期 | 三个几乎相同的 `make_*_operation`（`lifecycle.rs:142-291`）。事件接收端在 `Machine` 内、被操作独占，所以 Busy 期间看不到 GuestReset 和设备退出。Start 被取消时借 `Vm::Running` 选择拆除路径 | P4 |
| 7 | transport | `DeviceDesc.config` 不可变，ConfigGeneration 恒为 0；SHM 寄存器对不存在的区域返回 0，规范要求 ~0 | P6 / P5 |
| 8 | 测试 | 依赖固定 sleep（`tests/snapshot.rs:164,175`）；“没有 `/dev/kvm`”与“打不开 `/dev/kvm`”的处理不一致 | P5 / P0 |
| 9 | vCPU | MSR 列表硬编码 17 项 | backlog |
| 10 | 错误 | lib 内部全部是 `anyhow`，调用方无法区分错误 | 各阶段（§3.5） |

### 2.4 旧编号与本计划的对应

| 旧编号 | 状态 | 本计划 |
| --- | --- | --- |
| design.md M1–M6、L、S、R1–R5 | 完成 | 在 P1–P5 中重写 |
| design.mem.md M0–M2 | 完成 | 语义带入 P2 |
| design.mem.md M3 virtio-mem | 未开始 | P6（Population 归属改按新设计） |
| design.mem.md M4 virtio-pmem | 未开始 | backlog |
| design.mem.md M5 持久化 | 未开始 | P7 |
| new-mem 分期 1（Object/Window、唯一注册权） | — | P2 |
| new-mem 分期 2（租约原型） | — | backlog 第一项，移到 P6 之后，理由见 P6 |
| new-mem 分期 3（单一 WindowState 的 virtio-mem） | — | P6 |
| new-mem 分期 4（外部 DMA 域） | — | backlog |
| new-mem 分期 5（内容来源、dirty 会话） | — | 停机切面部分在 P7，dirty 在 backlog |

## 3. 定位与原则

### 3.1 定位

POC 回答了“这样写能不能成立、边界在哪里”。已经成立并值得延续的形状有：

- 设备是 `LocalRuntime` 上单一所有者的 `!Send` 任务；
- 资源分配是值类型账本，恢复时重放；
- 内存寿命由所有权结构保证；
- vCPU 踢出不会丢失。

它也暴露了分层、命名和控制协议上的临时做法（§2.3）。这些做法不再修补，而是重写。

### 3.2 带走什么

1. **事实**：附录 A。每一条都有代码位置，重写时逐条核对。
2. **测试设施**：
   - `tests/common/elf.rs` 现场生成 mini PVH ELF；
   - `tests/common/guest.rs`；
   - `tests/block.rs` 由测试扮演 guest 驱动；
   - `tests/snapshot.rs` 的 round-trip 场景；
   - 顶层 `tests/boot.rs` 的 REST 端到端测试；
   - 真实 Linux 冒烟测试。
3. **纯模块**：`arch/x86_64/{cpuid, mptable, pvh, regs, msr, lapic}`。只改签名以适配新的基础类型，逻辑不动。
4. **作为起点的形状**（不是约束）：值类型账本 + 记录 grant + replay；`KvmVm` 拥有存储、vCPU 持有 `Arc`；唯一 slot 凭据；`Activation` 会话与 reset 握手；不可变 Bus；工厂闭包跨线程、设备在目标线程上构造。

### 3.3 原则

1. **新 crate，自底向上**：
   - 每个阶段只把这一层需要的东西移入新 crate；
   - 移植代码时按新的类型与错误约定改写；
   - vmm.old 冻结，不修改，也不被任何 crate 依赖。
2. **其他 crate 原地修改**，不做兼容层、re-export 或过渡别名。
3. **新抽象要有当前消费者**。新内存设计中暂时没有消费者的类型只留在设计稿里，不写空 trait 或无作用的 ack。
4. **行为要重新覆盖**：vmm.old 的测试所覆盖的行为，在对应阶段的新测试中必须重新断言。阶段设计要列出“迁移的行为”清单，而不只是测试名。修 bug 时补的测试，去掉修复后必须失败。
5. **暂时 ignore 的测试**必须写明在哪个阶段恢复（`#[ignore = "P4: ..."]`），到期时恢复。

### 3.4 验收集合

| # | 内容 | 恢复于 |
| --- | --- | --- |
| A1 | mini PVH：start_info magic、串口输出、i8042 复位退出、双 vCPU 拆除 | P3（机器级）/ P4（可执行文件） |
| A2 | pause / resume，包括阻塞式 MMIO 读期间的 pause；启动失败后可重试 | P3 / P4 |
| A3 | virtio-blk：reset 后重新激活、非法输入、大请求、任务 panic 后的 quiesce | P5 |
| A4 | 同进程快照 round-trip（两种后端）、逐盘激活、grant 迁移、补发中断、拒绝变化的后端 | P5 |
| A5 | REST 端到端：配置 → Start → Pause → Resume → Shutdown | P4 |
| A6 | 真实 Linux 冒烟（`#[ignore]`，通过 `DBE_KERNEL` 等环境变量指定），从 P5 起每个阶段至少跑一次 | P5 |
| A7 | `make lint`、`make test`、`make api-features`、`make schema-verify` | 每个阶段 |

### 3.5 错误约定

- **lib**（`crates/vmm`，以及被修改到的其他 lib crate）：
  - 每个模块定义自己的错误类型，用 `thiserror::Error` 加 `displaydoc::Display` 派生，doc 注释就是错误消息（小写开头，不加句号）；
  - 原因用 `#[source]` / `#[from]` 挂在 source 链上，消息不重复 source 的内容；
  - 非测试代码不使用 `anyhow`。
- **app**（顶层 crate `dragonball-evolved`）：
  - 一般使用 `anyhow::Error`、`anyhow!`、`.context()`；
  - 需要被区分的错误实现为 ZST，用 `error.is::<T>()` 判断。
- **边界**：API 只携带字符串。由 vmm 主循环用同一个辅助函数沿 source 链把错误渲染成 `ApiError`。`VmmExitStatus::Error` 同理。
- **热路径**（vCPU 退出分发、总线、virtqueue 处理）：构造错误不分配内存，使用不含 `String`、`Box` 的 `Copy` 枚举；调用方只需要分支时，返回 `Option` / `bool`；不调用 `format!`。
- **测试**：可以自由使用 `unwrap` 和 `anyhow`。

## 4. 阶段

### P0 起步

- `git mv crates/vmm crates/vmm.old`；workspace 加 `exclude = ["crates/vmm.old"]`。
- 新 `crates/vmm` 只包含进程外壳：
  - `start()`、`VmmClient`、`VmmServer`；
  - `ProcessCtx`、`VmmExitStatus`；
  - `KvmContext` / `HostCapabilities`，宿主页大小也在这里取得；
  - `MachineSpec`（配置累积）。

  主循环只处理配置、查询、日志、`HandoverState`（返回 unsupported）和 Shutdown；`Start` 返回明确的“尚未支持”错误。
- 错误约定（§3.5）在外壳中落地，包括渲染 `ApiError` 的辅助函数。
- 测试设施先行：把 `tests/common/elf.rs` 移入新 crate，顶层 `tests/boot.rs` 改为指向它。需要 Start 的用例标为 `#[ignore = "P4: ..."]`。
- 环境：让 `fuu` 能打开 `/dev/kvm`（需要你执行，见 §6）。

**验收**：A7；顶层 REST 测试中不依赖 Start 的部分通过。

### P1 基础类型：区间、地址空间、页大小

**自有区间类型**（thinking.md 第 1 条）：

- **表示**：`{ start, len }`。
  - 不变量：`len == 0`，或 `start + (len − 1)` 不溢出。
  - 因此可以表达止于类型上限的区间。主要访问用 `last()`；`end()` 是 checked 的。
- **按地址空间泛型化**：`Range<A>`，其中 A 为 `Gpa(u64)`、`Pio(u16)` 等。GSI、memslot 是 ID 类型，由 `IdPool` 管理，`IdPool` 改用同一套区间类型。长度类型的选取在 P1 设计中决定。
- **参考 std 补全**：
  - const 构造（可在常量中做编译期断言）；
  - `is_empty`、`contains`、`contains_range`、`overlaps`、`intersection`、`split_at`；
  - 对齐判断与向内 / 向外对齐；
  - 十六进制的 `Debug` / `Display`；
  - `Eq` / `Hash`；
  - 与 `core::ops::Range{,Inclusive}` 之间的 `TryFrom`。

  不实现逐地址迭代，因为没有用例。

**页大小与对齐**（thinking.md 第 2 条）：

- `PageSize` / `Alignment`：2 的幂的 newtype，可 const 构造。
- 三种来源分开建模：
  - **guest**：x86 架构页 4 KiB，用于 e820 / PVH memmap 与启动数据。guest 结构粒度（memory block、virtio-mem block）属于 guest ABI，记在 plan 中。
  - **host**：宿主基础页大小，打开 KVM 时取得并放入 `HostCapabilities`（P0 已取得）。
  - **backing**：后端的页大小。匿名和普通 memfd 是基础页；`MAP_HUGETLB`、`MFD_HUGETLB` 和 hugetlbfs 文件是大页，hugetlbfs 的大页大小由 `fstatfs` 取得。它决定映射、memslot 和 discard 的粒度。
- 规则写成纯函数：
  - memslot 的 GPA、HVA 与大小都按宿主页对齐；
  - 大页后端还要求区域按大页对齐，并且 GPA ≡ HVA mod 大页；
  - discard 按后端页粒度进行。
- P1 只有基础页后端，但检查点就位。hugetlb 落地时只需补数据，不改结构。

**同阶段移植**：

- `resources`：值类型的 `AddressPool` / `IdPool`，基于新的区间类型；
- `arch/x86_64/{layout, cpuid, mptable, pvh, regs, msr, lapic}`：纯模块，改签名，错误类型化；
- 对应的单元测试。

**验收**：单测覆盖端口 `0xffff`、u64 顶端、空区间、交集与包含的边界；迁移旧的池与 arch 单测；A7。

### P2 内存 S1：存储与放置分开

依据：design.new-mem.md §3–§4、§7.1，以及 §15.3 分期第 1 步；吸收 design.mem.md M0–M2 已验证的所有权语义。

- **MemoryLayout**（纯值）：
  - Object：容量，内容契约暂时只有 volatile RAM；
  - Window：对象切片 → GPA，owner 为启动内存，kind 为启动 RAM。

  启动 RAM 是 1 个 Object 加 1–2 个 Window（绕开 32 位洞）。设备内存窗口仍由机器账本 `ResourceBook` 分配，layout 不自带 GPA 分配器。
- **HostPolicy**：与 guest 布局分开，包含后端种类（匿名私有 / 共享 memfd）与页大小。快照记录 layout；HostPolicy 只用于诊断，恢复时可以显式替换。
- **HostBacking**：一个 Object 一份宿主字节，memfd 后端只有一个 fd。Window 解析为（backing, 对象内 offset）。
- **KVM 与 CPU 注册**：
  - `KvmVm` 建立 VM fd、irqchip、PIT2；
  - 沿用 M2 的 `MemSlots` / `SlotPermit` / `Binding` 语义，绑定单位改为 Window。
- **寿命**：保留“`KvmVm` 拥有存储、vCPU 持有 `Arc`、全部 fd 关闭后交还”。不引入 `TeardownEvidence`。
- **视图**：仍是固定的 `Arc<GuestMemoryMmap>`，由 Window 派生。
- **已定**：每个 Window 单独映射，不使用 vm-memory 的 raw region。memfd 每个 Object 一个 fd，各 Window 按对象内 offset 映射；匿名私有内存每个 Window 一次映射。后端页大小与 memslot 对齐规则（P1 推迟过来的部分）也在本阶段落地。

**验收**：

- 一个 memfd 经两个 Window 被访问，fd 中的 offset 等于对象内 offset；
- 内存寿命与凭据的 KVM 测试迁移；
- A7。

**不做**：MemoryRuntime 服务、RamView 版本发布、WindowAccess / 租约、外部 DMA、StalledChange、dirty tracking。

### P3 机器核心

1. **vCPU**：线程、踢出、运行循环与命令（附录 A 的语义）、启动状态配置。
2. **总线与平台设备**：
   - PIO 与 MMIO 两种类型化的总线；
   - 串口与 stdio 控制台、i8042。
3. **统一设备控制协议**：
   - 每个设备任务自己持有控制接收端，接受 `Quiesce(reply: 本设备的静止状态)`、`Resume`、`Stop`，并在自己的循环中响应。
   - 不设外部 gate 与 `DeviceRole`。
   - `DeviceManager` 只负责三件事：放置（vmm-main / io 线程）、汇报退出、管理 io 线程寿命。
   - 局部故障语义：已退出的设备记为 Exited，pause 照常完成，capture 拒绝。
4. **PVH 构建**：读文件（副作用）→ 生成启动数据（纯）→ 写入。
5. **类型态**：
   - `Machine::pause(self) -> Result<PausedMachine, (Machine, Error)>`；
   - `PausedMachine::resume(self)`；
   - 两种状态都能 teardown；
   - 把 capture 放在 `PausedMachine` 上，留到 P5。
6. **plan 只记录决定**：不内嵌 spec。`MachineEvent` 与 `DeviceExit` 的接收端不放在 `Machine` 内。

**验收**：机器级 KVM 测试 A1、A2；类型态的 compile_fail 文档测试；A7。

### P4 生命周期与接线

- Controller 状态 × 请求保持穷举 match（design.md §4.3 的表仍然适用）。
- 用一个通用的操作构造（取消、回复、归还 Vm）实现 Start / Pause / Resume。
- Busy 期间仍观察 GuestReset、VcpuFailed、DeviceExit。pause 进行中收到 GuestReset：取消操作，然后拆除。
- 为“已构建、未运行”定义独立状态。
- 接通 Start，并恢复顶层被 ignore 的测试。

**验收**：Controller 单测；A1、A2、A5（通过可执行文件）；A7。

### P5 virtio-blk 与快照：追平 vmm.old

- **transport**：virtio-mmio，SHM 寄存器返回 ~0；reset 握手；中断与 resample。
- **virtio-blk**：放在 io 线程，按 P3 的控制协议协作 quiesce。
- **快照**：`capture(&PausedMachine)` 与同进程 rebuild。
  - 设备快照与 plan 按 id 键控，对应关系进入类型；
  - cmdline 中的 virtio-mmio 片段由 grant 派生；
  - 快照测试不用固定 sleep。
- 追平后，由你确认是否删除 vmm.old。

**验收**：A1–A7 全部通过。

### P6 virtio-mem：第一个运行期内存消费者

依据：

- design.mem.md §5：纯核 `handle()`、副作用顺序、强制协商 `UNPLUGGED_INACCESSIBLE`、`SetMemoryHotplug` / `ResizeMemory`；
- design.new-mem.md §9：Population 单一权威、`WindowControl` / `PendingMemory` / `WindowStatus`、设备循环不自锁。

各方职责：

- **内存服务**：io 线程 `LocalRuntime` 上的单一所有者任务。
  - 独占该 Window 的 WindowState（Population 与 chunk 绑定）和 CPU 注册权。
  - 按纯核给出的顺序执行副作用：插入时先 Bind，再提交，再 ACK；拔出时先 Unbind，再 Discard。
  - 失败时完成补偿，或报告失败。
- **设备**：拥有 requested_size / usable_region_size、队列与特性协商。plugged_size 与 STATE 请求从 `WindowStatus` 读取。`PendingMemory` 放进设备循环，与其他事件一起轮询，不嵌套 await。
- **transport**：设备发布可变配置；实现 ConfigGeneration 与配置变更中断。
- **discard**：唯一入口，按后端选择：匿名用 `MADV_DONTNEED`，memfd 用 `MADV_REMOVE`。
- **软件 DMA 视图**：包含整个区域，不做租约。已知限制：违反协议的 guest 发起的软件 DMA 不会被 Population 拦截。这是把租约原型挪到 P6 之后的理由：virtio-mem 不需要撤销软件访问。
- **失败保留**：只为“KVM 解绑失败”这一种具体情况保留有所有者的状态：Binding 不交回，该 chunk 不可复用。
- **快照**：Population 进机器快照的内存字段，requested_size 进设备快照。

**验收**：

- 纯核单测：对齐、超出 requested、副作用顺序、设备复位不改变块状态；
- 设备测试扮演驱动；
- KVM 测试观察 chunk 绑定；
- round-trip 保留已插块；
- 真实 Linux（kangaroo-6.6，`memhp_default_state=online_movable`）：调大后 MemTotal 增加，调小后减少、宿主 RSS 回落。

### P7 快照持久化（停机切面）

依据：design.mem.md §7.2；design.new-mem.md §12.1–12.3。

- `MachineSnapshot` 加版本头与 CRC；序列化格式在 P7 设计中决定。
- 内存内容按 Object + 对象内 offset 记录，每一段都有明确来源，缺口不默认视为零：
  - 启动 RAM 全量保存，memfd 用 `SEEK_DATA` 跳过空洞；
  - virtio-mem 只保存已插入的块。
- 恢复先做 Copy。`HandoverState` 交接 memfd（E10）。

**验收**：跨进程恢复 mini guest 与 virtio-mem 状态；快照损坏或版本不符时拒绝恢复。

### Backlog（不排序）

- **租约原型**（new-mem 分期第 2 步，也是最可能被否决的部分）：用 virtio-blk 加一个可撤销的 Window，证明租约能与 virtio-queue 无额外拷贝地组合。它是 pmem 热拔、ACPI DIMM 移除、vhost-user 等“撤销软件访问”需求的前置。
- hugetlb、预分配、宿主 NUMA：扩展 HostPolicy，落地 P1 的页大小规则。
- virtio-pmem；vhost-user（固定导出寿命）；VFIO / DmaProjection；dirty tracking 与热迁移。
- ACPI（A5）、bzImage（A2）、virtio-net、io_uring、限速接入、MSR 动态发现（`KVM_GET_MSR_INDEX_LIST`）。

## 5. 已确认的决定（2026-10-31）

1. `crates/vmm` 移为 `crates/vmm.old`，在新 crate 中重写；涉及的其他 crate 原地修改。
2. 区间表示为 `{start, len}`，按地址空间泛型化。
3. P2 引入 Object / Window。
4. virtio-mem 的 Population 归内存服务。
5. 租约原型放在 virtio-mem 之后。
6. 错误约定见 §3.5。
7. 执行方式：由子代理编写代码，主会话负责审查与验证。

## 6. 进度

- [x] **环境**：`fuu` 已加入 `kvm` 组。当前会话用 `sg kvm -c '…'` 运行需要 KVM 的命令，不必重新登录。
  - 基线：旧代码 `d970b2e` 在 KVM 下全部测试通过（vmm lib 102、block 17、boot 2 + 顶层 4、serial 5、snapshot 8）。
  - 真实 Linux 冒烟（A6）缺内核：design.md 中的 `/fuu.code/kangaroo-6.6/vmlinux` 在本机不存在，P5 前需要提供 PVH 内核（以及可选的 initrd / rootfs）。
- [x] **P0 起步**：
  - `crates/vmm` 已移为 `crates/vmm.old`，workspace 中 `exclude`；
  - 新 crate 包含进程外壳（`start`、`VmmClient`、`VmmServer`、`ProcessCtx`、`VmmExitStatus`、`KvmContext`、`MachineSpec::apply`、只处理配置与查询的主循环）、`error::render_chain`、`tests/common/elf.rs`；
  - 顶层需要 Start 的测试标为 `#[ignore = "P4: …"]`；新增 `rest_api_configures_and_shuts_down_before_start`。

  偏差：`MachineSpec` 字段私有，只能经 `apply` 修改；`update_logger` 返回带 source 链的错误，由 vmm 渲染。
- [x] **P1 基础类型**：
  - `space`：`Space` trait，`Gpa` / `Pio` / `Gsi`，`Range<A> { start, len }`；长度使用空间自身的整数类型，因此不能表示整个空间；const 构造为 `Range::<Gpa>::const_new` / `const_inclusive`；显示格式为 `start..=last`。
  - `align`：`Alignment`、`GuestPageSize::X86_4K`、`HostPageSize`。
  - `resources`：`AddressPool<A>` / `IdPool<T>`。
  - arch 纯模块全部移植，单测保留旧断言；`HostCapabilities` 增加 `host_cpuid`，`max_gpa()` 取代 `physical_address_limit()`。
  - 决定：后端页大小与 memslot 对齐规则随消费者放到 P2；`ResourceBook` 随机器 plan 放到 P3。
- [x] **P2 内存 S1**：
  - `memory::layout`：`MemoryLayout` / `Object` / `Window`，以及 `ObjectOffset` 空间；构造时校验不变量，`boot(size)` 产生 1 个对象 + 1–2 个窗口。
  - `memory::policy`：`HostPolicy { kind, page_size: BackingPageSize }` 与 `check`。
  - `memory::store`：`MemoryStore::materialize`；每个 Window 单独映射，memfd 每个对象一个封印 fd。
  - `vm`：`KvmVm`、`MemSlots` / `SlotPermit` / `Binding`（按 Window 绑定）、`KvmVcpu`、`create_vm`。
  - 旧的 memory / vm 测试全部移植，KVM 下通过；新增“一个 memfd 对象、两个窗口”的测试。

  遗留：
  - 内容契约暂缺（只有一个变体，没有消费者）；
  - 16 MiB 的 PVH 下限放到 P3b 的 plan；
  - `KvmVm::new` / `create_vm` 失败时会丢掉 `MemoryStore`，P5 的 rebuild 需要把它交回。
- [ ] **P3a 设备运行时与平台设备**（进行中）。
- [ ] P3 机器核心
- [ ] P4 生命周期与接线
- [ ] P5 virtio-blk 与快照
- [ ] P6 virtio-mem
- [ ] P7 快照持久化

## 附录 A：重写时必须保留的事实

路径相对于 `crates/vmm.old/src`。

| 事实 | 位置 |
| --- | --- |
| 踢出 vCPU：先写 `immediate_exit = 1`，再发 SIGRTMIN。处理函数只做 `fence`，在第一个 vCPU 线程启动前安装一次；vCPU 线程显式解除继承的屏蔽 | `machine/vcpu.rs:171`、`:425`、`:440` |
| MMIO/PIO 退出后直接重新 `KVM_RUN`，只有 EINTR / Intr 才处理命令，因此 Pause 的 ACK 意味着 pending 的用户态 I/O 已完成 | `machine/vcpu.rs:323-400` |
| `KVM_CREATE_IRQCHIP` 先于创建 vCPU；`SET_CPUID2` 先于 `SET_MSRS`；只有 BSP 设置 PVH 寄存器，AP 设为 `MP_STATE_UNINITIALIZED` | `machine/vm.rs:53-63`、`machine/build.rs:229-258` |
| GDT 在 G=1 时，写入 `kvm_segment` 的 limit 要换算成 `0xffff_ffff`，否则 32 位入口只能访问 1 MiB 以内 | `arch/x86_64/regs.rs:68-86` |
| CPUID：AMD 清 0x7.0 EDX[29]；新内核缺 0xB 子叶 1 时补上；Intel 与 AMD 的改写表 | `arch/x86_64/cpuid.rs`、design.md §7.1 |
| capture 顺序：设备 → 平台 → vCPU（`get_mp_state` 最先，`vcpu_events` 最后）→ VM（irqchip → PIT → clock） | `machine/snapshot.rs:127-145`、`machine/vcpu.rs:124` |
| restore 顺序：irqchip / PIT → 每个 vCPU 依次 cpuid → mp_state → regs → sregs → xsave → xcrs → debugregs → lapic → msrs → events，然后 `kvmclock_ctrl` → `set_clock` → 设备在 LAPIC 恢复之后挂上 | `machine/snapshot.rs:170-217`、`machine/vcpu.rs:215-254`、`machine/build.rs:262-267` |
| irqfd 是异步注入的：capture 读到的 irqchip 可能还不含最后一个中断，因此恢复时对未确认的中断补发一次 | `machine/block.rs:185-192` |
| virtio reset：transport 等设备确认已停止访问队列后才清寄存器（vmm-main 从不同步等待 vCPU，所以不会死锁）；超时则保留旧会话 | `devices/virtio/mmio.rs:280-288`、`devices/virtio/mod.rs:82-88` |
| 快照记录 `DeviceDesc`，恢复时拒绝提供内容已变化的后端；队列在 attach 时校验 | `devices/virtio/mmio.rs:207-221`、`machine/block.rs:158-168` |
| 块请求在任何磁盘修改之前完成方向、地址与范围的校验；staging 缓冲不超过 64 KiB | `devices/virtio/block.rs`、design.md §14 |
| 控制台：经 `/proc/self/fd/0` 重新打开 stdin 再设置 `O_NONBLOCK`，避免影响 stdout/stderr；raw mode 由 guard 恢复 | `devices/legacy/console.rs:94` |
| memfd：`MFD_NOEXEC_SEAL` 遇 EINVAL 时去掉重试；封印 `SHRINK \| GROW \| SEAL`；这些封印不阻止打洞 | `machine/memory.rs:155-187`、design.mem.md §6 |
| KVM 接受相同 slot 的重复注册，因此需要唯一凭据；删除 slot 时内核会执行 SRCU 同步；Drop 删除失败时泄漏映射，而不是 unmap | `machine/vm.rs:181-257`、design.mem.md §4.1 |
| VM fd 先于 RAM 声明（先关闭）；全部 vCPU fd（含复制出的踢出 fd）关闭后才能交还 RAM | `machine/vm.rs:44-49`、`:120-126` |
| 设备内存窗口：PVH 直接进入 vmlinux 时没有内存 KASLR，因此窗口紧跟 RAM、1 GiB 对齐、不超过 1 TiB；x86 用户 memslot 上限为 32764 | design.mem.md §3 |
| virtio-mem：必须协商 `UNPLUGGED_INACCESSIBLE`；设备复位不改变块状态；可靠拔出需要 `memhp_default_state=online_movable` | design.mem.md §5 |
