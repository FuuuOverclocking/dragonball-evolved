# 内存设施与抽象：本阶段设计

> 状态：计划已于 2026-10-26 确认，采纳全部推荐值；实施从 M0 开始，进度见 §10。
> 本文是 design.md 在内存方面的展开。代码是现状；与 design.md 不一致的地方记在 §11，随各步骤同步回 design.md。
> 参考：`.references/` 下的 firecracker（主要）、cloud-hypervisor、QEMU；`/fuu.code/kangaroo-6.6`（宿主 KVM 与 guest 内核同源）；virtio 1.4 规范。
> 步骤编号 M0–M5 只在本文内有效，与 design.md §12 早期的 M1–M6 无关。

## 0. 已确认的决定

| # | 问题 | 决定 |
| --- | --- | --- |
| 1 | 第一个真实消费者 | virtio-mem。virtio-pmem 作为可选的第二个消费者（M4） |
| 2 | virtio-mem 的 KVM 绑定粒度 | 按 chunk 动态绑定（firecracker 的做法），要求 guest 协商 `VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE` |
| 3 | 设备内存窗口 | 紧跟 RAM、1 GiB 对齐、不超过 1 TiB；窗口本身记入 plan |
| 4 | 所有权修正（M0）的位置 | 最先做 |
| 5 | hugetlb / 预分配 / NUMA | 本阶段只做设计（§6），实现放到 M5 之后 |
| 6 | API | 启动前 `SetMemoryHotplug(MemoryHotplugConfig { size_mib, block_size_mib, chunk_size_mib })`，运行期 `ResizeMemory(HotplugSizeMib)`；字段名按此确定 |

本阶段要能承载、但不必全部实现的需求：运行期增减内存（virtio-mem、balloon；ACPI DIMM 依赖 A5）；设备内存窗口（virtio-pmem、virtio-fs DAX）；hugepage、预分配、宿主 NUMA 绑定；vhost-user 内存表与 VFIO DMA 映射（G5）所需的区域变化通知；快照持久化、热升级（G6、E10）与 dirty page tracking。

## 1. 现状与断裂点

**可以沿用**：

- 值类型账本：再加一个设备内存池即可，grant 记在设备 plan 里，恢复时 `replay` 重放。
- `KvmVm` 拥有内存、`KvmVcpu` 持有 `Arc<KvmVm>`：映射比所有能访问它的 fd 活得久，这个结构可以推广到运行期绑定。
- 设备经 `Activation` 按值拿到内存视图，并有自己的控制 channel：已经具备"按值下发新视图 + oneshot 确认"的形状。
- vCPU 线程不直接读写 guest 内存，只经 KVM 访问，所以内存变化不涉及 vCPU 与总线。

**最先断裂**（按遇到的先后）：

1. `MemoryPlan.regions` 一身二用：既是 e820 RAM，又是全部映射。`pvh::boot_data(&plan.memory.regions)` 会把任何新区段写成 RAM，而规范要求 virtio-mem 区不出现在 e820 中（`mem/description.tex:29-30`）。
2. 4 GiB 以上没有设备内存窗口和池；phys-bits 校验只看 RAM 末端，memslot 校验只数 RAM 段数（`spec.rs:127-144`）。
3. slot 不能在运行期增删：slot 池是 `KvmVm::new` 的局部变量（`vm.rs:60-63`）；KVM 不允许修改已有 slot 的大小或 HVA（`kvm_main.c:2101-2104`）。运行期产生的 slot 没有所有者，`set_user_memory_region` 的 SAFETY 论证也覆盖不到它们。
4. 快照持有 `Arc<GuestMemoryMmap>`：源机器还活着时也能 rebuild，两台 VM 共用一块 RAM；`describe()` 靠 `FileOffset` 有无反推后端（`memory.rs:60-77`）；测试把快照当作访问 guest RAM 的后门（`tests/snapshot.rs:344`、`:458`）。
5. transport：`DeviceDesc.config` 不可变；ConfigGeneration（0x0fc）恒为 0；SHM 寄存器（0xac–0xbc）对不存在的区域返回 0，规范要求 −1 / ~0（`transport-mmio.tex:223-236`）。
6. `MemoryBacking` 是二值枚举，直接加 hugepage、文件、预分配、NUMA 会变成标志位组合。

