# 新内存设施：设计草稿

状态：第 5 轮工作稿，经过结构反例与两份独立审阅，仍有明确未决项。本文是草稿纸，不是实施计划，也不替代现有设计。只依据 AGENTS.md 与参考项目调研；不读取当前 VMM 代码或旧设计，不做迁移补丁。

## 1. 目标与评价方式

范围最接近 Cloud Hypervisor：现代 microVM，以 x86-64/KVM/Linux 为起点，涵盖多 backing、host/guest NUMA、virtio-mem、现代内存设备热插拔、vhost-user、VFIO、virtio-IOMMU、快照及迁移。高级消费者应能接入共同内存事实，但不要求一次实现全部能力。

不追求 QEMU 的任意地址树、优先级遮挡、PAM、传统 ROM 切换或完整机器仿真。不以“以后也许需要”为理由引入通用插件框架。机密内存的 private/shared 转换是单列扩展，不承诺本稿已解决；设计不能把 host 可见字节当作永久前提。

判断标准不是类型数量、完成勾选或形式上的不可变性，而是专家是否认可以下问题的答案：

1. 同一份字节被配置、设备、KVM、DMA、迁移分别看待时，身份和权威是否清晰？
2. 加入一个真正的新消费者，是否需要修改所有旧设备或增加第二个万能 manager？
3. 区域撤销、页回收、DMA 撤销和 VM 销毁的区别是否进入数据模型？
4. 失败与取消留下的资源是否仍有明确所有者，而不是一句“回滚”掩盖？
5. 热路径是否为控制面形式美承担不合理代价？
6. 哪些能力刻意不支持，边界能否被类型或构建验证表达？

保留关键否决与未决项。迭代结束只意味着草稿可供评判，不意味着设计已获认可。

## 2. 事实分层

| 事实 | 坐标与身份 | 权威候选 |
|---|---|---|
| 存储对象 | ObjectId、对象内 offset | MemoryStore |
| guest 放置 | WindowId、GPA、对象内 offset | MemoryLayout |
| host 映射 | HVA、fd、文件 offset、页粒度 | HostBacking |
| CPU 映射 | VM 身份、KVM slot、GPA/HVA | CpuBindings |
| 软件 DMA 读视图 | 查址表版本 | RamView |
| 外部 DMA | endpoint/domain、IOVA/GPA、pin 范围 | 各 adapter 的成功登记 |
| 区域内插入状态 | WindowId、block index | Population |
| 恢复与迁移 | 对象身份、布局、传输页集合 | 机器快照中的内存数据 / 迁移会话 |

“不止一个权威”不等于允许同一事实被多处独立修改。权威按事实划分；派生登记也要保存真实成功结果，因为外部系统可能只完成了部分操作。

### 2.1 当前候选的最短读法

```text
纯值：MemorySpec → 已选 MemoryLayout + HostPolicy

唯一控制 owner：MemoryRuntime
├─ Store：ObjectId → HostBacking
├─ Layout：WindowId → ObjectSlice + GPA + owner + guest 属性
├─ WindowState：已接受插入集合 + revision
├─ CPU 投影：唯一绑定权与实际成功登记
├─ MemoryChange / StalledChange：未结束副作用及保留资源
└─ RamView 发布权、外部 DMA 消费者成员关系

软件设备：RamReader → RamView → WindowAccess → MemoryLease → 字节
内存设备：自身协议状态 + 受限 WindowControl + 派生 WindowStatus
外部 DMA 域：DmaProjection（AllowedRam ∩ DomainMap）→ 实际映射
机器快照：Layout + Population + 对象内容清单；不序列化 runtime owner
```

RamView 只供软件 DMA 使用，不是完整机器地址空间，也不暴露所有 CPU 可见的 pmem/设备内存。CPU RAM/pmem 投影与软件 DMA 视图均派生自 Layout，但具有不同准入条件；MMIO 总线仍是独立访问语义。不同索引有明确派生关系，不各自成为布局分配器。

名称是工作名。下文第 3 节随迭代修订为当前候选；第 4 节是首轮被否决的假设；第 6 节及末尾保留变化理由。第 14 节进一步限定了 DMA 复用和异步写入的时间边界。新增类型若只能解释名词而不能表达独有不变量，应在后续评判中删除。

## 3. 当前候选关系（起于第 1 轮，后续持续修订）

```text
MemorySpec --纯规划--> MemoryLayout
                         | objects、windows、address claims
                         v
                    MemoryRuntime（独占控制所有者）
                    /       |         \
             MemoryStore  CpuBindings  DMA adapter handles
                 |          |             |
             HostBacking <--保活引用------+
                 ^
                 | 保活引用
              RamView <--发布/共享-- 软件设备

内存设备 --范围请求--> MemoryRuntime
MemoryRuntime --结果--> 内存设备的协议状态
MemoryRuntime --结构投影--> 机器快照的内存字段
```

MemoryRuntime 只协调内存事实与其外部投影，不拥有整个设备，不执行普通队列 I/O，不接管 IRQ、总线或设备任务调度。设备 future 继续单一所有者，可为 !Send，运行在 LocalRuntime。

### 3.1 MemoryLayout：可重放的纯值

概念形状，不是承诺的 Rust API：

```text
MemoryLayout {
    objects: ObjectId -> { capacity, content_contract },
    windows: WindowId -> { object_slice, gpa, owner, kind, guest_numa },
    claims: guest 地址空间的预留描述,
}
HostPolicy {
    object: ObjectId,
    allocation / host_numa / hugepage / reserve / prefault,
}
```

Object 是逻辑字节身份，不等于一次 mmap 或一个文件 fd。目标机换 backing 后其逻辑身份仍可保持。Window 是其 guest 放置；一 Object 可以因 32 位地址洞分成两个不重叠 Window，不要求 backing 也有洞。一个 Object 内采用一致的宿主物化政策；需要不同政策时拆 Object，不建对象内的通用政策树。

