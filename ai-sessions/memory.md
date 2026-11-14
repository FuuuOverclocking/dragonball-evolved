# Guest 内存: M1

> 状态: 设计草案, 等待推进下一阶段.
> 代码: `crates/vmm/src/memory.rs` (只有类型和签名, 函数体都是 `todo!()`, 可以编译).
> 交互图: [memory.html](memory.html), 可以调整内存大小和地址位宽观察地址图, 并逐步执行一次区域的添加/移除.

## 0. 本阶段回答什么

M1 只覆盖基线需要的静态内存: 一台 x86-64 PVH guest 的 GPA 怎么规划, RAM 区域怎么在主机上落地, 设备怎么访问 guest 内存.

但 scope 0.2 明确要求内存布局从第一天起就能在运行时增减, 分配要记录并重放, 要能容纳 DMA 固定和脏页跟踪. 所以 M1 的类型按 "布局是一个会变化的值" 来建, 静态启动只是这个值的初始变化. 第 8 节说明 M2-M4 在这些类型上加什么.

## 1. 需求与来源

| # | 需求 | 来源 | 对模型的影响 |
|---|------|------|--------------|
| R1 | PVH 直接启动, ACPI 含 MCFG, virtio-pci | `docs/scope.md` 0.1 | 需要规划 32 位 PCI 窗口, ECAM, 64 位 PCI 窗口, 并通过 E820, MCFG, `_CRS` 告诉 guest |
| R2 | 地址图一经发布就不能改 | E820 和 ACPI 表在启动时交给 guest | `AddressMap` 在 build 时确定, 之后不变. 运行时出现的内存必须落在启动时就预留的窗口里 (M2 的 `HotplugRam`) |
| R3 | 布局运行时可增可减 | `docs/scope.md` 0.2 | `MemoryLayout` 是值, 只通过 `LayoutChange` 变化; 启动也是从空布局开始应用变化 |
| R4 | 分配作为数据记录, 恢复和迁移时重放而非重算 | `docs/scope.md` 0.2 | 每个记录都有 "策略" (`plan`, `grant`) 和 "机制" (`replay`) 两个入口, 恢复只走机制 |
| R5 | KVM 内存槽不可重叠, 不可改大小, 只能新建, 删除, 移动, 改标志 | `linux/virt/kvm/kvm_main.c:2090-2131` (`-EINVAL` 拒绝改 npages, `-EEXIST` 拒绝重叠) | `LayoutChange` 只有 `Add` 和 `Remove`, 没有 `Resize`; 布局不变量 "区域两两不重叠" 与 KVM 一致 |
| R6 | 地址空间受主机物理地址位宽限制 | Cloud Hypervisor 用 `phys_bits` 决定设备区上界 (`vmm/src/memory_manager.rs:496`) | `PhysBits` 是 `plan` 的输入; 记录的地址图在位宽更小的主机上 `replay` 会失败 |
| R7 | 内存可能被 DMA 映射并固定, 布局变化要同步 IOMMU | `docs/scope.md` 0.2; Cloud Hypervisor 在 virtio-mem plug/unplug 时逐块 map/unmap, 新注册的 DMA 处理者先回放已插入的块 (`virtio-devices/src/mem.rs:529-575, 857-901`) | 主机侧的落地是分层的, 每层持有内层, 以后在外面再包一层 `DmaMapped` |
| R8 | 设备并发访问 guest 内存, 布局变化时不能悬空 | vm-memory `GuestMemoryAtomic` | 设备持有 `MemoryView`; 每次布局变化发布新视图, 旧视图里的映射被 `Arc` 保活 |
| R9 | KVM 需要 3 页不被 guest 使用的 GPA 给 TSS | `KVM_SET_TSS_ADDR`; Firecracker `KVM_TSS_ADDRESS 0xfffb_d000` (`arch/x86_64/layout.rs:41`) | `PlatformMmio::KvmTss` 作为固定窗口 |