## 2. 区域模型

### 2.1 两类所有者

- **机器拥有的 RAM**（`GuestRam`）：启动 RAM、virtio-mem 区，以及 A5 之后的 ACPI DIMM。内容就是 guest 状态：快照要转储，热升级要交接。
- **设备拥有的窗口**：virtio-pmem 的文件映射、virtio-fs DAX 窗口。内容在外部文件里，设备在 attach 时自己重建映射，不进内存镜像。

### 2.2 记在哪里

| 归属 | 判据 | 例子 |
| --- | --- | --- |
| plan（guest ABI） | guest 已观察到、没有 guest 配合就不能改；快照记录，恢复照搬 | 区间、种类；virtio-mem 的 addr / region_size / block_size（规范：只在系统复位时可变，`mem/description.tex:119-120`） |
| 设备快照（协议状态） | guest 经设备协议在运行期改变 | virtio-mem 的已插块 bitmap、requested_size、usable_region_size。设备复位不得改变块状态（`mem/description.tex:174`），所以它们放在会话之外 |
| 只在运行期（宿主资源） | 恢复时重新获得 | slot 号、HVA、fd、哪些页已 discard |

### 2.3 种类矩阵

| 区域 | 所有者 | plan 记录 | 运行期 / 设备快照 | KVM slot | e820 | 设备 DMA 视图 | 内存镜像 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 启动 RAM | 机器 | 区间 | — | 每段 1 个，静态 | RAM | 是 | 全量（memfd 可稀疏） |
| virtio-mem 区 | 机器 | 区间、block、chunk | bitmap、requested、usable | 每个 chunk 1 个，按需绑定 | 否 | 整个区域 | 只存已插的块 |
| ACPI DIMM（A5 后） | 机器 | 插入时产生新的 plan 值 | — | 每条 1 个 | 否（ACPI） | 是，需要传播 | 全量 |
| virtio-pmem | 设备 | 窗口、路径、只读 | — | 1 个；只读时加 `KVM_MEM_READONLY` | 否 | 否 | 不存，文件即内容 |
| virtio-fs DAX | 设备 | 窗口大小 | 子映射表 | 1 个覆盖整窗 | 否 | 否 | 不存 |

设备窗口第一版不进 DMA 视图：只读 pmem 的映射若在视图里，guest 只要让 virtio-blk 往那里写，整个 VMM 就会 SIGSEGV，违反局部故障隔离。代价是 guest 对 DAX 页做 O_DIRECT 会得到 I/O 错误。firecracker、cloud-hypervisor 同样不放；QEMU 则全部纳入地址空间。

## 3. GPA 空间

- **窗口**：`device_window = [align_up(max(RAM 末端, 4 GiB), 1 GiB), 起点 + min(1 TiB, 2^phys_bits − 起点))`，由纯函数 `layout::device_window` 给出。窗口与 RAM 由同一个纯函数决定，二者不相交由构造保证。
- **记账**：窗口记入 `MemoryPlan.device_window`；`ResourceBook` 增加 `device_memory: AddressPool`，其窗口取自 plan；每个设备的 grant 记入自己的 plan；`replay` 按记录的窗口重放，以后修改布局函数也不影响旧快照。
- **对齐**：窗口起点与 virtio-mem 区 1 GiB（不小于 guest 的 memory block，小于 64 GiB 的 guest 为 128 MiB；也能容纳 1 GiB 大页）；chunk 默认 128 MiB；block 默认 2 MiB；pmem / DAX 窗口 2 MiB。
- **纯校验**（plan 阶段）：全部 grant 末端 ≤ 2^phys_bits；memslot 预算 = RAM 段数 + Σ（virtio-mem 区大小 ÷ chunk）+ 窗口类设备数 ≤ `KVM_CAP_NR_MEMSLOTS`（x86 为 32767 − 3 = 32764）。
- **e820**：只列启动 RAM。virtio-mem 区与 pmem 窗口只经 virtio 配置空间告知 guest。
- **为什么紧跟 RAM、不超过 1 TiB**：PVH 直接进入 vmlinux 时没有 `KASLR_FLAG`（只有 bzImage 解压器设置，`arch/x86/boot/compressed/kaslr.c:855`），guest 的可热插上限只是 `MAX_PHYSMEM_BITS`。将来支持 A2（bzImage）后，内存 KASLR 会把上限压到 `(ceil(RAM / 1 TiB) + padding) TiB`（`arch/x86/mm/kaslr.c:109-110`；kangaroo 的 padding 为 1 TiB，超出部分 guest 只告警并弃用）。紧跟 RAM 且不超过 1 TiB 在两种启动方式下都成立。
- **对照**：firecracker 用固定窗口 [512 GiB, 1 TiB)，并把整个分配器序列化进快照；cloud-hypervisor 在 RAM 之后按 128 MiB 对齐、留 256 MiB 间隔，直到物理地址顶端；QEMU 用 `ROUND_UP(above_4g_end, 1 GiB)`，大小 = maxmem − ram + 1 GiB × 插槽数，选出的地址写回设备属性。