初版拒绝两个 Window 指向重叠的对象内范围，避免无需求的 guest RAM alias。允许同一 Object 的不重叠切片。物化时还要检查本 MemoryStore 内已打开的可写共享来源：相同文件实体的重叠 offset 不能伪装成两个 Object 绕过限制，路径不同也不够证明独立。只读来源或私有 CoW 映射不是同一种可写 alias；不能一概拒绝。此检查不假装能枚举其他进程的映射，外部宿主管理的共享约束要明确声明。

范围用半开区间；GPA、IOVA、对象内 offset、字节数有不同类型。ObjectSlice 的长度同时定义 Window 覆盖长度，避免两个长度可独立修改。owner 为启动内存或明确 MemoryDeviceId，不用 Role 冒充设备身份。kind 区分启动 RAM、整区可插拔 RAM、块级 RAM（含已协商 block_size）、持久内存 aperture；WindowControl 的作用域由 owner 与 WindowId 固定。

Reserved claim 不产生 RAM。PIO/GSI 与 GPA 分配域不同。本组件只接受机器地址分配 owner 已选出的描述与不可复制的 GpaGrant，不自行维护第二个可与 MMIO 冲突的 GPA 分配器。Layout 中可复制的 claim 是重放数据，不是 grant 权限。撤销完成才交出可释放 grant；StalledChange 不交出。服务无须等 vmm-main 消费返还 grant 才结束自己的操作。

guest 位置、容量、设备关系与协议粒度进入布局；HVA、fd、KVM slot、host IOMMU handle 不进入。HostPolicy 是独立物化输入：可以记录源配置用于诊断，但恢复不得把源 host NUMA 编号当 guest 结构原样执行。目标策略是保持、显式替换或不支持，不能默默重算 guest GPA/NUMA。

### 3.2 MemoryStore / HostBacking：唯一分配权与共享字节

MemoryStore 独占 ObjectId 到 backing 的记录。HostBacking 拥有映射、文件及释放责任；引用只保活，不获得布局修改权或 KVM 注册权。

backing 政策与 runtime backing 分开：匿名私有 RAM、可导出的共享 memfd、用户文件的共享/私有映射。共享、hugepage、reserve、prefault、host NUMA 是政策维度，不为每种组合创造一个 enum 变体。

持久性、可导出性和可丢弃性不是一个标志。共享 memfd 不等于持久存储；私有文件映射不把写入持久化到原文件；pmem 的 flush/durability 契约由该设备与存储后端提供。Object 的 content_contract 记录 volatile RAM 或具体持久来源与一致性要求，HostBacking 的能力决定该组合能否物化。virtio-mem/balloon 的破坏性 discard 不得落到需要保留内容的 pmem 对象。

文件 offset 不等于 GPA。导出能力不是所有 backing 的共同能力；vhost-user 在配置边界要求可导出的共享 backing，不能运行后遇到缺 fd 才静默改成复制路径。HostBacking 不向普通设备公开原始 mmap owner，RamView 也不直接暴露可绕过 WindowAccess 的 vm-memory 集合。同步访问适配器与异步 I/O 指针接口都必须借自 MemoryLease；是否能与既有第三方 queue/I/O API 无额外拷贝组合，仍需原型证明。

### 3.3 RamView / WindowAccess：索引版本与访问许可分开

MemoryRuntime 从 Layout 与 Store 派生当前查址表。消费者仅拿读入口，写发布入口只有一份。采用 ArcSwap 式版本发布；集合替换不复制字节，不自动等待旧读者。

RamView 是不可变索引，不是内容快照，也不拥有 grant、slot 或外部 DMA 权限。每项引用 HostBacking 及一个显式的 WindowAccess；后者不是“整台 VM 内存的锁”，只处理这一个 Window 的访问准入、在途计数及当前可访问范围。

```text
RamReader --共享发布入口--> RamView（版本）
RamView.entry --保活--> HostBacking
RamView.entry --引用--> WindowAccess { admission, inflight, current_population }
请求 --查址、取得许可--> MemoryLease --保活--> 所用 backing 与访问许可
```

软件设备可以长期缓存索引项，但不能长期缓存一份脱离 MemoryLease 的可解引用 HVA。取得租约后才解析可访问范围和 host slice；租约覆盖异步 I/O 的真实完成时间，不是提交 syscall 的时间。关闭准入与登记在途必须形成一个原子判定，不能用两个独立 load 拼成有竞态的检查。

拓扑撤销时永久关闭旧 WindowAccess。旧 RamView 可以继续存在，甚至保活旧 mapping，但不能再创建该 Window 的新访问。相同 GPA 重新使用时创建新 WindowId 与准入对象，旧表不能通过地址相同绕过撤销。

区域内部 Population 改变时暂时关闭这一 Window，等待其在途租约释放，再发布新 Population 并重开；旧索引访问也须读取新的准入状态，而不是使用表里旧 bitmap。初版故意以整个 Window 为撤销屏障，不引入每 block 的原子计数或通用区间锁。代价是插拔一个 block 也会短暂干扰该 Window 上的软件 I/O；按租约/批次计数，不按字节计数。

只对数据访问做此准入。设备读取容量、查询操作状态等控制行为不取得字节租约，因此内存操作等待在途 I/O 时不需要反过来等待同一设备的配置 ack。

### 3.4 CpuBindings：CPU 投影

绑定保留 backing 与 VM，独占 slot 使用权。slot 是运行期编号；绑定完成才记入成功表。Window 与 slot 不一一对应，绑定切分受 KVM 预算和设备粒度影响。

