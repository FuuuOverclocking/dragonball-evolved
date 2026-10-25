# dragonball-evolved VMM 设计：x86-64 / KVM / Linux guest

> 状态：首阶段已实现，已用 PVH Linux 6.6.102 验证启动到 Debian 13 systemd/login；其后完成了生命周期控制、状态可提取（同进程 capture / rebuild）、资源账本、guest RAM 所有权与 memfd 后端。第 1 节的勾选表示范围选择；实际进度见第 14 节。每项有编号（如 `C6`），便于讨论。
> 第 2 节起是按建议项展开的详细设计；修改勾选后，相关章节随之调整。
>
> 参考：firecracker 1.18-dev（主要）、cloud-hypervisor（对照）。git 中已删除的 `docs/vmm-architecture.md` 与 blog 系列只作背景参考，不作约束。

## 0. 目标与非目标

**第一个里程碑**：在 x86-64 Linux 宿主 + KVM 上，以 PVH 方式启动 Linux guest。

- 串口控制台可交互；rootfs 来自 virtio-blk，或只用 initramfs。
- guest 执行 `reboot` 后，VMM 干净退出。
- 通过配置文件或 REST API 完成配置，以及启动、暂停、恢复、关机、查询。

**贯穿全局的两条原则**：

1. **异步**：vmm-main 线程是 tokio `LocalRuntime`；设备是 `!Send` 的 async 任务，运行在 vmm-main 或独立 io 线程上，各自等待自己关心的东西，没有中央 epoll 注册表。
2. **决策与副作用分离**：配置、地址布局、启动数据、CPUID、寄存器、virtio transport 的寄存器语义都写成纯函数，没有 `/dev/kvm` 也能测试。副作用（ioctl、mmap、fd、线程、任务）集中在"按计划物化"的一段直线代码里。

**非目标**：见第 1 节 G 组。

---

## 1. 决策清单

### A. 启动与平台

- [x] **A1 仅支持 PVH 启动。**
  内核是带 `XEN_ELFNOTE_PHYS32_ENTRY` 的 vmlinux（`CONFIG_PVH=y`）。入口为 32 位保护模式、关分页，`rbx` 指向 `hvm_start_info`。
  不需要 zero page，也不需要给 guest 预建页表。
- [ ] **A2 同时支持 Linux 64-bit boot protocol**（无 PVH note 的 vmlinux / bzImage）。
  需要 zero page、e820、恒等映射页表、64 位 GDT。firecracker 两种协议都支持。
- [x] **A3 initrd**：经 PVH modlist 传递，放在低端 RAM 顶部、页对齐。
  只需几十行；可以先用 initramfs 冒烟，不依赖 virtio-blk。
- [x] **A4 平台描述只用 MP table**（放在 EBDA，`0x9fc00`）。
  guest 需要 `CONFIG_X86_MPPARSE=y`。Linux 找不到 RSDP 时会自行改用 MP table。
- [ ] **A5 ACPI**（RSDP / XSDT / FADT / MADT / DSDT）。
  不做的代价：guest `poweroff` 只会让 CPU 停住，VMM 无法感知，所以关机走 `reboot` 或 API；CPU/内存热插拔、VMGenID 也依赖 ACPI。
- [x] **A6 内核态 irqchip**：`KVM_CREATE_IRQCHIP`（PIC + IOAPIC + LAPIC）+ `KVM_CREATE_PIT2`（`KVM_PIT_SPEAKER_DUMMY`）。
  GSI 0–23 使用 KVM 默认路由；virtio 使用 `KVM_IRQFD` + resamplefd，在 EOI 后对仍有 pending status 的 level IRQ 重触发。
- [ ] **A7 split irqchip（用户态 IOAPIC）/ 显式 GSI 路由表 / MSI。** virtio-pci、VFIO 时才需要。
- [x] **A8 默认 cmdline**：`console=ttyS0 reboot=k panic=1 pci=off i8042.noaux i8042.nomux i8042.dumbkbd`。
  用户给出 cmdline 时整体替换默认值。VMM 只追加自己负责的片段：`virtio_mmio.device=…`，以及根盘的 `root=/dev/vda rw|ro`。

### B. 内存与 vCPU

- [x] **B1 guest RAM 每段一个 KVM memslot**，后端由 `MemoryBacking` 选择，缺省为匿名私有 mmap（`MAP_NORESERVE`），见 §6.7。
  memslot 编号从 `IdPool<MemSlot>` 分配，上限取 `KVM_CAP_NR_MEMSLOTS`；它是运行期资源，不进 plan，也不进快照。
- [x] **B2 布局含 32 位 MMIO 空洞 `[3 GiB, 4 GiB)`。** 超过 3 GiB 的 RAM 从 4 GiB 处继续。布局由纯函数 `ram_regions(size)` 给出。
- [x] **B3 memfd backing**（`MemoryBacking::Shared`）：每段一个封印的 memfd，`MAP_SHARED` 映射，fd 经 region 的 `FileOffset` 可取得。
  这是 vhost-user 内存表与跨进程 fd 交接需要的形式；快照本身不要求 memfd。backing 在启动时决定。hugetlbfs 尚未做。
- [x] **B4 SMP：1–254 个 vCPU**（xAPIC + MP table 的上限，同时受 `KVM_CAP_MAX_VCPUS` 约束）。拓扑固定为 1 socket、每核 1 线程。
- [ ] **B5 x2APIC 模式（>254 vCPU）/ 可配置拓扑（SMT、多 socket）/ NUMA。**
- [x] **B6 CPUID 归一化**：以 `KVM_GET_SUPPORTED_CPUID` 为基线，按厂商改写（见 §7.1）。
  归一化是纯函数；需要的宿主 CPUID 叶在打开 `/dev/kvm` 时读取一次，作为数据传入。
  本机是 AMD，Intel 分支只能靠单测覆盖。
- [ ] **B7 CPU template**（屏蔽特性，以便跨宿主迁移）。
- [x] **B8 启动时的 vCPU 状态**：
  - 所有 vCPU：boot MSR（同 firecracker）、LAPIC LINT0=ExtINT / LINT1=NMI。
  - 只有 BSP：PVH 寄存器与 FPU。AP 由 guest 通过 INIT-SIPI 启动，预设的寄存器会被覆盖，没有意义。
- [x] **B9 每个 vCPU 一条 OS 线程**，阻塞在 `KVM_RUN`；MMIO/PIO 退出在本线程同步分发。
- [x] **B10 vCPU 踢出**：控制方先写 `kvm_run.immediate_exit = 1`，再 `pthread_kill(SIGRTMIN)`（firecracker 的做法）。
  如果信号在两次 `KVM_RUN` 之间到达，下一次 `KVM_RUN` 会看到 `immediate_exit` 立即返回，所以踢出不会丢失，也就不需要 cloud-hypervisor 那种"发信号—轮询—重发"。
- [x] **B11 暂停与恢复**：控制命令携带 oneshot 回复，vmm-main 侧并发 `.await` 全部回复。vCPU 线程以 Paused 状态创建，`Start` 即全部 resume。

### C. 设备

- [x] **C1 不可变 Bus**（PIO、MMIO 各一条）：构建完成后只读，查找是排序数组上的二分，无锁；锁只存在于单个设备内部。不支持热插拔。
- [x] **C2 16550A 串口 COM1**（`0x3f8`，GSI 4），基于 vm-superio，沿用 POC 的输入任务与背压设计。
- [x] **C3 控制台输入**：stdin 是 tty 或 pipe 时接入。tty 下切 raw mode，退出时（含 panic）恢复。
  fd 0/1/2 通常共享同一个 open file description，直接给 stdin 设 `O_NONBLOCK` 会连带 stdout/stderr，日志写 stderr 时可能遇到 `EAGAIN`。
  因此先经 `/proc/self/fd/0` 重新打开，得到独立的描述，再设 `O_NONBLOCK` 并交给 `AsyncFd`。
- [x] **C4 串口输出**：vCPU 线程只把字节推入有界缓冲（满了就丢弃并计数），由异步写任务落到 stdout。这样 vCPU 线程永远不会因宿主终端而阻塞。
- [ ] **C5 串口输出到文件 / unix socket / pty**（可附着、可分离）。
- [x] **C6 i8042**：实现 firecracker 同等的子集——状态寄存器，命令 `0x20/0x60`（读写 CTR）、`0xD0/0xD1`（读写输出口）、`0xFE`（CPU 复位）。
  - 这个子集足以让 Linux 的 i8042 探测不再等超时。
  - `0xFE` 发出 `GuestReset` 事件；三重故障（`KVM_EXIT_SHUTDOWN`）同样视为 guest 复位。
- [ ] **C7 i8042 键盘注入 Ctrl-Alt-Del**（API 发起 guest 优雅关机）；需要 IRQ1 与扫描码队列。
- [x] **C8 virtio-mmio v2 transport**：
  - 寄存器语义是纯状态机；`DRIVER_OK` 时构造 queue，通过 channel 交给设备任务。
  - 支持 guest reset：设备确认后才返回 guest。
  - 队列通知走 `KVM_IOEVENTFD`（datamatch=队列号），中断走 irqfd。
- [x] **C9 virtio-blk**（在 POC 上补全）：
  - 配置空间 `capacity`；特性 `VERSION_1 / FLUSH`，只读盘加 `RO`。
  - 请求 IN / OUT / FLUSH / GET_ID；队列上限 256。
  - 根盘排第一，对应 `/dev/vda`。
- [ ] **C10 virtio-blk `EVENT_IDX`**（减少通知与中断次数）。
- [x] **C11 块 I/O 用同步 `pread/pwrite`**，但设备放在 io 线程上（D3），因此不阻塞 vmm-main。
- [ ] **C12 io_uring 后端。** ring 的 eventfd 交给 reactor，请求真正异步、可并发。可在 C11 之后替换，接口不变。
- [ ] **C13 virtio-net（tap）。**
- [ ] **C14 virtio-vsock / virtio-rng / balloon。**
- [ ] **C15 CMOS RTC**（`0x70/0x71`）。不做时 guest 墙钟由 kvmclock 提供，只多一条探测告警。

### D. 执行模型

- [x] **D1 vmm-main 主循环**：tokio `LocalRuntime`，沿用现有结构——`select!` 只判断发生了什么，处理放在 `select!` 之外。
- [x] **D2 新增的跨线程边界只有以下几类，全部是类型化 channel 或内核对象**（已有的请求 channel 与 `DeviceExit` channel 不变，完整列表见 §2）：
  - vCPU 线程与同步设备 → vmm-main：`MachineEvent`；
  - vmm-main → vCPU：控制命令；
  - transport → 设备任务：`Activation`；
  - guest ↔ 设备：ioeventfd / irqfd。

  不为用户态唤醒新增 eventfd。
- [x] **D3 io 线程**：
  - 每条 io 线程跑一个 `LocalRuntime`。
  - 设备由 `Send` 工厂闭包在目标线程上构造，因此设备本身可以是 `!Send`，热路径无锁。
  - 放置可选 `VmmMain` / `Io`：串口任务默认放在 vmm-main，块设备默认放在共享的一条 io 线程。
