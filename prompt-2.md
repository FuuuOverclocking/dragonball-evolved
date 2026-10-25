# 下一阶段：内存设施与抽象

这是 Dragonball 开源版本的 Rust 重构（/fuu.code/dragonball-evolved），当前目标为 x86-64 + KVM + Linux。参考 .references/firecracker（主要）与 .references/cloud-hypervisor, .references/qemu。

AGENTS.md 中的代码品味与架构约束继续有效。design.md 是设计背景，代码是现状；两者不一致时请指出。

## 现状

当前分支 `lifecycle-control`（HEAD `3eeff1f`），尚未合入 `main`。自上一份 prompt 以来完成了以下工作，详见 design.md §12、§14。

- **生命周期**
  - 主循环状态为 `Controller::{Idle(Vm), Busy { stable, operation }}`。
  - vCPU 运行循环区分 Handled / Interrupted，所以 Pause 的 ACK 意味着在途的 MMIO/PIO 已经完成。
  - 设备暂停有两种方式：GuestAffecting 任务由 gate 冻结，块设备协作 quiesce。
  - 客户端全部断开时，已启动的 VM 不会被终止。
- **状态可提取**
  - `MachineSnapshot` 是纯数据：KVM 的 irqchip/PIT/clock、vCPU 全套寄存器与 17 个 MSR、平台、设备、plan，以及内存的 `Arc`。
  - `capture` / `rebuild` 已在真实 KVM 上做过同进程 round-trip，两种内存后端都验证过。
- **资源**
  - `resources.rs` 提供值类型的 `AddressPool`、`IdPool<T>`、`ResourceBook`；`GpaRange`、`Gsi`、`MemSlot` 各自是独立类型。
  - plan 经账本分配并记录 grant；恢复时用 `plan::replay` 重放，不重新计算。
- **设备按 kind 纵切**
  - 设备用 `DeviceSpec` / `DevicePlan` / `AttachedDevice` / `DeviceSnapshot` 四个按 kind 的 enum 表示。
  - build 与 rebuild 共用 `assemble`。
- **内存**
  - 布局与后端记在 `MemoryPlan { backing, regions }`。
  - `KvmVm` 拥有 VM fd 与 guest memory，`KvmVcpu` 持有 `Arc<KvmVm>`。"映射要等所有 fd 关闭后才解除"由类型保证。
  - `MemoryBacking::Shared` 为每段内存建一个 sealed memfd，经 `SetMemoryBacking` API 选择。

已知限制见 design.md §13。与本阶段相关的有：

- 快照只在同进程内有效：没有序列化，RAM 以 `Arc` 共享。
- 源机器还活着时，同一个快照也能 rebuild，两台 VM 会共用一块 RAM。
- "已暂停"没有类型表示。

## 本阶段：内存设施与抽象

内存是 VMM 的重中之重。现在只有启动时一次定下的静态 RAM：每段一个 memslot、一个 `GuestMemoryMmap`、两种后端。需要演进到能承载下列需求的形态（本阶段不必全部实现）：

- 运行期增减内存：virtio-mem、balloon。ACPI DIMM 热插依赖 A5（ACPI）。
- 设备内存窗口：virtio-fs DAX、virtio-pmem（文件后端，`MAP_SHARED`）。
- hugepage（hugetlbfs / `MFD_HUGETLB`）、预分配、宿主 NUMA 绑定。guest 的 NUMA 拓扑依赖 ACPI SRAT，也就是 A5。
- vhost-user 内存表与 fd 交接、VFIO DMA 映射：这两项属于 G5，以后再做，但区域变化的通知机制要能扩展到它们。
- 快照持久化与热升级（G6、E10），以及 dirty page tracking。

**先不要实现。** 请先梳理现有内存代码：machine/memory.rs、machine/vm.rs、arch/x86_64/layout.rs、resources.rs、machine/plan.rs、machine/snapshot.rs，以及设备对 `SharedGuestMemory` 的使用。对照 firecracker 与 cloud-hypervisor 的做法，给出本阶段计划，包含以下内容：

