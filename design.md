# dragonball-evolved VMM 设计：x86-64 / KVM / Linux guest

> 状态：待评审。第 1 节是决策清单，`[x]` 为建议项，请逐项调整勾选。每项有编号（如 `C6`），便于讨论。
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
  GSI 0–23 使用 KVM 默认路由，设备只需 `KVM_IRQFD`。
- [ ] **A7 split irqchip（用户态 IOAPIC）/ 显式 GSI 路由表 / MSI。** virtio-pci、VFIO 时才需要。
- [x] **A8 默认 cmdline**：`console=ttyS0 reboot=k panic=1 pci=off i8042.noaux i8042.nomux i8042.dumbkbd`。
  用户给出 cmdline 时整体替换默认值。VMM 只追加自己负责的片段：`virtio_mmio.device=…`，以及根盘的 `root=/dev/vda rw|ro`。

### B. 内存与 vCPU

- [x] **B1 guest RAM 为匿名私有 mmap**（`MAP_NORESERVE`），每段一个 KVM memslot。
- [x] **B2 布局含 32 位 MMIO 空洞 `[3 GiB, 4 GiB)`。** 超过 3 GiB 的 RAM 从 4 GiB 处继续。布局由纯函数 `ram_regions(size)` 给出。
- [ ] **B3 memfd / hugetlbfs backing。** vhost-user、快照、共享内存的前提；backing 必须在启动时决定，不能事后更换。
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
  状态是一个拥有所有权的值，在主循环中流转：`vm = vm.handle(event).await`。"该做什么"由对（状态，输入）的穷举 match 决定，副作用随后顺序执行。
- [ ] **E2 完整 reducer + `Effect` 枚举 + executor + 完成事件回流**（已删除的 vmm-architecture.md 方案）。
  更彻底，但在当前规模下样板较多；与 E1 二选一。
- [x] **E3 配置累积为纯数据**：`MachineSpec::apply(op) -> Result<MachineSpec>`；`Start` 时依次执行 `validate → plan → build`。
- [x] **E4 新增/补全 API 操作**（见 §9）：
  - 配置：`machine.cpus`、`machine.memory_mib`、`machine.console`、`machine.kernel`、`machine.disks[]`（补全 `AddDisk`）；
  - 生命周期：`Start / Pause / Resume / Shutdown`，均带 HTTP 路由；
  - `GetInstanceInfo` 反映真实状态。
- [x] **E5 自动启动**：配置文件含 `machine.kernel` 时，main 在依次下发全部配置命令后，自动发送 `Start`。
- [x] **E6 API `Shutdown` 为硬停**：停 vCPU → quiesce 设备 → 释放机器 → 回复。沿用现有语义：回复表示"已经关完"。
- [x] **E7 退出原因**：
  - guest 复位 → VMM 正常退出；
  - vCPU 内部错误 → VMM 以错误状态退出；
  - 设备任务失败只记录日志，不停机。
- [ ] **E8 真正的 reboot**（guest 复位后在同一进程内重建机器）。
- [ ] **E9 启动后修改配置 / 热插拔。** 不做时，启动后的配置类请求一律返回错误。
- [x] **E10 `HandoverState`（热升级）先返回 unsupported。**

### F. 工程与测试

