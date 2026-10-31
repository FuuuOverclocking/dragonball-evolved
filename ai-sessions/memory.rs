// 补充材料：新内存设施的结构伪定义
//
// 对应 design.new-mem.md（第 5 轮整理稿）。这是评审附件，不是实现：
//   - 伪代码，不要求通过编译，不接入任何 crate；
//   - `Map`/`Set`/`Ranges`/`Bytes` 等是占位容器，不预设具体实现；
//   - 省略了错误类型层级、派生宏、生命周期标注与大部分方法体。
//
// 本文件要表达三件在图里画不清楚的事：
//   1. 字段持有关系之外的 **消耗 / 构造 / 消息** 关系；
//   2. 每个结构的 **线程归属**（单线程独占、可跨线程共享的不可变值、
//      还是带原子状态的共享入口）；
//   3. 失败时 **谁继续拥有资源**。
//
// 标注记法：
//   线程:  该类型的线程归属。显然的纯值不标（见 §0.2）。
//   构造:  由什么产生，谁有权产生。
//   消耗:  移动进来后原值不再可用的权限转移。
//   消息:  跨 owner 的请求与回执，不是函数调用。
//   失败:  操作失败后资源留在谁手里。
//   未定:  设计未决，不要当成已证明的同步或已选定的实现。
//
// 节号：`§x` 指本文件，`doc §x` 指 design.new-mem.md。
//
// 全文一条底线：**引用只保活，不授予权限**。Arc 能让字节不被 munmap，
// 不能让持有者获得布局修改权、KVM 注册权或访问准入。

// ===========================================================================
// §0 约定
// ===========================================================================

// §0.1 坐标与身份
//
// 不同地址空间用不同类型，不互相转换；长度与地址也不混用。
// 这些全是 Copy 纯值，线程属性显然。

struct Gpa(u64); // guest 物理地址
struct Iova(u64); // 某个 DMA 地址域内的地址
struct ObjectOffset(u64); // 对象内 offset，不等于 GPA
struct SourceOffset(u64); // 内容来源（文件/快照流）内 offset
struct Hva(usize); // 宿主虚拟地址；只出现在 unsafe 边界附近
struct ByteLen(u64);

struct ObjectId(u32);
struct WindowId(u64); // 同一 GPA 再次使用要换新 WindowId
struct MemoryDeviceId(u32);
struct DmaDomainId(u32);
struct VmEpoch(u64); // 同进程重建 VM 时递增；旧权限不跨 epoch
struct BlockIndex(u32); // Window 内的 guest block 序号

// 半开区间。GPA/IOVA/对象内 offset 各有自己的 Range 类型，不共用一个。
struct GpaRange {
    base: Gpa,
    len: ByteLen,
}
struct IovaRange {
    base: Iova,
    len: ByteLen,
}
struct ObjectRange {
    base: ObjectOffset,
    len: ByteLen,
}

// revision 只用于比较新旧与拒绝旧重放，不代表"已生效"。
struct Revision(u64);

// §0.2 线程标注的省略规则
//
// 以下三类不逐个标注：
//   - 纯值（规划输出、ID、范围、快照字段）：不可变、无内部可变性、无资源，
//     可自由 Clone 与跨线程移动。
//   - 由 MemoryRuntime 独占的内部状态：没有独立线程身份，跟随 runtime
//     所在的 LocalRuntime；既不 Clone 也不单独跨线程移动。
//   - 以 `*Evidence` / `*Receipt` 结尾的凭据：不可变值，但 **不可 Clone**，
//     因为复制一份证据等于凭空多出一份"已排空"的声明。
//
// 需要标注的是：共享入口、带原子状态的对象、权限令牌，以及任何
// 跨执行者移交的东西。

// ===========================================================================
// §1 纯值层：身份、放置与宿主政策
//   design.new-mem.md §3
// ===========================================================================

// 用户输入。规划是功能核：同样输入得同样布局。
struct MemorySpec {
    objects: Vec<ObjectSpec>,
    windows: Vec<WindowSpec>,
    host: Vec<HostPolicySpec>,
}

// 构造: MemorySpec::plan() 是纯函数，无副作用、不分配宿主资源。
//       失败只返回拒绝原因，不留下半成品。
impl MemorySpec {
    fn plan(&self, machine: &MachineAddressFacts) -> Result<Planned, PlanRejected>;
}

struct Planned {
    layout: MemoryLayout,
    policy: HostPolicySet,
}

// ---------------------------------------------------------------------------
// §1.1 MemoryLayout：可重放的纯值
// ---------------------------------------------------------------------------

// 整体可 Clone、可快照、可比较。HVA / fd / KVM slot / IOMMU handle 都不在这里。
struct MemoryLayout {
    objects: Map<ObjectId, ObjectDesc>,
    windows: Map<WindowId, WindowDesc>,
    claims: Vec<AddressClaim>, // 预留描述；不产生 RAM
    revision: Revision,
}

// 逻辑字节身份，不等于一次 mmap、一个 fd 或一段 GPA。
// 换 backing 后身份仍可保持。
struct ObjectDesc {
    capacity: ByteLen,
    content: ContentContract,
}

// 内容契约与"能不能物化"分开：前者是对象的要求，后者是 backing 的能力（§2.2）。
enum ContentContract {
    VolatileRam,
    Persistent {
        source: PersistentSourceId,
        consistency: ConsistencyRequirement,
    },
}

// 一个 ObjectSlice 的 guest 放置。
// slice.len 同时定义 Window 覆盖长度 —— 不存在两个可独立修改的长度。
struct WindowDesc {
    slice: ObjectSlice,
    gpa: Gpa,
    owner: WindowOwner,
    kind: WindowKind,
    guest_numa: Option<GuestNumaNode>,
}

struct ObjectSlice {
    object: ObjectId,
    range: ObjectRange,
}

enum WindowOwner {
    BootMemory,
    Device(MemoryDeviceId), // 明确设备身份，不用一个 Role 枚举冒充
}

enum WindowKind {
    BootRam,
    PluggableRam,                            // 整区插拔
    BlockRam { block_size: ByteLen },        // 已协商的 guest block 粒度
    PersistentAperture,
}

// 初版的 alias 限制，见 design.new-mem.md §3.3。
// 同一 Object 的不重叠切片允许；重叠的对象内范围拒绝。
impl MemoryLayout {
    fn check_no_overlapping_slices(&self) -> Result<(), PlanRejected>;
}

// ---------------------------------------------------------------------------
// §1.2 AddressClaim 与 GpaGrant：数据与权力的分界
// ---------------------------------------------------------------------------

// 可 Clone 的重放数据。它描述"布局占用了这段 GPA"，本身不是许可。
struct AddressClaim {
    range: GpaRange,
    purpose: ClaimPurpose,
}

enum ClaimPurpose {
    Ram(WindowId),
    Reserved, // 不产生 RAM
}

// 不可复制的权力，由机器地址分配 owner 签发。
// 内存服务不维护第二个可与 MMIO 冲突的 GPA 分配器。
//
// 线程: 不可 Clone、不可 Copy。移动它就是转移权力。
// 失败: 撤销完成才交还；StalledChange 持有的 grant 不交还（§8.3）。
struct GpaGrant {
    range: GpaRange,
    issuer: MachineAllocatorEpoch,
}

// ---------------------------------------------------------------------------
// §1.3 HostPolicy：与 guest 结构分开的宿主物化输入
// ---------------------------------------------------------------------------

// guest 布局跨主机稳定；宿主物化策略只能保持、显式替换或不支持，
// 不能默默重算 guest GPA / guest NUMA。
struct HostPolicySet {
    per_object: Map<ObjectId, HostPolicy>,
}

struct HostPolicy {
    object: ObjectId,
    allocation: BackingKind,
    host_numa: Option<HostNumaPolicy>,
    hugepage: HugepagePolicy,
    reserve: bool,
    prefault: bool,
}

enum BackingKind {
    AnonymousPrivate,
    SharedMemfd,                 // 可导出；但共享 != 持久
    File { shared: bool, path: HostPath },
}

// 恢复时读到的源策略：可用于诊断，不可当 guest 结构执行。
enum HostNumaPolicy {
    Keep,
    Replace(HostNumaNode),
    Unsupported,
}