VM 不需要拥有整个权威区域集合；它必须通过成功绑定保住映射，关闭 VM 前不释放仍可访问的 backing。safe 层不能用可复制 slot 编号构造第二份注册权。

### 3.5 WindowState：分配与插入事实唯一，设备协议仍由设备拥有

virtio-mem 的固定容量 Window 不因每个 block 插拔而创建/删除。MemoryRuntime 独占 WindowState：具体已插入位置、Population revision、当前成功 CPU/DMA 登记及未结束变更。Population 是不可变的 block 集合值，转移产生新值；bitmap 是其具体表示，不是第二份 KVM 或驻留 bitmap。

内存设备独占 requested_size、协商特性、队列及 guest 请求状态。不另拥有可独立修改的 plugged bitmap/计数；guest 配置中的实际 plugged_size 从只读 WindowStatus 取得。WindowStatus 是 WindowState 提交时发布的计数/revision 投影，不用于反向决定资源事实。

一次 plug 命令包含设备当时允许的目标上限；MemoryRuntime 根据自己的已插入集合与该上限决定请求能否接受。它不保存另一份 requested_size。设备处理请求期间串行处理该协议的目标更新，避免上限与请求交错；内存控制面不把目标容量强制变成已插入事实。

设备在读 guest 请求后，复制为拥有的地址/长度/命令值并释放数据租约，再提交资源请求。不能带着目标 Window 的 queue/descriptor 租约等待内存服务，否则自己阻止屏障完成。响应写入重新取租约；guest 把响应队列本身拔掉属于无效使用，必须报告访问失败，不能继续解引用缓存 HVA。

设备循环把这项请求保存在 PendingMemory { operation_id, reply } 中，同时继续处理已有 I/O 完成与控制事件；不能用一个阻塞整个设备循环的嵌套 await，让释放租约的完成事件无人消费。pause 关闭新业务提交，不关闭排空/完成/对账能力。仍然只有一个 !Send 设备 owner，不把整个对象交给第二个任务或 Mutex。

操作已接收后属于 MemoryRuntime，不属于回复 oneshot。调用方取消、丢弃回复或进入 Busy(pause) 都不能 drop 正在撤销的资源；设备状态导出必须等已提交请求得到确定结果且所有写授权排空，再导出自己的协议状态。静止结果带当前代次，不能用收到 pause 消息代替排空证据。窗口状态与设备状态各有一个所有者，暂停屏障取得的是两者一致的切面，不靠互相持有整个设备锁。

### 3.6 DMA 与迁移

外部 adapter 分开维护：KVM GPA/HVA 投影、VFIO IOVA/HVA/pin 投影、vhost-user fd/offset 导出。IOMMU 的域内翻译不是 RAM 分配。

迁移结构分开保存 Layout/Population 与内容传输集合。CPU、软件设备、直通设备是不同 dirty 生产者；迁移必须汇合全部参与者，不能以 KVM dirty bitmap 冒充全部写集。

## 4. 首轮被否决的假设（历史反例，不是当前保证）

- Arc 保活足以处理撤销：只保证不发生 use-after-unmap，不能阻止旧设备继续写入已退出 guest 的区域。
- 发送新视图并等 ack 足够：设备可能在等内存操作回复，内存操作又等该设备 ack；异步 I/O 也可能已经保存裸指针。
- Population 与设备计数自然一致：两个 owner 间成功响应丢失、取消或部分失败会打破这一点。
- 按反序调用就能回滚：VFIO unmap、KVM delete、文件 discard 都可能失败，且有些动作不可恢复原内容。
- 一个范围粒度适用于所有消费者：guest block、host hugepage、KVM slot 与 VFIO 可撤销粒度并不相同。

这些是下一轮必须改变结构或明确缩小保证的地方，不用“由调用者保证”直接抹掉。

## 5. 参考锚点

参考代码提供真实需求与反例，不证明本草稿的同步机制已经成立。

- Firecracker 的 region/slot/block：`.references/firecracker/src/vmm/src/vstate/memory.rs:403`、`.references/firecracker/src/vmm/src/devices/virtio/mem/device.rs:514`。
- Firecracker 的不保活 slice 与完成记脏：`.references/firecracker/src/vmm/src/vstate/memory_slice.rs:30`、`.references/firecracker/src/vmm/src/vstate/memory_slice.rs:111`。
- Cloud Hypervisor 的 zone、登记与发布：`.references/cloud-hypervisor/vmm/src/memory_manager.rs:108`、`.references/cloud-hypervisor/vmm/src/memory_manager.rs:190`、`.references/cloud-hypervisor/vmm/src/memory_manager.rs:247`。
- Cloud Hypervisor 的软件与外部 DMA：`.references/cloud-hypervisor/virtio-devices/src/block.rs:168`、`.references/cloud-hypervisor/virtio-devices/src/mem.rs:857`。
- QEMU 的 backing/地址拓扑：`.references/qemu/include/system/memory.h:560`、`.references/qemu/include/system/ramblock.h:25`、`.references/qemu/hw/i386/pc.c:795`。
- QEMU 的窗口坐标、状态源与 VFIO：`.references/qemu/include/system/memory.h:97`、`.references/qemu/system/ram-discard-manager.c:415`、`.references/qemu/hw/vfio/listener.c:220`。
- 共同依赖的索引发布语义：`/home/fuchengyu.fcy/.cargo/registry/src/rsproxy.cn-e3de039b2554c837/vm-memory-0.18.0/src/atomic.rs:28`。

## 6. 迭代记录

### 第 1 轮：把存储、放置与投影分开

保留：Object/Window 分离；纯值 Layout；软件视图是派生索引；外部登记不是 backing 所有权。