- [x] **F1 代码全部放在 `crates/vmm` 内**：`arch::x86_64`（纯：无 fd、无 tokio）、`machine`（副作用）、`devices`。
- [ ] **F2 把纯 arch 代码拆成独立 crate。** 编译隔离更强，但现阶段边界仍在变化。
- [x] **F3 新依赖**：`kvm-ioctls 0.25`、`kvm-bindings 0.14`（`fam-wrappers`）、`linux-loader 0.14`（`elf`、`pvh`）。tokio 不新增特性：控制台走 `AsyncFd`（属于已启用的 `net` 特性）。
  不引入 `vm-allocator`：布局一次算出，不需要运行时分配器。
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
- [ ] **G6** 快照 / 迁移
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
| 请求      | main / api → vmm-main     | `mpsc::UnboundedSender<VmmRequest>`             | 已有，请求自带 `Reply`     |
| 机器事件  | vcpu-N、i8042 → vmm-main  | `mpsc::UnboundedSender<MachineEvent>`           | 非运行时线程可直接 `send`  |
| vCPU 控制 | vmm-main → vcpu-N         | `std::sync::mpsc` + `immediate_exit` + SIGRTMIN | 回复用 `oneshot`           |
| 设备激活  | transport（vcpu-N）→ 设备任务 | `Activator`（已有）                         | 每次激活附带会话结束信号   |
| 队列通知  | guest → 设备任务          | ioeventfd → `AsyncEventFd`（已有）              | 不经过 vCPU 用户态         |
| 中断      | 设备 → guest              | irqfd（`IrqTrigger`，已有）                     |                            |
| 设备退出  | 设备任务 → vmm-main       | `DeviceExit` channel（已有）                    | io 线程上的设备同样适用    |

**一条推论**：vmm-main 从不同步等待 vCPU 或 io 线程。因此 vCPU 线程可以在少数场合同步等待设备任务的确认（virtio reset，§8.4），不会形成死锁环。

## 3. 纯核与副作用的分界

```text
配置类 VmmCommand ──apply──▶ MachineSpec           纯：可比较、可序列化
MachineSpec + HostCaps ──validate──▶ ValidSpec     纯
ValidSpec ──plan──▶ MachinePlan                    纯：RAM 区段、设备地址与 GSI、cmdline
MachinePlan + 内核/initrd ──build──▶ Machine       副作用：ioctl、mmap、fd、线程、任务
```

- **`HostCaps`**：打开 `/dev/kvm` 时一次性取得的纯数据投影。包括：
  - API 版本、必需能力、`KVM_CAP_MAX_VCPUS`；
  - `KVM_GET_SUPPORTED_CPUID`；
  - 需要透传的宿主 CPUID 叶。

  `validate` 与 CPUID 归一化只依赖它，不碰 fd。
- **`MachinePlan`**：机器的全部布局决定，完全由 `ValidSpec` 推出，构建后不再变化。
- **启动数据**：cmdline、`hvm_start_info`、memmap、modlist、MP table、GDT/IDT 由纯函数生成为一组 `(GuestAddress, Vec<u8>)`，`build` 只负责逐条写入。
  内核本身由 linux-loader 直接加载到 guest memory，这是唯一不经过中间数据的写入。
- **vCPU 状态**：CPUID、MSR、`kvm_regs`、`kvm_sregs`、`kvm_fpu`、LAPIC 寄存器都由纯函数计算，`build` 只调用对应的 `KVM_SET_*`。
- **`build` 是直线代码**：按 plan 顺序执行副作用，不含决策逻辑。中途失败时，已创建的资源随 RAII 释放；状态保持 `NotStarted`，错误作为 `Start` 的回复返回。

## 4. 主循环与生命周期

### 4.1 状态与事件

```rust
enum Vm {
    NotStarted(MachineSpec),
    Running(Machine),
    Paused(Machine),
}

enum Event {
    Machine(MachineEvent),      // vCPU 退出、guest 复位、设备退出；NotStarted 时永不就绪
    Request(Option<VmmRequest>),
}

enum MachineEvent {
    GuestReset,                           // i8042 0xFE、三重故障
    VcpuFailed { index: u8, error: String },
    DeviceExit(DeviceExit),
}
```

`Machine` 拥有一次启动所需的全部运行时资源：`VmFd`、guest memory、两条 Bus、vCPU 句柄、`DeviceManager`（含 io 线程）、`MachineEvent` 接收端。

### 4.2 主循环

```rust
let mut vm = Vm::NotStarted(MachineSpec::default());
let (vm, status, reply) = loop {
    let event = select! {
        biased;
        Some(e) = vm.next_machine_event() => Event::Machine(e),
        request = server.next_request() => Event::Request(request),
    };
    match vm.handle(event, &ctx).await {
        (next, Step::Continue) => vm = next,
        (next, Step::Stop { status, reply }) => break (next, status, reply),
    }
};
vm.teardown().await;
if let Some(reply) = reply { reply.send(()) }
status
```

