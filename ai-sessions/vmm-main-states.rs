//! vmm-main 的状态模型（伪代码）
//!
//! 目标：为一个完整的 VMM——CPU 热插拔、ACPI 内存热插拔、virtio-mem、快照/恢复、
//! 热迁移、热升级、进程内重启——给出状态与数据结构，并据此重新整理当前
//! `main_loop.rs`、`main_loop/controller.rs`、`machine/*` 的实现。思路见同名 .md。
//!
//! 读法
//! - 重心在类型：每个类型回答"拥有什么、由谁产生、能变成什么"。函数只给签名，
//!   用来说明类型之间的转换关系。
//! - 三种东西要分清：
//!     「值」   可克隆、可比较、可进快照的纯数据（Spec、Plan、Topology、ExitFacts）；
//!     「所有者」持有 fd、线程、映射；不可克隆；drop 只请求停止，不证明任何事；
//!     「证明」 只能由某个操作产生的标记或借用（CpusStopped、Cut、RunAuthority）。
//! - `Op<T>`：vmm-main 上的本地 future，单一所有者、可 !Send，只被借用轮询、从不被取消。
//! - [现有] = 当前代码已有（可能改名、拆分或移动）；[新] = 新增；[改] = 语义变化。
//!
//! 章节
//!   0 身份                        1 纯值：Spec → Plan → Topology → Snapshot
//!   2 所有者与证明                3 操作：按"在等什么"分类
//!   4 运行权与会话                5 整机状态
//!   6 输入                        7 决策与外壳
//!   8 退出：事实的并集            9 与当前实现的对应

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;

/// 不可伪造：只有本模型中对应的操作能构造含它的值。
struct Private;

/// 本地 future：单一所有者、!Send、运行在 LocalRuntime。
pub type Op<T> = Pin<Box<dyn Future<Output = T>>>;

// ═══════════════════════════════════════════════════════════════════════════
// 0. 身份
// ═══════════════════════════════════════════════════════════════════════════
//
// 不同空间用不同类型。编号只用于寻址与对账，不携带任何权限。

/// [新] 可能 CPU 的序号。APIC ID 由 `CpuPlan` 映射。现在是 `u8` 的 vCPU 下标。
pub struct CpuIndex(u16);
/// [新] DSDT 中声明的内存设备槽。
pub struct DimmSlot(u8);
/// [现有] 规范化后的设备 id（`"disk0"`、`VIRTIO_MEM_ID`）。现在是裸 `String`。
pub struct DeviceId(String);
/// [现有 memory/layout.rs]
pub struct WindowId(u32);
/// [现有 memory/layout.rs]
pub struct ObjectId(u32);
/// [现有 memory/service.rs 的 MemoryOperationId，推广] 一次已接受、可能晚于回复才结束的
/// 宿主变更。用于重试授权（`retry(id)`）与回复对账，不构成历史表。
pub struct OperationId(u64);
/// [新] 一次导出或导入会话。
pub struct SessionId(u64);
/// [新] 运行权代次：每次授予递增。跨进程时用来拒绝过期的授予消息。
pub struct AuthorityEpoch(u64);

// ═══════════════════════════════════════════════════════════════════════════
// 1. 纯值：Spec → Plan → Topology → Snapshot
// ═══════════════════════════════════════════════════════════════════════════
//
//   MachineSpec ──plan(host)──▶ MachinePlan ──initial()──▶ Topology ──物化──▶ Parts（所有者）
//   意图，Absent 时可改          guest ABI，此后不变          资源事实，运行期可变
//
//   MachineSnapshot = Plan + Topology + 各组件在同一切面上的状态（不含 RAM 内容、不含宿主端点）
//
// 分工
// - Spec 只在 Absent 时累积；Start 之后不再需要（快照也不带它）。
// - Plan 决定 guest 能看到的一切"上限"：可能的 CPU、DIMM 槽与热插范围、冷插设备与 grant、
//   ACPI 静态表。热插拔永远不改 Plan，只改 Topology。
// - Topology 记录"现在有什么"：哪些 CPU / DIMM / 设备存在、各自拿到的地址。
//   分配结果作为数据记在这里；恢复与迁移时重放，不重新分配。
//
// 单一权威（与 design.new-mem §9.2 同一原则）
// - 资源事实归机器 → Topology。
// - 协议状态归设备 → 插入/移除待处理位、guest 是否确认，归 ACPI 热插控制器（只读投影
//   `HotplugStatus`）；virtio-mem 的 requested_size 归 virtio-mem 设备；Population 归内存服务
//   （只读投影 `WindowStatus`）。
// 所以"等 guest"不是 Topology 的状态，而是设备协议状态——它天然随设备快照保存与恢复。

/// [现有 spec.rs，扩展] 用户意图。只有 `apply` 产生新值，因此每个值都满足配置规则。
pub struct MachineSpec {
    cpus: CpuSpec,
    memory: MemorySpec,          // 启动 RAM 与 backing（Anonymous / Shared memfd）
    hotplug: HotplugSpec,
    devices: Vec<DeviceSpec>,    // [现有] Block、virtio-mem
    kernel: Option<KernelConfig>,
    console: ConsoleConfig,
}

/// [新] 现在只有一个 `cpus: u8`。`max` 决定 MADT 中列出多少个可能的 CPU。
pub struct CpuSpec {
    boot: u16,
    max: u16,                    // ≥ boot；= boot 表示不支持 CPU 热插
}

/// [新] 0 个槽表示不启用 ACPI 内存热插。virtio-mem 仍是 `DeviceSpec::Mem`。
pub struct HotplugSpec {
    dimm_slots: u8,
    dimm_range: u64,             // 热插 GPA 范围的总大小
}

/// [现有 plan.rs，扩展] guest ABI 与全部地址 grant。Start / Restore 之后不变，随快照保存。
/// 恢复时 replay 其中的 grant（现有 `plan::replay`），而不是重新分配。
pub struct MachinePlan {
    cpus: CpuPlan,
    memory: MemoryLayout,                    // [现有] 启动 RAM 与 virtio-mem window；不含 DIMM
    dimms: DimmPlan,
    devices: BTreeMap<DeviceId, DevicePlan>, // [现有] 冷插设备
    acpi: AcpiPlan,
    boot: BootPlan,                          // [现有 kernel / initrd / base_cmdline 散字段]
}

pub struct CpuPlan {
    possible: BTreeMap<CpuIndex, ApicId>,    // MADT 列出的全部 CPU
    boot: BTreeSet<CpuIndex>,                // 冷启动时在位的子集
}

pub struct DimmPlan {
    slots: u8,
    range: Range<Gpa>,                       // 整体是一个 grant；具体 DIMM 从中切分
    align: u64,
}