不成立：撤销、设备互等、双写 Population 和部分失败仍缺少可执行的所有权解释。下一轮优先解决这些，不增加 QEMU 式地址树。

### 第 2 轮：拒绝“旧视图安全＝旧访问已经撤销”

改变：增加每 Window 的访问准入与真实在途 MemoryLease；旧索引可以存在但不保留新访问权；Population 的权威移入 WindowState，设备只拥有协议事实；异步变更由独立内存服务接收后拥有。

代价：软件设备必须遵守租约边界，异步指针缓存不能直接沿用；整个 Window 的屏障可能扰动较多 I/O。暂不引入细粒度锁消除这项代价。

仍需反驳：关门等待租约是否会与设备内未完成的异步请求互等；VFIO 不能用软件租约证明已 unpin；成功资源提交与 guest 响应之间的切面、部分失败及迁移仍待明确。

## 7. 第 2 轮补充：权限与控制关系

### 7.1 不是 vmm-main 中的一个新分支

MemoryRuntime 是专门的内存服务，拥有控制资源，运行在 LocalRuntime；它的进展不依赖 vmm-main 接收下一条事件。vmm-main 可等待暂停，内存设备仍可等内存服务完成自己的已提交请求。

它不持有设备对象。软件设备保存 RamReader；内存设备额外保存仅可操作自身 Window 的 WindowControl。DMA adapter 是外部映射的拥有者，向内存服务提供专门的映射请求句柄，不能借此操作整个设备或关闭设备 worker。

进入服务前可以取消尚未接收的请求；接收后的变更必须完成、补偿或保持为一个有所有者的待恢复状态。客户端关闭不等于操作安全取消。同步 ioctl 可能阻塞；不得声称 LocalRuntime 上的 async 保证它不会阻塞，可能阻塞的外部操作需要受控执行边界，但不把 !Send 设备 future 迁移到线程池。

### 7.2 VM、backing 与绑定权限

```text
UnboundMemory（不可 Clone）
  --消耗唯一分配/注册权--> AttachedMemory（一个 VM 的 MemoryRuntime）
AttachedMemory --独占--> 地址 grant、WindowState、CPU 绑定权
CPU Binding --保活--> VM 与 HostBacking
MemoryLease --保活--> HostBacking，不具备注册权
```

backing 的 Arc 可以被多个视图保活，分配/注册权不能复制。safe API 不接受设备给来的裸 HVA/fd/slot 来新建绑定；低层直接使用这些指针注册是 unsafe 边界，不属于安全层承诺。

同进程 teardown/rebuild 必须关闭旧所有准入、结束软件 I/O、撤销外部 DMA、停止 vCPU、关闭旧 VM，才可重新取得 UnboundMemory。恢复为同一 backing 建立新的 VM 身份与准入；旧 view 即使保活同一字节，也不能借旧权限写入新 VM。

### 7.3 关闭准入不是强制取消在途 I/O

已有租约必须能完成，不能要求在途请求重新经过已经关闭的准入才能完成自己。一个请求解析到的 buffers 与完成所需的 used-ring 访问应属于同一受控 I/O 生命周期；分阶段取新内存时，允许返回可重试的撤销状态，而不是无条件等待重开。

一个 I/O 的 payload 与完成所需的 used-ring 等范围必须在提交外部 I/O 前取得完整租约集合。跨 Window 获取采用固定顺序、非阻塞尝试；任一窗口正在撤销就释放本次已取得的租约并返回可重试状态，不持一部分租约 await 另一部分。已经提交的 I/O 不再等待一个被关闭窗口的新租约来报告完成，已有租约可继续使用到该 I/O 完结。

关闭后到来的新请求不得占用阻止在途排空的设备执行位置。窗口屏障可以等待、观察进度或超时；超时只能使撤销尚未完成，不能强制把裸指针视为已失效然后释放 backing。无法取消的外部 I/O 继续保留租约；取消请求的 API 成功返回也必须按该 backend 定义确认内核/回调不再持指针。

租约不是 vCPU 访问许可，也不是 VFIO pin 的替代品。CPU 与外部 DMA 必须分别得到各自的撤销证据。

## 8. 第 3 轮：外部投影与部分失败必须拥有实际资源

### 8.1 不建立一个无差别的 MemoryListener 框架

软件 I/O 用 RamReader/MemoryLease，不接收每一次 plug 的 ack 广播。外部 DMA 使用专门的 DmaProjection；它属于一个 host DMA 地址域，不一定一设备一份，例如多个 VFIO 设备可共享一个 host IOMMU 容器。

```text
MemoryRuntime --AllowedRam 版本--> DmaProjection <--DomainMap 版本-- guest IOMMU owner
                                      |
                           独占实际映射/导出登记
                                      |
                              host IOMMU / vhost backend
```

DmaProjection 的两个输入不能合并：AllowedRam 决定存储范围可否被这个消费者使用；DomainMap 决定 IOVA 翻译及读写权限。它串行维护两个输入和实际成功映射，作用域仅是一个外部地址域，不拥有内存分配或设备协议。

没有 guest IOMMU 时 DomainMap 是显式的 identity 模式，不维护一套假的页表。存在 IOMMU 时，IOVA/GPA/对象内 offset 都保留坐标，映射不得跨越 backing 或权限边界。

它保存受限 BackingExport 的保活引用以解析 HVA/fd，不向 MemoryRuntime 同步回调取内存；否则内存服务等 DMA ack、DMA adapter 等内存查询仍会互等。新 IOMMU 映射只能与 adapter 当前 AllowedRam 相交，不能凭旧 DomainMap 把已经撤销的 Window 重新 map 回去。每个来源分别有 revision；旧 revision 的重放被拒绝，不能把两个来源误称为一次全局事务。