- [ ] **D4 每个块设备独占一条 io 线程**（或 io 线程数可配置）。
- [x] **D5 vmm-main 永不同步阻塞**：耗时操作要么在 vCPU/io 线程上，要么以 `.await` 等待。
  唯一例外是 `Start` 构建机器时同步读内核与 initrd 文件（顺序读几十 MB）。

### E. 生命周期与 API

- [x] **E1 VM 状态 `NotStarted → Running ⇄ Paused → 退出`。**
  主循环持有 `Controller`（§4.1）：空闲时它拥有 `Vm`，执行生命周期操作时由操作 future 拥有 `Machine`。"该做什么"由对（状态，输入）的穷举 match 决定，副作用随后顺序执行。
- [ ] **E2 完整 reducer + `Effect` 枚举 + executor + 完成事件回流**（已删除的 vmm-architecture.md 方案）。
  更彻底，但在当前规模下样板较多；与 E1 二选一。
- [x] **E3 配置累积为纯数据**：`MachineSpec::apply(op) -> Result<MachineSpec>`；`Start` 时依次执行 `MachinePlan::new`（先 `validate`）→ `build`。
- [x] **E4 新增/补全 API 操作**（见 §9）：
  - 配置：`machine.cpus`、`machine.memory_mib`、`machine.memory_backing`、`machine.console`、`machine.kernel`、`machine.disks[]`（补全 `AddDisk`）；
  - 生命周期：`Start / Pause / Resume / Shutdown`，均带 HTTP 路由；
  - `GetInstanceInfo` 反映真实状态。
- [x] **E5 自动启动**：配置文件含 `machine.kernel` 时，main 在依次下发全部配置命令后，自动发送 `Start`。
- [x] **E6 API `Shutdown` 为硬停**：收到请求后立即回复 `Ok(())`，表示“已接收”；随后关闭请求入口，并按“停 vCPU → 停设备任务 → 释放机器”完成拆除（§4.5）。回复不表示机器已经关闭，拆除失败只反映在最终 `VmmExitStatus`。
- [x] **E7 退出原因**：
  - guest 复位 → VMM 正常退出；
  - vCPU 内部错误 → VMM 以错误状态退出；
  - 设备任务失败只记录日志，不停机；
  - 全部客户端断开：VM 未启动（含断开后 `Start` 失败）→ 正常退出；已启动 → 告警，guest 继续运行。
- [ ] **E8 真正的 reboot**（guest 复位后在同一进程内重建机器）。
- [ ] **E9 启动后修改配置 / 热插拔。** 不做时，启动后的配置类请求一律返回错误。
- [x] **E10 `HandoverState`（热升级）先返回 unsupported。**

### F. 工程与测试

- [x] **F1 代码全部放在 `crates/vmm` 内**：`arch::x86_64`（纯：无 fd、无 tokio）、`spec` 与 `resources`（纯数据）、`machine`（副作用；其中 plan 为纯）、`devices`。
- [ ] **F2 把纯 arch 代码拆成独立 crate。** 编译隔离更强，但现阶段边界仍在变化。
- [x] **F3 新依赖**：`kvm-ioctls 0.25`、`kvm-bindings 0.14`（`fam-wrappers`）、`linux-loader 0.14`（只启用 `elf`；PVH 模块随之提供，没有独立 `pvh` feature）。tokio 不新增特性：控制台走 `AsyncFd`（属于已启用的 `net` 特性）。
  不引入 `vm-allocator`：资源由自写的值类型池分配（`resources.rs`，§6.6），结果记录在 plan 中，恢复时重放。
- [x] **F4 纯函数单测**：布局、memmap、MP table（含校验和）、CPUID 归一化、寄存器、transport 状态机、cmdline、`MachineSpec` 校验。
- [x] **F5 KVM 集成测试**：测试代码现场生成一个 mini PVH ELF（几十字节手写 32 位代码 + Xen note）。
  - 验证内容：32 位入口、`rbx` 指向的 start_info magic、串口输出、i8042 复位退出、pause/resume。
  - 不需要真实内核，有 `/dev/kvm` 即可运行。
- [x] **F6 真实 Linux 冒烟测试**：默认 `#[ignore]`，由环境变量指定 kernel / initrd / rootfs，使用你提供的 PVH 内核。
- [x] **F7 每个里程碑**：`make lint`、`make test` 通过，并重新导出 schema（`make schema-export`）。

### G. 本阶段明确不做

勾选其中一项，即表示把它纳入本阶段。

- [ ] **G1** seccomp / jailer
- [ ] **G2** metrics
- [ ] **G3** 限速接入 API（`TokenBucket` POC 保留，不接线）
- [ ] **G4** PCI / virtio-pci
- [ ] **G5** vhost-user / VFIO
- [ ] **G6** 快照持久化（序列化、格式版本）/ 迁移。同进程 capture / rebuild 已实现（§8.8），用来验证状态可提取。
- [ ] **G7** aarch64
- [ ] **G8** TUI / GUI

---

## 2. 执行上下文

| 上下文       | 运行时              | 职责                                                  | 不做                    |
| ------------ | ------------------- | ----------------------------------------------------- | ----------------------- |
| `main`       | tokio local（已有） | CLI、配置、logger、信号；启动 API server；下发配置命令 | 持有 VM 内部状态        |
| `api-server` | current_thread（已有） | HTTP over UDS → `VmmRequest`                         | —                       |
| `vmm-main`   | `LocalRuntime`（已有） | 主循环、生命周期、构建与拆除机器、轻量设备任务       | 同步阻塞（D5）          |
| `io-N`       | `LocalRuntime`      | 重 I/O 设备任务（块设备）                              | 生命周期决策            |
| `vcpu-N`     | 无，阻塞线程        | `KVM_RUN`、同步 MMIO/PIO 分发                          | 异步 I/O；等待 vmm-main |

跨线程边界（D2）：

| 边界      | 方向                      | 载体                                            | 备注                       |
| --------- | ------------------------- | ----------------------------------------------- | -------------------------- |
| 请求      | main / api → vmm-main     | `mpsc::UnboundedSender<VmmRequest>`             | 请求自带 `Reply`；发送端全部 drop 后上报一次 `Detached` |
| 机器事件  | vcpu-N、i8042 → vmm-main  | `mpsc::UnboundedSender<MachineEvent>`           | 非运行时线程可直接 `send`  |
| vCPU 控制 | vmm-main → vcpu-N         | `std::sync::mpsc` + `immediate_exit` + SIGRTMIN | 回复用 `oneshot`           |
| 设备激活  | transport（vcpu-N）→ 设备任务 | `Activator`（已有）                         | 每次激活附带会话结束信号   |
| 队列通知  | guest → 设备任务          | ioeventfd → `AsyncEventFd`（已有）              | 不经过 vCPU 用户态         |
| 中断      | 设备 → guest              | irqfd（`IrqTrigger`，已有）                     |                            |
| 设备退出  | 设备任务 → vmm-main       | `DeviceExit` channel（已有）                    | io 线程上的设备同样适用    |

**一条推论**：vmm-main 从不同步等待 vCPU 或 io 线程。因此 vCPU 线程可以在少数场合同步等待设备任务的确认（virtio reset，§8.4），不会形成死锁环。

## 3. 纯核与副作用的分界

```text
配置类 VmmCommand ──apply──▶ MachineSpec                 纯：可比较
MachineSpec + HostCaps ──MachinePlan::new──▶ MachinePlan  纯：validate，再定 RAM 区段、设备 grant、cmdline
MachinePlan + 内核/initrd ──build──▶ Machine             副作用：ioctl、mmap、fd、线程、任务
暂停的 Machine ──capture──▶ MachineSnapshot              读取：设备、平台、vCPU、VM 状态 + 内存引用
MachineSnapshot ──rebuild──▶ Machine                     副作用：重放快照里 plan 的 grant，再恢复状态
```

- **`HostCaps`**：打开 `/dev/kvm` 时一次性取得的纯数据投影。包括：
  - API 版本、必需能力、`KVM_CAP_MAX_VCPUS`、`KVM_CAP_NR_MEMSLOTS`；
  - `KVM_GET_SUPPORTED_CPUID`；
  - 需要透传的宿主 CPUID 叶。

  `validate` 与 CPUID 归一化只依赖它，不碰 fd。
- **`MachinePlan`**：机器的全部布局决定，构建后不再变化。

  ```rust
  pub struct MachinePlan {
      pub spec: MachineSpec,
      pub memory: MemoryPlan,          // backing + 有序不相交的 RAM 区段（§6.7）
      pub devices: Vec<DevicePlan>,    // guest 探测顺序；每项带自己的 grant（§6.6）
      pub cmdline: Cmdline,
  }
  ```

  `MachinePlan::new` 先 `spec.validate(host)`（没有单独的 `ValidSpec` 类型），再经资源账本为每个设备分配 grant。快照携带 plan；restore 不重新分配，而是用 `plan::replay` 在新账本上 reserve 记录过的 grant 来校验它们。
- **启动数据**：cmdline、`hvm_start_info`、memmap、modlist、MP table、GDT/IDT 由纯函数生成为一组 `(GuestAddress, Vec<u8>)`，`build` 只负责逐条写入。
  内核本身由 linux-loader 直接加载到 guest memory，这是唯一不经过中间数据的写入。
- **vCPU 状态**：CPUID、MSR、`kvm_regs`、`kvm_sregs`、`kvm_fpu`、LAPIC 寄存器都由纯函数计算，`build` 只调用对应的 `KVM_SET_*`。
- **`build` 是直线代码**：按 plan 顺序执行副作用，不含决策逻辑。中途失败时，已创建的资源随 RAII 释放；状态保持 `NotStarted`，错误作为 `Start` 的回复返回。
- **一条组装路径**：`build` 与 `rebuild` 只在"vCPU 怎样得到初始状态"上不同（配置 vs 恢复），之后共用 `assemble`：挂平台设备与 plan 中的设备，再启动处于 Paused 的 vCPU 线程。

## 4. 主循环与生命周期

### 4.1 状态与事件

```rust
enum Vm {
    NotStarted(MachineSpec),
    Running(Machine),
    Paused(Machine),
}

enum Controller {
    Idle(Vm),
    Busy { stable: VmState, operation: Operation },
}

struct Operation {
    future: Pin<Box<dyn Future<Output = OperationCompletion> + 'static>>,
    cancel_tx: Option<oneshot::Sender<()>>,
}

struct OperationCompletion {
    vm: Vm,          // 操作自己决定回到哪个 Vm
    step: Step,      // Continue / Stop(VmmExitStatus)
}

enum Event {                              // Idle 时
    Request(VmmRequest),
    Detached,                             // 全部客户端已断开
    Machine(MachineEvent),
    Device(DeviceExit),
}

enum BusyEvent {                          // Busy 时
    Completion(Box<OperationCompletion>),
    Request(VmmRequest),
    Detached,
}

enum MachineEvent {
    GuestReset,                           // i8042 0xFE、三重故障
    VcpuFailed { index: u8, error: String },
}
```

`Machine` 拥有一次启动所需的全部运行时资源：`Arc<KvmVm>`（VM fd 与 guest memory，§6.7）、MMIO Bus、平台设备（i8042、COM1）、按 kind 挂上的设备、vCPU 句柄、`DeviceManager`（含 io 线程）、`MachineEvent` 接收端，以及构建它的 `MachinePlan`。