// ===========================================================================
// §2 资源层：存储与宿主字节
//   design.new-mem.md §4
// ===========================================================================

// ---------------------------------------------------------------------------
// §2.1 MemoryStore / HostBacking
// ---------------------------------------------------------------------------

// 线程: 由 MemoryRuntime 独占（§3.2）。不 Clone、不单独跨线程移动。
//       它是"唯一分配权"的所在；多个视图可以保活 backing，但只有 Store
//       能新增 / 移除 ObjectId → backing 的记录。
struct MemoryStore {
    objects: Map<ObjectId, Arc<HostBacking>>,
    // 可写共享来源的真实身份 → 已占用的来源内范围。
    // 用于拒绝"换个 ObjectId / 换个路径就绕过不重叠检查"（doc §3.3）。
    writable_sources: Map<SourceIdentity, Ranges<SourceOffset>>,
}

// 同一文件实体的身份，不是路径字符串。
struct SourceIdentity {
    device: u64,
    inode: u64,
}

impl MemoryStore {
    // 构造: 消耗 HostPolicy 与 ObjectDesc，产生 backing 记录。
    // 失败: 失败时不留下部分注册的 ObjectId；已 mmap 的部分在此函数内回收。
    //       注意这是**物化前**的失败，与已有 guest 契约无关（§8.2 第一行）。
    fn materialize(
        &mut self,
        desc: &ObjectDesc,
        policy: &HostPolicy,
    ) -> Result<ObjectId, MaterializeError>;

    // 只给保活引用，不给注册权。
    fn backing(&self, object: ObjectId) -> Option<Arc<HostBacking>>;
}

// 宿主字节的所有者：持有映射与文件，负责最终释放。
//
// 线程: Arc 共享。保活引用可以跨线程持有（RamView 项、CPU 绑定、
//       DMA adapter 都保活它），这一点是 Send + Sync 的。
//       但它**不提供** `&mut [u8]`，也不提供可长期解引用的 slice：
//       字节访问一律经 MemoryLease（§4.3）或 CPU / 外部映射。
//       因此"可共享"说的是生命周期，不是数据竞争已被解决。
// 失败: Drop 只在确定已撤销时释放。无法证明外部注册已解除时，
//       保留 / 泄漏优先于释放（§8.5）。
struct HostBacking {
    kind: BackingKind,
    mapping: HostMapping, // ptr + len + prot + flags；owned 表示由谁 munmap
    file: Option<BackingFile>,
    page_size: ByteLen,
    capability: BackingCapability,
}

struct BackingFile {
    fd: OwnedFd,
    base: SourceOffset, // 文件 offset != GPA
    identity: SourceIdentity,
}

// 持久性、可导出性、可丢弃性是三件独立的事（doc §4.2）。
struct BackingCapability {
    exportable: bool,     // 能否交出 fd（vhost-user 在配置期就要求，不运行时回落）
    discardable: bool,    // 能否打洞 / MADV_DONTNEED
    persistent: Option<PersistentSourceId>,
}

// ---------------------------------------------------------------------------
// §2.2 BackingExport：给外部域的受限解析能力
// ---------------------------------------------------------------------------

// 线程: 不可变值 + Arc<HostBacking> 保活，可跨线程移动，可被 DmaProjection
//       长期持有。
// 构造: 只有 MemoryRuntime 能签发，且只在 backing.capability 允许时签发。
//
// 存在的理由：DmaProjection 需要把允许范围解析成 HVA/fd 才能调 ioctl。
// 如果它改为回调 MemoryRuntime 查询，就会出现"内存服务等 DMA ack、
// DMA adapter 等内存查询"的互等（§6.2）。所以解析能力随消息一起交出去。
struct BackingExport {
    backing: Arc<HostBacking>,
    allowed: Ranges<ObjectOffset>, // 只能解析这些对象内范围
}

impl BackingExport {
    // 不在 allowed 内返回 None；这是描述性约束。对普通 fd backend
    // 它**不是**硬件强制的子范围权限（§6.5）。
    fn resolve(&self, range: ObjectRange) -> Option<(Hva, Option<(&OwnedFd, SourceOffset)>)>;
}

// ===========================================================================
// §3 控制层：唯一内存控制 owner
//   doc §5
// ===========================================================================

// ---------------------------------------------------------------------------
// §3.1 UnboundMemory → AttachedMemory：权限的一次性移交
// ---------------------------------------------------------------------------

// 已物化、尚未绑定到任何 VM 的存储与布局。
//
// 线程: **不可 Clone**。它承载"唯一分配 / 注册权"，复制它等于造出第二个
//       可以注册同一 backing 的权威。可以跨线程移动（尚未绑定 VM 时没有
//       线程亲和性），但同一时刻只有一个持有者。
struct UnboundMemory {
    store: MemoryStore,
    layout: MemoryLayout,
    policy: HostPolicySet,
    grants: Vec<GpaGrant>, // 已从机器分配 owner 取得
}

// 绑定到一个 VM 之后的控制 owner。
//
// 线程: 单线程独占，运行在专门的 LocalRuntime；允许 !Send。
//       它的推进**不依赖** vmm-main 接收下一条事件（doc §5.3）。
// 消耗: UnboundMemory + VM 句柄 → AttachedMemory。原 UnboundMemory 不再存在。
struct AttachedMemory {
    vm: Arc<VmHandle>,
    vm_epoch: VmEpoch,
    runtime: MemoryRuntime,
}

impl UnboundMemory {
    // 消耗: self。
    fn attach(self, vm: Arc<VmHandle>, epoch: VmEpoch) -> AttachedMemory;
}

impl AttachedMemory {
    // 消耗: self，并且**要求证据**，不是无条件拆解。
    //
    // 同进程 teardown/rebuild 的全部前提（doc §5.4）在类型里写出来：
    // 少一项就拿不回 UnboundMemory，于是旧 view 无法借旧权限写入新 VM。
    // 失败: 证据不全时把 self 还回来，不释放任何 backing。
    fn detach(self, evidence: TeardownEvidence) -> Result<UnboundMemory, (Self, MissingEvidence)>;
}

// 不可 Clone（§0.2）：每一项都是某个执行者给出的一次性声明。
struct TeardownEvidence {
    admission_closed: Vec<AdmissionClosed>,  // 全部 Window 准入永久关闭（§4.2）
    software_drained: Vec<LeaseDrained>,     // 在途软件访问已排空
    cpu_unbound: Vec<CpuUnbound>,            // CPU 投影解除、vCPU 已停
    dma_revoked: Vec<DmaRevoked>,            // 全部外部权限解除（§6.5）
    vm_closed: VmClosed,
}

// ---------------------------------------------------------------------------
// §3.2 MemoryRuntime：它拥有什么
// ---------------------------------------------------------------------------

// 线程: 单线程独占 owner，!Send 可接受，不 Clone，不放进 Arc<Mutex<..>>。
//       外部只能通过 §3.3 的句柄与它交互。
//
// 它**不**拥有：整个设备、普通队列 I/O、IRQ/总线、IOMMU 页表、
// 机器的暂停/运行权（doc §5.2）。
struct MemoryRuntime {
    store: MemoryStore,
    layout: MemoryLayout,
    windows: Map<WindowId, WindowState>,
    cpu: CpuBindings,

    // RamView 的**发布**入口只有这一份；消费者只拿 §4.1 的读入口。
    publisher: RamViewPublisher,

    // 外部 DMA 消费者的成员关系。注意是 Projection 的**句柄**，
    // 不是 Projection 本身：实际映射由对应的域 owner 独占（§6.2）。
    dma_consumers: Map<DmaDomainId, DmaConsumer>,

    // 未结束的副作用。空集才说明"没有正在进行的变更"。
    changes: Map<OperationId, MemoryChange>,
    stalled: Map<OperationId, StalledChange>,

    // 迁移写入追踪；None 表示当前没有受支持的 epoch（§9.3）。
    dirty: Option<DirtyEpoch>,
}

struct OperationId(u64);

// ---------------------------------------------------------------------------
// §3.3 对外句柄：三种，权限各不相同
// ---------------------------------------------------------------------------

// 给软件设备：只读索引入口。定义在 §4.1。
// 线程: Clone + Send + Sync。多个设备、多个线程可同时持有。
//   -> RamReader