## 4. 运行期变化

### 4.1 KVM 绑定：RAII 句柄（本阶段实现）

```rust
pub struct Binding {
    vm: Arc<KvmVm>,     // 删除 slot 需要 VM fd
    slot: MemSlot,
    range: GpaRange,
    mapping: …,         // 映射的 Arc：KVM 可能访问期间，映射一直活着
}

impl KvmVm {
    /// range 必须落在本 VM 自己的 GuestRam 中尚未绑定的区域内。
    pub fn bind(self: &Arc<Self>, slot: MemSlot, range: GpaRange) -> Result<Binding>;
}

impl Binding {
    /// 删除 slot 成功才交还映射；失败则原样还回句柄，它仍处于绑定状态。
    pub fn unbind(self) -> Result<…, (Self, anyhow::Error)>;
}
// Drop：尽力删除 slot；删除失败就泄漏映射，绝不 unmap KVM 仍能访问的内存。
```

"先解绑 → 再确认无人访问 → 最后 unmap"由所有权保证：

1. **先解绑**：只有删除 slot 的 ioctl 成功后，才能从 `unbind` 拿回映射。KVM 在该 ioctl 返回前执行 `synchronize_srcu_expedited`（`kvm_main.c:1673`、`2075-2082`）。
2. **再确认**：VMM 内的访问者都经视图值访问；新视图下发后，各自回 ack 即放弃旧视图（§4.2）。外部访问者（以后）：vhost-user 对 `REM_MEM_REG` 的应答、VFIO unmap 的返回。
3. **最后 unmap**：munmap 发生在最后一个 `Arc` 被 drop 时（vm-memory 的 `MmapRegion`）。

slot 号由 `Machine` 持有的值类型 `IdPool<MemSlot>` 分配，attach 时按 plan 给每个设备预留一段（单一所有者，不需要锁）；slot 号仍不进 plan 与快照。绑定由拥有区域的设备持有，不经 vmm-main：这样不会出现"设备等 vmm-main 回复、vmm-main 在 Busy(pause) 中等这个设备 quiesce"的死锁。

### 4.2 设备 DMA 视图：按值下发 + oneshot 确认（以后实现）

不用 `GuestMemoryAtomic`：它无法等待"旧视图已无人使用"（guard 可被持有任意久，块设备整个会话都持有 `Active.mem`），也是一个隐藏的共享可变单元。按值下发、各设备在自己的循环里替换后回复 ack，与 `Activation` 和协作 quiesce 同形；transport 在自己的 `Mutex` 里同步替换。代价是每个持有视图的设备都要处理这条消息，卡住的设备会拖慢屏障，需要超时策略。内存安全两者都由 `Arc` 保证。

本阶段不实现：virtio-mem 的整个区域在启动时映射并放进所有视图（firecracker、cloud-hypervisor 都如此），运行期只变 KVM 绑定。等第一个改变 DMA 视图的消费者（A5 的 ACPI DIMM、E9 的 pmem 热插、G5）出现时再落地。届时删除一块区域的顺序是：guest 弹出 → 下发新视图并等全部 ack → `unbind` → 释放 `Arc`（munmap）→ 释放 grant。