**`Controller`** 是主循环实际持有的顶层状态。`Idle(Vm)` 时，`Vm` 拥有 `Machine`。`Busy` 时，`Machine` 的所有权已移入 `Operation` 的 future 中，外层只记住转入前的 `stable: VmState`（`NotStarted` / `Running` / `Paused`）。

- **Idle**：主循环对机器事件和请求做 `select!`，全部处理逻辑在 `handle_idle` 中穷举。
- **Busy**：一个生命周期操作（Pause / Resume / Start 的 vCPU resume 阶段）正在进行中。主循环对操作 future 和请求做 biased `select!`。查询与 `UpdateLogger` 仍立即处理；其他写 / 生命周期请求返回错误 `"lifecycle operation in progress"`。`GetInstanceInfo` 返回 `stable` 中记录的前一稳定态。操作完成后带回 `Vm`，Controller 回到 `Idle`。

`Operation` 的 future 是 `'static + !Send` 的 `Pin<Box<dyn Future>>`，因此不需要 tokio `Send` bound，可安全地在 `LocalRuntime` 上 `.await`。请求方的 `Reply` 由 future 自己持有并发送；`Operation` 只保存 future 与 `cancel_tx`。`cancel_tx` 用于 Shutdown 时中断操作：发送后，future 内的 `select!` 立即退出，向请求方回复 `"interrupted by shutdown"`，并在 `OperationCompletion` 中归还 `Vm`。

### 4.2 主循环

```rust
let mut controller = Controller::new(MachineSpec::default());
let mut status = loop {
    match controller {
        Controller::Idle(mut vm) => {
            // Start 在全部客户端离开后失败：没有人能再启动它。
            if server.is_detached() && matches!(vm, Vm::NotStarted(_)) {
                controller = Controller::Idle(vm);
                break VmmExitStatus::Ok;
            }
            let event = select! {
                biased;
                event = vm.next_event() => event,
                inbound = server.next_inbound() => match inbound {
                    Inbound::Request(request) => Event::Request(request),
                    Inbound::Detached => Event::Detached,
                },
            };
            let (next, step) = handle_idle(vm, event, &ctx).await;
            match step {
                Step::Continue => controller = next,
                Step::Stop(s) => { controller = next; break s; }
            }
        }
        Controller::Busy { stable, mut operation } => {
            let event = select! {
                biased;
                completion = &mut operation.future => BusyEvent::Completion(Box::new(completion)),
                inbound = server.next_inbound() => match inbound {
                    Inbound::Request(request) => BusyEvent::Request(request),
                    Inbound::Detached => BusyEvent::Detached,
                },
            };
            let (next, step) = handle_busy(stable, operation, event, &BusyCtx::from(&ctx)).await;
            match step {
                Step::Continue => controller = next,
                Step::Stop(s) => { controller = next; break s; }
            }
        }
    }
};
drop(server);
if let Err(error) = controller.teardown().await {
    status = VmmExitStatus::Error(format!("teardown: {error:#}"));
}
status
```

- **Idle 分支**：机器事件与设备退出（`machine.tasks.next_exit()`）排在请求之前（biased `select!`），guest 已经复位时不再处理排队的请求。`handle_idle` 消费旧 `Vm`、返回新 `Controller`。生命周期请求（Pause / Resume / Start 的 vCPU resume）将 `Machine` 移入 `Operation`，返回 `Controller::Busy`。
- **Busy 分支**：两个分支都只产出一个 `BusyEvent`，由 `handle_busy` 穷举：
  - `Completion` → `complete_operation`：回到 `Idle(completion.vm)`，按 `completion.step` 继续或停止；
  - `Request` → `handle_busy_request`：查询与 `UpdateLogger` 立即处理；`Shutdown` 发送 cancel、等待 future 归还 `Vm`，再以 `Idle` 进入拆除；其余请求被拒绝；
  - `Detached` → 告警，操作继续。
- **请求入口**：`VmmServer::next_inbound()` 在全部发送端 drop 后返回一次 `Detached`，之后永远 pending，主循环不会因为已关闭的入口而空转。断开后，一个未启动的 VM 没有人能再启动：Idle 时直接退出，Busy 中的 `Start` 失败回到 `NotStarted` 时由循环开头的检查退出。
- `select!` 的借用在得出事件时已经结束，`handle_*` 消费旧状态、返回新状态，没有 `&mut` 共享。

### 4.3 请求 × 状态

| 请求                                       | NotStarted                             | Running            | Paused             | Busy                          |
| ------------------------------------------ | -------------------------------------- | ------------------ | ------------------ | ----------------------------- |
| 配置类（cpus / memory / memory_backing / console / kernel / disk） | `spec.apply(op)`     | 错误：已启动（E9） | 错误：已启动       | 错误：操作进行中              |
| `Start`                                    | `MachinePlan::new` → build → Busy(vCPU resume)；build 失败则回复错误、保持 NotStarted；resume 失败则拆除机器、回到 NotStarted（可重试） | 错误 | 错误 | 错误：操作进行中 |
| `Pause`                                    | 错误：未启动                           | → Busy(pause)      | 成功（幂等）       | 错误：操作进行中              |
| `Resume`                                   | 错误：未启动                           | 成功（幂等）       | → Busy(resume)     | 错误：操作进行中              |
| `Shutdown`                                 | 回复”已接收”，然后退出                 | 回复”已接收”，然后拆除并退出 | 同左               | 回复”已接收”，cancel → teardown |
| `GetInstanceInfo`                          | 纯查询                                 | 纯查询             | 纯查询             | 返回 `stable`（前一稳定态）   |
| `UpdateLogger`                             | 进程级副作用，与 VM 状态无关            | 同左               | 同左               | 同左                          |
| `HandoverState`                            | unsupported（E10）                     | 同左               | 同左               | 同左                          |

不合法的组合直接以错误回复，状态不变。Busy 时的错误回复文本为 `”lifecycle operation in progress”`。

### 4.4 机器事件与客户端断开

| 事件                 | 处理                                    |
| -------------------- | --------------------------------------- |
| `GuestReset`         | 拆除，`VmmExitStatus::Ok`（E7）         |
| `VcpuFailed`         | 拆除，`VmmExitStatus::Error`            |
| `DeviceExit`         | 记录日志，继续运行。已退出的块设备不阻碍 pause（记录为 `Exited`），但 capture 拒绝这台机器（§8.8） |
| `Detached`           | NotStarted：退出，`Ok`；Running / Paused：告警，guest 继续运行；Busy：告警，操作继续（结果见 §4.2） |

### 4.5 拆除顺序

不变量：任何还可能写 guest memory 的组件仍在运行时，不释放这块内存。

**从 Idle 状态拆除**（Shutdown 或 GuestReset）：

1. **接收 `Shutdown` 时回复**：立即发送 `Ok(())`，只确认请求已进入停止流程；然后关闭请求入口。
2. **停 vCPU**：向全部 vCPU 发 `Stop` 并踢出 → `.await` 每个线程的"已退出"通知 → 在 blocking pool 中 `join`。
3. **停设备**：`tasks.stop()`——设置 stop watch，向全部 guest gate 发 `Stop`，`.await` 全部 `DeviceExit`（含 io 线程上的）→ 结束 io 线程。
4. **drop `Machine`**：Bus 与 transport、ioeventfd/irqfd 注册、vCPU fd、`KvmVm`。
5. **释放 guest memory**：由所有权结构保证，拆除代码不再手工排序（§6.7）。`KvmVm` 拥有 VM fd 与 memory 且先关 fd；每个 `KvmVcpu`（含踢出用的复制 fd）持有 `Arc<KvmVm>`。所以映射只会在 VM fd 与全部 vCPU fd 关闭之后解除。快照另持有 memory 的 `Arc`，RAM 因此可以跨过源机器的拆除。

拆除结果写入最终 `VmmExitStatus`，不再修改已经发送的 Shutdown 回复。

**从 Busy 状态拆除**（Shutdown 到达时有操作在执行）：

1. **回复 `Ok(())`**：同上。
2. **Cancel**：发送 `cancel_tx`，操作 future 内的 biased `select!` 立即退出。
3. **向生命周期请求发送中断错误**：future 自己向被中断的 Pause / Resume / Start 回复 `"interrupted by shutdown"`，并在 `OperationCompletion` 中归还 `Vm`。
4. **Controller 回到 `Idle`**，随后执行上述 Idle 拆除流程。

## 5. vCPU

### 5.1 创建与配置

`KVM_CREATE_IRQCHIP` 必须先于 `KVM_CREATE_VCPU`。每个 vCPU 按以下顺序配置：

1. `KVM_SET_CPUID2`（按该 vCPU 归一化后的 CPUID）。必须先于 MSR：KVM 依据 guest CPUID 判断哪些 MSR 可用。
2. `KVM_SET_MSRS`（boot MSR，§7.2）。
3. 仅 BSP：`KVM_SET_REGS` / `KVM_SET_SREGS` / `KVM_SET_FPU`（§7.3）。
4. `KVM_SET_LAPIC`（LINT0 / LINT1）。

### 5.2 线程与句柄

```rust
// vcpu-N 线程独占
struct VcpuRunner {
    index: u8,
    fd: KvmVcpu,                     // VcpuFd + Arc<KvmVm>（§6.7）
    pio: Arc<Bus>,
    mmio: Arc<Bus>,
    commands: std::sync::mpsc::Receiver<VcpuCommand>,
    events: mpsc::UnboundedSender<MachineEvent>,
}

// vmm-main 持有
struct VcpuHandle {
    index: u8,
    kick_fd: Mutex<KvmVcpu>,         // KvmVcpu::try_clone 复制的 fd：写 immediate_exit，暂停时读状态
    commands: std::sync::mpsc::Sender<VcpuCommand>,
    exited: Option<oneshot::Receiver<()>>,   // 线程返回前触发，用于异步 join
    thread: Option<JoinHandle<()>>,
}

enum VcpuCommand {
    Pause(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    Stop,
}
```

- 线程名为 `vcpu-N`，以 Paused 状态启动：阻塞在 `commands.recv()`。
- 发送命令 = `send` + 踢出（B10）。`Vcpus::pause()` 并发等待全部回复。
- 状态读写：`VcpuHandle::capture_state()` 在 vCPU 暂停时经 kick fd 读取（`get_mp_state` 最先）；`vcpu::restore_state(fd, snapshot)` 按 Firecracker 的顺序写回（§8.8）。
- SIGRTMIN 的处理函数只做一个 `fence`，进程内经 `Once` 只安装一次，且在第一个 vCPU 线程启动前完成。
- 其他发往进程的信号（SIGINT、SIGTERM）也可能落到 vCPU 线程，使 `KVM_RUN` 返回 `EINTR`。处理方式与踢出相同：处理命令后继续运行。

### 5.3 运行循环

vCPU 线程的 `run_once()` 返回三种结果：

```rust
enum RunResult {
    Handled,      // I/O 退出已在本线程同步处理完毕
    Interrupted,  // EINTR / Intr：需要检查控制命令
    Stop,         // 终结性退出（GuestReset、VcpuFailed）
}
```