// 给内存设备：只能就**自己那一个 Window** 的插入状态提出请求。
// （本文件其余部分不标可见性；这里标出 pub，因为可见性本身就是约束。）
//
// 线程: 由该设备的单一 !Send owner 持有，不 Clone。
// 构造: 只有 MemoryRuntime 签发；签发时写死 (device, window)，
//       同时给出该 Window 的 WindowStatusReader（§7.2）。
// 消耗: plug / unplug / unplug_all 消耗 self，控制权随 PendingMemory 停在途中，
//       结果到达才交还（§7.4）。由此两条规则由类型表达，不靠设备自觉：
//         - 一个 Window 同时最多一个在途请求；
//         - 设备只有空闲时（重新持有 WindowControl）才能导出协议快照（doc §9.4）。
//
// 字段私有：设备拿不到 tx，也构造不出带别的 WindowId 的 MemoryRequest。
// 服务端仍按记录校验（§7.4）：句柄可能已过期（Window 已被移除），
// 消息自称的身份也不是证据。发送端盖章防误用，接收端校验防过期，两者不能互相替代。
//
// 只覆盖块级 Window。整区可插拔 Window 的移除路径未定（doc §15.1 第 8 条）。
pub struct WindowControl {
    device: MemoryDeviceId,
    window: WindowId,
    tx: MessageSender<MemoryRequest>,
}

impl WindowControl {
    // target_limit 是设备**当时**的 requested_size；服务按自己的 Population
    // 与它判定，不保存第二份（doc §9.2）。
    pub fn plug(self, range: GpaRange, target_limit: ByteLen) -> PendingMemory;
    pub fn unplug(self, range: GpaRange) -> PendingMemory;
    pub fn unplug_all(self) -> PendingMemory; // virtio-mem 的 UNPLUG_ALL
}

// 给外部 DMA 域 owner：只能提交本域的映射请求与接收 AllowedRam。
// 线程: 由该域的 owner 持有（§6.2）。不能借它操作设备或关闭设备 worker。
struct DmaConsumer {
    domain: DmaDomainId,
    ready: ConsumerReadiness, // 回放完成前不是 Ready（§6.3）
    tx: MessageSender<AllowedRamUpdate>,
}

enum ConsumerReadiness {
    Replaying,
    Ready,
    Revoking, // 实际外部状态解除前仍是消费者
}

// ===========================================================================
// §4 软件访问：索引版本与访问许可分开
//   doc §6
// ===========================================================================

// ---------------------------------------------------------------------------
// §4.1 RamReader / RamView：发布一个版本，不是发布字节
// ---------------------------------------------------------------------------

// 线程: Clone + Send + Sync，任意多个读者。
//       读者得到一个一致的集合版本；替换不阻塞读者，旧版本按引用继续存在。
// 未定: 具体是 ArcSwap 还是别的发布原语未定。可以确定的是
//       **它不是**字节快照、不是旧使用者的退出屏障、也不是 KVM/DMA 的全局事务。
struct RamReader {
    cell: Arc<ArcSwapLike<RamView>>,
}

// 线程: 只有 MemoryRuntime 持有，不 Clone（§3.2 publisher）。
struct RamViewPublisher {
    cell: Arc<ArcSwapLike<RamView>>,
}

// 不可变索引。不是内容快照，不拥有 grant / slot / 外部 DMA 权限。
//
// 线程: Arc 共享、Send + Sync。软件设备可以长期缓存其中的项，
//       但**不能**长期缓存一份脱离 MemoryLease 的可解引用 HVA（doc §6.2）。
// 范围: 只覆盖软件 DMA 能用的 RAM。不是完整机器地址空间，
//       不自动包含 pmem / 设备内存，MMIO 另有访问语义。
struct RamView {
    revision: Revision,
    entries: Vec<RamViewEntry>, // 按 GPA 排序，用于查址
}

struct RamViewEntry {
    gpa: GpaRange,
    slice: ObjectSlice,
    backing: Arc<HostBacking>,     // 保活，不授权
    access: Arc<WindowAccess>,     // 准入在这里，不在 entry 里
    window: WindowId,
}

impl RamReader {
    fn load(&self) -> Arc<RamView>;
}

impl RamView {
    // 查址只做定位，不授予访问权。返回值还要再 acquire（§4.3）。
    fn locate(&self, gpa: Gpa, len: ByteLen) -> Result<Located<'_>, LocateError>;
}

struct Located<'a> {
    entry: &'a RamViewEntry,
    offset: ObjectOffset,
    len: ByteLen,
}

// ---------------------------------------------------------------------------
// §4.2 WindowAccess：准入 + 在途计数 + 当前可访问范围
// ---------------------------------------------------------------------------

// 一个 Window 的访问准入，**不是**"整台 VM 内存的锁"。
//
// 线程: Arc 共享，含原子状态，多线程并发 acquire。
// 未定: 具体原子布局与内存序未定。设计上只固定一条**必须成立**的性质：
//       「关闭准入」与「登记一个在途访问」是**一个原子判定**，
//       不能用两个独立 load 拼出 check-then-use 竞态（doc §6.2）。
//       这也意味着 admission + inflight 不能是两个独立的原子变量。
// 代价: 屏障粒度是整个 Window（doc §6.4），计数按租约/批次，不按字节。
struct WindowAccess {
    window: WindowId,
    // 一个原子字里的组合状态，而不是三个独立字段：
    //   state: Open | Closing | Closed（Closed 永久）
    //   inflight: 在途租约数
    gate: AtomicGate,
    // 当前已接受的插入集合。Population 改变要经过"关门 → 排空 → 换值 → 开门"，
    // 旧索引的访问也必须读到新值，不能用表里的旧 bitmap（doc §6.3）。
    population: AtomicPopulationRef,
}

enum AcquireOutcome {
    Granted(MemoryLease),
    // 正在撤销 / Population 正在改变。调用方应释放已持有的租约后重试，
    // 不是无限等待（§4.4）。
    Revoking,
    // 范围当前未插入或超出 Window。
    NotPopulated,
    // 永久关闭。相同 GPA 的新用途会有新的 WindowId（doc §6.3）。
    Closed,
}

impl WindowAccess {
    // 构造: 唯一产生 MemoryLease 的地方。
    fn acquire(self: &Arc<Self>, range: ObjectRange, dir: Direction) -> AcquireOutcome;

    // 只有 MemoryRuntime 调用。返回凭据供 §3.1 的 TeardownEvidence 使用。
    // 失败: 排空未完成时返回进度，**不**允许据此把裸指针当作已失效（doc §6.5）。
    fn begin_close(self: &Arc<Self>) -> CloseProgress;
}

enum CloseProgress {
    Drained(AdmissionClosed, LeaseDrained),
    StillInflight { count: u32 },
}

// ---------------------------------------------------------------------------
// §4.3 MemoryLease：覆盖真实完成时间的许可
// ---------------------------------------------------------------------------

// 线程: 不可 Clone。可以跨线程**移交**给实际完成 I/O 的 owner，
//       但必须是显式的受控接口；不能让一个被取消的 future 顺手 drop 掉它。
// 边界: 它覆盖的是**异步 I/O 的真实完成时间**，不是提交 syscall 的时间
//       （doc §6.2）。
//       它不是 vCPU 访问许可，也不是 VFIO pin 的替代品（doc §6.5）。
// 失败: 无法取消的外部 I/O 继续持有它。取消请求的 API 成功返回，
//       也要按该 backend 的定义确认内核/回调不再持指针。
struct MemoryLease {
    access: Arc<WindowAccess>, // Drop 时递减在途计数
    backing: Arc<HostBacking>, // 保活，不授权注册
    range: ObjectRange,
    dir: Direction,
    epoch: AccessEpoch, // 取得租约时的代次；用于写入归属（§9.4）
}

enum Direction {
    Read,          // 从 guest 内存读
    Write,         // 写入 guest 内存 → 需要记脏（§9.4）
    ReadWrite,
}

impl MemoryLease {
    // 只有拿到租约才解析 host slice。解析结果不得存活过 lease。
    fn slice(&self) -> VolatileSliceLike<'_>;

    // 消耗: 写方向的租约转成写授权，把记脏责任交给真正的完成 owner（§9.4）。
    fn into_write_authority(self) -> WriteAuthority;
}