新 adapter 接入时，在内存服务序列中登记一个未就绪成员，给它当前窗口与可访问状态；完成回放才成为 Ready。回放失败时它仍拥有已成功创建的映射，必须撤销或隔离，不能直接删除句柄。撤销成员在实际外部状态解除之前仍属于相应资源的消费者集合。

DMA 登记以 DmaDomainId + MappingId/代次标识，不只用 IOVA。一个地址域的真实副作用串行应用；某旧范围仍有未确认 map/unmap 时，不允许该 IOVA 的新绑定越过它。迟到结果归回原操作登记，不能按地址删除“现在恰好在这里”的新映射。仅忽略旧 revision 的回复不能撤销已经发生的内核副作用。

代次只保护宿主登记，硬件 DMA 不携带这个软件代次。成功 unmap 也不能单独证明 endpoint 不会继续发送旧队列请求。host 控制的对象重用、域转移或跨 VM 移交还需该 endpoint/domain 的停止或排空证据；缺证据时不得换用途再 map 同一 IOVA。guest 自己控制的 IOMMU 映射复用受 guest 协议约束，不等于 host 获得了安全移交证明。

vhost-user 不能被假定具有 VFIO 的逐 block unmap 语义。它通常导出整段共享文件，fd 本身授权访问整份 backing，而不是某 Window 的租约。即使只发送一个 offset/length，远端仍可能保存 fd、缓存指针或映射；REM_MEM_REG/socket 关闭不能自动充当能力撤销证明。

初版 vhost-user 支持固定导出生命周期。在线删除、同一 backing 跨 VM 重用与精确 block 回收，只在 backend 的可信排空/撤销协议或外部进程终止能提供明确证据时开放；尚未验证就拒绝相关组合。AllowedRam 对普通 fd backend 是描述性输入，不能承诺硬件般强制的子范围权限。VFIO 则必须按允许范围 pin，不用整区 map 偷偷填充已拔出的块。

### 8.2 MemoryChange：一个操作的具体未完成状态

不建立通用 saga、事件溯源或永久事务日志。内存服务一次接收一个结构/Population 变更，具体持有：

```text
MemoryChange {
    operation_id,
    before: 受影响的布局/Population,
    proposal: 已验证的新纯值,
    retained: 新旧 backing 与 grant,
    closed_windows: 准入关闭凭据,
    applied: 已成功创建/删除的 CPU、DMA 登记结果,
    pending: 正在等待的外部结果,
}
```

操作 ID 用于未结束请求与回复对账，不创造无限历史表。applied 不能只有一个整数进度；它保存可用于补偿或继续的真实注册范围、handle 和资源保活关系。等待外部调用时，操作所有权留在服务中。新方案未成为事实之前不能丢掉 before 中的资源。

外部操作从未承诺跨 KVM/多个 DMA 域原子成功。补偿成功才能声称未改变；补偿失败进入具体的 StalledChange，仍持有全部未证实可释放的资源及关闭凭据。可以重试完成或隔离消费者，不能用 timeout/free/drop 抹掉它。

StalledChange 关联受影响的 Window/外部域，而非把服务永久停在一个全局 Busy future 中。无交集的布局操作仍可被验证和处理；涉及同一未确认硬件域的操作可局部拒绝。保留的 grant 不返回机器分配 owner，保留的对象切片不改变用途。若 before/proposal 无法分解出独立范围，说明这项操作必须扩大隔离范围，不能凭“局部故障”口号继续。

### 8.3 三种失败，不同后果

| 情况 | guest 与资源后果 |
|---|---|
| 提案验证失败，尚未产生副作用 | 返回拒绝，旧状态不变 |
| 副作用失败，补偿已完成 | 返回失败，重新开放旧状态 |
| 部分副作用无法补偿 | 保留 StalledChange，禁止相关 grant/backing 重用；按 CPU 可见状态决定是否必须暂停 |

VFIO unmap 失败不能仅靠保留软件 Arc 宣称撤销完成：设备仍可 DMA。必须保留它覆盖的 backing、外部登记及地址 claim；未解除的范围不得 discard、清零、重置内容或交给新用途。解除映射和结束旧用途分别取得证据，不能用“unmap 或 stop 任一成功”泛化为全部资源可释放。关闭业务队列不等于停止 DMA，发出 reset 也不等于已经停止。

若无法恢复 guest 已接受的 CPU 可见映射或内容，继续执行会破坏正确性，因此暂停并保留诊断/恢复路径，不自动销毁 VM。新区域的多余映射或回收失败，在不破坏既有 guest 契约且资源仍被保留时可以局部处理；不能把所有失败都升级为 guest 退出。

逻辑 unplug 成功与宿主页回收成功分开。先确保访问/注册状态满足已提交的逻辑结果，再进行不可逆的 discard；discard 失败仍可保持逻辑已拔出，只是驻留资源没有回收，记录可重试回收范围。不能在可能回滚成 plugged 时先把旧内容打洞。

### 8.4 四种粒度，各自有位置

- guest block：Population 与协议的单位。
- host backing page：discard/大页分配/文件打洞的单位。
- CPU binding chunk：从插入集合编译为 KVM 注册的单位。
- DMA mapping granularity：该 adapter 将来能够独立撤销的单位。

BindingShape 是运行期编译结果，不是新的 guest 拓扑。保留稳定 chunk 边界，避免相邻合并后拔一个范围扰动其他绑定。slot 编号仍不进入恢复数据，恢复可在保持 guest 地址与协议的前提下重新分配编号。

初版允许 CPU chunk 大于 guest block：只要 chunk 内有 plugged block 就保留整个注册，完全空闲才撤销。它不承诺恶意 guest 无法用 CPU 重新填充已 unplug 的页，也不能据此协商不成立的“逐 block 不可访问”保证。软件 DMA 通过 Population 校验，VFIO 按其可撤销粒度使用允许集合。