状态迁移：

```text
Paused   ──Resume──▶ 回复，进入 Running
Paused   ──Pause───▶ 回复，保持 Paused（幂等）
Paused   ──Stop────▶ 线程结束
Running  ──run_once() → Handled────▶ 直接再次进入 KVM_RUN（不检查命令）
Running  ──run_once() → Interrupted──▶ 清 immediate_exit，try_recv 取出全部命令：
             Pause  → 回复，进入 Paused
             Resume → 回复，继续运行
             Stop   → 线程结束
             无命令 → 重新进入 KVM_RUN
Running  ──run_once() → Stop──▶ 线程结束
```

| `KVM_RUN` 结果                         | `RunResult` | 处理                                                        |
| -------------------------------------- | ----------- | ----------------------------------------------------------- |
| `IoIn` / `IoOut`                       | `Handled`   | PIO Bus。未命中时读返回 0、写忽略，打限速日志                |
| `MmioRead` / `MmioWrite`              | `Handled`   | MMIO Bus，未命中处理同上                                     |
| `Shutdown`（三重故障）                 | `Stop`      | 发 `GuestReset`，线程结束                                    |
| `SystemEvent(SHUTDOWN / RESET)`        | `Stop`      | 同上                                                         |
| `FailEntry` / `InternalError` / 其他   | `Stop`      | 发 `VcpuFailed`，线程结束（内核态 irqchip 下 `Hlt` 不应出现，归入此类） |
| `Intr`                                 | `Interrupted` | 见上图                                                     |
| `Err(EINTR)`                           | `Interrupted` | 见上图                                                     |
| `Err(EAGAIN)`                          | `Handled`   | 重新进入                                                     |

**关键语义**：MMIO/PIO 退出返回 `Handled`，直接再次 `KVM_RUN`，不检查命令队列。只有 `EINTR` / `Intr`（由踢出信号或进程信号引起）才触发命令处理。这保证了 Pause 的 ACK 发出时，该 vCPU 上所有 pending 的 userspace I/O（MMIO/PIO 分发）已经完成：如果 Pause 信号在 MMIO/PIO 处理期间到达，处理先完成，然后 re-enter `KVM_RUN`，此时 `immediate_exit` 使其立即以 `EINTR`/`Intr` 返回，线程随后处理 Pause 命令并回复。

## 6. 内存布局与 PVH 启动

### 6.1 物理地址

| GPA                         | 用途                                                     |
| --------------------------- | -------------------------------------------------------- |
| `0x0000_0500`               | 启动 GDT（null / code / data / tss）                     |
| `0x0000_0520`               | 启动 IDT（空，limit 7）                                  |
| `0x0000_6000`               | `hvm_start_info`                                         |
| `0x0000_6040`               | modlist（initrd）                                        |
| `0x0000_7000`               | memmap 表                                                |
| `0x0002_0000`               | 内核 cmdline（≤ 2 KiB）                                  |
| `0x0009_fc00`–`0x000e_0000` | 系统数据区（MP table），memmap 中为 reserved              |
| `0x0010_0000`               | 高端内存起点；内核按 ELF `p_paddr` 加载（通常为 16 MiB） |
| 低端 RAM 顶部               | initrd（页对齐）                                         |
| `0xc000_0000`–`0xc100_0000` | virtio-mmio 窗口（16 MiB，`VIRTIO_MMIO_WINDOW`）：每个设备一个 4 KiB grant，从 `AddressPool` 分配并记在 plan 中（§6.6） |
| `0xc000_0000`–`0x1_0000_0000` | 32 位 MMIO 空洞（B2）                                  |
| `0xfec0_0000`               | IOAPIC                                                   |
| `0xfee0_0000`               | LAPIC                                                    |
| `0xfffb_d000`               | `KVM_SET_TSS_ADDR`（3 页）                               |
| `0x1_0000_0000` 起          | 超出 3 GiB 的 RAM                                        |

### 6.2 I/O 端口与 GSI

| 端口            | 设备                                  | GSI          |
| --------------- | ------------------------------------- | ------------ |
| `0x20`、`0xa0`  | PIC（内核态）                         | —            |
| `0x40`–`0x43`、`0x61` | PIT（内核态，speaker dummy）    | 0            |
| `0x60`–`0x64`   | i8042（C6）                           | 1（C7 才用） |
| `0x3f8`–`0x3ff` | COM1（C2）                            | 4            |
| —               | virtio-mmio 设备：从 `LEGACY_GSIS`（`IdPool<Gsi>`）分配，记在 plan 中 | 5–23（最多 19 个） |

### 6.3 memmap

PVH memmap 使用 e820 类型，由纯函数 `memmap(ram_regions)` 生成：

| 区段                               | 类型     |
| ---------------------------------- | -------- |
| `[0, 0x9fc00)`                     | RAM      |
| `[0x9fc00, 0xe0000)`               | reserved |
| `[1 MiB, min(size, 3 GiB))`        | RAM      |
| `[4 GiB, 4 GiB + (size − 3 GiB))`  | RAM（仅 size > 3 GiB） |

`[0xe0000, 1 MiB)` 不列出，guest 视为不可用。

### 6.4 启动数据

```rust
hvm_start_info {
    magic: 0x336e_c578,
    version: 1,
    cmdline_paddr: 0x2_0000,
    memmap_paddr: 0x7000,
    memmap_entries,
    nr_modules,            // 有 initrd 时为 1
    modlist_paddr,         // 有 initrd 时为 0x6040
    rsdp_paddr: 0,         // 无 ACPI（A5）
    ..
}
```

MP table（A4），按 MP Spec 1.4 生成字节，并计算两处校验和：

- MP floating pointer，指向其后的 MPC 表头；
- N 个 processor 项：APIC ID = vCPU 序号，0 号带 BSP 标志；
- 1 个 ISA 总线；
- 1 个 IOAPIC，ID = N + 1；
- 24 个 I/O interrupt 项：ISA IRQ i → IOAPIC pin i，与 KVM 默认路由一致；
- 2 个 local interrupt 项：ExtINT → BSP 的 LINT0，NMI → 全部 LAPIC 的 LINT1。

### 6.5 `Start` 中的启动步骤

1. 解析内核 ELF；没有 PVH note 时报错 `kernel has no PVH entry point`。
2. linux-loader `Elf::load` 加载到 ≥ 1 MiB 处，得到 PVH 入口地址。
3. 有 initrd 时读入低端 RAM 顶部（页对齐）；放不下则报错。
4. 由纯函数生成启动数据（§3），一次写入。
5. 配置 vCPU（§5.1）。

### 6.6 资源账本（`resources.rs`）

```rust
pub struct GpaRange(std::range::Range<u64>);   // guest 物理地址区间；与 PIO 端口、GSI 用不同类型
pub struct Gsi(pub u32);
pub struct MemSlot(pub u32);                    // machine/vm.rs

pub struct AddressPool { /* name, window: GpaRange, allocated: Vec<GpaRange> */ }
impl AddressPool {
    pub fn allocate(&self, len: u64, align: u64) -> Result<(GpaRange, Self), ResourceError>;
    pub fn reserve(&self, range: GpaRange) -> Result<Self, ResourceError>;
    pub fn release(&self, range: GpaRange) -> Result<Self, ResourceError>;
}
pub struct IdPool<T> { /* name, ids: RangeInclusive<T>, allocated */ }   // allocate / reserve / release，同上

pub struct ResourceBook { pub virtio_mmio: AddressPool, pub legacy_gsi: IdPool<Gsi> }
pub struct VirtioMmioGrant { pub mmio: GpaRange, pub gsi: Gsi }          // machine/plan.rs
```

- **值类型**：每次分配返回结果与新的池，旧值不变；失败时调用方手中的旧账本仍然有效，不需要回滚。
- **plan 记录 grant**：`MachinePlan::new` 经账本为每个设备分配 `VirtioMmioGrant`，记在 `DevicePlan` 中；账本本身不保存。
- **恢复时重放，不重算**：`plan::replay` 在 `resource_book()` 给出的新账本上逐个 `reserve` 记录的 grant。重叠或越出窗口的 grant 在挂任何设备之前就被拒绝，设备随后挂在记录的位置，而不是按 spec 重新排出的位置。
- **memslot** 由 `KvmVm::new` 从 `IdPool<MemSlot>` 分配（上限 `KVM_CAP_NR_MEMSLOTS`），只属于运行期，不进 plan 与快照。
- 窗口与池的范围是 arch 数据：`layout::VIRTIO_MMIO_WINDOW`（编译期断言它止于 IOAPIC 之下）、`layout::LEGACY_GSIS`。

### 6.7 guest RAM：plan、后端与所有权

```rust
pub struct MemoryPlan {
    pub backing: MemoryBacking,      // Anonymous（缺省）| Shared
    pub regions: Vec<GpaRange>,      // 有序、不相交；属于 guest ABI，快照记录、恢复照搬
}
pub fn create(plan: &MemoryPlan) -> Result<SharedGuestMemory>;
pub fn describe(memory: &GuestMemoryMmap) -> Option<MemoryPlan>;   // 实际映射出的 plan

pub struct KvmVm {
    fd: VmFd,                        // 先声明，先关闭
    memory: SharedGuestMemory,
}
impl KvmVm {
    pub fn new(context: &KvmContext, memory: SharedGuestMemory) -> Result<Arc<Self>>; // TSS、irqchip、PIT2、memslot
    pub fn create_vcpu(self: &Arc<Self>, index: u8) -> Result<KvmVcpu>;
}
pub struct KvmVcpu { fd: VcpuFd, vm: Arc<KvmVm> }   // Deref<Target = VcpuFd>；try_clone 给踢出用
```

- **后端**：
  - `Anonymous`：`MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE`。
  - `Shared`：每段一个 memfd（`MFD_CLOEXEC | MFD_ALLOW_SEALING`，内核支持时加 `MFD_NOEXEC_SEAL`，6.3 之前的内核拒绝该标志则去掉重试），封印 `SHRINK | GROW | SEAL`，以 `MAP_SHARED | MAP_NORESERVE` 映射；fd 经 region 的 `FileOffset` 取得。
- **所有权**："映射只在 VM fd 与全部 vCPU fd 关闭之后解除"由类型保证，而不是靠拆除代码的顺序。KVM 在任一 vCPU fd 打开时保持 VM 存活，所以每个 `KvmVcpu`（含复制的 fd）持有 `Arc<KvmVm>`；`KvmVm` 内 fd 先于 memory 声明。两处 unsafe（`set_user_memory_region`、`create_vcpu_from_rawfd`）只出现在 `machine/vm.rs`。
- **快照与恢复**：快照持有 memory 的另一个 `Arc`。`rebuild` 先用 `describe` 核对快照中的内存与 `plan.memory` 一致，再交给新的 `KvmVm`。

## 7. CPUID、MSR、寄存器

### 7.1 CPUID 归一化（B6）

纯函数 `normalize(base, host, topology, index) -> CpuidEntries`。所有 vCPU 共用同一份基线，逐个 vCPU 计算。下表中 N 为 vCPU 数，`W = ceil(log2(N))`（N=1 时 W=0）。