// ---------------------------------------------------------------------------
// §4.4 LeaseSet：提交外部 I/O 前取得完整租约集合
// ---------------------------------------------------------------------------

// 一个 I/O 的 payload 与完成所需的 used-ring 等范围，必须在提交**之前**
// 一次性取齐（doc §6.5）。否则会出现：payload 完成了，却要为了写 used-ring
// 去向一个已经关门的 Window 申请新租约 —— 自锁。
//
// 线程: 不可 Clone，随 I/O 一起移交。
struct LeaseSet {
    leases: Vec<MemoryLease>, // 按固定顺序获取，避免跨 Window 死锁
}

impl LeaseSet {
    // 跨 Window 固定顺序 + 非阻塞尝试。
    // 失败: 任一窗口正在撤销 → 释放本次已取得的全部租约，返回可重试，
    //       **不持一部分租约 await 另一部分**（doc §6.5）。
    fn acquire_all(req: &[(Arc<WindowAccess>, ObjectRange, Direction)]) -> Result<Self, Revoking>;
}

// ===========================================================================
// §5 CPU 投影与粒度
//   doc §7
// ===========================================================================

// 线程: 由 MemoryRuntime 独占（§3.2）。唯一的 slot 注册权威。
struct CpuBindings {
    vm: Arc<VmHandle>,
    vm_epoch: VmEpoch,
    // 只记录**已成功**的绑定。提交中的绑定在 MemoryChange.applied 里（§8.1）。
    active: Map<SlotId, CpuBinding>,
    permits: SlotPermitPool,
}

struct SlotId(u32);

// 运行期编号。**不进入恢复数据**；恢复可重新分配编号（doc §7.2）。
//
// 线程: 不可 Clone、不可 Copy。safe 层不能用一个可复制的 slot 编号
//       构造出第二份注册权（doc §7.1）。
// 构造: 只能从 SlotPermitPool 取得。
struct SlotPermit {
    slot: SlotId,
}

struct SlotPermitPool {
    free: Vec<SlotId>,
    budget: u32, // KVM slot 预算；具体默认值未定（doc §15.1 第 6 条）
}

// 一次成功的 CPU 映射。
//
// 线程: 由 CpuBindings 持有。
// 消耗: SlotPermit + Arc<HostBacking> → CpuBinding。
//       绑定**保活** VM 与 backing：关闭 VM 前不释放仍可访问的字节。
// 失败: 解绑失败时 CpuBinding **留在 active 里**，permit 不归还 —— 否则
//       这个 slot 会被当成空闲再次发放（§8.2 第三行）。
struct CpuBinding {
    permit: SlotPermit,
    vm: Arc<VmHandle>,
    backing: Arc<HostBacking>,
    gpa: GpaRange,
    hva: Hva, // 只在注册 ioctl 的 unsafe 边界使用
    shape: BindingShape,
}

// 运行期编译结果，**不是**新的 guest 拓扑（doc §7.2）。
// 保留稳定的 chunk 边界：避免相邻合并后，拔一个范围扰动其他绑定。
struct BindingShape {
    window: WindowId,
    chunk: ChunkIndex,
    covers: Set<BlockIndex>, // 这个 chunk 覆盖的 guest block
}

struct ChunkIndex(u32);

impl CpuBindings {
    // 消耗: permit。
    // 失败: 注册失败时 permit 随错误一起返回，可重新发放；
    //       **没有**部分注册残留。
    fn bind(
        &mut self,
        permit: SlotPermit,
        backing: Arc<HostBacking>,
        gpa: GpaRange,
        shape: BindingShape,
    ) -> Result<SlotId, (SlotPermit, BindError)>;

    // 初版策略：chunk 内只要还有 plugged block 就保留整个注册，
    // 完全空闲才撤销（doc §7.3）。
    //
    // 它**不承诺**恶意 guest 无法用 CPU 重新填充已 unplug 的页，
    // 也不能据此对外协商"逐 block 不可访问"。
    fn unbind_if_empty(&mut self, chunk: ChunkIndex) -> Result<Option<CpuUnbound>, UnbindFailed>;
}

// 四种粒度在类型里分开，不共用一个 "page size"（doc §7.2）。
// 兼容性在构建与接入时检查一次，不扩散为每次普通 I/O 的四份判断。
struct GranularityFacts {
    guest_block: ByteLen,     // Population 与协议的单位
    host_page: ByteLen,       // discard / 大页 / 打洞的单位
    cpu_chunk: ByteLen,       // 编译为 KVM 注册的单位
    dma_revocable: ByteLen,   // 该 adapter 能独立撤销的单位
}

impl GranularityFacts {
    // guest block 小于不可独立回收的 hugepage 时：对齐约束、延迟回收整个
    // 大页，或拒绝组合。**不能**悄悄 discard 邻接的 plugged block。
    fn check(&self) -> Result<(), IncompatibleGranularity>;
}

// ===========================================================================
// §6 外部 DMA 投影
//   doc §8
// ===========================================================================

// ---------------------------------------------------------------------------
// §6.1 两个输入，各自有来源与 revision
// ---------------------------------------------------------------------------

// 来自 MemoryRuntime：这个消费者**可以使用**哪些存储范围。
// 线程: 不可变值 + 保活引用，随消息跨 owner 移动。
struct AllowedRam {
    revision: Revision,
    domain: DmaDomainId,
    entries: Vec<AllowedEntry>,
}

struct AllowedEntry {
    gpa: GpaRange,
    slice: ObjectSlice,
    export: BackingExport, // 自带解析能力，避免回调内存服务（§2.2）
}

// 来自 guest IOMMU owner：IOVA 翻译与读写权限。
// 线程: 不可变值。
struct DomainMap {
    revision: Revision,
    domain: DmaDomainId,
    mode: TranslationMode,
}

enum TranslationMode {
    // 没有 guest IOMMU 时是**显式的** identity，不维护一套假页表（doc §8.2）。
    Identity,
    Translated { entries: Vec<IovaEntry> },
}

struct IovaEntry {
    iova: IovaRange,
    gpa: Gpa,
    perm: DmaPerm,
}

struct DmaPerm {
    read: bool,
    write: bool,
}

// ---------------------------------------------------------------------------
// §6.2 DmaProjection：一个外部地址域的 owner
// ---------------------------------------------------------------------------

// 线程: **一个 host DMA 地址域**的单一 owner，串行应用真实副作用。
//       不一定一设备一份 —— 多个 VFIO 设备可共享一个 IOMMU 容器。
//       它不是设备之间的共享锁，也不拥有内存分配或设备协议。
// 未定: 它跑在哪个执行者上未定。倾向于独立执行边界，因为 ioctl 可能阻塞，
//       而 LocalRuntime 上的 async **不保证**不阻塞（doc §5.3）。
//       确定的是：它不同步回调 MemoryRuntime 查内存（§2.2）。
struct DmaProjection {
    domain: DmaDomainId,
    allowed: AllowedRam,   // 最新接受的版本
    map: DomainMap,        // 最新接受的版本
    actual: ActualMappings,
    backend: DmaBackend,
}

enum DmaBackend {
    // 必须按允许范围 pin，不用整区 map 偷偷填充已拔出的块（doc §8.5）。
    HostIommu { container: IommuContainer },
    // fd 本身授权整份 backing；offset/length 不是硬隔离（§6.5）。
    VhostUser { socket: VhostSocket },
}

impl DmaProjection {
    // 消息: 来自 MemoryRuntime。旧 revision 的重放被拒绝。
    //       两个输入**不是**一次全局事务（doc §8.2）。
    fn on_allowed_ram(&mut self, update: AllowedRamUpdate) -> ProjectionAck;

    // 消息: 来自 guest IOMMU owner。
    // 约束: 新映射只能与**当前** AllowedRam 相交；不能凭旧 DomainMap
    //       把已经撤销的 Window 重新 map 回去（doc §8.2）。
    fn on_domain_map(&mut self, update: DomainMapUpdate) -> ProjectionAck;
}

struct AllowedRamUpdate {
    allowed: AllowedRam,
    operation: OperationId, // 回执归回原操作，不按地址归并（§6.4）
}

struct DomainMapUpdate {
    map: DomainMap,
    operation: OperationId,
}