/// [新，依赖 A5] 由 CpuPlan / DimmPlan / devices 推导的静态表，以及热插控制器的寄存器窗口与 GSI。
pub struct AcpiPlan {
    hotplug_regs: Range<Pio>,
    hotplug_gsi: Gsi,
    tables: AcpiTables,                      // RSDP / XSDT / FADT / MADT / DSDT / SRAT
}

/// [新] 运行期资源事实。纯值：每次变更产生新值，版本递增。
///
/// 与其他类型的关系
/// - Plan：键集合与取值范围由 Plan 限定（`validate`）。
/// - Parts：Topology 是权威，Parts 是它的物化；两者不一致只能出现在一次 Change 的中途。
/// - 快照：原样进入 `MachineSnapshot`；恢复时据它物化 Parts。
/// - 复位（E8 reboot）：Topology 保留——热插进来的 CPU 和 DIMM 在复位后仍然存在。
pub struct Topology {
    version: u64,
    cpus: BTreeMap<CpuIndex, CpuPresence>,        // 键集合 == plan.cpus.possible
    dimms: BTreeMap<DimmSlot, DimmPresence>,      // 键集合 == 0..plan.dimms.slots
    devices: BTreeMap<DeviceId, DevicePresence>,  // E9 设备热插的位置；冷插设备恒为 Present
}

pub enum CpuPresence {
    /// 从未插入：没有 KVM vCPU。
    Absent,
    /// 有 vCPU fd 与线程，随机器 Running / Paused。guest 是否已 online 由 guest 决定，不在这里。
    Present,
    /// guest 弹出后：线程已结束，fd 保留。KVM 只有 KVM_CREATE_VCPU，没有销毁 vCPU 的接口，
    /// 所以再次插入时复用这个 fd（Cloud Hypervisor cpu.rs `remove_vcpu` 同样只结束线程）。
    Dormant,
}

pub enum DimmPresence {
    Empty,
    Present(Dimm),
    /// guest 已弹出，逻辑上已拔出；宿主回收尚未完成或已卡住。资源与地址仍被保留，
    /// 同一槽不能重新插入，直到回收提交（design.new-mem §10.4：逻辑拔出与物理回收分开）。
    Reclaiming(Dimm),
}

/// 一条 DIMM 的资源事实，即一次可重放的分配结果。
pub struct Dimm {
    window: WindowId,
    object: ObjectId,
    placement: Range<Gpa>,   // 从 DimmPlan.range 切出；空闲空间由已记录的 placement 推导，不另存分配器
    node: GuestNode,
    backing: BackingKind,    // 只有 Shared(memfd) 能在热升级中交接，Anonymous 只能复制
}

pub enum DevicePresence {
    Present,
    Removing,
    Absent,
}

// ── 拓扑变更：与内存服务的 PopulationProposal 同构——"新值 + 效果" ──

/// [新] 拓扑请求（API 动词）。
pub enum TopologyRequest {
    PlugCpu(CpuIndex),
    UnplugCpu(CpuIndex),
    PlugMemory(DimmConfig),
    UnplugMemory(DimmSlot),
}

/// [新] 热插控制器报告的 guest 动作，作为事实到达（§6）。
pub enum GuestHotplug {
    CpuEjected(CpuIndex),    // guest 对该 CPU 执行了 _EJ0
    DimmEjected(DimmSlot),
}

/// [新] 一次已验证、尚未生效的拓扑变更。提交前不改变任何事实；被拒绝时没有副作用。
pub struct TopologyProposal {
    next: Topology,
    effect: HostEffect,
}

/// [新] 宿主侧要做的事，按"进哪种槽"分成三类（§3）。类型本身就决定了外壳怎么执行它。
pub enum HostEffect {
    /// 改变 vCPU 集合：与 pause / resume 互斥，有界。→ Exec 迁移（Launch::PlugCpu / ReleaseCpu）
    Cpu(CpuEffect),
    /// 只涉及一个作用域、可能卡住。→ `Machine::begin` → Changes
    Scoped(ScopedEffect),
    /// 只改热插控制器的协议状态并发中断，请求 guest 弹出。同步完成，不占任何槽。
    /// 之后 guest 可能永远不弹出——这段等待只体现在设备协议状态里。
    Signal(EjectTarget),
}

pub enum CpuEffect {
    /// 建立或复用（Dormant）vCPU fd、配置、起线程（Running 时直接运行，在内核 LAPIC 里等 SIPI），
    /// 然后通知 guest 插入。线程应答有期限，超时则整机只能 teardown。
    Start(CpuIndex),
    /// 停线程、保留 fd → Dormant。
    Stop(CpuIndex),
}

pub enum ScopedEffect {
    /// virtio-mem：写 config 并发 Config 中断。worker 只在没有进行中的 Population 变更时发布，
    /// 而那个变更可能卡在 mlock 或租约上。（现有 `resize_memory` + `ResizeReply`）
    PublishTarget(WindowId, u64),
    /// 创建 backing、Window 与内存服务，绑定，加入 CPU / DMA 视图，然后通知 guest 插入。
    /// DMA 视图要等消费者确认，可能卡住。
    MaterializeDimm(DimmSlot, Dimm),
    /// 永久关闭 Window 准入、排空租约、解绑、撤销 DMA、归还地址。可能卡住，且不能因超时而释放。
    ReclaimDimm(DimmSlot, Dimm),
}

impl Topology {
    /// 冷启动拓扑：boot CPU 为 Present，其余 Absent；DIMM 全部 Empty。
    fn initial(plan: &MachinePlan) -> Topology;
    /// 纯函数：校验请求（槽位、上限、对齐、重复、协议状态）并给出提议。
    /// 读 `HotplugStatus` 是为了拒绝"已在请求弹出"之类的重复请求，不修改它。
    fn propose(
        &self,
        plan: &MachinePlan,
        protocol: &HotplugStatus,
        request: TopologyRequest,
    ) -> Result<TopologyProposal, Rejection>;
    /// 纯函数：guest 弹出之后需要的回收。
    fn on_guest(&self, event: GuestHotplug) -> Result<TopologyProposal, GuestProtocolError>;
    /// 恢复与接收时校验：键集合与 Plan 一致，placement 落在 DimmPlan.range 内且互不重叠。
    fn validate(&self, plan: &MachinePlan) -> Result<(), TopologyError>;
}

/// [现有 snapshot.rs，扩展] 同一切面上的整机状态，不含 RAM 内容与宿主端点。
pub struct MachineSnapshot {
    plan: MachinePlan,
    topology: Topology,                          // [新]
    vm: VmKvmState,                              // [现有]
    vcpus: BTreeMap<CpuIndex, VcpuSnapshot>,     // [改] 只含 Present 的 CPU；现在是按位置排列的 Vec
    platform: PlatformSnapshot,                  // [改] 增加 ACPI 热插控制器的协议状态
    devices: BTreeMap<DeviceId, DeviceSnapshot>, // [现有]
    memory: BTreeMap<WindowId, MemorySnapshot>,  // [现有] Population + revision；DIMM 的 window 也在内
}