若 guest block 小于不可独立回收的 hugepage，必须选择对齐约束、延迟回收完整大页或拒绝组合；不能悄悄 discard 邻接的 plugged block。backing/guest/CPU/DMA 粒度兼容性在构建与接入时检查，不扩散为每次普通 I/O 的四份判断。

## 9. 第 3 轮：迁移不是把运行期对象序列化

### 9.1 恢复结构与字节内容

不设一个独立 MemoryImage runtime 层、管理器或序列化入口。机器快照直接拥有内存的版本化 Layout、已接受 Population，以及内容清单：

```text
ObjectContents {
    object_id,
    extents: { object_offset, length, source, source_offset },
}
source = 快照字节 / 显式零区 / 已验证的持久来源 / 按需 pager 来源
```

内容清单的缺口不能默认解释为零。每个需要恢复的对象内范围必须有确定来源，稀疏文件空洞与“本次增量未发送，沿用父代内容”是不同语义；父代来源需有身份与完整性约束。未插入的 volatile 块可以按协议记为显式零/不需恢复内容，但不得将其套到 pmem。

快照字段不保存 HVA、host fd、KVM slot、DMA handle 或源 VM 引用。内容以 ObjectId+offset 标识；一个 backing 被两个 GPA Window 使用时，不把 guest 地址洞搬进内容文件，也不重复传一份数据。source offset 不等于对象 offset，仍需明确映射。

普通 RAM、私有快照 CoW、持久共享文件、按需 pager 的内容来源不同。pmem 如果由外部文件/存储复制，必须附带不可变来源身份与可验证的恢复要求；仅保存路径或源机 inode 不能保证跨主机恢复的是同一版本。不支持某种来源时拒绝恢复，不能回落成全零内存。

requested_size 属于内存设备协议快照，Population 属于机器快照的内存字段；停机切面校验两者的身份与容量约束。CPU binding chunk 可以重编译，但不能改变已协商的 guest block size。目标机重放 GPA 与 guest NUMA，host NUMA/页尺寸可按显式恢复政策重新放置，不用目标机的偏好默默修改 guest 结构。

### 9.2 DirtyEpoch / TransferSet：工作状态，不是 Population

DirtyEpoch 代表一轮受支持的迁移写入追踪，具有布局版本与全部写入参与者。CPU dirty、软件写入 dirty、直通设备 dirty 经 GPA/IOVA 映射到对象内坐标，形成 TransferSet；“页已发送”“源再次变脏”“目的页已接收”分别属于迁移会话的工作状态。

租约保活不等于记录软件写入。软件写授权包含对象范围、方向与访问代次，并把记脏责任交给实际完成 owner；同步写入结束及异步 read_into_guest 真正完成后报告所写范围。只在提交异步 I/O 时标脏不够：迁移可能先复制并清位，内核随后才写入。

软件对象的 DirtySet 是写入汇合点，完成路径不只引用取得租约时的旧 epoch。迁移开始前已经提交、开始后才完成的写入也必须进入当前追踪；不能让换 epoch 丢掉其责任。采集并清位发生在相应字节复制之前，复制期间与之后完成的写入保留到下一轮；最终排空全部写授权后才取得不再变化的最后写集。具体实现仍需证明 bitmap 发布/清位与完成之间的内存序，不能把这些句子当已实现的同步。

裸指针 escape 只能发生在有拥有者的 I/O 资源中；完成前不能提前提交 quiesce、释放 lease 或销毁记脏责任。无法确认取消的请求同样属于在途写入。

初版 live precopy 期间固定 Layout 与 Population，拒绝新热插与 block 插拔，普通 guest/设备 I/O 仍运行。这是明确的能力取舍，不声称支持拓扑变化与迁移无缝并发。guest 内存请求使用协议允许的 busy/retry 响应；不支持该表达的消费者使迁移无法进入该模式。

最终切面需要：暂停 vCPU；设备在自己的循环中停止新 I/O 并排空旧 I/O；排空已接收内存变更；同步全部 dirty 生产者；采集一致的内存/设备状态。内存服务不等待一个已经在等它的设备给自己发送新视图 ack。

VFIO 必须分别具备设备状态迁移、DMA 写入追踪与最终停止的支持，只有 dirty logging 不够。缺任一能力时拒绝 live migration；stop-and-copy 也不能凭“vCPU 已停”代替设备 DMA 已停。

跨进程 handoff 的源/目标运行权由机器迁移协议决定，不属于内存分配器。目标只在状态/内容/设备恢复完整且获得运行权后运行，内存恢复数据不是避免双运行的凭据。

## 10. 用具体场景检查形状

| 场景 | 应改变的事实 | 不应改变的事实 |
|---|---|---|
| 低/高 RAM 绕过 guest 地址洞 | 两个 Window 的 GPA 放置 | 可以仍用一个连续 backing |
| 添加一个现代 DIMM 类区域 | Layout、Store、CPU/DMA 投影与读索引 | 其他设备的对象所有权 |
| 拔掉仍有异步 I/O 的区域 | 关闭准入、等待租约、撤销外部注册 | 不因请求超时释放 backing |
| virtio-mem 拔一个 block | Population、相应 CPU chunk/DMA、回收记录 | Object 身份、固定容量与 guest 放置 |
| guest IOMMU 改 IOVA 映射 | DomainMap 与该外部域的投影 | host backing 与 GPA 分配 |
| VFIO 加入已插部分块的窗口 | consumer 登记、回放、实际 pin 集合 | 不把整个容量窗口视作全部可 pin |
| VFIO 撤销失败 | StalledChange 与保留资源 | 不释放/reuse claim 与 backing |
| private 快照恢复后 balloon | 页内容来源与回收 | 不让旧快照文件内容再被 fault 回来 |
| 同进程换一个 VM 恢复 | VM 身份、准入、CPU/DMA runtime handles | 可复用唯一移交的存储对象与已选布局 |
| live precopy 的第二次写入 | dirty/TransferSet 工作状态 | 不把“已经传过”当成“以后不再变化” |