扩展到 vhost-user / VFIO 需要两类事件，都按值发送、带 ack，注册时先回放当前状态（参考 cloud-hypervisor 的 `ExternalDmaMapping`、QEMU 的 `RamDiscardListener`）：映射变化（对应 vhost-user 的 `ADD/REM_MEM_REG`、VFIO 的 map / unmap）；区域内的填充变化（virtio-mem 插拔产生的 Populated / Discarded 范围，VFIO 只钉住已插的块）。

### 4.3 vCPU 与总线

不涉及。guest 访问已解绑的 GPA 会变成 MMIO 退出，总线未命中时读 0、写忽略，VMM 照常运行。firecracker 用 `PROT_NONE` 保护未插的 slot，VMM 自己误触就会 SIGSEGV；本设计不这样做。

## 5. virtio-mem

### 5.1 配置与 API

- **启动前**：`SetMemoryHotplug(MemoryHotplugConfig)`，绑定 `.machine.memory_hotplug`，路由 `PUT /v1/machine/memory-hotplug`。
  - `size_mib`：区域大小（可热插的上限），必须是 chunk 的整数倍。
  - `block_size_mib`：缺省 2 MiB；2 的幂，不小于宿主后端的页大小。
  - `chunk_size_mib`：缺省 128 MiB；block 的整数倍，2 的幂。guest 看不到它，它只决定 KVM slot 的粒度。
- **运行期**：`ResizeMemory(HotplugSizeMib)`，路由 `PATCH /v1/machine/memory-hotplug`，只在 Running 时接受。只改 `requested_size`（block 的整数倍，≤ `size_mib`），不等待 guest 完成插拔。
- 启动前配置不等于启动时插入：规范要求初始全部未插（`mem/description.tex:24`）；宿主以 `MAP_NORESERVE` 映射整个区域，只有插入并被访问的页才占用内存。启动后要提高上限，只能重启，或等 E9 / A5。
- 对照：firecracker 的 `SetMemoryHotplugDevice` 只在启动前（`rpc_interface.rs:862-864`），`UpdateMemoryHotplugSize` 只在运行期；cloud-hypervisor 启动参数 `hotplug_size` 加运行期 `resize`；QEMU 的 `size` 在设备创建时固定，`requested-size` 运行期可改。

### 5.2 纯核

```rust
// devices/virtio/mem.rs：没有 fd，也没有 KVM
pub struct MemState { pub requested: u64, pub usable: u64, pub plugged: BlockBitmap } // plugged_size 由 bitmap 推出
pub enum MemRequest { Plug(BlockRange), Unplug(BlockRange), UnplugAll, State(BlockRange) }
pub enum MemEffect { Bind(Chunk), Unbind(Chunk), Discard(GpaRange) }

pub fn handle(geometry: &MemGeometry, state: &MemState, request: MemRequest)
    -> (MemState, MemResponse, Vec<MemEffect>);
```

副作用的顺序由纯核固定，单测直接断言：

- **插入**：`[Bind(k)]`（chunk k 原本没有已插块），执行成功后才提交新状态并回 ACK；ACK 之前 slot 已存在。
- **拔出、chunk 拔空**：`[Unbind(k), Discard(range)]`。先解绑再 discard，避免 guest 在两步之间经 KVM 把刚释放的页又分配回来。
- **拔出、chunk 仍有已插块**：`[Discard(range)]`，chunk 保持绑定；guest 违反协议再访问这些块，最多重新分配一个 chunk 的宿主页。
- 任一副作用失败：回 ERROR，状态不变（事务性）。cloud-hypervisor 在 IOMMU unmap 之前就 discard，插入时先改 bitmap、失败也不回滚（`virtio-devices/src/mem.rs:518-556`），本设计避免这两点。
- 以后接 VFIO：插入在最后加 `Notify(Populated)`，拔出在最前加 `Notify(Discarded)`，都等外部 ack。

### 5.3 设备任务与 transport