/// [新，G6] 每个 Object 的内容来源：快照字节 / 显式零区 / 持久来源 / 交接的 memfd。
/// 见 design.new-mem §12.2。快照落盘与跨进程恢复需要它，同进程 rebuild 不需要。
pub struct ObjectContents {
    sources: BTreeMap<ObjectId, ContentSource>,
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. 所有者与证明
// ═══════════════════════════════════════════════════════════════════════════
//
// 所有者持有 fd、线程、映射。机器的执行状态用不同类型表示（typestate）：
// 每个迁移消耗旧值、产生新值，失败时产生"只能 teardown"的值，而不是回到出发状态。
//
//            pause                       resume(&RunAuthority)
//   Machine ───────▶ PausedMachine ─────────────────────────▶ Machine
//      │  失败            │  失败                                  ▲
//      ▼                  ▼                                       │ plug_cpu / release_cpu
//   FailedMachine ◀───────┘                                Machine┘（有界，失败同样 → Failed）
//      │
//      ▼ teardown（Machine / PausedMachine 也走同一条）
//   Ok(MemoryStore)  或  Err(PendingCleanup)  —— 到期不声称完成，继续持有

/// [现有 machine/state.rs Parts] 一台已构建机器的全部所有者。字段按 drop 顺序排列：
/// 先让 vCPU 停，再释放它们能触及的东西。
struct Parts {
    vcpus: Vcpus,                                 // [改] 按 CpuIndex 索引；Dormant 的只剩 fd
    runtime: DeviceRuntime,                       // [现有] 设备任务的监督者
    platform: Platform,                           // [改] 增加 ACPI 热插控制器
    devices: BTreeMap<DeviceId, AttachedDevice>,  // [改] 合并现在的 blocks / mems 两张表
    memory: BTreeMap<WindowId, MemoryOwner>,      // [现有 memory_services] 启动 RAM 之外每个 Window 一个服务
    bindings: Vec<Binding>,                       // [现有] 启动 RAM 的 memslot 绑定
    memslots: MemSlots,                           // [现有] 不可克隆的 slot 池
    vm: Arc<KvmVm>,
    plan: MachinePlan,
}

/// [现有] vCPU 在运行。
pub struct Machine {
    parts: Box<Parts>,
}

/// [现有] vCPU 已停；设备与内存服务按各自的应答 quiesce（应答可能是 Exited / TimedOut）。
/// "已暂停"不等于"可捕获"：后者是 `Cut`。
pub struct PausedMachine {
    parts: Box<Parts>,
    acks: QuiesceAcks,
}

/// [现有] CPU 迁移失败后的唯一所有者：CPU 状态未知。只有 teardown，没有 resume、没有 capture。
pub struct FailedMachine {
    parts: Box<Parts>,
}

/// [现有 cleanup.rs] 清理未能证明完成：继续持有全部未证实可释放的资源（分阶段：
/// Tasks → Unstarted → Vm）。drop 只请求停止，不证明清理完成，也不交还 RAM。
pub struct PendingCleanup(Stage);

/// [现有，收拢] 每个组件对 quiesce 的应答。现在散在 PausedMachine 的五个字段里。
pub struct QuiesceAcks {
    platform: PlatformQuiesced,
    devices: BTreeMap<DeviceId, Response<DeviceQuiesced>>,
    memory: BTreeMap<WindowId, Response<MemoryQuiesced>>,
}

// ── 证明：把"只有在 X 之后才能 Y"从注释变成参数 ──

/// [新] 全部在位 vCPU 已确认停在 Paused。只由 `Vcpus::pause` 成功产生。
/// `Machine::pause` 用它作为 quiesce 设备的前提（顺序：CPU → 设备 → 内存服务）。
pub struct CpusParked(Private);

/// [新] 全部 vCPU 线程已经结束，不会再进入 KVM_RUN。只由 `Vcpus::stop_wait` 成功产生。
/// 现在 `MemoryHandle::stop` / `stop_wait` 的文档写着 "Only after CPUs stop"——
/// 改为要求 `&CpusStopped` 参数，同时用它解除对应的 `RamWatch`（§6）。
pub struct CpusStopped(Private);

/// [新] 可捕获的切面：PausedMachine 的全部应答都是 Replied，且没有在途或卡住的作用域变更。
/// 现在由 `capture()` 内部逐项检查；提升为类型后，Dump / StopCopy / Handover 都只接受 Cut，
/// 检查只发生一次。借用 Topology，是因为快照必须与"此刻的资源事实"一致。
pub struct Cut<'m> {
    machine: &'m PausedMachine,
    topology: &'m Topology,
    _p: Private,
}

impl Machine {
    async fn pause(self) -> Result<PausedMachine, (FailedMachine, PauseError)>;
    /// 改变 vCPU 集合是 CPU 迁移：与 pause / resume 互斥，因此消耗整台机器。
    async fn plug_cpu(self, cpu: CpuIndex) -> Result<Machine, (FailedMachine, CpuError)>;
    async fn release_cpu(self, cpu: CpuIndex) -> Result<Machine, (FailedMachine, CpuError)>;
    /// 作用域变更的同步起点：借用 &mut 分配 slot permit、建映射与服务、向设备入队，
    /// 不等待。返回的工作不再借用 Machine——与现在的 `resize_memory(&mut self)` 同型。
    fn begin(&mut self, effect: ScopedEffect) -> Result<Stallable<ChangeOutcome>, Rejection>;
}

impl PausedMachine {
    /// 恢复执行需要运行权。Start 的首次运行同样走这里。
    async fn resume(self, authority: &RunAuthority) -> Result<Machine, (FailedMachine, ResumeError)>;
    /// 暂停时是否允许作用域变更由决策决定（初版沿用现状：ResizeMemory 在 Paused 时拒绝）；
    /// guest 弹出触发的 DIMM 回收在 Paused 时也可以开始，它不产生 guest 可见 I/O。
    fn begin(&mut self, effect: ScopedEffect) -> Result<Stallable<ChangeOutcome>, Rejection>;
    fn cut<'m>(&'m self, topology: &'m Topology, changes: &Changes) -> Result<Cut<'m>, NotCuttable>;
}

impl Cut<'_> {
    async fn capture(&self) -> Result<MachineSnapshot, CaptureError>;
}

/// 三种机器共用的拆除。到期不声称完成：返回 PendingCleanup，继续持有资源。
async fn teardown(parts: Box<Parts>) -> Result<MemoryStore, (PendingCleanup, TeardownError)>;

/// 运行权交出后的本地释放：停线程、关 fd、解除映射，但不对共享 backing 做任何破坏性操作
/// （discard、截断、清零）——新进程或目标端可能正在使用同一个 memfd。
async fn release_local(parts: Box<Parts>, _: &Relinquishment)
    -> Result<(), (PendingCleanup, TeardownError)>;