## 2. 三层模型: guest 可见的 vs 主机本地的

| 层 | 类型 | 何时变化 | 进快照 | 恢复时 |
|----|------|----------|--------|--------|
| 地址图 | `AddressMap` | build 时确定, 之后不变 | 是 | `AddressMap::replay` |
| 分配账本 | `GpaLedger` | build 时 (启动信息, 内核, ACPI), 以后还有 PCI BAR, 内存设备 | 是 | `GpaLedger::replay` |
| 内存布局 | `MemoryLayout` | build 时; M2 起热插拔 | 是 | `MemoryLayout` 本身 |
| 填充状态 | (设备状态) virtio-mem 块位图, balloon 页 | 运行时 | 是, 属于设备 | 设备恢复 |
| 主机落地 | `GuestMemory` 中的映射, 内存槽号, 视图 | 跟随布局 | **否** | 重新推导 |

划分规则只有一条: **guest 能观察到的进记录, 恢复时重放; 主机本地的不进记录, 每个进程重新推导.** 槽号, 主机虚拟地址, 后备方式 (匿名/memfd/大页) 都属于主机本地, 恢复的进程可以做不同选择.

"填充" 和 "布局" 分开是关键: balloon 回收页面, virtio-mem 插拔块, 都不改变布局, 也不改变 KVM 内存槽, 只改变某个区域里哪些页有后备. 只有增删 DIMM 或增删 virtio-mem 设备才改变布局.

## 3. M1 的 x86-64 地址图

```
0                                                                          1 << phys_bits
├─ BootRam (low) ─┤ PciMmio32 ┤ PciEcam ┤ (空) + Platform ├─ BootRam (high) ─┤ PciMmio64 ──┤
0          min(ram, 3G)      3G       3.5G            3.75G   4G      4G + (ram - 3G)        end

低 1 MiB 内 (都在 BootRam(low) 里, 以 GpaLedger 授予记录):
  0x6000   BootInfo    PVH hvm_start_info, 模块列表, 内存图
  0x20000  Cmdline
  0x9fc00  LegacyHole  [0x9fc00, 1 MiB) EBDA/VGA/BIOS, E820 reserved
  1 MiB+   Kernel      按 ELF 程序头放置
  高处     Initrd, AcpiTables (top_down)

4G 以下的固定区 (Platform, 几个小窗口, 其间未分配):
  0xfec0_0000 IoApic   0xfee0_0000 LocalApic   0xfffb_d000 KvmTss
```

| 常量 | 本设计 | Firecracker | Cloud Hypervisor |
|------|--------|-------------|------------------|
| 32 位洞起点 | `0xC000_0000` | `0xC000_0000` (`layout.rs:98-100`) | `0xC000_0000` (`layout.rs:90`) |
| ECAM | `0xE000_0000`, 256 MiB | `0xEC00_0000`, 256 MiB (`:105-107`) | `0xE800_0000`, 256 MiB (`:100-102`) |
| 高端 RAM 起点 | 4 GiB | 4 GiB | 4 GiB (`:129`) |
| 64 位 PCI 窗口 | RAM 之后, 1 GiB 对齐, 到地址空间末尾 | 固定 `[256G, 512G)` (`:122-124`) | 由 `phys_bits` 决定 (`memory_manager.rs:496`) |

具体数值未定, 见第 7 节.

## 4. 关键类型决策

### 4.1 地址类型

- `Gpa = vm_memory::GuestAddress`: 与设备侧的 vm-memory 生态直接互通.
- `GpaRange { start, len: NonZeroU64 }`: 构造器保证非空且 `start + len` 不溢出, 所以 `end()` 不需要处理溢出.
- IOVA (virtio-iommu 之后设备看到的地址) 和主机虚拟地址出现时各自用新类型, 彼此不隐式转换. M1 里没有它们, 所以现在不定义.
- `RamSize`: 非零, 1 MiB 的整数倍. `PhysBits`: `32..=52`.