- 设备任务放在 io 线程（ioctl 和 madvise 可能耗时毫秒级，不能阻塞 vmm-main，D5）；协作 quiesce，在自己的循环里用 `&mut` 导出状态、经 oneshot 回复。
- 块状态属于设备任务本身，不属于某一次激活；guest 的 virtio reset 不改变它。
- 插拔状态只存一份（`MemState`，按块），chunk 是否绑定由它推出。firecracker 按 slot 和按块各存一份，恢复时不核对。
- transport 需要：设备发布可变配置、transport 读取；实现 ConfigGeneration（0x0fc）与配置变更中断。SHM 寄存器按规范返回 −1 / ~0 的修正留到第一个使用共享内存区的设备（virtio-fs DAX、pmem 的 SHMEM_REGION）。
- 要求 guest 协商 `VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE`：协商后 guest 不得访问未插的块，才能把空 chunk 从 KVM 解绑。kangaroo-6.6 的驱动支持它（`virtio_mem.c:3443-3448`）；firecracker 同样强制要求。
- 未插的 chunk 仍保持映射（只是 discard 过）：guest 违反协议的 DMA 只会重新分配宿主页（上限是区域大小），不会让 VMM 崩溃。

### 5.4 guest 要求

kangaroo-6.6 已开启：`CONFIG_VIRTIO_MEM`、`CONFIG_MEMORY_HOTPLUG`、`CONFIG_MEMORY_HOTREMOVE`、`CONFIG_CONTIG_ALLOC`、`CONFIG_MEMORY_HOTPLUG_DEFAULT_ONLINE`。可靠拔出需要内核参数 `memhp_default_state=online_movable`（插入的块进 ZONE_MOVABLE）。

## 6. 后端（本阶段只做设计）

按"谁能观察到它"分三层：

| 层 | 类型 | guest ABI |
| --- | --- | --- |
| plan | 区间、种类、virtio-mem 的 block / chunk | 是 |
| 页从哪来 | `Backing` | 否；决定 fd 可用性、discard 方式、对齐 |
| 怎样准备页 | `MemoryPolicy` | 否；恢复时可另选 |

```rust
pub enum Backing {
    Anonymous(Pages),  // 私有、无 fd；Huge 即 MAP_HUGETLB
    Memfd(Pages),      // 共享、有 fd；Huge 即 MFD_HUGETLB
    File(PathBuf),     // 共享、有 fd；页大小由文件系统决定
}
pub enum Pages { Base { thp: bool }, Huge(HugePageSize) }
pub struct MemoryPolicy { pub prealloc: bool, pub numa: Option<HostNodes> }
```

- 唯一的建映射入口：预留对齐 VA（2 MiB / 1 GiB，参考 firecracker 的 `RawGuestRegionMmap`）→ `MAP_FIXED` 映射 backing → madvise → mbind → populate。先 mbind 再预分配是 QEMU `host_memory_backend_memory_complete` 的顺序；大块预分配放 blocking 线程，不扩大 D5 的例外。
- 唯一的 `discard(range)`：匿名内存 `MADV_DONTNEED`；memfd / 文件 `MADV_REMOVE`（等同打洞，会发出 `UFFD_EVENT_REMOVE`；现有的 `SHRINK | GROW | SEAL` 封印不阻止打洞，只有写封印会，`mm/shmem.c:3741`）；hugetlb 只释放整页。cloud-hypervisor 的 virtio-mem 与 balloon 各有一套 discard，已经不一致。
- virtio-mem 用现有两种后端即可；类型拆分与 API 的 `MemoryBacking` 扩展在 hugetlb 落地时一起做。

## 7. 快照与所有权

### 7.1 所有权（M0）

- `GuestRam`：guest RAM 映射的唯一所有者，不实现 `Clone`，记录自己是按哪份 `MemoryPlan` 映射的（取代 `describe()`）。只有它能绑定到 VM、被转储或交接。
- 视图 `SharedGuestMemory`（`Arc<GuestMemoryMmap>`）：可克隆，只能读写字节。全 crate 只定义一次。
- `KvmVm { fd, ram: GuestRam }`；`KvmVm::into_ram(self: Arc<Self>)` 只在没有其他持有者时成功：先关闭 VM fd，再交还 `GuestRam`。
- `Machine::teardown(self) -> Result<GuestRam>`：vCPU 与设备都停下后才交还 RAM；`Machine::memory()` 给出视图，取代测试里经快照访问 RAM 的后门。
- `MachineSnapshot` 不再有 `memory` 字段；`rebuild(snapshot, ram, context)` 按值接收 `GuestRam`，并核对 `ram` 的 plan 与快照的 plan 一致。没有 RAM 就无法 rebuild，同一块 RAM 也不可能被用两次。