// ═══════════════════════════════════════════════════════════════════════════
// 3. 操作：按"在等什么"分类
// ═══════════════════════════════════════════════════════════════════════════
//
// 判据不是快慢，而是"超时能否有一个安全的含义"。
//
// | 类别        | 在等什么                        | 到期 / 中止的含义          | 放在哪                | Shutdown 时       |
// |-------------|---------------------------------|----------------------------|-----------------------|-------------------|
// | Bounded     | vCPU、设备、服务的应答          | 失败封闭：Failed / Pending | Exec::Moving（独占）  | 等它落地          |
// | Stallable   | 宿主内部依赖：租约、mlock、发布 | 没有安全含义，只能重试     | Changes（按作用域）   | 与 teardown 并行  |
// | （等 guest）| guest 的 _EJ0、online           | 没有安全含义               | 设备协议状态（数据）  | 直接 teardown     |
// | Abortable   | 对端、磁盘、网络                | 只丢弃会话，guest 不受影响 | Exec 中的 Export      | 先中止            |
// | 交出运行权  | ——                              | 不可逆                     | 消耗 RunAuthority     | ——                |
//
// 等 guest 的那一类刻意没有类型：它不是操作，只是设备里的一个协议位加上一个将来可能到达的事实。

/// [新，给现有约定命名] 构造者承诺：内部每个等待都有期限，到期结果失败封闭。
/// 现在这一承诺写在 machine/state.rs 的模块文档里（"every wait inside them is bounded"）。
pub struct Bounded<T>(Op<T>);

/// [新，推广现有 Busy::Resizing] 可能无限期卡在宿主内部依赖上。不持有 Machine，
/// 只持有本作用域的能力（WindowControl、设备回复的接收端……），所以机器可以同时暂停
/// 或 teardown——而 teardown 往往正是解开它的东西。暂停不会等它：设备在有变更未完成时
/// 对 quiesce 的应答会超时，于是这次暂停得不到 Cut，但暂停本身照样落地。
pub struct Stallable<T>(Op<T>);

/// [新] 对 guest 只读的长任务。持有中止句柄，Shutdown 与 Cancel 先结束它。
pub struct Abortable<T> {
    op: Op<T>,
    abort: AbortHandle,
}

// ── 3.1 Exec 迁移：独占、有界 ──

/// [新，取代 Busy::{Starting, Pausing, Resuming}] 唯一的 Exec 迁移槽。
/// 操作在尾部按结果回复请求方，然后交出一个 `Landed`。所有迁移共用这一个完成类型：
/// 控制器只需把落点装回 Exec——不再有"(状态, 完成) 交叉匹配 + unreachable!"。
pub struct Move {
    kind: MoveKind,         // 报告 VmState 与拒绝请求时用
    op: Bounded<Landed>,    // 拥有机器
}

pub enum MoveKind {
    Starting { spec: MachineSpec },  // 失败且已清理时据此回到 Absent(spec)
    Pausing,
    Resuming,
    PluggingCpu(CpuIndex),
    ReleasingCpu(CpuIndex),
    Capturing,                       // capture 借用切面；结果交给 Dump 或回复
    Rebooting,                       // E8：teardown 后按 plan + topology 在本进程内重建
}

/// [新] 有界迁移的落点。
pub enum Landed {
    Running(Machine),
    Paused(PausedMachine, Option<CutExport>),
    /// Start 失败，且已在操作内清理干净：可以重试。
    Vacant(Fault),
    /// CPU 状态未知：只能 teardown。
    Failed(FailedMachine, Fault),
    /// 清理没能证明完成：保留所有权直到线程结束。
    Stuck(PendingCleanup, Fault),
}

// ── 3.2 作用域变更：可卡住，互不阻塞 ──

/// [新] 同一作用域同时最多一个变更；不同作用域互不阻塞（design.new-mem §10.3：
/// StalledChange 关联受影响的 Window，而不是把整个服务停在一个全局 Busy 里）。
/// 没有 Cpu 作用域：CPU 集合的变化是 Exec 迁移。
pub enum Scope {
    Window(WindowId),   // virtio-mem 发布
    Dimm(DimmSlot),     // DIMM 物化 / 回收
    Device(DeviceId),   // E9 设备热插
}

/// [新，取代 Busy::Resizing] 一次已接受的作用域变更。
pub struct Change {
    id: OperationId,
    /// virtio-mem 的 resize 不改 Topology（requested_size 归设备），所以是 None。
    proposal: Option<TopologyProposal>,
    /// 请求发起的变更在"已发布"时回复；guest 弹出触发的回收没有回复者。
    reply: Option<Reply<ApiResult<()>>>,
    work: Stallable<ChangeOutcome>,
}

pub enum ChangeOutcome {
    /// 提交 proposal.next。
    Committed,
    /// 补偿已完成：事实不变，回复错误。
    RolledBack(Fault),
    /// 补偿不完整：保留资源，作用域保持占用，可凭 OperationId 重试（design.new-mem §10.2）。
    Stalled(StalledChange),
    /// guest 已接受的可见映射或内容无法恢复：继续运行会破坏正确性，交给故障策略（§5 Held）。
    Fatal(Fault),
}

pub struct StalledChange {
    id: OperationId,
    retry: RetryCapability,   // [现有 WindowControl::retry 的推广]
    diagnosis: String,
}

/// [新] 一台机器上全部作用域变更。导出会话通过 `TopologyFreeze` 冻结它。
pub struct Changes {
    active: BTreeMap<Scope, Change>,
    stalled: BTreeMap<Scope, StalledChange>,
    frozen_by: Option<SessionId>,
}