这些是设计反例，不是已经跑过的测试结果。后续实现应把每条资源释放前提做成可观察证据，尤其是失败路径；不能用表格打勾代替运行验证。

## 11. 本轮仍由专家评判的取舍

1. Object/Window 分离有真实需求，但非重叠切片限制是否足够？当前不以通用 alias 解决所有后续问题。
2. 唯一 MemoryRuntime 是否职责恰当？它协调同一内存操作的资源，不拥有 IOMMU 页表、设备协议或普通 I/O。
3. Window 级访问屏障能否接受？更细粒度屏障不是免费优化，必须用测得的负担和明确消费者来推动。
4. 迁移期间固定 Population 是否可接受？若业务要求连续 resize，必须再设计变化记录与迁移 epoch 的关系。
5. 哪些 vhost-user backend 真能撤销导出/排空 DMA，哪些只能固定生命周期？需要按协议与实际 backend 验证，不用统一 trait 名字掩盖差异。
6. KVM slot、host pin、回收粒度的预算应如何选？数据模型保留差别，具体默认值不在没有工作负载证据时拍板。
7. 机密内存接入时 AllowedRam 需要叠加 shared/private 来源；现在只保留明确权限边界，不预先实现任意 source registry。

### 第 3 轮反思记录

否决：通用 listener 广播给所有软件设备；一份 GPA 范围代表所有 DMA 地址域；“失败后反序 drop”；迁移就是序列化 runtime manager。

增加：受作用域限制的 DmaProjection、实际副作用拥有记录、StalledChange、对象内内容坐标和完整 dirty 参与者。区别声明：逻辑已拔出不是驻留页消失；跨后端成功不是原子；安全保活不是撤销证据。

风险仍在：软件 lease 的队列完成边界、异步控制循环的饥饿/互等、DMA domain 两个输入的一致性与预留资源增长。独立审阅应给出具体反例，不因文档变长就把这些风险算作已解决。

## 12. 第 4 轮：收缩边界与否决记录

独立结构审阅读取的是第 1 轮，不是第 4 轮的认可。以下记录采纳了哪些反例，以及哪些建议没有直接照搬。

| 反例/建议 | 本轮处理 |
|---|---|
| 同一可写共享文件可以绕过 ObjectId 的不重叠检查 | 增加 MemoryStore 内真实来源+offset 检查；不声称检测全宿主 alias |
| block bitmap 缺少设备身份与粒度 | Window 描述加入 owner/MemoryDeviceId，块级 kind 包含 block_size |
| host NUMA 不应原样跨主机重放 | 拆出 HostPolicy；guest 布局稳定、host 物化策略显式替换 |
| pmem/共享/可丢弃混成同一种 backing 属性 | 拆分 content_contract 与 backing 能力，禁止破坏持久来源的 discard |
| Runtime 可能接管所有设备/IOMMU/迁移 | 正面列出它拥有的内存事实；DMA 域、设备协议、机器暂停各保留 owner |
| 单版本号不能代表外部映射已同步 | 分来源 revision、实际成功登记、MappingId 代次；禁止迟到清理跨新映射 |
| MemoryImage 没有独立运行期不变量 | 删除独立包装/服务概念，快照直接拥有内存字段与对象内容清单 |
| 只保留 Arc 的旧设备仍可以写恢复后的 backing | 准入永久撤销与新 VM 身份；导出 fd 的撤销另外要求真实证据 |
| payload 完成又要取得新 used-ring 访问，可能自锁 | 外部 I/O 前取得完整租约集合；不能跨关闭窗口无限等待 |

保留 Object/Window：地址洞、内容传输坐标、host 策略边界是真需求，不只是将“region”换两个名字。暂不采纳任意 alias、通用区间权限树、多来源 discard 注册框架或运行期设备对象共享锁。

### 12.1 失败退出与 Drop 的底线

正常停止使用显式排空/撤销路径，不靠 Drop 执行异步协议。内存服务任务不可在超时后直接 abort；必须保留已接收操作与其资源。资源类的 Drop 只能在确定已经撤销时释放 backing。

若 panic/宿主任务意外消失导致无法证明外部注册已解除，最后防线是保留/泄漏仍可能被访问的 backing 与相关 handle，而不是释放；常规错误应留在 StalledChange 中可检查、可重试。泄漏不是恢复成功，服务也不能从一份未证实的区域记录自动重启并重新发放权限。本稿不承诺无监督的进程内 crash recovery。

### 12.2 可分期实现，不把设计范围当一次重写

依赖顺序只用于检查抽象能否逐步存在，不是已批准的开发计划：

1. Object/Window 与纯布局、backing 生命周期、唯一 CPU 注册权；固定视图也可以运行。
2. RamReader/MemoryLease 的真实 I/O 适配与撤销屏障原型；未支持消费者明确保持固定生命周期。
3. 单一 WindowState 的 virtio-mem，分别验证逻辑插入、CPU chunk 与物理回收，不先引入所有高级 adapter。
4. 首个真实外部 DMA 域接入 replay、实际登记与失败保留；随后才支持相关区域在线撤销。
5. 快照内容来源与 dirty 会话；先停机切面，再在已支持设备集合上开放 precopy。

不为后期能力写空 trait 实现或无作用的 ack。每一步必须能独立运行，不能为了“最终统一”让现有功能先全部失效。

## 13. 给人类专家的评判入口