**通用**：

| 叶            | 修改                                                                                     |
| ------------- | ---------------------------------------------------------------------------------------- |
| `0x0`         | 厂商字符串取宿主值                                                                        |
| `0x1`         | EBX[15:8]=8（CLFLUSH 64 B）；EBX[23:16]=N；EBX[31:24]=APIC ID                             |
|               | ECX[31]=1（hypervisor）；ECX[24]=1（TSC-deadline）；ECX[15]=0（PDCM）；EDX[28]=(N>1)（HTT） |
| `0xB`         | 子叶 0（SMT 层）：EAX=0，EBX=1，ECX=`0x100`                                                |
|               | 子叶 1（Core 层）：EAX=W，EBX=N，ECX=`0x201`；各子叶 EDX=x2APIC ID |
|               | 新内核的 KVM 不再返回子叶 1，缺失时补上                                                    |
| `0x8000_0005` / `0x8000_0006` | 透传宿主值；`0x8000_0006` 的 EDX[17:16] 清零                               |

**Intel**：

| 叶      | 修改                                                     |
| ------- | -------------------------------------------------------- |
| `0x4`   | EAX[25:14]：L1/L2 为 0，L3 为 N−1；EAX[31:26]=min(N−1,63)，完整拓扑由 0xB/0x1F 提供 |
| `0x6`   | 清 Turbo（EAX[1]）、EPB（ECX[3]）                         |
| `0x7.0` | EBX[6]、EBX[13] 置 1；清 WAITPKG（ECX[5]）                |
| `0xA`   | 全部清零（不暴露 vPMU）                                   |
| `0x1F`  | 宿主有此叶时复制 `0xB`                                    |

**AMD**：

| 叶            | 修改                                                         |
| ------------- | ------------------------------------------------------------ |
| `0x7.0`       | 清 EDX[29]（AMD 没有 `IA32_ARCH_CAPABILITIES`，KVM 却总是置位） |
| `0x8000_0001` | ECX[22]=1（TopologyExtensions）                               |
| `0x8000_0008` | ECX[15:12]=W（ApicIdSize）；ECX[7:0]=N−1                      |
| `0x8000_001D` | 透传宿主；L1/L2 NumSharingCache=0，L3 为 N−1                  |
| `0x8000_001E` | 透传后改写：EAX=APIC ID；EBX[7:0]=core id（= vCPU 序号），EBX[15:8]=0；ECX[10:0]=0（单节点） |

品牌字符串保持 KVM 给出的宿主值（firecracker 会改写成通用字符串，这里不做）。

### 7.2 boot MSR（B8，所有 vCPU）

| MSR                                                                                      | 值                         |
| ---------------------------------------------------------------------------------------- | -------------------------- |
| `SYSENTER_CS/ESP/EIP`、`STAR`、`CSTAR`、`KERNEL_GS_BASE`、`SYSCALL_MASK`、`LSTAR`、`TSC` | 0                          |
| `IA32_MISC_ENABLE`                                                                       | `FAST_STRING`（bit 0）     |
| `MTRRdefType`                                                                            | `(1 << 11) \| 6`：启用 MTRR，默认 WB |

### 7.3 PVH 入口状态（仅 BSP）

| 状态   | 值                                                                                     |
| ------ | -------------------------------------------------------------------------------------- |
| regs   | `rip` = PVH 入口，`rbx` = `0x6000`，`rflags` = `0x2`，其余为 0                           |
| GDT    | `@0x500`：null；code `0xc09b`；data `0xc093`（均为 base 0、limit 4 GiB）；tss `0x008b`（limit `0x67`） |
| 段     | `cs`=`0x08`；`ds/es/fs/gs/ss`=`0x10`；`tr`=`0x18`                                       |
| IDT    | `@0x520`，limit 7                                                                      |
| 控制寄存器 | `cr0` = PE \| ET；`cr4` = 0；`efer` = 0                                              |
| FPU    | `fcw` = `0x37f`，`mxcsr` = `0x1f80`                                                     |
| LAPIC（所有 vCPU） | LVT0 = ExtINT，LVT1 = NMI                                                   |

注意 GDT 的 limit 字段只有 20 位：`G=1` 时写入 `kvm_segment` 的 limit 必须按 4 KiB 粒度换算成 `0xffff_ffff`。否则 32 位入口只能访问 1 MiB 以内的内存。

## 8. 设备模型

### 8.1 Bus（C1）

```rust
pub trait BusDevice: Send + Sync {
    fn read(&self, offset: u64, data: &mut [u8]);
    fn write(&self, offset: u64, data: &[u8]);
}

/// 按起始地址排序、互不重叠；构建后只读。
pub struct Bus { entries: Box<[(Range<u64>, Arc<dyn BusDevice>)]> }

impl Bus {
    pub fn new(entries: Vec<(Range<u64>, Arc<dyn BusDevice>)>) -> Result<Self, Overlap>;
    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool;   // false = 未命中
    pub fn write(&self, addr: u64, data: &[u8]) -> bool;
}
```

- 接口是 `&self`，锁由设备自己决定：串口锁住 `Serial`；transport 锁住寄存器状态；i8042 用一个小 `Mutex`。
- 重叠检查在 `Bus::new` 中完成，属于构建期错误，运行期不再检查。

### 8.2 串口与控制台（C2–C4）

```text
vcpu-N ──PIO──▶ Mutex<Serial<IrqTrigger, FifoEvents, ConsoleOut>>
                    │ THR 写入                       ▲ enqueue_raw_bytes
                    ▼                                │
               ConsoleOut（有界字节缓冲 + Notify）   输入任务（POC 的 SerialDevice::run）
                    │                                ▲
                    ▼                                │
               输出任务 ──▶ stdout                  stdin（重新打开 + AsyncFd）
```

- **输入任务**：沿用 POC 的背压逻辑——FIFO 满时不读宿主输入，`in_buffer_empty` 回调经 `Notify` 唤醒任务。
- **`ConsoleOut`** 实现 `io::Write`：
  - `write` 加锁追加到环形缓冲（例如 64 KiB），满了就丢弃并计数，然后 `notify_one`，永不阻塞；
  - 输出任务被唤醒后取走全部字节写到 stdout：tty/pipe 走 `AsyncFd`，普通文件直接写。
- **raw mode**：stdin 是 tty 时，在 `Start` 中切换为 raw mode，由 guard 在拆除时恢复；panic hook 已经会恢复 canonical mode。
  - raw mode 下 Ctrl-C 交给 guest；要停止 VMM，用 API 或 SIGTERM。
- **非交互场景**：stdin 不是 tty/pipe（例如 `/dev/null`，epoll 不支持）时不接入输入，只保留输出。

### 8.3 i8042（C6）

- 状态与 firecracker 相同：`status`、`control`（CTR）、`outp`，以及一字节输出缓冲。
- 支持的命令：`0x20` 读 CTR、`0x60` 写 CTR、`0xD0` 读输出口、`0xD1` 写输出口、`0xFE` CPU 复位，其余忽略。
  这足以让 Linux 的 i8042 驱动快速完成探测（配合 A8 的 `i8042.*` 参数）。
- `0xFE` → `events.send(MachineEvent::GuestReset)`。设备本身不决定后果，由主循环拆除机器（E7）。
- 寄存器语义写成纯函数 `fn write(state, port, byte) -> (state, Option<I8042Effect>)`，外层只负责加锁和发事件。

### 8.4 virtio-mmio transport（C8）

**纯状态机**：

```rust
struct MmioRegs {
    status: u32,
    features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    queues: Vec<QueueRegs>,      // size / ready / desc / avail / used
}

/// 设备提供给 guest 的描述：类型、特性、各队列最大长度、配置空间。
struct DeviceDesc { device_type: u32, features: u64, queue_max: Vec<u16>, config: Vec<u8> }

fn read(regs: &MmioRegs, desc: &DeviceDesc, irq_status: u32, offset: u64) -> u32;
fn write(regs: MmioRegs, desc: &DeviceDesc, offset: u64, value: u32) -> (MmioRegs, Option<Transition>);

enum Transition { Activate, Reset, AckInterrupt(u32), Notify(u16) }
```

- **状态推进**：按 virtio 1.x §3.1.1 进行：`ACKNOWLEDGE → DRIVER → FEATURES_OK → DRIVER_OK`。
  - driver 特性不是 device 特性的子集时，不置 `FEATURES_OK`；
  - 非法转移打日志，状态不变；写 0 产生 `Reset`。
- 设备特性统一加上 `VIRTIO_F_VERSION_1`（bit 32）。
- `Notify` 只是兜底：正常情况下，通知寄存器（`0x50`）的写入被 ioeventfd 截获，不会走到用户态。

**外层 `MmioTransport`**（实现 `BusDevice`，运行在 vCPU 线程上）：

- **`Activate`**：由 `QueueRegs` 构造 `virtio_queue::Queue`，用 `is_valid(mem)` 校验，然后经 `Activator` 发送：

  ```rust
  struct Activation {
      mem: SharedGuestMemory,
      queues: Vec<Queue>,
      interrupt: Arc<IrqTrigger>,
      driver_features: u64,
      session: Session,      // 本次激活的会话：结束信号 + 结束确认
  }
  ```

  校验失败时置 `DEVICE_NEEDS_RESET`，并触发 config 中断。
- **`Reset`**（已激活时）：结束 `Session`，同步等待设备任务确认"已停止访问队列"（带超时）。拿到确认后才清空寄存器、返回 guest。
  §2 的推论保证这里不会死锁。
- **设备任务的形状**：

  ```rust
  loop {
      let activation = select! { stop => return, a = activator.recv() => a? };
      serve(activation).await?;   // serve 在 Session 结束、Stop 或出错时返回
  }
  ```

  一个设备因此可以被 guest 反复 reset 与重新激活，"未激活"与"已激活"只是这个循环停在哪一行。
- **注册**：
  - 每个队列一个 `KVM_IOEVENTFD`：地址 `base + 0x50`，datamatch = 队列号；
  - 一个 `KVM_IRQFD` 对应设备的 GSI；
  - 中断状态 `0x60` 读自 `IrqTrigger` 的原子位，`0x64` 写入时清位。
- **cmdline**：经 linux-loader 的 `add_virtio_mmio_device` 追加 `virtio_mmio.device=4K@0x<base>:<gsi>`。
- **快照与恢复**：

  ```rust
  pub struct MmioTransportSnapshot { pub desc: DeviceDesc, pub regs: MmioRegs, pub interrupt_status: u32 }

  impl MmioTransport {
      pub fn snapshot(&self) -> MmioTransportSnapshot;
      /// 后端现在提供的 desc 与快照不同（例如磁盘大小变了）时拒绝。
      pub fn restore(desc, memory, activator, interrupt, queue_notifiers, snapshot) -> Result<Self>;
      /// 恢复一个已激活的会话，使 guest 的 reset 能到达它。
      pub fn inject_session(&self, control: SessionControl);
  }
  ```

  快照记录 desc，是因为 guest 驱动在探测时已经读过它并依赖它：restore 不在驱动脚下改变设备。

### 8.5 virtio-blk（C9、C11）