```text
现在：pause → capture（snap 持有 RAM 的 Arc）→ teardown（可省略）→ rebuild(snap)
计划：pause → capture（纯数据）→ teardown（交还 GuestRam）→ rebuild(snap, ram)
```

### 7.2 持久化（M5）

- 内存镜像是区域模型的投影：启动 RAM 全量（memfd 用 `SEEK_DATA` 跳过空洞），virtio-mem 区只存已插的块，设备窗口不存。所以持久化排在区域模型与 virtio-mem 之后。
- `MachineSnapshot` 加版本头与 CRC（firecracker：bitcode + CRC64，只认同一主版本）。
- 一个稀疏镜像文件，manifest 显式记录每段的文件偏移（firecracker、cloud-hypervisor 都由区段顺序隐含偏移，镜像没有头和版本）。
- 恢复先做 Copy；`MAP_PRIVATE` 写时复制与 uffd 以后再加。
- 热升级（E10）：memfd 后端时 `GuestRam::into_fds(self)` 交出所有权，借 API 已有的 fd 传递发送；新进程用 `GuestRam::adopt` 接收，零拷贝。cloud-hypervisor 的本地迁移就是逐个 slot 发送 memfd。
- dirty tracking 放在最后：KVM 侧只改已有 slot 的 flags（`kvm_main.c:2108-2109` 允许）；VMM 侧把视图的 bitmap 类型参数从 `()` 换成 `Option<AtomicBitmap>`；slot 号不持久化，脏页按 GPA 区间记录。

## 8. 代表性类型（spec → plan → machine）

```rust
// api
pub struct SetMemoryHotplug(pub MemoryHotplugConfig);   // .machine.memory_hotplug，启动前
pub struct MemoryHotplugConfig { pub size_mib: u32, pub block_size_mib: Option<u32>, pub chunk_size_mib: Option<u32> }
pub struct ResizeMemory(pub HotplugSizeMib);            // 运行期

// spec（纯）
pub enum DeviceSpec { Block(BlockSpec), Mem(MemSpec) }
pub struct MemSpec { pub id: String, pub size: u64, pub block_size: u64, pub chunk_size: u64 }

// plan（纯；快照记录，恢复时 replay）
pub struct MemoryPlan { pub backing: MemoryBacking, pub ram: Vec<GpaRange>, pub device_window: GpaRange }
pub enum DevicePlan { Block(BlockPlan), Mem(MemPlan) }
pub struct MemPlan { pub id: String, pub transport: VirtioMmioGrant, pub region: GpaRange, pub block_size: u64, pub chunk_size: u64 }
pub struct ResourceBook { pub virtio_mmio: AddressPool, pub legacy_gsi: IdPool<Gsi>, pub device_memory: AddressPool }

// machine（副作用）
pub struct GuestRam { /* 映射及其布局；不实现 Clone */ }
pub type SharedGuestMemory = Arc<GuestMemoryMmap>;      // 视图
pub struct KvmVm { fd: VmFd, ram: GuestRam }            // new() 只绑定启动 RAM
pub struct Binding { vm: Arc<KvmVm>, slot: MemSlot, range: GpaRange, mapping: … }
pub enum AttachedDevice { Block(AttachedBlock), Mem(AttachedMem) }

// 快照
pub enum DeviceSnapshot { Block(BlockSnapshot), Mem(MemSnapshot) }
pub struct MemSnapshot { pub id: String, pub transport: MmioTransportSnapshot, pub queue: Option<QueueState>, pub state: MemState }
pub struct MachineSnapshot { pub plan: MachinePlan, pub vm: VmKvmState, pub vcpus: Vec<VcpuSnapshot>, pub platform: PlatformSnapshot, pub devices: Vec<DeviceSnapshot> }
impl Machine { pub async fn teardown(self) -> Result<GuestRam>; }
pub fn rebuild(snapshot: MachineSnapshot, ram: GuestRam, context: &KvmContext) -> Result<Machine>;
```

## 9. 实施步骤

每一步一个可独立验证的提交，结束时 `make lint`、`make test` 通过；API 有变化时再跑 `make api-features`、`make schema-export`、`make schema-verify`。修 bug 时补的测试，去掉修复后必须失败。