// 回执只报告"这个操作在这个域的结果"。
// 它**不**代表硬件已停止发送旧请求（§6.5）。
struct ProjectionAck {
    operation: OperationId,
    domain: DmaDomainId,
    applied: Vec<MappingId>,
    failed: Vec<(MappingId, DmaError)>,
}

// ---------------------------------------------------------------------------
// §6.3 新消费者接入与回放
// ---------------------------------------------------------------------------

impl MemoryRuntime {
    // 先登记为 Replaying 成员并交出当前窗口与可访问状态，
    // 回放完成才转 Ready（doc §8.3）。
    fn register_dma_consumer(&mut self, domain: DmaDomainId) -> (DmaConsumer, AllowedRam);
}

impl DmaProjection {
    // 失败: 回放失败时它**仍然拥有**已成功创建的映射，必须撤销或隔离，
    //       不能直接删除句柄（doc §8.3）。
    fn replay(&mut self, allowed: AllowedRam) -> Result<ConsumerReadiness, ReplayPartial>;
}

struct ReplayPartial {
    created: Vec<MappingId>, // 已经发生的副作用，必须交代
    error: DmaError,
}

// ---------------------------------------------------------------------------
// §6.4 ActualMappings：真实成功的登记
// ---------------------------------------------------------------------------

// 线程: 由 DmaProjection 独占。
// 键是 DmaDomainId + MappingId/代次，**不只是 IOVA**（doc §8.4）：
// 迟到结果必须归回原操作登记，不能按地址删掉"现在恰好在这里"的新映射。
struct ActualMappings {
    domain: DmaDomainId,
    live: Map<MappingId, ActualMapping>,
    // 某旧范围仍有未确认的 map/unmap 时，不允许该 IOVA 的新绑定越过它。
    unconfirmed: Map<IovaRange, MappingId>,
}

struct MappingId {
    seq: u64,
    generation: u64, // 软件代次
}

struct ActualMapping {
    id: MappingId,
    iova: IovaRange,
    slice: ObjectSlice,
    perm: DmaPerm,
    handle: BackendHandle, // 真实句柄，不是"进度百分比"
}

// ---------------------------------------------------------------------------
// §6.5 撤销证据：代次只保护宿主登记
// ---------------------------------------------------------------------------

// 这一组类型存在的唯一目的，是让"已解绑 ≠ 可复用"写进签名里（doc §11.1）。
// 全部不可 Clone（§0.2）。

// unmap 回执：已登记范围的映射按 backend 保证解除。
// **不能**单独证明 endpoint 不会再发旧请求，或同 IOVA 可换用途。
struct UnmapReceipt {
    domain: DmaDomainId,
    ids: Vec<MappingId>,
}

// endpoint/domain 在**指定代次**停止或排空。
// 不接受一个通用的 `stopped: bool`（doc §11.3）：证据绑定域与代次。
// **不能**证明尚存映射/pin 已释放，也不能代表共享域的其他成员已停。
struct EndpointQuiesced {
    domain: DmaDomainId,
    endpoint: EndpointId,
    generation: u64,
}

// 远端可信排空/导出撤销协议完成，或进程终止证据。
// 仅 socket 已关、REM_MEM_REG 已发送**不算**（doc §11.1）。
enum ExportRevoked {
    ProtocolDrained { socket: VhostSocket, proof: DrainProof },
    PeerTerminated { pid: Pid, status: ExitStatus },
}

// 完整的"外部权限已解除"，供 TeardownEvidence 使用（§3.1）。
// 组合而非单项：三类证据各自覆盖不同执行者。
struct DmaRevoked {
    unmapped: UnmapReceipt,
    quiesced: Option<EndpointQuiesced>, // 会复用 IOVA/端点时必需
    export: Option<ExportRevoked>,      // 有 fd 导出时必需
}

// 初版 vhost-user 只支持固定导出生命周期（doc §8.5）。
// 在线删除、跨 VM 重用、精确 block 回收需要上面的真实证据；
// 没有就**拒绝组合**，而不是假设能撤销。
enum VhostExportLifetime {
    Fixed,
    Revocable { protocol: TrustedDrainProtocol },
}

// 软件代次不出现在硬件请求里。这个函数故意不存在：
//   fn stop_hardware_dma_by_generation(gen: u64) -> ! { unimplemented!() }

// ===========================================================================
// §7 设备协议边界
//   doc §9
// ===========================================================================

// ---------------------------------------------------------------------------
// §7.1 WindowState：Population 只有一个权威
// ---------------------------------------------------------------------------

// 线程: 由 MemoryRuntime 独占（§3.2）。
// virtio-mem 的固定容量 Window **不**因每个 block 插拔而创建/删除。
struct WindowState {
    window: WindowId,
    kind: WindowKind,
    population: Population,
    revision: Revision,
    access: Arc<WindowAccess>,     // 发给 RamView 的那一份（§4.2）
    cpu_chunks: Map<ChunkIndex, SlotId>,   // 当前成功的 CPU 登记
    dma_domains: Set<DmaDomainId>,         // 当前成功登记的外部域
}

// 不可变的 block 集合值：状态转移产生新值。
// bitmap 只是它的表示，**不是**第二份 KVM bitmap，也不是驻留（RSS）信息。
//
// 线程: 纯值，可 Clone，可随 WindowStatus 发布给多个读者。
struct Population {
    block_size: ByteLen,
    total: u32,
    plugged: BitSet<BlockIndex>,
}

impl Population {
    fn with_plugged(&self, blocks: &Set<BlockIndex>) -> Population; // 返回新值
    fn with_unplugged(&self, blocks: &Set<BlockIndex>) -> Population;
}

// ---------------------------------------------------------------------------
// §7.2 WindowStatus：提交时发布的派生投影
// ---------------------------------------------------------------------------

// 线程: 不可变值，经 WindowStatusReader 发布。
// 它是 WindowState 提交时的**派生**投影，**不**用于反向决定资源事实（doc §9.2）。
//
// 带上 Population 的只读值，是因为设备必须就地回答 STATE 请求：virtio-mem 规范
// 要求设备报告一段 block 是 plugged / unplugged / mixed。
// 这不是第二份 bitmap：设备只能读，替换它的只有服务的下一次提交。
struct WindowStatus {
    window: WindowId,
    revision: Revision,
    population: Population,
}

impl WindowStatus {
    fn plugged_size(&self) -> ByteLen; // 由 population 派生，不单独存
    fn state_of(&self, blocks: Range<BlockIndex>) -> BlockState;
}

enum BlockState {
    Plugged,
    Unplugged,
    Mixed,
}

// 线程: Clone + Send + Sync 的只读入口，与 RamReader（§4.1）同类，但只覆盖一个 Window。
// 读取不取字节租约、不发消息：回答配置读取与 STATE 请求不会卷入撤销屏障，
// 内存操作等待在途 I/O 时也不会反过来等这个设备（doc §6.2 末段）。
// 请求在途、设备不持有 WindowControl 时照样可读。
struct WindowStatusReader {
    cell: Arc<ArcSwapLike<WindowStatus>>,
}

// ---------------------------------------------------------------------------
// §7.3 设备侧：拥有协议，不拥有资源事实
// ---------------------------------------------------------------------------

// 线程: 单一 !Send owner（设备 future 自己）。不放进 Arc<Mutex<..>>。
//
// 它**没有** plugged bitmap / plugged 计数字段 —— guest 配置里的 plugged_size
// 与 STATE 请求的回答都从只读 WindowStatus 取（doc §9.2）。
struct MemoryDeviceProtocol {
    id: MemoryDeviceId,
    control: ControlState,
    status: WindowStatusReader,

    // 设备独占的协议事实：
    requested_size: ByteLen,     // 目标容量。服务**不保存**第二份
    usable_region_size: ByteLen, // 按协议由设备设定，不小于 requested_size
    features: NegotiatedFeatures,
    queues: DeviceQueues,
}

// 空闲时持有 WindowControl，在途时只持有 PendingMemory，两者不会同时存在。
enum ControlState {
    Idle(WindowControl),
    Pending(PendingMemory),
}

impl MemoryDeviceProtocol {
    // 只有 Idle 才能导出：已提交的请求必须先得到确定结果（doc §9.4）。
    fn export(&self) -> Result<MemoryDeviceSnapshot, StillPending>;