- **配置空间**：`capacity`（le64，单位为 512 B 扇区）。镜像大小不是 512 的整数倍时，向下取整并告警。
- **特性**：`VERSION_1`、`FLUSH`（bit 9）；只读盘加 `RO`（bit 5）。队列长度上限 256。
- **请求**：沿用 POC 的处理（IN / OUT / FLUSH / GET_ID、越界、方向错误、只读、不支持的类型）；每个错误都写入状态字节，不影响设备本身。
- **根盘**：`root = true` 的盘在 plan 中排第一，因而是 `/dev/vda`，并追加 `root=/dev/vda rw|ro`。至多一个根盘。
- **id**：`AddDisk` 未给 id 时，`apply` 一次性规范化为 `disk{n}`，之后各阶段不再处理缺省；id 在全部设备中唯一，快照按 id 对应设备状态。
- **构造拆分**：POC 的 `BlockDevice::new` 在构造时创建 `AsyncEventFd`，要求调用方处在 reactor 中。改为两步：
  1. vmm-main 上创建纯资源（`Send`）：打开的 `File`、队列 `EventFd`、`Arc<IrqTrigger>`、`ActivationRx`；
  2. 工厂闭包在目标线程上把 `EventFd` 包成 `AsyncEventFd`，再运行设备循环。

### 8.6 io 线程与设备放置（D3）

```rust
pub enum Placement { VmmMain, Io }

pub enum DeviceRole {
    GuestAffecting,   // 串口输入、IRQ 重采样：guest 可观察，由 gate 冻结
    HostOnly,         // 不经 gate：控制台输出排空（仅宿主可见），以及自己协作 quiesce 的块设备后端（§8.8）
}

impl DeviceManager {
    pub fn spawn<F>(&mut self, name: String, role: DeviceRole, device: F)
    where F: Future<Output = Result<()>> + 'static;

    pub fn spawn_on<F, Fut>(&mut self, name: String, placement: Placement, role: DeviceRole, factory: F) -> Result<()>
    where
        F: FnOnce(Stop) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + 'static;
}
```

- io 线程内部是一个 `LocalRuntime`，循环接收 `Box<dyn FnOnce() + Send>` 任务，在本线程上调用 `spawn_local(factory(stop))`。
- `Stop`（`watch::Receiver`）与 `DeviceExit` 发送端都是 `Send`，生命周期控制原样适用于 io 线程上的设备。
- io 线程在设备全部退出后结束，由 `DeviceManager` 在拆除时 join。
- **原则**：KVM 与 vCPU 需要的那一半（transport、ioeventfd、irqfd）总是在 vmm-main 上建好再注册；目标线程只拿到设备侧的一半。

### 8.7 设备生命周期：quiesce / resume / stop

`DeviceManager` 把设备任务的生命周期拆为三个操作，对应暂停（可恢复冻结）、恢复、和最终终止：

**DeviceRole 分类**：

| 角色              | 说明                              | quiesce 行为       | stop 行为          |
| ----------------- | --------------------------------- | ------------------ | ------------------ |
| `GuestAffecting`  | 串口输入、IRQ resample            | gate 冻结 future   | 终止               |
| `HostOnly`        | 控制台输出排空；块设备后端        | 不经 gate（块设备由 `Machine` 协作 quiesce） | 终止 |

**gate 机制**：`GuestAffecting` 设备在 spawn 时被包裹在一个 supervisor 函数 `gated(gate_rx, device)` 中。gate 是一个 biased `select!` 循环，不修改原 future：

```text
Running 态（外层循环）：biased select { gate 命令, &mut device future }
  Quiesce → 回复 ack，进入 Quiesced 态
  Resume  → 回复 ack（已在运行，幂等）
  Stop    → 退出 gate 循环，return device.await（future 看到 stop watch）
  device 完成 → 直接返回结果

Quiesced 态（内层循环）：只 recv gate 命令
  Resume  → 回复 ack，break 回 Running 态
  Stop    → 退出 gate 循环，return device.await
  Quiesce → 回复 ack（幂等）
```

gate 是纯 supervisor 层控制，不修改也不替换原始 future；quiesce 只是停止对 device future 的 poll，future 内部的 async 栈与 `!Send` 状态完好保留，resume 后从冻结点继续。

**三个操作**：

1. **`quiesce()`**：向所有 `GuestAffecting` 的 gate channel 发 `Quiesce`，并发 `.await` 全部 ack。已退出的设备（gate channel 已断开）静默跳过。`HostOnly` 设备不受影响。
2. **`resume()`**：向所有 gate channel 发 `Resume`，并发 `.await` 全部 ack。已在运行的 gate 幂等回复。
3. **`stop()`**：设置 stop watch（`send_replace(true)`），向所有 guest gate 发 `Stop`（解冻 quiesced 的 future），`.await` 全部 `DeviceExit`（含 io 线程上的），然后 join io 线程。从 Running 和 Quiesced 态均可调用。

**与 Machine 的配合**：

- `Machine::pause()`：`vcpus.pause()`（不再产生新的 I/O 退出）→ 每个已挂设备协作 quiesce 并记录自己的状态（块设备：`BlockQuiesced`）→ `tasks.quiesce()`（gate 类任务）。
- `Machine::resume()`：`tasks.resume()` → 设备 resume（清除记录的状态）→ `vcpus.resume()`（guest 可以立即发起 I/O）。
- `Machine::teardown()`：`vcpus.stop()` → `tasks.stop()` → drop；内存何时释放由所有权决定（§6.7）。

**按 kind 纵切**：每类设备一个 `machine/<kind>.rs`（目前是 `block.rs`；i8042 与 COM1 合在 `platform.rs`），负责该类设备的 plan、attach（新建或恢复）、quiesce、capture。`DeviceSpec`、`DevicePlan`、`AttachedDevice`、`DeviceSnapshot` 都是按 kind 的 enum：新增一类设备即各加一个变体，由穷举 match 让编译器指出所有要补的地方。

### 8.8 状态可提取性：capture / rebuild

**MachineSnapshot**：一次 capture 的全部内容。字段都是普通数据，不加 serde derive（同进程 round-trip 不需要序列化，见 G6），不持有任何内核对象。

```rust
pub struct MachineSnapshot {
    pub plan: MachinePlan,              // 布局与 grant；rebuild 只需要快照，不再需要 spec
    pub vm: VmKvmState,                 // irqchip×3（PIC master / slave、IOAPIC）、PIT2、clock
    pub vcpus: Vec<VcpuSnapshot>,       // mp_state、regs、sregs、XSAVE、xcrs、debug_regs、lapic、CPUID、MSR、vcpu_events
    pub platform: PlatformSnapshot,     // i8042；COM1（console 为 None 时没有）
    pub devices: Vec<DeviceSnapshot>,   // plan 顺序；Block(BlockSnapshot { id, transport, queue: Option<QueueState> })
    pub memory: SharedGuestMemory,      // Arc 引用，同进程共享映射
}
```

**Capture 顺序**（机器必须已暂停）：

1. **设备**：每个设备在 pause 时已记录自己的状态。capture 拒绝仍在运行的设备（从未 pause 或已 resume：它的队列还在前进），也拒绝已退出的设备（没有状态可取，恢复它会掩盖故障）。设备排在 irqchip 之前：设备完成最后一个请求时发出的中断，应包含在随后读取的中断控制器状态中。
2. **平台**：i8042、COM1。
3. **vCPU**：`get_mp_state` 最先（`kvm_apic_accept_events` 有副作用），`get_vcpu_events` 最后。
4. **VM**：irqchip → PIT → clock。

**Rebuild 顺序**（只接收 `MachineSnapshot`）：

1. **纯校验，先于任何 KVM 对象**：`plan::replay` 校验 grant；`match_devices` 按 id 把设备状态对齐到 plan（缺失、重复、多余都报错）；vCPU 必须恰为 `0..cpus`；`memory::describe(&memory)` 必须等于 `plan.memory`。
2. `KvmVm::new`（TSS、irqchip、PIT2、memslot）→ `set_irqchip`×3 → `set_pit2`。
3. 每个 vCPU：`create_vcpu` + `restore_state`：`set_cpuid2` → `set_mp_state` → regs → sregs → xsave → xcrs → debug_regs → lapic → msrs → `vcpu_events`，最后 `kvmclock_ctrl`（失败只告警）。
4. `set_clock`。
5. `build::assemble`（与 build 共用）：恢复平台（i8042、COM1，重新接入控制台输入），在记录的 grant 上挂设备，最后启动 Paused 的 vCPU 线程。设备在 vCPU（LAPIC）恢复之后才挂上：恢复出来的设备可能立刻发中断，之后的 LAPIC 恢复会把它覆盖。挂块设备时：
   - transport 以快照状态恢复；后端的 `DeviceDesc` 与快照不同即拒绝（§8.4）；
   - 有 `QueueState` 的盘在 attach 时校验队列，注入会话，后端在 io 线程上以该队列重新激活；是否重新激活由后端记录的状态决定，而不是由 transport 的 status 推断；
   - transport 记录了未确认的中断（`interrupt_status != 0`）时，重新写一次 irqfd。KVM 异步注入 irqfd 写入，capture 读到的 irqchip 可能还不含它；漏掉的话，guest 会一直等一个已经在 used ring 里的完成。重复触发是无害的：level 中断在 remote IRR 置位时不会重复投递。

**Block 内部协作 quiesce**

block 不使用外部 `gated()` supervisor，而是在 `serve()` 的 `select!` 中处理 quiesce 请求：

- 设备用 `&mut` 直接调 `queue.state()` 导出 `QueueState`，经 oneshot 回复 `BlockQuiesced::{Inactive, Active(QueueState), Exited}`（`Exited`：任务已不在，由句柄侧得出）。未激活时，先处理已在途的激活，再响应 quiesce。
- 随后在 parked 循环中等待 resume，同一 session / queue / limiter 保持完好。
- `AttachedBlock.state: Option<BlockQuiesced>` 只在 pause 到下一次 resume 之间为 `Some`。已退出的后端记录为 `Exited`：pause 照常完成（局部故障不扩散），capture 拒绝。

**MSR 列表**

vCPU 快照保存与恢复的 MSR 共 17 项：

| 分组 | MSR |
| --- | --- |
| boot_msrs（11 项） | `SYSENTER_CS`、`SYSENTER_ESP`、`SYSENTER_EIP`、`STAR`、`CSTAR`、`KERNEL_GS_BASE`、`SYSCALL_MASK`、`LSTAR`、`TSC`、`IA32_MISC_ENABLE`、`MTRRdefType` |
| 额外（6 项） | `IA32_TSC`、`IA32_FS_BASE`、`IA32_GS_BASE`、`MSR_EFER`、`MSR_TSC_AUX`、`IA32_TSC_DEADLINE` |

## 9. API 与配置（E4、E5）

### 9.1 操作定义