| 步骤 | 内容 | 验证 | 可运行性 |
| --- | --- | --- | --- |
| M0 所有权与视图拆分 | `GuestRam` 与唯一的视图别名；`KvmVm { fd, ram }`、`into_ram`；`teardown` 交还 RAM；快照去掉 memory，`rebuild` 按值接收 RAM；删除 `describe()` | 两种后端的 round-trip 改为 teardown → rebuild；内存比 VM 与 vCPU 活得久；vCPU 未退出时交不出 RAM；文档测试证明 RAM 不能用两次 | 功能不变 |
| M1 区域模型与设备内存窗口 | `MemoryPlan { backing, ram, device_window }`；`layout::device_window`；`ResourceBook.device_memory`；`replay` 覆盖内存 grant；phys-bits 与 slot 预算；e820 只取启动 RAM | 单测：窗口位置与对齐（RAM ≤ 3 GiB / > 3 GiB）、池耗尽、replay 拒绝越窗与重叠、slot 预算 | 还没有设备使用窗口 |
| M2 运行期 memslot 绑定 | `Binding`、`KvmVm::bind`；`Machine` 持有 `IdPool<MemSlot>` | KVM：mini guest 循环读一个运行期区间（放在 4 GiB 以下）：绑定前读到 0 且 VMM 不退出，绑定后读到标记，解绑后再读到 0；映射在解绑后才释放；删除失败走泄漏路径 | 只新增 API |
| M3 virtio-mem | 纯核、设备任务、`machine/mem.rs` 纵切、transport 可变配置、API、按 chunk 绑定与 discard、`DeviceSnapshot::Mem` | 纯核单测（对齐、超出 requested、副作用顺序、设备复位不改块状态）；设备测试扮演 guest 驱动；KVM 测试观察 chunk 绑定；同进程 round-trip 保留已插块；真实 Linux：调大后 MemTotal 增加，调小后减少、宿主 RSS 回落 | 不配置时与之前一致 |
| M4 virtio-pmem（可选） | 设备拥有的文件映射（`MAP_SHARED`，只读加 `KVM_MEM_READONLY`）、io 线程上 flush、不进 DMA 视图、快照只记配置 | guest 中 `mount -o dax` 读到宿主写入的标记；写只读 pmem 被丢弃且 VMM 不退出 | 新增设备 |
| M5 快照持久化 | 带版本与 CRC 的序列化、稀疏镜像 + manifest、Copy 恢复、`HandoverState` 交接 memfd | 跨进程恢复 mini guest 与 virtio-mem 插拔状态；损坏或版本不符被拒；交接后新进程继续运行 | 新增能力 |

之后：hugetlb / 预分配 / NUMA（同时拆分后端类型）、dirty tracking、视图传播与 listener（A5、G5）、balloon（复用同一个 discard）。

## 10. 进度

- [x] M0 所有权与视图拆分：两种后端的 KVM round-trip、复制 vCPU fd 关闭前后交接、RAM 不可克隆与不可二次恢复的文档测试通过；移除 plan 核对与添加 Clone 的变异均被测试捕获。`make lint`、`make test`、`make api-features`、`make schema-verify` 通过。
- [ ] M1 区域模型与设备内存窗口
- [ ] M2 运行期 memslot 绑定
- [ ] M3 virtio-mem
- [ ] M4 virtio-pmem（可选）
- [ ] M5 快照持久化

## 11. 需要同步回 design.md 的偏差

1. 已修正（M0）：移除预设运行期增长使用 `GuestMemoryAtomic` 的注释；视图传播方案见 §4.2。
2. 已修正（M0）：`SharedGuestMemory` 只在 `devices/virtio/mod.rs` 定义，design.md §6.7 已同步。
3. 已修正（M0）：删除 `describe()`，由 `GuestRam` 保存映射时的 plan；design.md §6.7 与 §8.8 已同步。`MemoryPlan.backing` 与 `plan.spec.memory_backing` 仍各自记录，plan 的内部一致性校验仍是已知限制。
4. §6.6 与 B1 说 memslot "从 `IdPool<MemSlot>` 分配"，实际上池只是 `KvmVm::new` 的局部变量（M2 修正）。
5. §8.4 没有说明 transport 不实现 SHM 寄存器与 ConfigGeneration（M3 补充）。
6. 附录 A 仍以 `/fuu.code/kernel-5.10/.config` 为准；冒烟测试用的已是 kangaroo-6.6，且未列出内存设备所需的内核选项（M3 补充）。