    // reset 不丢弃 Pending，并且在 Pending 时推迟报告 reset 完成
    // （device status 暂不归 0），直到结果到达、收回控制权。这样 guest 在
    // reset 完成后读到的 block 状态，不会再被 reset 之前提交的请求改变；
    // 规范要求 reset 本身不改变 block 状态。回答不再写回已被 reset 的队列。
    fn reset(&mut self);
}

// ---------------------------------------------------------------------------
// §7.4 消息与 PendingMemory：设备循环不能自锁
// ---------------------------------------------------------------------------

// 消息: WindowControl → MemoryRuntime。字段与构造器私有，只能由 WindowControl 构造。
struct MemoryRequest {
    device: MemoryDeviceId, // 由 WindowControl 盖章
    window: WindowId,       // 由 WindowControl 盖章
    op: WindowOp,
    reply: ReplyTo<MemoryResult>,
}

enum WindowOp {
    // target_limit：设备**当时**允许的目标上限。服务用自己的已插入集合
    // 与该上限判定，不保存第二份 requested_size。
    Plug { range: GpaRange, target_limit: ByteLen },
    Unplug { range: GpaRange },
    UnplugAll,
}

impl MemoryRuntime {
    // 接收端校验，不信任消息自称的身份：Window 是否仍存在，
    // WindowDesc.owner 是否仍是 Device(device)。不符则拒绝，不产生副作用。
    fn accept(&mut self, req: MemoryRequest) -> Result<OperationId, RejectReason>;
}

// 设备把它存进 ControlState::Pending，然后**继续处理**已有 I/O 完成与控制事件：
// 回复在设备循环里与其他事件一起轮询，不是嵌套 await。否则释放租约的完成事件
// 无人消费，屏障永远等不到排空（doc §9.3）。
//
// 线程: 由设备的单一 !Send owner 持有，不 Clone。
// 失败: 丢弃它**不会**取消已提交的操作（操作属于服务，doc §9.4），
//       只会让设备永久失去这个 Window 的控制权 —— 状态未知时这是正确的退路。
struct PendingMemory {
    operation_id: OperationId,
    control: WindowControl, // 停在途中
    reply: OneshotReceiver<MemoryResult>,
}

impl PendingMemory {
    // 消耗: 结果到达后交还控制权。
    fn finish(self, result: MemoryResult) -> (WindowControl, MemoryResult);
}

// 关键顺序（doc §9.3）：
//   1. 取租约读 guest 请求；
//   2. **复制**为拥有的地址/长度/命令值；
//   3. 释放数据租约；
//   4. 才提交 MemoryRequest。
// 带着目标 Window 的 queue/descriptor 租约去等内存服务，是自己阻止屏障完成。
struct OwnedRequestValues {
    range: GpaRange,
    command: GuestCommand,
}

// §8.2 ChangeOutcome 给设备的投影。
// 只在该 Window 的 Population 已经确定时回复；新值已先发布到 WindowStatusReader，
// 所以结果本身不携带状态。Population 尚不能确定时不回复：设备保持 Pending，
// guest 请求保持未完成；CPU 可见状态无法恢复时按 §8.2 暂停整机。
struct MemoryResult {
    outcome: MemoryOutcome,
    // 有副作用未证实可释放时，指向服务中的 StalledChange（§8.3），仅供诊断。
    // 对设备而言请求已结束；服务之后的保留与重试不得再改变 Population ——
    // guest 已经按它得到回答。
    retained: Option<OperationId>,
}

enum MemoryOutcome {
    Done,                   // 请求生效
    Rejected(RejectReason), // 验证失败，未产生副作用；含迁移冻结期的 Busy（doc §12.7）
    Failed(ChangeError),    // 请求未生效，旧 Population 仍有效
}

// 已接收的操作属于 MemoryRuntime，**不属于**这个 oneshot（doc §9.4）。
// 调用方取消、丢弃回复或进入 Busy(pause)，都不会 drop 正在撤销的资源。
struct ReplyTo<T> {
    inner: OneshotSender<T>,
}

// ===========================================================================
// §8 变更、失败与保留
//   doc §10、§11
// ===========================================================================

// ---------------------------------------------------------------------------
// §8.1 MemoryChange：一个操作的具体未完成状态
// ---------------------------------------------------------------------------

// 线程: 由 MemoryRuntime 独占（§3.2 changes）。等待外部调用期间，
//       操作所有权**留在服务里**，不跟着回复通道走。
//
// 不是通用 saga / 事件溯源 / 永久事务日志：一次只持有一个操作的具体资源，
// operation_id 只用于对账，不创造无限历史表（doc §10.1）。
struct MemoryChange {
    operation_id: OperationId,

    // 新方案成为事实之前，**不能丢掉** before 里的资源。
    before: BeforeFacts,
    proposal: Proposal, // 已验证的新纯值

    // 下面三项是"真实资源"，不是进度数字：
    retained: RetainedResources,
    closed_windows: Vec<AdmissionClosed>,
    applied: AppliedEffects,

    pending: Vec<PendingExternal>,
}

struct BeforeFacts {
    windows: Map<WindowId, Population>,
    layout: MemoryLayout, // 受影响部分的旧纯值
}

enum Proposal {
    AddWindow { desc: WindowDesc, grant: GpaGrant },
    RemoveWindow { window: WindowId },
    ChangePopulation { window: WindowId, next: Population },
}

// 新旧 backing 与 grant 都留在这里，直到确定可以释放。
struct RetainedResources {
    backings: Vec<Arc<HostBacking>>,
    grants: Vec<GpaGrant>, // 撤销完成才交还机器分配 owner（§1.2）
}

// applied **不能**只有一个整数进度（doc §10.1）：
// 它保存可用于补偿或继续的真实注册范围、handle 与保活关系。
struct AppliedEffects {
    cpu_bound: Vec<SlotId>,
    cpu_unbound: Vec<CpuUnbound>,
    dma_applied: Map<DmaDomainId, Vec<MappingId>>,
    dma_unmapped: Map<DmaDomainId, UnmapReceipt>,
    discarded: Vec<ObjectRange>, // 不可逆，最后做（§8.4）
}

struct PendingExternal {
    domain: Option<DmaDomainId>,
    what: PendingKind,
}

// ---------------------------------------------------------------------------
// §8.2 三种失败，不同后果
// ---------------------------------------------------------------------------

// 外部操作从未承诺跨 KVM / 多个 DMA 域原子成功。
// **补偿成功才能声称未改变**（doc §10.2）。
enum ChangeOutcome {
    // 1) 提案验证失败，尚未产生副作用 → 旧状态不变。
    Rejected(RejectReason),

    // 2) 副作用失败，补偿已完成 → 重新开放旧状态。
    FailedCompensated { error: ChangeError },

    // 3) 部分副作用无法补偿 → 保留。
    Stalled(StalledChange),
}

impl MemoryChange {
    // 消耗: self。三条出口都必须交代资源去向，没有"直接 drop"这条路。
    fn settle(self) -> ChangeOutcome;
}

// 判断是否必须暂停整机的依据：**guest 已接受的 CPU 可见状态**能否恢复
// （doc §10.2）。不是"失败就退出 guest"。
enum CorrectnessImpact {
    // 无法恢复 guest 已接受的 CPU 可见映射或内容 → 暂停并保留诊断/恢复路径，
    // **不自动销毁 VM**。
    MustPause,
    // 新区域的多余映射、回收失败等：资源仍被保留且既有契约未破坏 → 局部处理。
    LocalOnly,
}

// ---------------------------------------------------------------------------
// §8.3 StalledChange：有 owner 的"未完成"
// ---------------------------------------------------------------------------

// 线程: 由 MemoryRuntime 独占（§3.2 stalled）。
//
// 它关联**受影响的** Window 与外部域，而不是把服务永久停在一个全局
// Busy future 里（doc §10.3）：无交集的布局操作仍可被验证和处理。
//
// 不能用 timeout / free / drop 抹掉它。可以重试完成或隔离消费者。
struct StalledChange {
    operation_id: OperationId,

    // 哪个外部状态未确认 —— 评审要求每个 StalledChange 都能回答这一点
    // （doc §15.2 末条）。
    unconfirmed: Vec<PendingExternal>,

    // 全部未证实可释放的资源。保留的 grant **不**返还机器分配 owner；
    // 保留的对象切片**不**改变用途。
    retained: RetainedResources,
    closed_windows: Vec<AdmissionClosed>,
    applied: AppliedEffects,