```rust
define_schema! {
    VmmOp {
        instance_info::GetInstanceInfo -> instance_info::InstanceInfo { route: GET /v1/; }
        logger::UpdateLogger -> ApiResult<()> { route: PATCH /v1/logger; }

        machine::SetCpus -> ApiResult<()> {
            config: ".machine.cpus";
            route: PUT /v1/machine/cpus;
        }
        machine::SetMemory -> ApiResult<()> {
            config: ".machine.memory_mib";
            route: PUT /v1/machine/memory-mib;
        }
        machine::SetMemoryBacking -> ApiResult<()> {
            config: ".machine.memory_backing";
            route: PUT /v1/machine/memory-backing;
        }
        machine::SetConsole -> ApiResult<()> {
            config: ".machine.console";
            route: PUT /v1/machine/console;
        }
        boot::SetKernel -> ApiResult<()> {
            config: ".machine.kernel";
            route: PUT /v1/machine/kernel;
        }
        block::AddDisk -> ApiResult<()> {
            config: ".machine.disks[]";
            route: PUT /v1/disks;
        }

        lifecycle::Start -> ApiResult<()> { route: PUT /v1/actions/start; }
        lifecycle::Pause -> ApiResult<()> { route: PUT /v1/actions/pause; }
        lifecycle::Resume -> ApiResult<()> { route: PUT /v1/actions/resume; }
        lifecycle::Shutdown -> ApiResult<()> { route: PUT /v1/actions/shutdown; }

        live_upgrade::HandoverState -> ApiResult<live_upgrade::LiveUpgradeState> {
            route: POST /v1/live-upgrade/handover-state;
            fd_in: resp;
        }
    }
}
```

操作使用动词命名，承载的配置/值类型使用名词；操作层为 `#[serde(transparent)]`，不增加 JSON/TOML 嵌套，也不改变路由、操作名或字段。

| 操作类型 | 配置/值类型 | serde 形状 | 校验 |
| --- | --- | --- | --- |
| `SetCpus(VcpuCount)` | `VcpuCount(u8)` | 整数 | 1 ≤ n ≤ min(254, `KVM_CAP_MAX_VCPUS`) |
| `SetMemory(MemorySizeMib)` | `MemorySizeMib(u32)` | 整数，单位 MiB | 非零；放不下内核时在 `Start` 报错 |
| `SetMemoryBacking(MemoryBacking)` | `MemoryBacking::{Anonymous, Shared}` | `"anonymous"` / `"shared"` | —（§6.7） |
| `SetConsole(ConsoleConfig)` | `ConsoleConfig::{Stdio, None}` | `"stdio"` / `"none"` | — |
| `SetKernel(KernelConfig)` | `KernelConfig { path, cmdline, initrd }` | 原有对象 | cmdline 加上追加片段后 ≤ 2 KiB |
| `AddDisk(BlockDevConfig)` | `BlockDevConfig { id, path, read_only, root }` | 原有对象 | id 缺省为 `disk{n}`，在全部设备中唯一；至多一个根盘 |
| `UpdateLogger(LoggerConfig)` | `LoggerConfig { level, target, ... }` | 原有对象 | 缺省字段保留当前值 |
| `Start` 等 | 单元结构体 | HTTP body 为空 | 见 §4.3 |

- **默认值**：不配置时，`cpus` = 1、`memory_mib` = 128、`memory_backing` = `anonymous`、`console` = `Stdio`。没有 `machine.kernel` 时，`Start` 报错。
- **覆盖规则**：同一配置项重复设置时，后者覆盖前者（`apply` 是纯覆盖）；`AddDisk` 累加。
- `InstanceInfo.state` 沿用现有的 `VmState`：`NotStarted / Running / Paused`。

### 9.2 配置文件示例

```toml
[machine]
cpus = 2
memory_mib = 1024
memory_backing = "shared"                             # 可选，缺省 "anonymous"
console = "stdio"

[machine.kernel]
path = "/path/to/vmlinux"
cmdline = "console=ttyS0 reboot=k panic=1 pci=off"   # 可选，缺省为 A8 的默认值
initrd = "/path/to/initramfs.cpio.gz"                # 可选

[[machine.disks]]
id = "rootfs"
path = "/path/to/rootfs.ext4"
read_only = false
root = true
```

### 9.3 启动流程（main.rs）

1. 解析配置（已有），得到 `ProcessConfig` 与 `Vec<VmmCommand>`。
2. 启动 vmm 线程与 API server（已有）。
3. 依次对每个 `VmmCommand` 执行 `into_request()`、发送并等待回复；任一失败即报错退出（fail fast）。
4. 命令中含 `SetKernel` 时，再发送 `Start`。
5. 等待 vmm 线程结束（已有）。

## 10. 模块布局（F1）

```text
crates/vmm/src/
├── lib.rs                 start()、vmm_main 主循环
├── vmm_server.rs          请求入口：next_inbound()（Request / Detached）
├── process_ctx.rs
├── exit_status.rs
├── kvm_context.rs         /dev/kvm：API 版本、必需能力、HostCaps 投影
├── lifecycle.rs           Vm / Controller / Operation；handle_idle、handle_busy
├── spec.rs                MachineSpec、DeviceSpec：apply / validate（纯）
├── resources.rs           GpaRange、Gsi、AddressPool、IdPool、ResourceBook（纯，值类型）
├── arch/x86_64/           纯：无 fd、无 tokio
│   ├── layout.rs          地址常量、资源窗口与 GSI 范围、ram_regions()、resource_book()
│   ├── pvh.rs             start_info / memmap / modlist
│   ├── mptable.rs         MP table 字节与校验和
│   ├── cpuid.rs           CPUID 归一化（通用 / Intel / AMD）
│   ├── regs.rs            PVH regs / sregs / fpu、GDT 编码
│   ├── msr.rs             boot MSR
│   └── lapic.rs           LINT0 / LINT1
├── machine/               副作用侧（plan 为纯）
│   ├── plan.rs            MachinePlan、DevicePlan、VirtioMmioGrant；replay
│   ├── memory.rs          MemoryPlan；create / describe；memfd
│   ├── vm.rs              KvmVm（VM fd + memory + memslot）、KvmVcpu
│   ├── build.rs           build、assemble；Machine：pause / resume / teardown
│   ├── block.rs           块设备纵切：plan / attach / quiesce / capture
│   ├── platform.rs        i8042 + COM1：attach / capture
│   ├── snapshot.rs        MachineSnapshot、capture、rebuild
│   ├── vcpu.rs            VcpuRunner / VcpuHandle / 踢出；capture_state / restore_state
│   └── bus.rs             不可变 Bus
└── devices/
    ├── mod.rs             DeviceManager：spawn / spawn_on，quiesce / resume / stop
    ├── io_thread.rs       io 线程
    ├── async_fd.rs
    ├── rate_limiter.rs    （本阶段不接线）
    ├── legacy/            serial.rs、console.rs、i8042.rs
    └── virtio/            mod.rs（Activation、Session）、interrupt.rs、block.rs、mmio.rs
```

主要依赖方向：`resources` ← `arch` ← `spec` / `machine` ← `lifecycle` ← `lib`。`devices` 只引用 `machine` 中的边界类型 `BusDevice`、`MachineEvent`，不访问机器构建、KVM 或 vCPU 管理。transport 通过 `BusDevice` 接入 Bus。

## 11. 测试（F4–F6）

| 层次           | 内容                                                                                       | 运行条件      |
| -------------- | ------------------------------------------------------------------------------------------ | ------------- |
| 纯函数         | `ram_regions`、memmap、MP table（结构与校验和）、CPUID（每张表逐项断言）、GDT 编码、寄存器、MMIO 状态机（合法与非法转移、特性协商、reset）、i8042 状态机、cmdline 拼接、`MachineSpec` 校验；资源池（分配、对齐、reserve 冲突、release）、plan（根盘排序、grant、`replay`）、快照设备按 id 对齐 | 任意          |
| 设备（已有形式） | 沿用 `tests/block.rs`、`tests/serial.rs`：测试扮演 guest 驱动；改为经 transport 激活，覆盖 reset 后重新激活、io 线程放置、quiesce / resume | 任意          |
| 内存           | 两种后端的分段映射与读写；memfd 能看到 guest 写入、拒绝改变大小；guest memory 比 VM 与 vCPU 活得久 | 任意 / `/dev/kvm` |
| KVM 集成       | mini PVH ELF：32 位代码先检查 `[ebx]` 是否等于 start_info magic，再向 COM1 输出 `OK`，最后 `out 0x64, 0xFE`；断言输出与 `GuestReset`。另测 pause/resume、Shutdown 立即确认及后续拆除、配置文件选择 shared 内存后启动 | `/dev/kvm`    |
| 快照（`crates/vmm/tests/snapshot.rs`） | 寄存器递增 guest 的 round-trip（两种后端）；plan 顺序与逐盘激活在 rebuild 后保持；修改快照中的 grant 会移动设备；已退出的盘不阻碍 pause 但被 capture 拒绝；运行中的盘被拒绝；磁盘大小变化时拒绝恢复；capture 漏掉的中断在恢复时补发 | `/dev/kvm`    |
| 真实 Linux     | `#[ignore]`；`DBE_KERNEL`、`DBE_INITRD`、`DBE_ROOTFS`。断言串口出现约定标记，guest `reboot` 后 VMM 以 `Ok` 退出 | PVH 内核      |

## 12. 实施顺序

每一步结束时都能单独验证，并满足 F7。

| 步骤 | 内容                                                                          | 验证                         |
| ---- | ----------------------------------------------------------------------------- | ---------------------------- |
| M1   | `arch::x86_64` 全部纯模块                                                     | 纯函数单测                   |
| M2   | `KvmContext`、guest memory、irqchip/PIT、vCPU 线程与踢出、Bus                 | mini PVH：向调试端口写值、复位退出 |
| M3   | 串口、控制台、i8042                                                            | mini PVH 输出 `OK`           |
| M4   | virtio-mmio transport、virtio-blk 改造、io 线程                                | 状态机单测 + 设备测试        |
| M5   | `MachineSpec` / plan / build、生命周期、API 操作、main 自动启动、schema 导出   | 用你的 PVH 内核：initramfs → rootfs |
| M6   | 收尾：README / 文档、拆除路径与错误信息打磨                                    | `make lint`、`make test`     |
| L    | 生命周期控制：vCPU 完成屏障、设备 quiesce gate、`Controller` / `Operation`      | Controller 单测 + KVM pause 测试 |
| S    | 状态可提取：纯数据快照、块设备协作 quiesce、同进程 rebuild                      | 同进程 round-trip            |
| R1   | 资源账本：值类型的池，plan 记录 grant，restore 重放                             | 池与 plan 单测               |
| R2   | 设备按 kind 纵切；build 与 rebuild 共用 `assemble`；快照携带 plan               | grant 迁移、逐盘激活的 KVM 测试 |
| R3   | guest RAM 所有权：`KvmVm` / `KvmVcpu`                                          | 内存比 VM 与 vCPU 活得久     |
| R4   | memfd 共享后端 + `SetMemoryBacking`                                             | 两种后端的 round-trip 与启动 |
| R5   | 深度 review 修复                                                                | 新增回归测试（§11）          |

## 13. 已知限制与风险