- 机器事件排在请求之前：guest 已经复位时，不再处理排队的请求。
- `handle` 消费旧状态、返回新状态，没有 `&mut` 共享。`select!` 的借用在得出 `Event` 时已经结束（沿用现有注释中的理由）。

### 4.3 请求 × 状态

| 请求                                       | NotStarted                             | Running            | Paused             |
| ------------------------------------------ | -------------------------------------- | ------------------ | ------------------ |
| 配置类（cpus / memory / console / kernel / disk） | `spec.apply(op)`                | 错误：已启动（E9） | 错误：已启动       |
| `Start`                                    | validate → plan → build → resume → Running | 错误           | 错误               |
| `Pause`                                    | 错误：未启动                           | 暂停 vCPU → Paused | 成功（幂等）       |
| `Resume`                                   | 错误：未启动                           | 成功（幂等）       | 恢复 vCPU → Running |
| `Shutdown`                                 | 退出                                   | 拆除后退出         | 拆除后退出         |
| `GetInstanceInfo`                          | 纯查询                                 | 纯查询             | 纯查询             |
| `UpdateLogger`                             | 进程级副作用，与 VM 状态无关            | 同左               | 同左               |
| `HandoverState`                            | unsupported（E10）                     | 同左               | 同左               |

不合法的组合直接以错误回复，状态不变。

### 4.4 机器事件

| 事件                 | 处理                                    |
| -------------------- | --------------------------------------- |
| `GuestReset`         | 拆除，`VmmExitStatus::Ok`（E7）         |
| `VcpuFailed`         | 拆除，`VmmExitStatus::Error`            |
| `DeviceExit`         | 记录日志，继续运行                      |

### 4.5 拆除顺序

不变量：任何还可能写 guest memory 的组件仍在运行时，不释放这块内存。

1. **停 vCPU**：向全部 vCPU 发 `Stop` 并踢出 → `.await` 每个线程的"已退出"通知 → `join`。`join` 此时立即返回，不阻塞 vmm-main。
2. **停设备**：广播 `Stop`（已有 `watch`）→ `.await` 全部 `DeviceExit`（含 io 线程上的）→ 结束 io 线程。
3. **drop `Machine`**：Bus 与 transport、ioeventfd/irqfd 注册、`VmFd`。
4. **释放 guest memory**：最后一个 `Arc` 在这里释放。
5. **回复 `Shutdown`**。

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
    fd: VcpuFd,
    pio: Arc<Bus>,
    mmio: Arc<Bus>,
    commands: std::sync::mpsc::Receiver<VcpuCommand>,
    events: mpsc::UnboundedSender<MachineEvent>,
}

// vmm-main 持有
struct VcpuHandle {
    kick_fd: VcpuFd,                 // dup 出的 fd，只用于写 immediate_exit
    thread: JoinHandle<()>,
    commands: std::sync::mpsc::Sender<VcpuCommand>,
    exited: oneshot::Receiver<()>,   // 线程返回前触发，用于异步 join
}