    scope: StalledScope,
    impact: CorrectnessImpact,
}

// 若 before/proposal 无法分解出独立范围，说明这项操作必须扩大隔离范围，
// 不能凭"局部故障"口号继续（doc §10.3）。
struct StalledScope {
    windows: Set<WindowId>,
    domains: Set<DmaDomainId>,
}

impl MemoryRuntime {
    // 与 stalled.scope 无交集 → 正常处理；
    // 涉及同一未确认硬件域 → 局部拒绝，不是整机 Busy。
    fn admits(&self, proposal: &Proposal) -> Result<(), BlockedByStalled>;
}

// 未解除的范围禁止这些动作（doc §10.2、§11.3）：
// discard、清零、重置内容、交给新用途、清零后重插。
// 这个类型故意只有"禁止"一侧，没有 force 变体。
struct ReuseForbidden {
    ranges: Vec<ObjectRange>,
    reason: UnconfirmedRevocation,
}

// ---------------------------------------------------------------------------
// §8.4 逻辑 unplug 与物理回收分开
// ---------------------------------------------------------------------------

// 顺序固定（doc §10.4）：先让访问/注册状态满足已提交的逻辑结果，
// 再做不可逆的 discard。**不能**在可能回滚成 plugged 时先打洞。
enum UnplugStage {
    LogicalAccepted { window: WindowId, next: Population },
    PhysicalReclaimed { ranges: Vec<ObjectRange> },
    // discard 失败 → 逻辑仍是已拔出，只是驻留资源没回收。
    ReclaimDeferred { retry: Vec<ObjectRange>, error: DiscardError },
}

// ---------------------------------------------------------------------------
// §8.5 Drop、panic 与泄漏：最后防线
// ---------------------------------------------------------------------------

// 正常停止走显式排空/撤销路径，**不靠 Drop 执行异步协议**（doc §10.5）。
//
// 资源类的 Drop 只能在确定已撤销时释放 backing。无法证明时的选择是
// **保留 / 泄漏**，而不是释放：
//
//   impl Drop for HostBacking {
//       fn drop(&mut self) {
//           if !self.revocation_proven() {
//               self.leak(); // 泄漏不是恢复成功，但释放会让旧 DMA 命中新用途
//               return;
//           }
//           self.unmap();
//       }
//   }
//
// 内存服务任务不可在超时后直接 abort —— 必须保留已接收操作与其资源。
// 服务也不能从一份未证实的区域记录自动重启并重新发放权限：
// 本稿不承诺无监督的进程内 crash recovery。

// ===========================================================================
// §9 快照与迁移
//   doc §12
// ===========================================================================

// ---------------------------------------------------------------------------
// §9.1 快照字段：没有独立的 MemoryImage runtime 层
// ---------------------------------------------------------------------------

// 纯值。机器快照**直接拥有**这些字段，不经过一个 MemoryImage 管理器
// 或序列化入口 —— 那一层没有自己的运行期不变量（doc §12.1）。
//
// 不保存: HVA、host fd、KVM slot、DMA handle、源 VM 引用（doc §12.3）。
struct MemorySnapshotFields {
    layout: MemoryLayout,                       // 版本化
    population: Map<WindowId, Population>,      // 已接受的插入集合
    contents: Vec<ObjectContents>,
    host_policy_source: HostPolicySet,          // 仅用于诊断/显式替换
}

// requested_size 属于**设备**协议快照，不在上面（doc §12.3）。
struct MemoryDeviceSnapshot {
    id: MemoryDeviceId,
    requested_size: ByteLen,
    usable_region_size: ByteLen,
    features: NegotiatedFeatures,
}

impl MemoryRuntime {
    // 消息: 需要一个已证明的静止切面，不是随时可取（§9.5）。
    fn capture(&self, quiesced: &QuiescedSnapshotPoint) -> MemorySnapshotFields;
}

// ---------------------------------------------------------------------------
// §9.2 ObjectContents：内容来源与坐标
// ---------------------------------------------------------------------------

// 内容以 ObjectId + 对象内 offset 标识。一个 backing 被两个 GPA Window
// 使用时，不把 guest 地址洞搬进内容文件，也不重复传一份数据（doc §12.3）。
struct ObjectContents {
    object_id: ObjectId,
    extents: Vec<ContentExtent>,
}

struct ContentExtent {
    object_offset: ObjectOffset,
    len: ByteLen,
    source: ContentSource,
    source_offset: SourceOffset, // != object_offset，需要显式映射
}

// 缺口**不能**默认解释为零（doc §12.2）：
// "稀疏空洞" 与 "本次增量未发送、沿用父代" 是不同语义。
enum ContentSource {
    SnapshotBytes,
    ExplicitZero,                        // 协议上确实不需要恢复内容
    Parent { id: IncrementalParentId, integrity: IntegrityProof },
    Persistent { id: PersistentSourceId, integrity: IntegrityProof },
    Pager { id: PagerSourceId },
}

impl ObjectContents {
    // 每个需要恢复的对象内范围都必须有确定来源。
    // 失败: 不支持某种来源时**拒绝恢复**，不回落成全零内存（doc §12.2）。
    fn check_complete(&self, desc: &ObjectDesc) -> Result<(), ContentGap>;
}

// 未插入的 volatile 块可以记为 ExplicitZero；
// 这条**不适用于** pmem —— 持久对象必须有可验证来源。
fn zero_fill_allowed(contract: &ContentContract) -> bool {
    matches!(contract, ContentContract::VolatileRam)
}

// ---------------------------------------------------------------------------
// §9.3 DirtyEpoch / TransferSet：工作状态，不是 Population
// ---------------------------------------------------------------------------

// 线程: 由 MemoryRuntime 持有（§3.2 dirty）。一个 epoch 代表**一轮**
//       受支持的写入追踪，绑定布局版本与全部写入参与者。
//
// CPU、软件设备、直通设备是不同的 dirty 生产者；
// **不能以 KVM dirty bitmap 冒充全部写集**（doc §12.4）。
struct DirtyEpoch {
    epoch: AccessEpoch,
    layout_revision: Revision,
    producers: Vec<DirtyProducer>,
    // 初版 precopy 期间固定 Layout 与 Population（doc §12.7）：
    // 新热插与 block 插拔返回 busy/retry，普通 I/O 继续。
    topology: TopologyFreeze,
}

struct AccessEpoch(u64);

enum DirtyProducer {
    Cpu { vm: Arc<VmHandle> },
    Software { set: Arc<DirtySet> },
    Passthrough { domain: DmaDomainId, caps: VfioMigrationCaps },
}

// 只有 dirty logging 不够（doc §12.6）：缺任一能力拒绝 live migration。
struct VfioMigrationCaps {
    device_state: bool,
    dirty_tracking: bool,
    stop: bool,
}

// 迁移会话的工作状态。三件事分开，不混为一个 bitmap（doc §12.4）。
struct TransferSet {
    epoch: AccessEpoch,
    dirty: Map<ObjectId, Ranges<ObjectOffset>>,   // 需要传
    sent: Map<ObjectId, Ranges<ObjectOffset>>,    // 已发送
    received: Map<ObjectId, Ranges<ObjectOffset>>, // 目的侧已接收
}

// ---------------------------------------------------------------------------
// §9.4 WriteAuthority / DirtySet：写入完成决定记脏时间
// ---------------------------------------------------------------------------

// 反例（doc §12.5）：提交磁盘读入 guest 时 P 被标脏 → 迁移复制 P 并清位
// → 内核**之后**才写 P → 最终 bitmap 不含这次软件写入。
//
// 修订：记脏责任属于**实际完成 owner**，不属于提交者。
//
// 线程: 不可 Clone，随 I/O 移交到真正完成它的执行者。
// 失败: 不能靠"租约在结束时 drop"自动成立，除非该 drop 确实与外部 I/O
//       完成绑定。等待 future 被取消、io_uring cancel 请求已发出，
//       **都不足以证明**完成（doc §12.5）。
struct WriteAuthority {
    lease: MemoryLease,
    object: ObjectId,
    range: ObjectRange,
    // 取得时的代次。但上报要进入**当前** epoch，不能只引用这个旧值。
    acquired_epoch: AccessEpoch,
    sink: Arc<DirtySet>,
}