我目前最愿意捍卫的部分是 Object/Window 分离、按事实唯一 owner、布局与 host 物化分开、以及失败操作继续拥有实际资源。最不愿提前拍板的是软件访问租约的代价与 vhost-user 撤销边界。

优先评判这些问题，而不是逐个工作名是否漂亮：

- **租约是否值得？** 它比依赖永久映射/可信 guest 协议更严格，但会影响第三方 queue、异步 buffer 和 I/O batching。必须用无额外拷贝的最小原型证明可组合性，再决定是否接受或缩小在线撤销范围。
- **MemoryRuntime 是否仍太宽？** 若其操作必须不断调用机器/设备方法才能完成，说明 owner 边界尚未成立；不能只凭“它是专门服务”免责。
- **一个 Window 的屏障是否太粗？** 性能需求足够真实时，可以沿独立撤销粒度拆 Window/屏障；未测量之前不增加每 block 原子状态。
- **外部 DMA 统一到了合适层吗？** AllowedRam/DomainMap 分离有必要，但 fd backend 的能力弱于硬件 pin。若统一接口掩盖撤销证据，应宁可保留不同 endpoint 变体。
- **拒绝并发迁移与热插是否符合 scope？** 初期拒绝有利于保持切面可证明；若专家认为这是核心能力而非优化，需要实质扩展状态模型，不是删掉限制一句话。
- **失败保留是否可操作？** 每个 StalledChange 应指出哪个外部状态未确认、保留哪些字节/grant、怎样隔离。若只能“留着以后再说”，仍是未完成的设计。

这轮没有性能、协议适配或故障注入实证，不据此宣称可直接落地。工作稿保留反例和有意限制，下一轮可以删除本稿的核心抽象，而不以保全既有文字为目标。

## 14. 第 5 轮：时间边界不能藏在代次号里

生命周期审阅也读取了第 1 轮。它与结构审阅都支持存储/放置分离，但它们没有审阅或认可本稿最新版本。这里保留新一轮实质修订，不把已经增加的术语当作答案。

### 14.1 撤销证据按执行者分类

| 证据 | 能证明什么 | 不能单独证明什么 |
|---|---|---|
| WindowAccess 已关、该代次租约排空 | 新软件访问被拒绝，已授权软件访问结束 | vCPU、硬件 DMA、远端 fd 不再访问 |
| KVM 删除完成/旧 VM 关闭且无 vCPU 使用者 | 相应旧 CPU 投影不再访问 | 同一 backing 的软件/DMA 权限也结束 |
| 外部 DMA unmap 回执 | 已登记范围的映射已按 backend 保证解除 | endpoint 不会再发送旧请求或同 IOVA 可换用途 |
| endpoint/domain 在指定代次停止或排空的证据 | 旧用途不能再发出/完成影响新用途的 DMA | 尚存映射/pin 已释放，或所有共享域成员均已停 |
| 远端可信排空/导出撤销协议完成，或进程终止证据 | 在该协议/信任边界内，旧 fd/指针不再访问 | 仅 socket 已关、REM 消息已发送就足够 |

软件类型能够约束谁可以取得新权限，也能够让未完成副作用继续拥有资源；不能强迫既有租约在有限时间归还，不能让软件 MappingId 出现在硬件请求里。这份设计可争取的是安全保留与可诊断撤销，不是无条件、有限时间的回收。

### 14.2 逻辑退插与 host 换用途不是同一请求

virtio-mem 请求建立在 guest 不再访问已 unplug 范围的协议上。CPU chunk 粗粒度时仍明确不承诺恶意 guest 无法重新触碰；VFIO 后续由 guest 违规提交的旧 DMA 同样不能被软件 bitmap 神奇识别。

host 要释放 grant、改变 backing 用途、迁移设备域或把同一存储交给另一个 VM 时，要求更强：软件准入永久撤销、在途访问排空、CPU 投影解除、全部外部权限解除，并在会复用 IOVA/端点时结束旧执行用途。相关证据绑定 Window、VM、域与代次，不接受一个通用的 stopped bool。

unmap 失败不仅禁止 munmap，也禁止清零后重插或把内容当成新对象使用。映射已解除但旧 endpoint 尚未排空时，不允许以新 MappingId 为理由在同 IOVA 安装新用途。共享域的成员关系也必须被证据覆盖。

### 14.3 写入完成决定迁移记脏的时间

反例：提交磁盘读入 guest 时 P 被标脏；迁移复制 P 并清位；内核后来写 P；最终 KVM bitmap 不包含这次软件写入。

修订：写授权的真实完成 owner 持有范围、方向和记脏责任。提交时标记最多是提前保守记录，不能取代完成后记录。迁移 epoch 开始前的在途写同样要在完成时进入当前写集；最终切面同时要求 producer 排空与记脏汇合。页复制与清位的先后不能随意颠倒。

此项不能靠“租约在结束时 drop”自动成立，除非该 drop 确实与外部 I/O 完成绑定；等待 future 被取消或 io_uring cancel 请求发出均不足以证明。具体适配器的完成/取消契约是必须验证的边界。

### 14.4 本轮最需要继续反驳的两个结论

- 租约与队列完成路径的组合是草稿中最重的新增机制。若它需要设备四处额外持锁、复制数据或维护隐蔽状态机，专家应当否决它，或者将在线撤销缩小到能够满足契约的消费者。
- 内存服务和 DmaProjection 都是控制面 owner，而不是万能 manager 的两个分身。其输入是否足以独立前进、是否把不可完成的等待局限到受影响资源，要通过具体设备/域生命周期反例继续推敲。

本稿没有把“第五轮结束”视作设计通过。它只是把可捍卫的关系、已否决的假设和仍需专家裁决的代价放到同一份草稿中。