## 12. 源码参照

路径相对于 `.references/<项目>/`；firecracker 的路径再相对于 `src/vmm/src/`。

| 主题 | firecracker | cloud-hypervisor | QEMU |
| --- | --- | --- | --- |
| 区域类型 | `vstate/memory.rs`：`GuestRegionType`、`GuestRegionMmapExt`、`RawGuestRegionMmap` | `vmm/src/memory_manager.rs`：`MemoryManager`、`MemoryZone`、`VirtioMemZone`、`GuestRamMapping`；`vm-device/src/lib.rs`：`UserspaceMapping` | `include/system/memory.h`：`MemoryRegion`、`FlatView` |
| GPA 窗口 | `arch/x86_64/layout.rs`；`vstate/resources.rs`：`past_mmio64_memory`；`builder.rs`：`allocate_virtio_mem_address` | `start_of_device_area`、`HOTPLUG_RAM_ALIGN_SIZE`；`device_manager.rs`：`create_mmio_allocators` | `hw/i386/pc.c`：`pc_get_device_memory_range`；`hw/mem/memory-device.c`：`memory_device_get_free_addr` |
| slot 管理 | `vstate/vm.rs`：`next_kvm_slot`；`memory.rs:698-740`：`update_slot` | `vm-allocator/src/memory_slot.rs`；`create_userspace_mapping` / `remove_userspace_mapping` | `accel/kvm/kvm-all.c`：`KVMSlot`、`kvm_set_phys_mem`、`kvm_region_commit` |
| 变化传播 | 无：映射静态，激活时克隆 | `GuestMemoryAtomic` + `DeviceManager::update_memory` | RCU `FlatView` + `MemoryListener`；`ram-discard-manager.h` |
| virtio-mem | `devices/virtio/mem/`；`vmm_config/memory_hotplug.rs` | `virtio-devices/src/mem.rs` | `hw/virtio/virtio-mem.c` |
| virtio-pmem | `devices/virtio/pmem/device.rs`：`PmemMmap`、`KvmMemSlot` | `device_manager.rs`：`make_virtio_pmem_device`；`virtio-devices/src/pmem.rs` | `hw/virtio/virtio-pmem.c` |
| 后端 | `vmm_config/machine_config.rs`：`HugePageConfig`；`memory.rs`：`memory_region_from_range`、`discard_range` | `vmm/src/vm_config.rs`：`MemoryConfig`、`MemoryZoneConfig`；`create_ram_region(_raw)`；`prefault.rs` | `include/system/hostmem.h`：`HostMemoryBackend`；`backends/hostmem.c` |
| 快照与交接 | `memory.rs`：`GuestMemoryState`、`dump` / `dump_dirty`；`persist.rs`；`snapshot/mod.rs` | `MemoryManagerSnapshotData`、恢复模式；`vmm/src/{uffd,sparse,pagemap}.rs`；`vm.rs`：`send_memory_fds` | — |
| vhost-user | `devices/virtio/vhost_user.rs`：`update_mem_table` | `virtio-devices/src/vhost_user/vu_common_ctrl.rs` | `hw/virtio/vhost.c`：`vhost_commit`；`vhost-user.c` |

## 13. 本阶段的已知限制（预期）

- 设备 DMA 视图包含整个 virtio-mem 区：guest 违反协议对未插块做 DMA 时，会重新分配宿主页（上限为区域大小），而不是得到 I/O 错误。
- 设备窗口（pmem、DAX）不在 DMA 视图中：guest 对这些页做 O_DIRECT 会得到 I/O 错误。
- 启动后不能提高热插上限，也不能新增 virtio-mem 设备（E9）。
- `ResizeMemory` 只在 Running 时接受。
- 将来的 vhost-user 后端会看到整个 virtio-mem 区，不受"未插不可访问"约束（firecracker 有同样的限制）。