- 无 ACPI（A5）：guest `poweroff` 只会停住 CPU，VMM 不退出；用 `reboot` 或 API `Shutdown`。
- MP table + xAPIC 限制 vCPU ≤ 254；拓扑固定。
- io 线程上的块 I/O 是同步的：同一 io 线程上的设备会互相阻塞（D4 / C12 可解决）。
- 只在 AMD 宿主上实际运行；Intel CPUID 分支仅有单测。
- SIGRTMIN 处理函数是进程级的：VMM 作为库嵌入时，与宿主程序共享这个信号。
- `Start` 在 vmm-main 上同步读取内核与 initrd 文件（D5 的例外）。
- 控制台 raw mode 下，Ctrl-C 不会终止 VMM。
- Busy 期间不处理机器事件（`MachineEvent`、`DeviceExit`）：操作 future 独占 Machine，events 和 device exits 的 receiver 在 future 内部不被 poll。操作完成后回到 Idle 时才会在下一轮 select 中处理积压的事件。
- 快照只在同进程内有效：没有序列化与格式版本，RAM 以 `Arc` 共享而不复制；跨进程恢复需要 dump（或交接 memfd）以及 dirty page tracking（G6）。
- 同一个快照可以在源机器仍存活时 rebuild，结果是两台 VM 共用一块 RAM。应在类型上排除：capture 消费 `Machine`，或 rebuild 要求源机器已拆除。
- "已暂停"没有类型表示：capture 依赖各设备记录的状态做运行时检查。
- rebuild 不核对 plan 的内部一致性（例如 cmdline 中的 virtio-mmio 片段与 grant 是否一致）。
- `Restore.devices` 与 plan 按位置对应（`match_devices` 已按 id 排好序），类型上没有绑定。
- VM 级状态的恢复顺序（irqchip 与 PIT 先于 vCPU，clock 最后）尚未与 Firecracker 逐项核对。
- 没有客户端的已暂停 VM 永远不会退出。
- `Start` 被 Shutdown 取消时以 `Vm::Running` 归还机器，只用于选择拆除路径；其 vCPU 可能从未运行。
- MSR 列表是硬编码的 17 项；长期应改为 `KVM_GET_MSR_INDEX_LIST` 动态发现。
- block quiesce 的 parked loop 中缺少 `session.requested()` 分支（延迟清理，非正确性问题）。
- rate limiter 的 `Instant` 在暂停期间继续流逝，resume 后第一个窗口可能不限流。
- 部分 KVM 测试仍用固定 sleep 让 guest 运行（round-trip 中的 50 ms）。

## 14. 实现与验证进度

- [x] M1–M5 代码已接通：纯架构模块、KVM/vCPU、串口与 i8042、virtio-mmio/blk、IO LocalRuntime、生命周期、API 与配置自动启动。
- [x] 在真实 KVM 上运行生成的 PVH ELF，验证串口、块设备读盘、initrd modlist、复位退出；另测双 vCPU 拆除、暂停/恢复及启动失败重试。
- [x] REST API 端到端测试：配置 → Start → Pause → Resume → Shutdown，验证 socket 清理。
- [x] 设备回归测试包含 reset 后重新激活、非法 avail 索引、错误对齐、超大/越界描述符、大请求有界缓冲，以及任务 panic 后正常 quiesce。
- [x] `make lint`、`make test`、`make api-features`，并导出与校验 OpenAPI / JSON Schema。
- [x] 真实 Linux guest：`/fuu.code/kangaroo-6.6/vmlinux`（Linux 6.6.102，PVH）完成双 vCPU、virtio-mmio block、ext4 root、systemd 与自动登录验证。rootfs 以 `ro,rootflags=noload` 挂载，未修改镜像；测试仍默认忽略，以环境变量显式运行。
- [x] vCPU 运行循环区分 Handled/Interrupted：MMIO/PIO 后直接再次 KVM_RUN，只在 EINTR/Intr 后处理控制命令。Pause ACK 保证 pending userspace I/O 已完成。KVM 回归测试验证阻塞式 MMIO read 期间的 Pause 正确等待 I/O 完成后再暂停。
- [x] DeviceManager 拆为三操作：quiesce（可恢复冻结 GuestAffecting future）、resume（解除冻结）、stop（最终终止）。DeviceRole 区分 GuestAffecting 与 HostOnly。gate 是 supervisor 层 biased-select，不修改原 future。
- [x] Machine::pause() 先 vcpus.pause() 再 devices.quiesce()；Machine::resume() 先 devices.resume() 再 vcpus.resume()；teardown 先 vcpus.stop() 再 devices.stop()。
- [x] 顶层 Controller::{Idle(Vm), Busy{stable, operation}} 替换原来的 Vm handle。Operation 持久化为本地 !Send Pin<Box<Future>>，独占 Machine。Busy 时 GET 返回前一稳定态，UpdateLogger 仍工作，其他写/生命周期返回错误。Shutdown 在 Busy 时通过 cancel oneshot 中断 Operation。Controller 单元测试覆盖 Busy 状态下的查询、拒绝、Shutdown 取消及客户端断开。
- [x] MachineSnapshot 纯数据容器：VmKvmState、VcpuSnapshot（XSAVE）、DeviceSnapshots、block QueueState 导出。
- [x] capture 函数：设备先 → vCPU → VM KVM 状态，带 Firecracker ordering 注释。
- [x] block device 内部协作 quiesce：serve() select! 中处理 quiesce 信号，用 &mut 直接提取 QueueState。
- [x] rebuild 函数：从 MachineSnapshot 重建 Machine，KVM 恢复顺序匹配 Firecracker，含 kvmclock_ctrl 和 XSAVE。
- [x] 同进程 round-trip 验证：寄存器递增 guest 经历 capture→teardown→rebuild→resume 后继续运行；block reactivation 从 captured QueueState 恢复。
- [x] 深度 review 修复：XSAVE 替代 FPU、kvmclock_ctrl、FS/GS BASE + EFER + TSC_AUX + TSC_DEADLINE MSR、transport session 注入。
- [x] 局部故障就地隔离（8b4b36d）：全部客户端断开不再终止已启动的 VM；已退出的块设备不阻碍 pause，capture 拒绝它；按盘记录 quiesce 状态。
- [x] 资源账本（6368bab）：`GpaRange` / `Gsi` 类型，值类型的 `AddressPool` / `IdPool` / `ResourceBook`；plan 经账本分配并记录 grant，restore 用 `replay` 重放校验。
- [x] 设备按 kind 纵切（3859e58）：`DeviceSpec` / `DevicePlan` / `AttachedDevice` / `DeviceSnapshot`；`machine/block.rs`、`machine/platform.rs`；build 与 rebuild 共用 `assemble`，先恢复 vCPU 再挂设备；快照携带 plan，rebuild 不再需要 spec；恢复时重新接入控制台输入。
- [x] guest RAM 所有权（ff79a8f）：`KvmVm` 拥有 VM fd 与 memory，`KvmVcpu` 持有 `Arc<KvmVm>`；memslot 从 `IdPool<MemSlot>` 分配；`MemoryPlan` 取代 `MachinePlan.ram`。
- [x] memfd 共享后端（f9e333f）：`SetMemoryBacking`（`.machine.memory_backing`、`PUT /v1/machine/memory-backing`）；sealed memfd；rebuild 核对内存与 plan 一致。
- [x] 第二轮深度 review 修复（f050e2b）：恢复时补发未确认的中断；快照记录 `DeviceDesc`，恢复时拒绝变化的后端；队列在 attach 时校验；capture 拒绝运行中的设备；断开后 `Start` 失败时 vmm-main 退出；memfd 加 `MFD_NOEXEC_SEAL`。

运行真实 Linux 冒烟测试：

```sh
DBE_KERNEL=/path/to/vmlinux \
DBE_INITRD=/path/to/initramfs.cpio.gz \
DBE_ROOTFS=/path/to/rootfs.ext4 \
cargo test -p dragonball-evolved --test boot linux_pvh_smoke_boot -- --ignored --nocapture
```

`DBE_INITRD`、`DBE_ROOTFS`、`DBE_CMDLINE` 可选；guest init 须输出 `DBE_LINUX_OK` 后执行 `reboot`，测试同时检查 Linux 启动日志、标记与正常退出。

实现中收紧的边界：

- guest RAM 至少 16 MiB，检查宿主物理地址宽度、memslot 与 IRQ 数量。
- guest 请求在任何磁盘修改前完成描述符方向、全部地址及磁盘范围校验；大描述符分块处理，staging 不超过 64 KiB。
- 激活 channel 容量为 1；reset 超时保留旧会话，拒绝在旧队列仍可能运行时重新激活。
- vCPU 线程显式解除继承的 SIGRTMIN 屏蔽，复制的 fd 保留 CLOEXEC；线程 join 放到 blocking pool。
- 控制台普通文件输出也交给 blocking pool；raw-mode guard 恢复原始 termios；停机时控制台最多尝试 flush 一秒，避免不可读终端阻止拆除。
- IO runtime 创建有一次启动握手；机器构建的 open/read/ioctl 仍属于 D5 的启动期例外。

## 附录 A：guest 内核要求

| 选项                                          | 用途                    |
| --------------------------------------------- | ----------------------- |
| `CONFIG_PVH=y`                                | PVH 入口（A1）          |
| `CONFIG_X86_MPPARSE=y`                        | MP table（A4）          |
| `CONFIG_SERIAL_8250=y`、`CONFIG_SERIAL_8250_CONSOLE=y` | 串口控制台     |
| `CONFIG_VIRTIO_MMIO=y`、`CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y` | virtio-mmio 设备经 cmdline 发现 |
| `CONFIG_VIRTIO_BLK=y`                         | 根盘                    |
| `CONFIG_KVM_GUEST=y`                          | kvmclock（墙钟、时钟源） |
| `CONFIG_BLK_DEV_INITRD=y`                     | initrd（A3）            |
| `CONFIG_EXT4_FS=y`、`CONFIG_DEVTMPFS=y`       | 常见 rootfs             |

`/fuu.code/kernel-5.10/.config` 中除 `CONFIG_PVH` 外均已开启。

## 附录 B：与参考实现的对照

| 主题        | firecracker                    | cloud-hypervisor            | 本设计                         |
| ----------- | ------------------------------ | --------------------------- | ------------------------------ |
| 启动协议    | PVH + Linux boot + bzImage     | PVH + 固件                  | 仅 PVH（A1）                   |
| 平台描述    | MP table + ACPI                | ACPI                        | MP table（A4）                 |
| 设备驱动    | event-manager 的 epoll 订阅者  | 每设备 epoll 线程           | async 任务（D1、D3）           |
| vCPU 踢出   | `immediate_exit` + SIGRTMIN    | SIGRTMIN + 轮询重发         | 同 firecracker（B10）          |
| transport   | virtio-mmio + virtio-pci       | virtio-pci                  | virtio-mmio（C8）              |
| 设备激活    | `activate_evt` eventfd         | 同步调用                    | channel + 会话（C8）           |
| 主循环状态  | `Arc<Mutex<Vmm>>`              | `Vmm` 结构体 + epoll 循环   | `Controller`：`Idle(Vm)` / `Busy(Operation)`（E1） |
| 资源分配    | 可变 `ResourceAllocator`；设备的 `MMIODeviceInfo`（地址、GSI）随快照保存，恢复时沿用 | 可变 `SystemAllocator` | 值类型账本；plan 记录 grant，恢复时 `replay`（§6.6） |