### 4.2 策略与机制分离

每个记录都有两个入口:

| 记录 | 策略 (首次) | 机制 (重放) |
|------|-------------|-------------|
| `AddressMap` | `plan(&MapRequest, PhysBits)` | `replay(PhysBits, windows)` |
| `GpaLedger` | `grant(&self, &map, &GrantRequest) -> (Self, Grant)` | `replay(&map, grants)` |
| `MemoryLayout` | `boot(&map)` | `replay(&map, changes)` |

策略可以改进 (例如换一个放置算法), 机制只检查不变量. 恢复和迁移只走机制, 所以策略的变化不会让旧快照恢复出不同的地址. 这正是 scope 0.2 "重放而非重算" 的落点.

所有这些函数都是纯的, 返回新值而不是修改旧值. `GpaLedger` 用 `BTreeMap`; 授予次数多了以后可以换 `imbl::OrdMap` 以共享结构.

### 4.3 `LayoutChange` 只有 `Add` 和 `Remove`

这直接对应 KVM 的约束 (R5). 启动 = `replay(map, diff(empty, boot(map)))`, 恢复 = 同一个调用加记录的布局, 热插拔 = 再 `commit` 一个变化. 只有一条落地路径.

### 4.4 洋葱所有权: 添加由内向外, 移除由外向内

```
Backed { region, mapping: Arc<GuestRegionMmap> }      主机映射
  └─ Registered { backed, slot: SlotId }              已注册为 KVM 内存槽
       └─ (M3) DmaMapped { registered, .. }           已在 IOMMU 中映射
```

- `back(&RamRegion, &Backing) -> Backed`
- `Backed::register(self, &VmFd, SlotId) -> Result<Registered, Failed<Backed>>`
- `Registered::unregister(self, &VmFd) -> Result<(Backed, SlotId), Failed<Registered>>`

外层按值持有内层, 所以:

- 不可能先注册一个没有后备的槽 (`register` 需要 `Backed`);
- 不可能在槽还存在时释放映射 (映射被 `Registered` 拿着, 拿不出来);
- 移除只能逐层剥开, 顺序由类型决定.

失败时 `Failed<T>` 把输入原样交回, 调用者决定重试或丢弃, 不会出现 "一半成功" 的值.

**Drop 的取舍**: 如果 `Registered` 未经 `unregister` 就被丢弃 (例如 panic 展开), 它会泄漏映射而不是 munmap. 泄漏是安全的; 而 munmap 一个仍被槽指向的地址, 一旦该地址被复用, guest 就能读写主机上无关的内存. 正常路径上 VM fd 先于内存析构 (lifecycle.md 3.7), 槽已随 VM 消失.

### 4.5 `commit` 是原子的

```rust
fn commit(&mut self, vm: &VmFd, change: LayoutChange, backing: &Backing) -> Result<()>
```

- `Add`: `layout.apply` (纯, 检查不变量) → `back` → `register` → 发布新视图 → 写回 `layout` 和 `realized`.
- `Remove`: `layout.apply` → 发布不含该区域的视图 → `unregister` → 丢弃.

任一步失败, `layout` 和 `realized` 都保持原样; 已经完成的内层随 `Failed` 交回后丢弃 (未注册的映射可以安全 munmap). 不变量: `realized` 的键集 = `layout` 的区域集, 视图恰好映射 `realized`.

`commit` 用 `&mut self`: 主机资源天然是可变的, 不可变性留给决策层的值 (`MemoryLayout`, `GpaLedger`).

### 4.6 `MemoryView` 是能力

设备通过持有 `MemoryView` (内部是 `GuestMemoryAtomic<GuestMemoryMmap>`) 获得访问 guest 内存的权限, 没有它就访问不了. 每次 `memory()` 得到一份一致的布局快照; 布局变化后, 仍在处理旧请求的设备继续使用旧快照, 映射被 `Arc` 保活, 不会悬空. guest 已经同意拔出的内存在旧快照里多活一会儿, 不影响正确性.