1. **简短判断**：现有抽象哪里够用，面对上述需求时哪里会最先断裂。
2. **3–5 个关键问题**，至少覆盖：
   - **区域模型**：区域有哪些种类（启动 RAM、热插 RAM、设备内存……）？哪些属于 plan（guest ABI：快照记录，恢复照搬），哪些只是运行期状态？
   - **GPA 空间**：热插内存与设备内存放在哪个窗口？如何经值类型账本分配并记录？
   - **运行期变化**：
     - memslot 增删和 guest memory 视图的更新，怎样传给 vCPU 总线与设备任务？是用 `GuestMemoryAtomic` 这类共享可变视图，还是经设备自己的消息通道下发新值？
     - 删除区域时要"先解绑，再确认无人访问，最后 unmap"。这个顺序怎样像现在一样由所有权保证？
   - **后端**：hugepage、文件后端、预分配与 NUMA 放在哪一层，才能不让 `MemoryBacking` 变成一堆标志位？
   - **快照**：内存如何持久化或交接（例如经 memfd 传 fd）？它与区域模型谁先谁后？
3. **依赖顺序**：每一步怎样保持系统可运行、可验证。
4. **代表性路径**：spec → plan → machine 各层的 Rust 类型。
5. **源码参照**：firecracker / cloud-hypervisor 中对应的文件与类型名。

我倾向的切入顺序：

1. 先扩展区域模型与设备内存窗口。这部分是纯数据，可以单测。
2. 再接一个最小的真实消费者来检验抽象：virtio-mem 或 virtio-pmem，请给出建议。
3. 最后做快照持久化。

如果你认为顺序应当不同，请说明理由。上面列出的已知限制，若与内存所有权直接相关（例如快照与源机器共用 RAM），可以考虑在本阶段一并用类型解决。

计划经我确认后再动手实现。

## 工作方式

- **角色**：你是评估者。可以运行子 Agent 迭代代码，结果由你审查。在合适的时候更新 design.md：第 1 节的勾选、相关章节、§13 与 §14。
- **分支**：可以用 git 分支分叉或回滚。建议在 `lifecycle-control` 之上新建分支开展本阶段。
- **提交**：每个能独立验证的步骤一个提交。提交信息沿用 `area: 一句话` 加列表说明"为什么"的风格，参照 `git log`。
- **回归测试**：修 bug 时补的测试，在去掉修复后必须失败。可以临时关掉修复，验证一次。
- **检查命令**：
  - `make lint`：nightly fmt 加 `clippy -D warnings`；
  - `make test`、`make api-features`、`make schema-verify`；
  - API 有变化时再跑 `make schema-export`。
- **环境**：
  - `/dev/kvm` 可用（AMD，内核 6.6.102，`vm.memfd_noexec=0`），KVM 集成测试会真实运行。
  - 真实 Linux 冒烟测试的用法见 design.md §14，内核为 `DBE_KERNEL=/fuu.code/kangaroo-6.6/vmlinux`。
  - shell 里的 `ls` 是 eza 的别名；脚本中请用 cargo 打印出的精确路径。
  - vmm crate 的 `rust-version` 是 1.96。
- **语言**：与我交流用中文。

## 已明确的偏好（AGENTS.md 之外）

- 导出设备状态时不要包进 `Arc<Mutex<…>>`：在设备自己的循环里用 `&mut` 取出，经 oneshot 回复。
- 区间用 `std::range::Range` 等新的 range 类型；常量范围写成 const。
- 资源分配用不可变的值类型，结果记录在 plan 中，恢复时重放。
- 嵌套过深的分支，抽成对称的处理函数。
- 意外情况（例如客户端全部断开）不应终止 guest，其他组件照常运行。

## 暂缓，待功能演进后再评估（来自 thoughts.md）

- `DeviceExit` 是否从 `MachineEvent` 中拆出，例如改为 `Machine::next_event`。
- `lifecycle.rs` 按 idle / busy 拆成子模块，并合并各个 `make_*_operation`。
- CPU 拓扑与 CPU template：放在内存之后。