impl Changes {
    /// 只有在 active 与 stalled 都为空时才发放；持有期间拒绝宿主侧的新变更。
    fn freeze(&mut self, session: SessionId) -> Result<TopologyFreeze, NotCuttable>;
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. 运行权与会话
// ═══════════════════════════════════════════════════════════════════════════
//
// 快照、迁移、热升级共享三件事：
// - 一个切面（Cut）：Paused 且全部组件应答、没有在途变更；
// - 一个只读的导出会话（可中止，不改变 guest）；
// - 迁移与升级还多一步：交出运行权（不可逆）。
//
// 源端                                   目标端
//   Owned{Running}                         Absent
//     │ SendMigration                        │ ReceiveMigration / AdoptState
//     ▼                                      ▼
//   Running{Precopy} ──收敛──▶ Pausing     Importing ──就绪──▶ Standby（有机器，无运行权）
//     ▼                                      ▲                  │
//   Paused{StopCopy | Handover} ──对端就绪───┘                  │
//     │ grant(RunAuthority) ─────────── Grant（数据）──────────▶ │ accept
//     ▼                                                         ▼
//   Leaving::Releasing ─release_local─▶ 退出(Migrated/Upgraded)  Owned{Paused} ──Resume──▶ Running
//
// 授予之前任何一步失败：源端中止会话，回到 Paused（原先 Running 的则 Resume）；目标端丢弃机器。

/// [新] "本进程可以让这台 guest 执行指令"的能力。不可克隆、不可伪造。
/// 来源只有三种：冷启动、本地快照恢复、接受对端授予。Resume 借用它；grant 消耗它。
/// - 进程内：交出之后再 Resume 在类型上不可表示。
/// - 跨进程：类型无能为力，只能靠协议——源端先停 vCPU 再授予；授予一旦发出，即使确认丢失
///   也不再运行（GrantDelivery::Unknown）。
pub struct RunAuthority {
    epoch: AuthorityEpoch,
    origin: Origin,
    _p: Private,
}

pub enum Origin {
    Booted,
    /// 本地快照可以被恢复多次，得到多台互相独立的 guest——这是克隆，不是脑裂；
    /// 但它们身份相同，恢复时需要更新 VMGenID（依赖 A5）。
    Restored,
    Granted { session: SessionId },
}

/// [新] 授予消息：跨进程传递的数据，本身不是能力。目标端用 epoch 拒绝过期或重放的消息。
pub struct Grant {
    session: SessionId,
    epoch: AuthorityEpoch,
}

/// [新] 证明本进程已交出运行权：`release_local` 需要它，`resume` 拿不到它。
pub struct Relinquishment {
    session: SessionId,
    _p: Private,
}

/// 消耗运行权。调用前 Exec 必须是 Paused 且导出为 StopCopy / Handover。
fn grant(authority: RunAuthority, session: SessionId) -> (Grant, Relinquishment);

/// 目标端：校验 epoch 与会话后产生运行权。
fn accept(standby: &Standby, grant: Grant) -> Result<RunAuthority, GrantError>;

// ── 4.1 源端：导出 ──

/// [新] Running 时唯一允许的导出：只有 precopy 需要 guest 继续运行。
pub enum LiveExport {
    Precopy(Precopy),
}

pub struct Precopy {
    id: SessionId,
    peer: PeerLink,
    /// 期间拒绝宿主侧拓扑请求与 resize（design.new-mem §12.7：初版固定 Layout 与 Population）。
    freeze: TopologyFreeze,
    /// CPU、软件设备、直通设备的写入汇合（design.new-mem §12.4），不能只用 KVM dirty bitmap。
    dirty: DirtyEpoch,
    rounds: Abortable<Converged>,
}

/// [新] 只在切面上存在的导出：创建时要求一个 `Cut`。
pub enum CutExport {
    /// 快照落盘：状态 + 内存内容（ObjectContents）。写完后留在 Paused。
    Dump(Dump),
    /// 迁移最后一轮：状态 + 最后的写集 → 等对端就绪 → grant。
    StopCopy(StopCopy),
    /// 热升级：状态 + memfd + 设备后端 fd → 等新进程确认 → grant。
    Handover(Handover),
}

pub struct Dump {
    id: SessionId,
    write: Abortable<Result<(), DumpError>>,
}

pub struct StopCopy {
    id: SessionId,
    peer: PeerLink,
    freeze: TopologyFreeze,
    send: Abortable<TargetReady>,
}

pub struct Handover {
    id: SessionId,
    freeze: TopologyFreeze,
    /// fd 已交给编排者之后，本端不得再对共享 backing 做破坏性操作（discard、截断）。
    bundle_out: bool,
    confirm: Abortable<Adopted>,
}

/// [新] 冻结令牌：只在没有在途或卡住的作用域变更时由 `Changes::freeze` 发放。
/// guest 仍可能自行弹出（Linux 经 sysfs eject 触发 _EJ0）：这只记录在设备协议状态里，
/// 回收推迟到会话结束；随状态一起交给目标端的是"已弹出、待回收"。
pub struct TopologyFreeze {
    session: SessionId,
    version: u64,
    _p: Private,
}

// ── 4.2 目标端：导入 ──

/// [新] 获得一台机器的三种方式；都以 parked 机器结束，期间没有运行权。
pub enum Import {
    /// 本地快照（现有同进程 `rebuild` 的推广）：成功即得到运行权（Origin::Restored）。
    Restore(Bounded<Result<PausedMachine, RebuildFailure>>),
    /// 迁移目标：先收 Plan + Topology 建 parked 机器，再收内存轮次与最终状态。
    Receive(Receive),
    /// 热升级目标：接管 memfd 与设备后端 fd，按快照重建 VM。
    /// KVM 的 VM / vCPU fd 不能跨进程使用（kvm_main.c：kvm->mm != current->mm 时 ioctl 返回 -EIO），
    /// 所以必须重建，能交接的只有内存与后端。
    Adopt(Adopt),
}

pub struct Receive {
    id: SessionId,
    peer: PeerLink,
    stage: ReceiveStage,
}

pub enum ReceiveStage {
    AwaitingPlan,
    Memory { ram: MemoryStore, rounds: Abortable<FinalState> },
    Rebuilding(Bounded<Result<PausedMachine, RebuildFailure>>),
}

pub struct Adopt {
    id: SessionId,
    rebuild: Bounded<Result<PausedMachine, RebuildFailure>>,
}

/// [新] 机器已就绪但没有运行权：只能等授予（→ Owned）或被放弃（→ 本地 teardown）。
/// 没有客户端又没有运行权时，没人能再推进它，于是退出——与 Absent 收到 Detached 同理。
pub struct Standby {
    id: SessionId,
    machine: PausedMachine,
    topology: Topology,
    views: Views,
    facts: FactSources,
}

/// [新] 运行权交出瞬间的残余，随即由外壳交给 `release_local`，进入 Leaving::Releasing。
/// 类型上没有 resume。
pub struct Remnant {
    machine: PausedMachine,
    proof: Relinquishment,
    handoff: Handoff,
    delivery: GrantDelivery,
}

pub enum Handoff {
    Migrated,
    Upgraded,
}

pub enum GrantDelivery {
    Acknowledged,
    /// 发出后连接断开：对端是否收到未知。同样不能再运行，退出状态如实报告。
    Unknown,
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. 整机状态
// ═══════════════════════════════════════════════════════════════════════════
//
// 几个彼此独立的维度，各自是小枚举；跨维度的约束尽量由嵌套表达：
//
//   Vmm ─┬─ Serving { admission, guest }
//        └─ Stopping { leaving, exit }                   准入关闭由"没有 server 字段"表达
//
//   Guest ─┬─ Absent(spec) ─ Importing ─ Standby ─ Held
//          └─ Owned { authority, exec, topology, changes, facts }
//
//   Exec ─┬─ Running { machine, export: LiveExport? }   Precopy 只能在这里
//         ├─ Paused  { machine, export: CutExport? }    Dump / StopCopy / Handover 只能在这里
//         └─ Moving(Move)                               至多一个有界迁移
//
// 不做成一个扁平大枚举：维度的乘积会爆炸，而且大部分组合的规则只与其中一个维度有关。

/// [新，取代 serve() 中 controller / server / stopping 三个局部变量]
pub enum Vmm {
    Serving(Serving),
    /// 准入已关闭。类型上不再持有 VmmServer——取代现在"stopping ⇒ server == None"的约定。
    Stopping(Stopping),
}

pub struct Serving {
    admission: Admission,
    guest: Guest,
}

pub enum Admission {
    Open(VmmServer),
    /// 所有客户端都已离开（只报告一次）。有运行权的 guest 继续运行。
    /// 取代现在 `Starting.detached` 标记：Start 失败时直接看这里判断"还有没有人能重试"。
    Detached,
}

/// [新] 这台 guest 对本进程而言是什么。
pub enum Guest {
    /// [现有 NotStarted] 只有意图。
    Absent(MachineSpec),
    Importing(Import),
    Standby(Standby),
    Owned(Owned),
    /// [新，可选策略] 继续运行会破坏正确性：暂停并保留现场（design.new-mem §10.2），
    /// 运行权被扣押，只允许查询、诊断性捕获与 Shutdown。默认策略可以仍是直接退出。
    Held(Held),
    // 没有 Relinquished 变体：grant 之后直接进入 Stopping（Leaving::Releasing），
    // 交出运行权的进程不再接受任何请求。
}

/// [新] 持有运行权的机器。`X` 是 Exec 槽：外壳构造操作期间用 `Vacated` 占位，
/// 使"机器已移入操作、Exec 尚未填回"有自己的类型（见 §7 `Next::Move`）。
pub struct Owned<X = Exec> {
    authority: RunAuthority,
    exec: X,
    topology: Topology,
    changes: Changes,
    /// 设备与服务拥有的协议状态的只读投影：热插控制器的待处理位、各 Window 的 Population。
    /// 决策读它们（例如拒绝重复的弹出请求），但从不写——写只能经过设备。
    views: Views,
    /// 机器产生的事实来源属于整机，不属于某个操作。现在每个 Busy 变体各存一份 events，
    /// 完成时还要从对应变体里找出来补收。
    facts: FactSources,
}

pub struct Vacated;

/// [新] 只读投影的集合。
pub struct Views {
    hotplug: HotplugStatus,                     // [新，依赖 A5]
    windows: BTreeMap<WindowId, WindowStatus>,  // [现有 memory/service.rs]
}

/// [新，取代 Controller::{Running, Paused, Busy}]
pub enum Exec {
    Running {
        machine: Machine,
        export: Option<LiveExport>,
    },
    Paused {
        machine: PausedMachine,
        export: Option<CutExport>,
    },
    Moving(Move),
}

pub struct Held {
    machine: HeldMachine,
    fault: Fault,
    suspended: RunAuthority,   // 扣押而不是销毁：诊断后也只能退出，不能 Resume
    topology: Topology,
    changes: Changes,          // 卡住的变更继续持有资源
    facts: FactSources,
}

pub enum HeldMachine {
    Paused(PausedMachine),     // CPU 状态已知：还可以做诊断性捕获
    Failed(FailedMachine),     // CPU 状态未知：只能 teardown
}

/// [新，取代 teardown() / poll_teardown() / shutdown_resize() / Next::StartTeardown 四条路径]
pub struct Stopping {
    leaving: Leaving,
    exit: ExitFacts,
}

/// 退出的阶段。每个阶段只持有它需要的东西；事实在每个阶段都继续折叠进 `exit`。
pub enum Leaving {
    /// 先中止导出、等有界迁移落地。Guest 保持原形，只是不再接受请求。
    Settling(Guest),
    /// teardown 与仍在途的作用域变更并行推进：卡住的发布往往要靠 teardown 释放内存才能结算，
    /// 而它的回复不能丢（现有 shutdown_resize 的推广）。
    TearingDown {
        teardown: Bounded<Result<MemoryStore, (PendingCleanup, TeardownError)>>,
        changes: Changes,
        facts: FactSources,
    },
    /// 运行权已交出：只做本地释放（不对共享 backing 做破坏性操作）。
    Releasing {
        remnant_release: Bounded<Result<(), (PendingCleanup, TeardownError)>>,
        handoff: Handoff,
        delivery: GrantDelivery,
        facts: FactSources,
    },
    /// 结束。尚未证明可释放的所有者保留到线程返回之后才 drop。
    Done {
        retained: Option<PendingCleanup>,
    },
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. 输入
// ═══════════════════════════════════════════════════════════════════════════

/// [现有 main_loop::Input，重组] 主循环每轮取一个。
pub enum Input {
    Request(Request),
    Detached,
    Fact(Fact),
    /// Exec::Moving 落地。
    Landed(Landed),
    /// 作用域变更结算。
    Changed(Scope, ChangeOutcome),
    /// 导出 / 导入会话的进展（收敛、对端就绪、对端失败、写完）。
    Session(SessionEvent),
    /// Stopping 中 teardown / release 完成。
    Left(LeaveOutcome),
}

// 顺序规则（取代现在的 biased select + 完成后 try_recv 补收 + Start 失败的特判合并）：
//
//   应用任何 Landed / Changed / Session / Left 之前，先把全部已就绪的 Fact 折叠进状态；
//   Request 排在最后。
//
// 于是"操作在最后一次 poll 中入队的事件"不再是特例：它在落点被应用之前一定已经被折叠。
// 再加上 ExitFacts 的并集语义（§8），事实与落点谁先谁后都不影响最终裁决。

/// [新] 机器产生的事实。它们描述已经发生的事，控制器不能拒绝，只能折叠。
pub enum Fact {
    /// vCPU 线程结束。带序号：`TransitionExited(i)` 只能由同一个 CPU 的结束事实解释。
    /// 现在的 MachineEvent::GuestReset / VcpuFailed 不区分来源也不带序号。
    CpuEnded { cpu: CpuIndex, ending: CpuEnding },
    /// 平台设备请求复位（i8042）。此时 vCPU 仍然存活。
    GuestReset(ResetSource),
    /// 设备任务意外结束：局部故障，只记录。
    DeviceExited(DeviceExit),
    /// 内存服务结束而不是被要求停止。由与 MemoryHandle 同源的 watcher 报告——身份来自能力，
    /// 不来自任务名。取代现在循环入口与收到输入之后两次轮询 `failed_memory_service()`。
    RamDependencyLost(WindowId),
    /// 热插控制器：guest 执行了 _EJ0。
    Guest(GuestHotplug),
}

pub enum CpuEnding {
    Reset,             // KVM shutdown / system-reset 退出
    Fault(String),     // 已渲染的错误链
}

pub enum ResetSource {
    I8042,
    Cpu(CpuIndex),
}

/// [现有 MachineEvents，扩展] 事实来源。属于整机（Owned / Standby / Held / Leaving），不属于操作。
pub struct FactSources {
    machine: UnboundedReceiver<MachineFact>,  // vCPU 线程、平台设备
    devices: DeviceExits,                     // [现有]
    /// 每个内存服务一个 `finished` 观察者。Stop 路径凭 `CpusStopped` 解除它，
    /// 此后服务结束是预期的，不再报告 RamDependencyLost。
    ram: RamWatch,
    hotplug: HotplugEvents,                   // [新，依赖 A5]
}

/// [现有 controller::Request，扩展] 每个请求固定一个回复时机（ReplyPoint），写进 API 文档。
pub enum Request {
    // ── 查询：任何状态立即回答 ──
    GetInstanceInfo(Reply<InstanceInfo>),
    GetTopology(Reply<TopologyInfo>),          // [新] Topology + HotplugStatus + 各 Window 的 Population 摘要
    UpdateLogger(LoggerConfig, Reply<ApiResult<()>>),