### 4.7 E820 是推导出来的

`e820(&layout, &ledger)`: RAM 区域减去 guest 不能复用的授予 (`LegacyHole` 记为 reserved, `AcpiTables` 记为 ACPI). E820, MCFG, `_CRS` 都应当是记录的纯函数, 不另存一份.

## 5. 不变量与性质

| # | 性质 |
|---|------|
| P1 | `AddressMap`: 窗口有序, 两两不重叠, 都在 `1 << phys_bits` 之下; 平台窗口位于架构固定地址 |
| P2 | `plan` 确定: 同样的输入得到同样的地址图; `replay(bits, plan(r, bits).windows()) == plan(r, bits)` |
| P3 | `GpaLedger`: 授予两两不重叠, 每个都在一个接纳其所有者的窗口内; `replay(map, l.grants()) == l` |
| P4 | `MemoryLayout`: 区域两两不重叠, 每个都在一个接纳其角色的窗口内 |
| P5 | `replay(map, cs) == cs.fold(empty, apply)`; `diff(a, b)` 作用于 `a` 得到 `b`; `diff(a, a)` 为空 |
| P6 | `plan` 的启动 RAM 总量等于请求的 `RamSize` |
| P7 | `commit` 原子: 失败后 `record()` 不变 |
| P8 | 任何时刻, 每个已注册槽指向的映射都存活 (由所有权保证, 测试只需覆盖 Drop 泄漏路径) |
| P9 | E820 中 RAM 条目两两不重叠, 且都落在布局区域内 |

P1-P6, P9 都在纯函数上, 适合性质测试.

## 6. build 时的伪代码

```rust
fn build_memory(spec: &VmSpec, vm: &VmFd, host: &HostInfo) -> Result<GuestMemory> {
    // 纯: 规划
    let map = AddressMap::plan(&MapRequest { boot_ram: spec.ram }, host.phys_bits)?;
    let mut ledger = GpaLedger::default();
    for req in boot_grants(&spec.boot) {           // LegacyHole, BootInfo, Cmdline, Kernel, Initrd, AcpiTables
        let (next, _) = ledger.grant(&map, &req)?;
        ledger = next;
    }
    let layout = MemoryLayout::boot(&map);

    // 副作用: 落地
    GuestMemory::realize(vm, map, ledger, &layout, &Backing::AnonymousPrivate)
    // 之后: 写内核, initrd, 启动信息, ACPI 表到各自的授予区间, E820 来自 e820(&layout, &ledger)
}

// 恢复 (L2): 同一个 realize, 记录来自快照而不是规划
fn restore_memory(record: &MemoryRecord, vm: &VmFd, host: &HostInfo) -> Result<GuestMemory> {
    let map = AddressMap::replay(host.phys_bits, record.windows.clone())?;
    let ledger = GpaLedger::replay(&map, record.grants.iter().cloned())?;
    GuestMemory::realize(vm, map, ledger, &record.layout, &host.backing)
}
```

内核的放置不是 VMM 的策略, 而是 ELF 程序头决定的, 所以走 `Placement::Fixed`; 但它仍然进账本, 这样后面所有授予都能检查不和它重叠.

## 7. 未决问题

1. **数值**: ECAM 放在 `0xE000_0000` 还是贴着 IOAPIC; 64 位 PCI 窗口取固定大小 (Firecracker) 还是用尽地址空间 (Cloud Hypervisor). 后者在主机位宽不同的机器间迁移时更容易失败.
2. **限制 guest 位宽**: 是否允许配置一个小于主机的 `PhysBits`, 让快照能在位宽较小的主机上恢复.
3. **低端 RAM 是否从 0 连续**: 本设计和 Firecracker 一样, `[0, 3G)` 一个区域一个槽, legacy 洞只在 E820 层保留. 代价是洞里约 385 KiB 也有后备.
4. **AMD HyperTransport 洞**: `[1012G, 1024G)` 在 AMD 主机上不能用作 RAM (Cloud Hypervisor `layout.rs:132-133`). RAM 很大时 `plan` 需要绕开.
5. **后备与 prefault**: M1 只有匿名私有映射. prefault 大内存会让 build 阻塞 (lifecycle.md 第 8 节问题 2).
6. **单槽大小上限**: KVM 单槽页数有上限, 超大内存可能需要把一个区域拆成多个槽, 届时 `Registered` 持有槽号集合.