enum VcpuCommand {
    Pause(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    Stop,
}
```

- 线程名为 `vcpu-N`，以 Paused 状态启动：阻塞在 `commands.recv()`。
- 发送命令 = `send` + 踢出（B10）。`Vcpus::pause()` 并发等待全部回复。
- SIGRTMIN 的处理函数只做一个 `fence`，进程内经 `Once` 只安装一次，且在第一个 vCPU 线程启动前完成。
- 其他发往进程的信号（SIGINT、SIGTERM）也可能落到 vCPU 线程，使 `KVM_RUN` 返回 `EINTR`。处理方式与踢出相同：处理命令后继续运行。

### 5.3 运行循环

```text
Paused   ──Resume──▶ 回复，进入 Running
Paused   ──Pause───▶ 回复，保持 Paused
Paused   ──Stop────▶ 线程结束
Running  ──KVM_RUN 返回 EINTR──▶ 清 immediate_exit，try_recv 取出全部命令：
             Pause  → 回复，进入 Paused
             Resume → 回复，继续运行
             Stop   → 线程结束
             无命令 → 重新进入 KVM_RUN
```

| `KVM_RUN` 结果                         | 处理                                                        |
| -------------------------------------- | ----------------------------------------------------------- |
| `IoIn` / `IoOut`                       | PIO Bus。未命中时读返回 0、写忽略，打限速日志                |
| `MmioRead` / `MmioWrite`               | MMIO Bus，未命中处理同上                                     |
| `Shutdown`（三重故障）                 | 发 `GuestReset`，线程结束                                    |
| `SystemEvent(SHUTDOWN / RESET)`        | 同上                                                         |
| `FailEntry` / `InternalError` / 其他   | 发 `VcpuFailed`，线程结束（内核态 irqchip 下 `Hlt` 不应出现，归入此类） |
| `Err(EINTR)`                           | 见上图                                                       |
| `Err(EAGAIN)`                          | 重新进入                                                     |

**已知限制**：暂停时没有补完进行中的 MMIO/PIO（快照前需要带着 `immediate_exit` 再进入一次 `KVM_RUN`）。本阶段没有快照，不做。

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
| `0xc000_0000` 起            | virtio-mmio 设备，每个 4 KiB，按顺序分配                 |
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
| —               | virtio-mmio 设备，按顺序分配           | 5–23（最多 19 个） |

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

## 7. CPUID、MSR、寄存器

### 7.1 CPUID 归一化（B6）

纯函数 `normalize(base, host, topology, index) -> CpuidEntries`。所有 vCPU 共用同一份基线，逐个 vCPU 计算。下表中 N 为 vCPU 数，`P = next_power_of_two(N)`。

**通用**：

| 叶            | 修改                                                                                     |
| ------------- | ---------------------------------------------------------------------------------------- |
| `0x0`         | 厂商字符串取宿主值                                                                        |
| `0x1`         | EBX[15:8]=8（CLFLUSH 64 B）；EBX[23:16]=P；EBX[31:24]=APIC ID                             |
|               | ECX[31]=1（hypervisor）；ECX[24]=1（TSC-deadline）；ECX[15]=0（PDCM）；EDX[28]=(N>1)（HTT） |
| `0xB`         | 子叶 0（SMT 层）：EAX=0，EBX=1，ECX=`0x100`                                                |
|               | 子叶 1（Core 层）：EAX=log2(P)（与 `0x1` 的 EBX[23:16] 一致；firecracker 用固定的 log2(32)），EBX=N，ECX=`0x201`；各子叶 EDX=x2APIC ID |
|               | 新内核的 KVM 不再返回子叶 1，缺失时补上                                                    |
| `0x8000_0005` / `0x8000_0006` | 透传宿主值；`0x8000_0006` 的 EDX[17:16] 清零                               |

**Intel**：

| 叶      | 修改                                                     |
| ------- | -------------------------------------------------------- |
| `0x4`   | EAX[25:14]：L1/L2 为 0，L3 为 N−1；EAX[31:26]=N−1         |
| `0x6`   | 清 Turbo（EAX[1]）、EPB（ECX[3]）                         |
| `0x7.0` | EBX[6]、EBX[13] 置 1；清 WAITPKG（ECX[5]）                |
| `0xA`   | 全部清零（不暴露 vPMU）                                   |
| `0x1F`  | 宿主有此叶时复制 `0xB`                                    |

**AMD**：

| 叶            | 修改                                                         |
| ------------- | ------------------------------------------------------------ |
| `0x7.0`       | 清 EDX[29]（AMD 没有 `IA32_ARCH_CAPABILITIES`，KVM 却总是置位） |
| `0x8000_0001` | ECX[22]=1（TopologyExtensions）                               |
| `0x8000_0008` | ECX[15:12]=7（ApicIdSize）；ECX[7:0]=N−1                      |
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
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    queues: Vec<QueueRegs>,      // size / ready / desc / avail / used
    config_generation: u32,
}

/// 设备的静态描述：类型、特性、各队列最大长度、配置空间。
struct DeviceDesc { device_type: u32, features: u64, queue_max: Vec<u16>, config: Arc<[u8]> }

fn read(regs: &MmioRegs, dev: &DeviceDesc, irq_status: u32, offset: u64) -> u32;
fn write(regs: MmioRegs, dev: &DeviceDesc, offset: u64, value: u32) -> (MmioRegs, Option<Transition>);

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

### 8.5 virtio-blk（C9、C11）

- **配置空间**：`capacity`（le64，单位为 512 B 扇区）。镜像大小不是 512 的整数倍时，向下取整并告警。
- **特性**：`VERSION_1`、`FLUSH`（bit 9）；只读盘加 `RO`（bit 5）。队列长度上限 256。
- **请求**：沿用 POC 的处理（IN / OUT / FLUSH / GET_ID、越界、方向错误、只读、不支持的类型）；每个错误都写入状态字节，不影响设备本身。
- **根盘**：`root = true` 的盘排在设备列表第一位，因而是 `/dev/vda`，并追加 `root=/dev/vda rw|ro`。至多一个根盘。
- **构造拆分**：POC 的 `BlockDevice::new` 在构造时创建 `AsyncEventFd`，要求调用方处在 reactor 中。改为两步：
  1. vmm-main 上创建纯资源（`Send`）：打开的 `File`、队列 `EventFd`、`Arc<IrqTrigger>`、`ActivationRx`；
  2. 工厂闭包在目标线程上把 `EventFd` 包成 `AsyncEventFd`，再运行设备循环。

### 8.6 io 线程与设备放置（D3）

```rust
pub enum Placement { VmmMain, Io }

impl DeviceManager {
    pub fn spawn<F, Fut>(&mut self, name: String, placement: Placement, factory: F)
    where
        F: FnOnce(Stop) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + 'static;   // 设备 future 无需 Send
}
```

- io 线程内部是一个 `LocalRuntime`，循环接收 `Box<dyn FnOnce() + Send>` 任务，在本线程上调用 `spawn_local(factory(stop))`。
- `Stop`（`watch::Receiver`）与 `DeviceExit` 发送端都是 `Send`，现有的 quiesce 逻辑原样适用于 io 线程上的设备。
- io 线程在设备全部退出后结束，由 `DeviceManager` 在拆除时 join。
- **原则**：KVM 与 vCPU 需要的那一半（transport、ioeventfd、irqfd）总是在 vmm-main 上建好再注册；目标线程只拿到设备侧的一半。

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

| 请求类型      | 形状                                                                  | 校验                                  |
| ------------- | --------------------------------------------------------------------- | ------------------------------------- |
| `SetCpus`     | `u8` newtype（`serde(transparent)`）                                  | 1 ≤ n ≤ min(254, `KVM_CAP_MAX_VCPUS`) |
| `SetMemory`   | `u32` newtype，单位 MiB                                               | 非零；放不下内核时在 `Start` 报错      |
| `SetConsole`  | `enum { Stdio, None }`                                                | —                                     |
| `SetKernel`   | `{ path: PathBuf, cmdline: Option<String>, initrd: Option<PathBuf> }` | cmdline 加上追加片段后 ≤ 2 KiB         |
| `AddDisk`     | `{ id: Option<String>, path: PathBuf, read_only: bool, root: bool }`  | id 唯一（缺省为 `disk<N>`）；至多一个根盘 |
| `Start` 等    | 单元结构体，HTTP body 为空                                            | 见 §4.3                               |

- **默认值**：不配置时，`cpus` = 1、`memory_mib` = 128、`console` = `Stdio`。没有 `machine.kernel` 时，`Start` 报错。
- **覆盖规则**：同一配置项重复设置时，后者覆盖前者（`apply` 是纯覆盖）；`AddDisk` 累加。
- `InstanceInfo.state` 沿用现有的 `VmState`：`NotStarted / Running / Paused`。

### 9.2 配置文件示例

```toml
[machine]
cpus = 2
memory_mib = 1024
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
├── vmm_server.rs          （已有）
├── process_ctx.rs         （已有）
├── exit_status.rs         （已有）
├── kvm_context.rs         /dev/kvm：API 版本、必需能力、HostCaps 投影
├── lifecycle.rs           Vm 状态与 handle()；请求 × 状态的穷举分派
├── spec.rs                MachineSpec：apply / validate（纯）
├── arch/x86_64/           纯：无 fd、无 tokio
│   ├── layout.rs          地址常量、ram_regions()
│   ├── pvh.rs             start_info / memmap / modlist
│   ├── mptable.rs         MP table 字节与校验和
│   ├── cpuid.rs           CPUID 归一化（通用 / Intel / AMD）
│   ├── regs.rs            PVH regs / sregs / fpu、GDT 编码
│   ├── msr.rs             boot MSR
│   └── lapic.rs           LINT0 / LINT1
├── machine/               副作用侧
│   ├── plan.rs            MachinePlan：设备地址、GSI、cmdline（纯）
│   ├── build.rs           按 plan 物化 Machine
│   ├── memory.rs          guest memory 创建与 memslot 注册
│   ├── vcpu.rs            VcpuRunner / VcpuHandle / 踢出
│   └── bus.rs             不可变 Bus
└── devices/
    ├── mod.rs             DeviceManager（已有，增加 Placement）
    ├── io_thread.rs       io 线程
    ├── async_fd.rs        （已有）
    ├── rate_limiter.rs    （已有，本阶段不接线）
    ├── legacy/            serial.rs（已有）、console.rs、i8042.rs
    └── virtio/            mod.rs、interrupt.rs、block.rs（已有）、mmio.rs
```

依赖方向：`arch` ← `machine` ← `lifecycle` ← `lib`；`devices` 不依赖 `machine`。transport 通过 `BusDevice` 接入 Bus。

## 11. 测试（F4–F6）

| 层次           | 内容                                                                                       | 运行条件      |
| -------------- | ------------------------------------------------------------------------------------------ | ------------- |
| 纯函数         | `ram_regions`、memmap、MP table（结构与校验和）、CPUID（每张表逐项断言）、GDT 编码、寄存器、MMIO 状态机（合法与非法转移、特性协商、reset）、i8042 状态机、cmdline 拼接、`MachineSpec` 校验 | 任意          |
| 设备（已有形式） | 沿用 `tests/block.rs`、`tests/serial.rs`：测试扮演 guest 驱动；改为经 transport 激活，覆盖 reset 后重新激活、io 线程放置 | 任意          |
| KVM 集成       | mini PVH ELF：32 位代码先检查 `[ebx]` 是否等于 start_info magic，再向 COM1 输出 `OK`，最后 `out 0x64, 0xFE`；断言输出与 `GuestReset`。另测 pause/resume 与 `Shutdown` 的拆除顺序 | `/dev/kvm`    |
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

## 13. 已知限制与风险

- 无 ACPI（A5）：guest `poweroff` 只会停住 CPU，VMM 不退出；用 `reboot` 或 API `Shutdown`。
- MP table + xAPIC 限制 vCPU ≤ 254；拓扑固定。
- 暂停不补完进行中的 MMIO/PIO（§5.3），引入快照前必须补上。
- io 线程上的块 I/O 是同步的：同一 io 线程上的设备会互相阻塞（D4 / C12 可解决）。
- 只在 AMD 宿主上实际运行；Intel CPUID 分支仅有单测。
- SIGRTMIN 处理函数是进程级的：VMM 作为库嵌入时，与宿主程序共享这个信号。
- `Start` 在 vmm-main 上同步读取内核与 initrd 文件（D5 的例外）。
- 控制台 raw mode 下，Ctrl-C 不会终止 VMM。

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
| 主循环状态  | `Arc<Mutex<Vmm>>`              | `Vmm` 结构体 + epoll 循环   | 栈上的拥有所有权的状态值（E1） |