    // ── 配置：只在 Absent ──
    Configure(VmmCommand, Reply<ApiResult<()>>),

    // ── 生命周期 ──
    Start(Reply<ApiResult<()>>),               // Landed
    Pause(Reply<ApiResult<()>>),               // Landed
    Resume(Reply<ApiResult<()>>),              // Landed
    Reboot(Reply<ApiResult<()>>),              // [新，E8] Landed
    Shutdown(Reply<ApiResult<()>>),            // Accepted：结果只在退出状态里

    // ── 拓扑 ──
    // 回复都表示"宿主侧已完成并已通知 guest"，guest 何时完成另行查询。
    // PlugCpu 是 Exec 迁移，在落地时回复；PlugMemory 在物化结算时回复；Unplug* 在信号发出时回复。
    Topology(TopologyRequest, Reply<ApiResult<()>>),    // Landed / Published
    ResizeMemory(HotplugSizeMib, Reply<ApiResult<()>>), // Published

    // ── 快照 ──
    SaveSnapshot(SnapshotTarget, Reply<ApiResult<()>>),     // 要求 Paused；Completed
    RestoreSnapshot(SnapshotSource, Reply<ApiResult<()>>),  // 只在 Absent；Landed（得到 Paused）

    // ── 迁移 ──
    SendMigration(Destination, Reply<ApiResult<()>>),       // Completed：已交出，或已回到原状态
    ReceiveMigration(Source, Reply<ApiResult<()>>),         // 只在 Absent；Landed（得到运行权）
    CancelMigration(Reply<ApiResult<()>>),