## 8. 后续阶段 (由你推动)

### M2: virtio-mem 与 balloon (scope 里程碑 1)

```rust
enum WindowKind { .., HotplugRam }                    // M1 已有变体, M2 开始由 plan 产生
struct MapRequest { boot_ram: RamSize, hotplug_ram: u64 }
enum RegionRole { .., VirtioMem(DeviceId), Dimm(DimmSlot) }   // 只被 HotplugRam 接纳
enum Owner { .., MemoryDevice(DeviceId) }             // 在 HotplugRam 中的授予
```

- 热插窗口必须在 build 时预留, 因为 ACPI 在启动时发布; virtio-mem 的 `addr` 和 `region_size` 在设备生命周期内固定, 只有 `usable_region_size` 和 `requested_size` 可变 (virtio 1.4 virtio-mem 一节, `specs-pdf-to-txt/virtio-v1.4-cs01.txt:16256-16295`).
- 对齐: Cloud Hypervisor 要求 virtio-mem 区域 128 MiB 对齐, 默认块 2 MiB (`virtio-devices/src/mem.rs:53-55, 763`).
- 一个 virtio-mem 设备 = 一个布局区域 (整体注册为槽), 块的插拔是设备状态里的位图, 拔出的块用 `madvise` 丢弃后备. 布局和槽都不变.
- 复位时所有块视为拔出 (spec `:16207-16209`), 这是设备状态的初始值, 不涉及布局.
- balloon 只改填充, 不改布局.

### M3: DMA 映射与丢弃互斥 (scope 里程碑 3)

- 洋葱外加一层 `DmaMapped`. 对 virtio-mem 区域, DMA 映射的粒度是块, 不是区域: 设备状态中的块位图变化同时驱动 DMA map/unmap; 新注册的 DMA 接收者先回放当前已插入的块, 与 Cloud Hypervisor 的做法一致.
- VFIO 与 balloon 互斥, VFIO 与 virtio-mem 兼容 (scope 0.3). QEMU 用一对互斥计数实现: VFIO 调用 "禁止非协调丢弃", virtio-mem 调用 "要求协调丢弃", balloon 在丢弃被禁止时拒绝工作 (`qemu/system/physmem.c:4417, 4450`, `hw/virtio/virtio-balloon.c:76`). 在这里可以表达为能力: 一个 `DiscardPolicy` 值只能发出 `PinRight` 或 `UncoordinatedDiscardRight` 之一, VFIO 设备和 balloon 设备分别要求持有对应的能力才能创建.

### M4: 脏页跟踪与布局冻结 (scope 里程碑 4)

- 开启脏页跟踪是对槽的 `KVM_MR_FLAGS_ONLY` 变化 (`kvm_main.c:2121-2124`), 不改布局.
- 迁移期间冻结布局 (scope 0.3): 修改布局的权限做成值 `LayoutWriter`, 平时由 `GuestMemory` 持有, 迁移会话开始时移走, 结束时交还. 迁移期间收到的 virtio-mem 调整请求因为拿不到 `LayoutWriter` 而被拒绝, 不需要 "是否在迁移" 的布尔量.

### 横向: 后备方式 (随 L2/L4)

- 快照恢复: 文件 `MAP_PRIVATE` 映射, 按需缺页.
- 热升级: memfd 后备, fd 交给新进程, 新进程映射到不同的主机地址. 记录不变, 只是主机落地换了一次. 这是第 2 节 "主机本地不进记录" 的直接收益.