impl WriteAuthority {
    // 在真正完成之后调用，上报**实际写入**的范围。
    // 消耗: self —— 上报一次，然后许可结束。
    fn complete(self, written: ObjectRange) -> DirtyReported;

    // 提交时的保守标记是允许的"提前记录"，但**不能取代** complete()。
    fn mark_optimistic(&self);
}

// 软件写入的汇合点。
//
// 线程: Arc 共享，多个完成 owner 并发上报。
// 未定: bitmap 的发布/清位与完成上报之间的内存序**尚未证明**。
//       这是必须用原型验证的边界，不是写在注释里就成立的同步。
struct DirtySet {
    object: ObjectId,
    bits: AtomicBitmapLike,
}

impl DirtySet {
    // 迁移开始前已提交、开始后才完成的写入，也必须进入当前追踪 ——
    // 换 epoch **不能**丢掉它们的责任（doc §12.5）。
    fn record(&self, range: ObjectRange, epoch: AccessEpoch);

    // 顺序固定：采集并清位发生在相应字节**复制之前**；
    // 复制期间与之后完成的写入留到下一轮。两者不能颠倒。
    fn take_and_clear(&self, range: ObjectRange) -> Ranges<ObjectOffset>;
}

// ---------------------------------------------------------------------------
// §9.5 最终切面
// ---------------------------------------------------------------------------

// 不可 Clone（§0.2）：它是五项证据的组合，不是一个 bool。
// 内存服务**不等待**一个已经在等它的设备给自己发送新视图 ack（doc §12.6）。
struct QuiescedSnapshotPoint {
    vcpus_paused: VcpuPaused,
    devices_drained: Vec<DeviceDrained>,  // 设备在自己的循环里停新 I/O 并排空旧 I/O
    changes_settled: ChangesSettled,      // 已接收的内存变更排空（§8.1）
    writers_drained: Vec<DirtyReported>,  // 全部 dirty 生产者同步（§9.4）
    generation: AccessEpoch,              // 静止结果带代次，不用"收到 pause 消息"代替
}

// 跨进程 handoff 的运行权由机器迁移协议决定，**不属于内存分配器**。
// 内存恢复数据不是避免双运行的凭据（doc §12.6）。
struct RunPermission {
    granted_by: MachineHandoffProtocol,
}

// ===========================================================================
// §10 总表
// ===========================================================================

// §10.1 线程归属
//
// | 结构 | 归属 |
// |---|---|
// | MemorySpec / Planned / MemoryLayout / HostPolicySet | 纯值，可 Clone、可跨线程 |
// | AddressClaim | 纯值（重放数据） |
// | GpaGrant | 权力：不可 Clone/Copy，移动即转移 |
// | UnboundMemory | 不可 Clone；未绑定 VM 时可跨线程移动 |
// | AttachedMemory / MemoryRuntime | 单线程独占，!Send 可接受，不 Clone |
// | MemoryStore / WindowState / CpuBindings / MemoryChange / StalledChange | 随 MemoryRuntime，无独立线程身份 |
// | HostBacking | Arc 共享保活（Send+Sync）；但不提供 &mut [u8]，字节访问经 lease |
// | BackingExport | 不可变值 + 保活引用，可随消息跨 owner |
// | RamReader | Clone + Send + Sync，任意多读者 |
// | RamViewPublisher | 仅 MemoryRuntime 持有，不 Clone |
// | RamView / Population / WindowStatus / AllowedRam / DomainMap | 不可变发布值，Arc 共享 |
// | WindowAccess | Arc 共享 + 原子状态；原子布局与内存序**未定** |
// | MemoryLease / LeaseSet / WriteAuthority | 不可 Clone，经显式接口移交给完成 owner |
// | DirtySet | Arc 共享 + 原子 bitmap；内存序**未证明** |
// | SlotPermit | 不可 Clone/Copy（否则出现第二份注册权） |
// | DmaProjection | 一个外部域的单一 owner，串行；执行者边界**未定** |
// | ActualMappings | 随 DmaProjection |
// | WindowControl / PendingMemory / MemoryDeviceProtocol | 设备的单一 !Send owner；WindowControl 不可 Clone，在途时停在 PendingMemory 内 |
// | WindowStatusReader | Clone + Send + Sync，只读一个 Window 的发布状态 |
// | 各类 *Evidence / *Receipt / *Quiesced | 不可变但**不可 Clone** |
// | 快照与迁移纯值（MemorySnapshotFields / ObjectContents / TransferSet） | 纯值 |

// §10.2 消耗（移动后原值不再可用）
//
//   HostPolicy + ObjectDesc      --materialize-->  ObjectId（Store 内记录）
//   GpaGrant                     --进入-->          UnboundMemory.grants
//   UnboundMemory + VmHandle     --attach-->        AttachedMemory
//   AttachedMemory + TeardownEvidence --detach-->   UnboundMemory
//   SlotPermit + Arc<HostBacking>--bind-->          CpuBinding
//   MemoryLease                  --into_write_authority--> WriteAuthority
//   WriteAuthority               --complete-->      DirtyReported
//   WindowControl                --plug/unplug/unplug_all--> PendingMemory
//   PendingMemory + MemoryResult --finish-->        WindowControl
//   MemoryChange                 --settle-->        ChangeOutcome
//
// §10.3 构造（谁有权产生）
//
//   MemorySpec::plan              → Planned（纯函数）
//   MemoryRuntime                 → RamReader / WindowControl + WindowStatusReader / DmaConsumer
//   WindowControl（唯一）         → MemoryRequest（盖章 device + window）
//   MemoryRuntime                 → BackingExport、AllowedRam
//   MemoryRuntime（唯一）         → RamView 的新版本
//   WindowAccess::acquire（唯一） → MemoryLease
//   SlotPermitPool                → SlotPermit
//   WindowState 提交              → WindowStatus（派生，不反向决定事实）
//   MemoryRuntime::capture        → MemorySnapshotFields
//
// §10.4 消息与回执（跨 owner，不是函数调用）
//
//   WindowControl --MemoryRequest{device, window, op}--> MemoryRuntime（accept 校验）
//   MemoryRuntime --MemoryResult{outcome, retained}-->  PendingMemory
//   MemoryRuntime --WindowStatus 发布-->           WindowStatusReader
//   MemoryRuntime --AllowedRamUpdate{revision}-->  DmaProjection
//   IOMMU owner   --DomainMapUpdate{revision}-->   DmaProjection
//   DmaProjection --ProjectionAck{operation}-->    MemoryRuntime
//   完成 owner    --DirtySet::record-->            当前 DirtyEpoch
//   机器迁移协议  --RunPermission-->               目标机
//
// §10.5 失败时资源留在谁手里
//
//   物化失败                → 无残留（尚未产生 guest 可见事实）
//   bind 失败               → SlotPermit 随错误返回，无部分注册
//   unbind 失败             → CpuBinding 留在 active，permit 不归还
//   acquire 遇 Revoking     → 释放本次已取租约，调用方重试
//   设备丢弃 PendingMemory  → 操作照常由服务完成，设备永久失去该 Window 控制权
//   回放失败                → DmaProjection 仍拥有已创建映射，须撤销或隔离
//   补偿成功                → 旧状态重新开放
//   补偿失败                → StalledChange 持全部资源 + 关闭凭据
//   discard 失败            → 逻辑已拔出保持，记录可重试回收范围
//   panic / 任务意外消失    → 保留/泄漏 backing，不释放

// ===========================================================================
// 本文件的边界
// ===========================================================================
//
// 这些签名表达的是**第 5 轮候选的所有权与时间关系**，不是已验证的实现：
//
//   - WindowAccess 的原子布局、DirtySet 的内存序、DmaProjection 的执行者
//     边界都标为未定。把它们写成类型不等于证明了同步正确。
//   - 没有 `unsafe impl Send/Sync` —— 哪些类型真能跨线程，取决于尚未写出的
//     内部实现，不由这份附件宣布。
//   - 租约与第三方 queue / 异步 I/O 的无额外拷贝组合性，仍需最小原型证明
//     （doc §15.2 第一条）。这是本设计最可能被否决的部分。
//
// 参考实现证明的是**需求存在**（doc §16），不证明这里的机制成立。