    // ── 热升级 ──
    HandoverState(Reply<ApiResult<LiveUpgradeState>>),      // [现有 API] 要求 Paused；回复 bundle
    AdoptState(LiveUpgradeState, Reply<ApiResult<()>>),     // [新] 新进程，只在 Absent；得到 Standby
    CommitHandover(Reply<ApiResult<Grant>>),                // [新] 旧进程：grant 并开始退出
    AbortHandover(Reply<ApiResult<()>>),                    // [新] 旧进程：收回 bundle，留在 Paused
    AcceptGrant(Grant, Reply<ApiResult<()>>),               // [新] 新进程：Standby → Owned{Paused}
}

/// [新] 回复的含义。现在只在个别注释里（例如 Shutdown 的 "Accepted, not done"）。
pub enum ReplyPoint {
    Immediately,  // 查询、拒绝
    Accepted,     // Shutdown
    Landed,       // 有界迁移落地
    Published,    // 宿主侧已完成并通知 guest；guest 何时完成另行查询
    Completed,    // 会话结束
}

/// [现有，扩展]
pub enum Rejection {
    NotStarted,
    AlreadyStarted,
    NotRunning,
    NotPaused,                      // [新] SaveSnapshot / HandoverState 要求先 Pause
    ConfigurationFrozen,
    InProgress,                     // Exec 正在迁移
    ScopeBusy(Scope),               // [新] 同一作用域已有变更（含已卡住的）
    TopologyFrozen(SessionId),      // [新] 导出期间
    Exporting(SessionId),           // [新] 会话进行中，先 Cancel / Abort
    NoAuthority,                    // [新] Standby
    NotCuttable(NotCuttable),       // [新] 有设备没应答 quiesce、或有变更未结算
    HandoverUnsupported,            // [现有] 后端不是 Shared memfd 时仍然如此
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. 决策与外壳
// ═══════════════════════════════════════════════════════════════════════════
//
// 分工与现在相同：控制器按值消耗旧状态，就地回答能回答的，返回一个 Next；外壳执行它
// （同步 build / begin、构造操作 future、丢弃 server）。不追求完整 reducer（design.md E2）。
//
// 决策函数对所有者只依赖"形状"，因此像现在的 Controller<R, P, F> 一样，可以用假的
// Machine / PausedMachine / FailedMachine 在没有 KVM 的情况下测试。

/// [现有 controller::Next，重组]
pub enum Next {
    Stay(Vmm),
    /// 从 Absent 冷启动。外壳同步 plan + build（vmm-main 上唯一的阻塞例外），成功后才有
    /// Owned 的全部材料：铸造 RunAuthority(Booted)、Topology::initial(plan)、build 交出的
    /// FactSources，并以 Exec::Moving(Starting) 装配 Owned。同步失败则回复错误、回到 Absent(spec)。
    Start(Admission, MachineSpec, Reply<ApiResult<()>>),
    /// 从 Absent 开始导入（Restore / Receive / Adopt）。
    Import(Admission, ImportRequest),
    /// 外壳构造一个有界迁移，装回 `Owned<Exec>` 的 `Exec::Moving`。
    /// `Owned<Vacated>` 让"机器已交给 Launch、Exec 尚未填回"成为一个具体类型，
    /// 而不是一个暂时不成立的不变量。
    Move(Admission, Owned<Vacated>, Launch),
    /// 外壳调用 `begin(&mut machine)`，成功则放进 Changes，失败则回复并留在原状态。
    Change(Serving, Scope, ScopedEffect, Option<Reply<ApiResult<()>>>),
    /// 外壳建立、推进或中止会话。
    Session(Serving, SessionCommand),
    /// 进入 Stopping。外壳只需丢弃 VmmServer；其余由 Leaving 的阶段驱动。
    Stop(Stopping),
}

/// [现有 Next::{Pause, Resume}，扩展] 每个变体带着它要消耗的机器。
pub enum Launch {
    Pause { machine: Machine, reply: Reply<ApiResult<()>> },
    Resume { machine: PausedMachine, reply: Reply<ApiResult<()>> },  // 外壳从 Owned 借 &RunAuthority
    PlugCpu { machine: Machine, proposal: TopologyProposal, reply: Reply<ApiResult<()>> },
    ReleaseCpu { machine: Machine, proposal: TopologyProposal },     // guest 弹出触发，无回复者
    Capture { machine: PausedMachine, then: AfterCapture },
    Reboot { machine: Machine, reply: Option<Reply<ApiResult<()>>> }, // guest 复位触发时无回复者
}

pub enum AfterCapture {
    Dump(SnapshotTarget, Reply<ApiResult<()>>),
    StopCopy(SessionId),
    Handover(Reply<ApiResult<LiveUpgradeState>>),
}

/// 唯一的决策入口。按 (状态, 输入) 匹配；不可能的组合由类型排除，而不是 unreachable!。
fn decide(vmm: Vmm, input: Input, instance: Instance<'_>) -> Next;

/// 复位策略：GuestReset / CpuEnded(Reset) 到达时，是退出（现状），还是进程内重建（E8）。
pub enum ResetPolicy {
    Exit,
    Reboot,
}

/// 故障策略：继续运行会破坏正确性时，是退出（现状），还是 Held 保留现场。
pub enum FaultPolicy {
    Exit,
    Hold,
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. 退出：事实的并集
// ═══════════════════════════════════════════════════════════════════════════
//
// 现在的 `prefer_exit(prior, next)` 是覆盖：后到的原因按优先级替换先到的，结果依赖到达顺序。
// 例：stopping 已是 Shutdown 时，KVM 级 reset 被吸收为 Shutdown；随后 Pausing 因该 CPU 已退出
// 而失败，TransitionExited 覆盖 Shutdown → Error。若 reset 晚于失败才被观察到 → Ok。
//
// 改为只记录事实，在终点一次性裁决。`join` 是逐字段并集：交换、结合、幂等，
// 因此裁决与到达顺序无关；"谁的消息更重要"只在裁决时决定措辞，不影响 Ok / Error。

#[derive(Default, Clone)]
pub struct ExitFacts {
    asked: BTreeSet<Ask>,                       // Shutdown、Detached
    resets: BTreeSet<ResetSource>,
    cpu_ended: BTreeMap<CpuIndex, CpuEnding>,
    lost_ram: BTreeSet<WindowId>,
    unsafe_resume: Vec<Rendered>,               // 内存服务没能证明 resume 安全
    transitions: Vec<TransitionFault>,
    cleanup: Vec<Rendered>,                     // teardown / Start 的清理 / DIMM 回收失败
    handoff: Option<(Handoff, GrantDelivery)>,
}

pub enum Ask {
    Shutdown,
    Detached,
}

pub struct TransitionFault {
    during: MoveKind,
    cpu: Option<CpuIndex>,      // VcpuError 现在带 u8 序号，可以直接填
    kind: TransitionKind,
    rendered: Rendered,
}

pub enum TransitionKind {
    Exited,      // 线程已结束：需要由同一 CPU 的结束事实来解释
    TimedOut,
    Panicked,
    Join,
}

impl ExitFacts {
    /// 逐字段并集。
    fn join(self, other: ExitFacts) -> ExitFacts;

    /// 裁决规则，与事实到达顺序无关：
    /// 1. cleanup 非空 → Error（资源可能仍被持有，压过一切；对应现在 teardown 错误覆盖原因）。
    /// 2. 任一 CpuEnding::Fault、lost_ram 非空、unsafe_resume 非空 → Error。
    /// 3. transitions 中有 TimedOut / Panicked / Join → Error。
    /// 4. transitions 中的 Exited(cpu)：cpu_ended 含该 cpu → 已解释（Reset 则正常，Fault 已在 2 中）；
    ///    没有序号时，resets 非空即视为已解释；否则 → Error。
    /// 5. 其余 → Ok，原因按 handoff > resets > asked 报告。
    /// 多条 Error 同时成立时按上面的编号选择报告哪条消息。
    fn verdict(&self) -> VmmExitStatus;
}

/// [现有，扩展] 线程返回值。
pub enum VmmExitStatus {
    Ok(ExitReason),
    Error(String),
}

pub enum ExitReason {
    Shutdown,
    Detached,
    GuestReset,
    Migrated,
    Upgraded,
    /// 授予已发出但对端是否收到未知。本端不再运行；仍算 Ok，由编排者核对对端。
    HandedOffUnconfirmed,
}

// ═══════════════════════════════════════════════════════════════════════════
// 9. 与当前实现的对应
// ═══════════════════════════════════════════════════════════════════════════
//
// | 现在                                                     | 模型中                                         |
// |----------------------------------------------------------|------------------------------------------------|
// | serve() 的 controller / server / stopping 三个局部变量   | Vmm::{Serving, Stopping}、Admission            |
// | "stopping ⇒ server == None" 的约定                       | Stopping 没有 server 字段                      |
// | Controller<R, P, F>::{NotStarted, Running, Paused, Busy} | Guest::Absent / Owned { exec: Exec }           |
// | Busy::{Starting, Pausing, Resuming} 与各自的 events      | Exec::Moving(Move)；events 移到 Owned.facts    |
// | Input::{Started, Paused, Resumed, Resized} + 5 个 unreachable! 组合 | Landed（单一完成类型）、Changed(Scope, _) |
// | Starting.detached                                        | Admission::Detached                            |
// | StartError::{Plan, Build} 同步失败                       | Start 请求的同步拒绝（不进入 Move）            |
// | StartError::Resume / Teardown / BuildCleanup             | Landed::Vacant / Stuck                         |
// | Busy::Resizing（全局，拒绝一切）                         | Changes 中的 Scope::Window 变更（互不阻塞）    |
// | failed_memory_service() 在入口与输入后各轮询一次         | Fact::RamDependencyLost（RamWatch）            |
// | 完成后 try_recv 补收 + Start 的 resume 失败特判合并      | §6 顺序规则 + §8 并集语义                      |
// | Exit + prefer_exit（覆盖，依赖顺序）                     | ExitFacts::join + verdict（并集，与顺序无关）  |
// | teardown() / poll_teardown() / shutdown_resize() / Next::StartTeardown | Leaving 的四个阶段              |
// | MemoryHandle::stop 文档中的 "Only after CPUs stop"       | &CpusStopped 参数                              |
// | capture() 内部逐项检查应答                               | PausedMachine::cut → Cut                       |
// | PausedMachine 的五个应答字段                             | QuiesceAcks                                    |
// | Parts.blocks / Parts.mems 两张表                         | Parts.devices: BTreeMap<DeviceId, AttachedDevice> |
// | MachinePlan.vcpus: u8、MachineSnapshot.vcpus: Vec        | CpuPlan、BTreeMap<CpuIndex, _>                 |
// | MachineEvent::{GuestReset, VcpuFailed}                   | Fact::{CpuEnded{cpu, ending}, GuestReset(source)} |
// | Rejection::HandoverUnsupported + HandoverState           | CutExport::Handover、AdoptState / CommitHandover |
// | （无）                                                   | Topology、RunAuthority、Import、Standby、Held  |
//
// 不变的部分：Machine / PausedMachine / FailedMachine / PendingCleanup 的 typestate、
// "操作只被借用、从不被取消"、"失败不回到出发状态"、决策与外壳的分工、Shutdown 回复"已接受"。
