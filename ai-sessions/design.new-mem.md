# 新内存设施：设计草稿

状态：第 5 轮工作稿的整理版本。本文按层次重写，不是新一轮设计；逐轮演进的原始草稿纸保留在 `design.new-mem.draft.md`。

这是草稿，不是实施计划，也不替代现有设计。内容只依据 AGENTS.md 与参考项目调研，不读取当前 VMM 代码。两份独立审阅读取的都是第 1 轮，它们支持存储与放置分离，但没有审阅或认可本版本。整理掉的是叙述顺序，不是未决项：被否决的假设、有意的能力取舍和待裁决的代价都在第 12–14 节原样保留。

---

## 第一部分：定位

### 1. 范围与评价方式

#### 1.1 对标范围

范围最接近 Cloud Hypervisor：现代 microVM，以 x86-64/KVM/Linux 为起点，涵盖多 backing、host/guest NUMA、virtio-mem、现代内存设备热插拔、vhost-user、VFIO、virtio-IOMMU、快照与迁移。高级消费者应能接入共同的内存事实，但不要求一次实现全部能力。

#### 1.2 刻意不支持

- 不追求 QEMU 的任意地址树、优先级遮挡、PAM、传统 ROM 切换或完整机器仿真。
- 不以“以后也许需要”为理由引入通用插件框架、通用区间权限树或多来源 discard 注册框架。
- 机密内存的 private/shared 转换是单列扩展，不承诺本稿已解决；设计不得把 host 可见字节当作永久前提。
- 不支持任意 Window alias，不支持运行期设备对象共享锁。

边界要能被类型或构建期检查表达，而不是写在注释里。

#### 1.3 判断标准

判断标准不是类型数量、完成勾选或形式上的不可变性，而是专家是否认可以下问题的答案：

1. 同一份字节被配置、设备、KVM、DMA、迁移分别看待时，身份和权威是否清晰？
2. 加入一个真正的新消费者，是否需要修改所有旧设备或增加第二个万能 manager？
3. 区域撤销、页回收、DMA 撤销和 VM 销毁的区别是否进入数据模型？
4. 失败与取消留下的资源是否仍有明确所有者，而不是一句“回滚”掩盖？
5. 热路径是否为控制面形式美承担不合理代价？
6. 哪些能力刻意不支持，边界能否被类型或构建验证表达？

迭代结束只意味着草稿可供评判，不意味着设计已获认可。

### 2. 事实分层

全部结构围绕一件事组织：同一份字节存在多个事实，它们有不同的坐标、粒度、生命周期和权威。

| 事实 | 坐标与身份 | 权威 |
|---|---|---|
| 存储对象 | ObjectId、对象内 offset | MemoryStore |
| guest 放置 | WindowId、GPA、对象内 offset | MemoryLayout |
| host 映射 | HVA、fd、文件 offset、页粒度 | HostBacking |
| CPU 映射 | VM 身份、KVM slot、GPA/HVA | CpuBindings |
| 软件 DMA 读视图 | 查址表版本 | RamView |
| 外部 DMA | endpoint/domain、IOVA/GPA、pin 范围 | 各 adapter 的成功登记 |
| 区域内插入状态 | WindowId、block index | Population |
| 恢复与迁移 | 对象身份、布局、传输页集合 | 机器快照的内存字段 / 迁移会话 |

两条原则贯穿全文：

- **“不止一个权威”不等于可多处独立修改。** 权威按事实划分，每个事实只有一个 owner。
- **派生登记必须保存真实成功结果。** 外部系统可能只完成了部分操作，派生状态不能由本地推算冒充。

#### 2.1 最短读法

```text
纯值：MemorySpec → 已选 MemoryLayout + HostPolicy

唯一控制 owner：MemoryRuntime
├─ MemoryStore：ObjectId → HostBacking
├─ MemoryLayout：WindowId → ObjectSlice + GPA + owner + guest 属性
├─ WindowState：已接受插入集合 + revision
├─ CpuBindings：唯一绑定权与实际成功登记
├─ MemoryChange / StalledChange：未结束副作用及保留资源
└─ RamView 发布权、外部 DMA 消费者成员关系

软件设备：RamReader → RamView → WindowAccess → MemoryLease → 字节
内存设备：自身协议状态 + 受限 WindowControl + 派生 WindowStatus
外部 DMA 域：DmaProjection（AllowedRam ∩ DomainMap）→ 实际映射
机器快照：Layout + Population + 对象内容清单；不序列化 runtime owner
```

RamView 只供软件 DMA 使用，不是完整机器地址空间，也不暴露所有 CPU 可见的 pmem/设备内存。CPU RAM/pmem 投影与软件 DMA 视图都派生自 Layout，但准入条件不同；MMIO 总线仍是独立访问语义。不同索引有明确派生关系，不各自成为布局分配器。

#### 2.2 层次与阅读顺序

| 层 | 内容 | 节 |
|---|---|---|
| 纯值 | 身份、放置、宿主政策 | 3 |
| 资源 | 宿主字节的所有权与能力 | 4 |
| 控制 | 唯一内存控制 owner 与权限移交 | 5 |
| 投影 | 软件访问、CPU、外部 DMA | 6、7、8 |
| 协议 | 内存设备与服务的职责切分 | 9 |
| 时间 | 变更失败、撤销证据 | 10、11 |
| 恢复 | 快照与迁移 | 12 |

名称是工作名。新增类型若只能解释名词而不能表达独有不变量，应在评判中删除。

---

## 第二部分：静态结构

### 3. 纯值层：身份与放置

#### 3.1 MemoryLayout 与 HostPolicy

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

`MemorySpec` 是用户输入，经纯规划得到已选的 `MemoryLayout` 与 `HostPolicy`。规划是功能核：同样输入得同样布局，可重放、可快照、可比较。

guest 位置、容量、设备关系与协议粒度进入布局；HVA、fd、KVM slot、host IOMMU handle 不进入。

#### 3.2 Object / ObjectSlice / Window

- **Object** 是逻辑字节身份，不等于一次 mmap 或一个文件 fd。目标机换 backing 后其逻辑身份仍可保持。
- **ObjectSlice** 是对象内的半开区间。它的长度同时定义 Window 覆盖长度，避免两个长度可独立修改。
- **Window** 是一个 ObjectSlice 的 guest 放置。一个 Object 可以因 32 位地址洞分成两个不重叠 Window，不要求 backing 也有洞。

范围统一用半开区间；GPA、IOVA、对象内 offset、字节数是不同类型。

`owner` 为启动内存或明确的 `MemoryDeviceId`，不用一个 Role 枚举冒充设备身份。`kind` 区分启动 RAM、整区可插拔 RAM、块级 RAM（含已协商 block_size）、持久内存 aperture。`WindowControl` 的作用域由 owner 与 WindowId 固定。

一个 Object 内采用一致的宿主物化政策；需要不同政策时拆 Object，不建对象内的通用政策树。

#### 3.3 alias 限制与可写共享来源检查

初版拒绝两个 Window 指向重叠的对象内范围，避免无需求的 guest RAM alias。允许同一 Object 的不重叠切片。

物化时还要检查本 MemoryStore 内已打开的可写共享来源：相同文件实体的重叠 offset 不能伪装成两个 Object 绕过限制，路径不同不足以证明独立。只读来源或私有 CoW 映射不是同一种可写 alias，不能一概拒绝。

此检查不假装能枚举其他进程的映射；外部宿主管理的共享约束要明确声明，而不是假设已被覆盖。

#### 3.4 地址 claim 与 GpaGrant

Reserved claim 不产生 RAM。PIO/GSI 与 GPA 是不同分配域。

本组件只接受机器地址分配 owner 已选出的描述与不可复制的 `GpaGrant`，不自行维护第二个可与 MMIO 冲突的 GPA 分配器。Layout 中可复制的 claim 是重放数据，不是 grant 权限。

撤销完成才交出可释放的 grant，`StalledChange` 不交出。服务无须等 vmm-main 消费返还 grant 才结束自己的操作。

#### 3.5 宿主物化与 guest 结构分开

HostPolicy 是独立物化输入。可以记录源配置用于诊断，但恢复不得把源 host NUMA 编号当 guest 结构原样执行。目标策略只能是保持、显式替换或不支持，不能默默重算 guest GPA/NUMA。

### 4. 资源层：存储与宿主字节

#### 4.1 MemoryStore 与 HostBacking

MemoryStore 独占 ObjectId 到 backing 的记录。HostBacking 拥有映射、文件及释放责任。

**引用只保活，不授予权限**：持有 backing 引用不等于获得布局修改权或 KVM 注册权。

backing 政策与 runtime backing 分开：匿名私有 RAM、可导出的共享 memfd、用户文件的共享/私有映射。共享、hugepage、reserve、prefault、host NUMA 是政策维度，不为每种组合创造一个 enum 变体。

#### 4.2 持久性、可导出性、可丢弃性是三件事

不是一个标志。

- 共享 memfd 不等于持久存储。
- 私有文件映射不把写入持久化到原文件。
- pmem 的 flush/durability 契约由该设备与存储后端提供。

Object 的 `content_contract` 记录 volatile RAM 或具体持久来源与一致性要求；HostBacking 的能力决定该组合能否物化。virtio-mem/balloon 的破坏性 discard 不得落到需要保留内容的 pmem 对象。

#### 4.3 导出与字节访问的边界

文件 offset 不等于 GPA。

导出能力不是所有 backing 的共同能力：vhost-user 在配置边界就要求可导出的共享 backing，不能运行后遇到缺 fd 才静默改成复制路径。

HostBacking 不向普通设备公开原始 mmap owner，RamView 也不直接暴露可绕过 WindowAccess 的 vm-memory 集合。同步访问适配器与异步 I/O 指针接口都必须借自 `MemoryLease`。是否能与既有第三方 queue/I/O API 无额外拷贝组合，仍需原型证明。

---

## 第三部分：控制与投影

### 5. 控制层：MemoryRuntime

#### 5.1 它拥有什么

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

MemoryRuntime 独占：Store、Layout、WindowState、CPU 绑定权、未结束变更、RamView 发布权、外部 DMA 消费者成员关系。

#### 5.2 它不拥有什么

不拥有整个设备，不执行普通队列 I/O，不接管 IRQ、总线或设备任务调度，不拥有 IOMMU 页表，不拥有机器的暂停/运行权。设备 future 继续单一所有者，可为 !Send，运行在 LocalRuntime。

#### 5.3 为什么是独立服务

MemoryRuntime 是专门的内存服务，拥有控制资源，运行在 LocalRuntime；它的进展不依赖 vmm-main 接收下一条事件。vmm-main 可以等待暂停，内存设备仍可等内存服务完成自己已提交的请求。

它不持有设备对象。软件设备保存 `RamReader`；内存设备额外保存仅可操作自身 Window 的 `WindowControl`。DMA adapter 是外部映射的拥有者，向内存服务提供专门的映射请求句柄，不能借此操作整个设备或关闭设备 worker。

进入服务前可以取消尚未接收的请求；接收后的变更必须完成、补偿或保持为一个有所有者的待恢复状态。客户端关闭不等于操作安全取消。

同步 ioctl 可能阻塞。不得声称 LocalRuntime 上的 async 保证它不会阻塞；可能阻塞的外部操作需要受控执行边界，但不把 !Send 设备 future 迁到线程池。

#### 5.4 VM 身份与绑定权限

```text
UnboundMemory（不可 Clone）
  --消耗唯一分配/注册权--> AttachedMemory（一个 VM 的 MemoryRuntime）
AttachedMemory --独占--> 地址 grant、WindowState、CPU 绑定权
CPU Binding --保活--> VM 与 HostBacking
MemoryLease --保活--> HostBacking，不具备注册权
```

backing 的 Arc 可以被多个视图保活，分配/注册权不能复制。safe API 不接受设备给来的裸 HVA/fd/slot 来新建绑定；低层直接使用这些指针注册是 unsafe 边界，不属于安全层承诺。

同进程 teardown/rebuild 必须关闭旧全部准入、结束软件 I/O、撤销外部 DMA、停止 vCPU、关闭旧 VM，才可重新取得 `UnboundMemory`。恢复为同一 backing 建立新的 VM 身份与准入；旧 view 即使保活同一字节，也不能借旧权限写入新 VM。

### 6. 软件访问：索引版本与访问许可分开

#### 6.1 RamView 是索引，不是快照

MemoryRuntime 从 Layout 与 Store 派生当前查址表。消费者仅拿读入口，写发布入口只有一份。采用 ArcSwap 式版本发布：集合替换不复制字节，不自动等待旧读者。

RamView 是不可变索引，不是内容快照，也不拥有 grant、slot 或外部 DMA 权限。

#### 6.2 WindowAccess 与 MemoryLease

```text
RamReader --共享发布入口--> RamView（版本）
RamView.entry --保活--> HostBacking
RamView.entry --引用--> WindowAccess { admission, inflight, current_population }
请求 --查址、取得许可--> MemoryLease --保活--> 所用 backing 与访问许可
```

每个 RamView 项引用 HostBacking 以及一个显式的 WindowAccess。WindowAccess 不是“整台 VM 内存的锁”，它只处理这一个 Window 的访问准入、在途计数与当前可访问范围。

软件设备可以长期缓存索引项，但不能长期缓存一份脱离 MemoryLease 的可解引用 HVA。取得租约后才解析可访问范围与 host slice。**租约覆盖异步 I/O 的真实完成时间，不是提交 syscall 的时间。**

关闭准入与登记在途必须形成一个原子判定，不能用两个独立 load 拼成有竞态的检查。

只对数据访问做此准入。设备读取容量、查询操作状态等控制行为不取得字节租约，因此内存操作等待在途 I/O 时，不需要反过来等待同一设备的配置 ack。

#### 6.3 撤销与重用

拓扑撤销时永久关闭旧 WindowAccess。旧 RamView 可以继续存在，甚至保活旧 mapping，但不能再创建该 Window 的新访问。相同 GPA 重新使用时创建新的 WindowId 与准入对象，旧表不能通过地址相同绕过撤销。

区域内部 Population 改变时暂时关闭这一 Window，等待其在途租约释放，再发布新 Population 并重开；旧索引访问也须读取新的准入状态，而不是使用表里的旧 bitmap。

#### 6.4 Window 级屏障：有意的粗粒度

初版故意以整个 Window 为撤销屏障，不引入每 block 的原子计数或通用区间锁。代价是插拔一个 block 也会短暂干扰该 Window 上的软件 I/O；计数按租约/批次，不按字节。

#### 6.5 关闭准入不是强制取消在途 I/O

已有租约必须能完成，不能要求在途请求重新经过已经关闭的准入才能完成自己。一个请求解析到的 buffers 与完成所需的 used-ring 访问应属于同一受控 I/O 生命周期。

**一个 I/O 的 payload 与完成所需的 used-ring 等范围必须在提交外部 I/O 前取得完整租约集合。** 跨 Window 获取采用固定顺序、非阻塞尝试；任一窗口正在撤销就释放本次已取得的租约并返回可重试状态，不持一部分租约 await 另一部分。已经提交的 I/O 不再等待一个被关闭窗口的新租约来报告完成，已有租约可继续使用到该 I/O 完结。

关闭后到来的新请求不得占用阻止在途排空的设备执行位置。窗口屏障可以等待、观察进度或超时；**超时只能表示撤销尚未完成，不能强制把裸指针视为已失效然后释放 backing。** 无法取消的外部 I/O 继续保留租约；取消请求的 API 成功返回，也必须按该 backend 的定义确认内核/回调不再持指针。

租约不是 vCPU 访问许可，也不是 VFIO pin 的替代品。CPU 与外部 DMA 必须分别得到各自的撤销证据。

### 7. CPU 投影与粒度

#### 7.1 CpuBindings

绑定保留 backing 与 VM，独占 slot 使用权。slot 是运行期编号；绑定完成才记入成功表。Window 与 slot 不一一对应，绑定切分受 KVM 预算与设备粒度影响。

VM 不需要拥有整个权威区域集合；它必须通过成功绑定保住映射，关闭 VM 前不释放仍可访问的 backing。safe 层不能用可复制的 slot 编号构造第二份注册权。

#### 7.2 四种粒度，各自有位置

- **guest block**：Population 与协议的单位。
- **host backing page**：discard / 大页分配 / 文件打洞的单位。
- **CPU binding chunk**：从插入集合编译为 KVM 注册的单位。
- **DMA mapping granularity**：该 adapter 将来能够独立撤销的单位。

`BindingShape` 是运行期编译结果，不是新的 guest 拓扑。保留稳定 chunk 边界，避免相邻合并后拔一个范围扰动其他绑定。slot 编号不进入恢复数据；恢复可在保持 guest 地址与协议的前提下重新分配编号。

#### 7.3 粗粒度 chunk 的明确不承诺

初版允许 CPU chunk 大于 guest block：只要 chunk 内有 plugged block 就保留整个注册，完全空闲才撤销。

它**不承诺**恶意 guest 无法用 CPU 重新填充已 unplug 的页，也不能据此协商不成立的“逐 block 不可访问”保证。软件 DMA 通过 Population 校验，VFIO 按其可撤销粒度使用允许集合。

若 guest block 小于不可独立回收的 hugepage，必须选择对齐约束、延迟回收完整大页或拒绝组合，不能悄悄 discard 邻接的 plugged block。backing/guest/CPU/DMA 的粒度兼容性在构建与接入时检查，不扩散为每次普通 I/O 的四份判断。

### 8. 外部 DMA 投影

#### 8.1 不建无差别的 MemoryListener 框架

外部 adapter 分开维护，各自的坐标与证据不同：KVM 的 GPA/HVA 投影、VFIO 的 IOVA/HVA/pin 投影、vhost-user 的 fd/offset 导出。**IOMMU 的域内翻译不是 RAM 分配。**

软件 I/O 用 RamReader/MemoryLease，不接收每一次 plug 的 ack 广播。外部 DMA 使用专门的 `DmaProjection`；它属于一个 host DMA 地址域，不一定一设备一份——多个 VFIO 设备可共享一个 host IOMMU 容器。

#### 8.2 两个输入不能合并

```text
MemoryRuntime --AllowedRam 版本--> DmaProjection <--DomainMap 版本-- guest IOMMU owner
                                      |
                           独占实际映射/导出登记
                                      |
                              host IOMMU / vhost backend
```

`AllowedRam` 决定存储范围可否被这个消费者使用；`DomainMap` 决定 IOVA 翻译及读写权限。DmaProjection 串行维护两个输入与实际成功映射，作用域仅是一个外部地址域，不拥有内存分配或设备协议。

没有 guest IOMMU 时 DomainMap 是显式的 identity 模式，不维护一套假的页表。存在 IOMMU 时，IOVA、GPA、对象内 offset 都保留各自坐标，映射不得跨越 backing 或权限边界。

它保存受限 `BackingExport` 的保活引用以解析 HVA/fd，**不向 MemoryRuntime 同步回调取内存**；否则内存服务等 DMA ack、DMA adapter 等内存查询仍会互等。

新 IOMMU 映射只能与 adapter 当前 AllowedRam 相交，不能凭旧 DomainMap 把已经撤销的 Window 重新 map 回去。每个来源分别有 revision；旧 revision 的重放被拒绝，不能把两个来源误称为一次全局事务。

#### 8.3 新消费者接入与回放

新 adapter 接入时，在内存服务序列中登记一个未就绪成员，给它当前窗口与可访问状态；完成回放才成为 Ready。

回放失败时它仍拥有已成功创建的映射，必须撤销或隔离，不能直接删除句柄。撤销成员在实际外部状态解除之前，仍属于相应资源的消费者集合。

#### 8.4 代次只保护宿主登记

DMA 登记以 `DmaDomainId + MappingId/代次` 标识，不只用 IOVA。一个地址域的真实副作用串行应用；某旧范围仍有未确认的 map/unmap 时，不允许该 IOVA 的新绑定越过它。迟到结果归回原操作登记，不能按地址删除“现在恰好在这里”的新映射。仅忽略旧 revision 的回复，不能撤销已经发生的内核副作用。

**代次不出现在硬件请求里。** 成功 unmap 不能单独证明 endpoint 不会继续发送旧队列请求。host 控制的对象重用、域转移或跨 VM 移交，还需该 endpoint/domain 的停止或排空证据；缺证据时不得换用途再 map 同一 IOVA。guest 自己控制的 IOMMU 映射复用受 guest 协议约束，不等于 host 获得了安全移交证明。

#### 8.5 vhost-user 的弱撤销语义

vhost-user 不能被假定具有 VFIO 的逐 block unmap 语义。它通常导出整段共享文件，fd 本身授权访问整份 backing，而不是某 Window 的租约。即使只发送一个 offset/length，远端仍可能保存 fd、缓存指针或映射；REM_MEM_REG 或 socket 关闭不能自动充当能力撤销证明。

初版 vhost-user 支持固定导出生命周期。在线删除、同一 backing 跨 VM 重用与精确 block 回收，只在 backend 的可信排空/撤销协议或外部进程终止能提供明确证据时开放；尚未验证就拒绝相关组合。

AllowedRam 对普通 fd backend 是描述性输入，不能承诺硬件般强制的子范围权限。VFIO 则必须按允许范围 pin，不用整区 map 偷偷填充已拔出的块。

---

## 第四部分：协议与时间

### 9. 设备协议边界

#### 9.1 Population 只有一个权威

virtio-mem 的固定容量 Window 不因每个 block 插拔而创建或删除。

MemoryRuntime 独占 `WindowState`：具体已插入位置、Population revision、当前成功的 CPU/DMA 登记及未结束变更。Population 是不可变的 block 集合值，转移产生新值；bitmap 是其具体表示，不是第二份 KVM 或驻留 bitmap。

#### 9.2 设备拥有协议，服务拥有资源事实

内存设备独占 requested_size、协商特性、队列及 guest 请求状态。它**不**另外拥有可独立修改的 plugged bitmap 或计数；guest 配置中的 plugged_size 与 STATE 请求的回答都从只读 `WindowStatus` 取得。WindowStatus 是 WindowState 提交时发布的只读投影，含 revision 与 Population 值，计数由其派生；设备只能读，不用它反向决定资源事实。

一次 plug 命令包含设备当时允许的目标上限；MemoryRuntime 根据自己的已插入集合与该上限决定请求能否接受。**它不保存另一份 requested_size。** 设备在处理请求期间串行处理该协议的目标更新，避免上限与请求交错；内存控制面不把目标容量强制变成已插入事实。

#### 9.3 设备循环不能自锁

设备在读 guest 请求后，复制为拥有的地址/长度/命令值并释放数据租约，再提交资源请求。不能带着目标 Window 的 queue/descriptor 租约等待内存服务，否则是自己阻止屏障完成。响应写入重新取租约；guest 把响应队列本身拔掉属于无效使用，必须报告访问失败，不能继续解引用缓存的 HVA。

设备循环把这项请求保存在 `PendingMemory { operation_id, reply }` 中，同时继续处理已有 I/O 完成与控制事件。**不能用一个阻塞整个设备循环的嵌套 await**，让释放租约的完成事件无人消费。pause 关闭新业务提交，不关闭排空/完成/对账能力。仍然只有一个 !Send 设备 owner，不把整个对象交给第二个任务或 Mutex。

#### 9.4 已接收的操作属于服务

操作已接收后属于 MemoryRuntime，不属于回复 oneshot。调用方取消、丢弃回复或进入 Busy(pause)，都不能 drop 正在撤销的资源。

设备状态导出必须等已提交请求得到确定结果、且所有写授权排空，再导出自己的协议状态。静止结果带当前代次，不能用“收到 pause 消息”代替排空证据。窗口状态与设备状态各有一个所有者，暂停屏障取得的是两者一致的切面，不靠互相持有整个设备锁。

### 10. 变更、失败与保留

#### 10.1 MemoryChange：一个操作的具体未完成状态

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

操作 ID 用于未结束请求与回复对账，不创造无限历史表。`applied` 不能只有一个整数进度，它保存可用于补偿或继续的真实注册范围、handle 与资源保活关系。等待外部调用时，操作所有权留在服务中。新方案未成为事实之前，不能丢掉 `before` 中的资源。

#### 10.2 三种失败，不同后果

| 情况 | guest 与资源后果 |
|---|---|
| 提案验证失败，尚未产生副作用 | 返回拒绝，旧状态不变 |
| 副作用失败，补偿已完成 | 返回失败，重新开放旧状态 |
| 部分副作用无法补偿 | 保留 StalledChange，禁止相关 grant/backing 重用；按 CPU 可见状态决定是否必须暂停 |

外部操作从未承诺跨 KVM / 多个 DMA 域原子成功。**补偿成功才能声称未改变。**

VFIO unmap 失败不能仅靠保留软件 Arc 宣称撤销完成：设备仍可 DMA。必须保留它覆盖的 backing、外部登记及地址 claim；未解除的范围不得 discard、清零、重置内容或交给新用途。解除映射和结束旧用途要分别取得证据，不能用“unmap 或 stop 任一成功”泛化为全部资源可释放。关闭业务队列不等于停止 DMA，发出 reset 也不等于已经停止。

若无法恢复 guest 已接受的 CPU 可见映射或内容，继续执行会破坏正确性，因此暂停并保留诊断/恢复路径，不自动销毁 VM。新区域的多余映射或回收失败，在不破坏既有 guest 契约且资源仍被保留时可以局部处理；不能把所有失败都升级为 guest 退出。

#### 10.3 StalledChange 的作用域

补偿失败进入具体的 `StalledChange`，仍持有全部未证实可释放的资源及关闭凭据。可以重试完成或隔离消费者，不能用 timeout/free/drop 抹掉它。

StalledChange 关联受影响的 Window 与外部域，而不是把服务永久停在一个全局 Busy future 中。无交集的布局操作仍可被验证和处理；涉及同一未确认硬件域的操作可局部拒绝。保留的 grant 不返回机器分配 owner，保留的对象切片不改变用途。

若 before/proposal 无法分解出独立范围，说明这项操作必须扩大隔离范围，不能凭“局部故障”口号继续。

#### 10.4 逻辑 unplug 与物理回收分开

逻辑 unplug 成功与宿主页回收成功是两件事。先确保访问/注册状态满足已提交的逻辑结果，再进行不可逆的 discard；discard 失败仍可保持逻辑已拔出，只是驻留资源没有回收，记录可重试的回收范围。**不能在可能回滚成 plugged 时先把旧内容打洞。**

#### 10.5 Drop、panic 与泄漏作为最后防线

正常停止使用显式排空/撤销路径，不靠 Drop 执行异步协议。内存服务任务不可在超时后直接 abort，必须保留已接收操作与其资源。资源类的 Drop 只能在确定已经撤销时释放 backing。

若 panic 或宿主任务意外消失导致无法证明外部注册已解除，最后防线是**保留/泄漏**仍可能被访问的 backing 与相关 handle，而不是释放；常规错误应留在 StalledChange 中可检查、可重试。

泄漏不是恢复成功。服务不能从一份未证实的区域记录自动重启并重新发放权限。本稿不承诺无监督的进程内 crash recovery。

### 11. 撤销证据

时间边界不能藏在代次号里。这是第 5 轮的核心修订：把“谁能证明什么”单独列出来，而不是靠术语暗示。

#### 11.1 证据按执行者分类

| 证据 | 能证明什么 | 不能单独证明什么 |
|---|---|---|
| WindowAccess 已关、该代次租约排空 | 新软件访问被拒绝，已授权软件访问结束 | vCPU、硬件 DMA、远端 fd 不再访问 |
| KVM 删除完成 / 旧 VM 关闭且无 vCPU 使用者 | 相应旧 CPU 投影不再访问 | 同一 backing 的软件/DMA 权限也结束 |
| 外部 DMA unmap 回执 | 已登记范围的映射已按 backend 保证解除 | endpoint 不会再发送旧请求，或同 IOVA 可换用途 |
| endpoint/domain 在指定代次停止或排空的证据 | 旧用途不能再发出/完成影响新用途的 DMA | 尚存映射/pin 已释放，或所有共享域成员均已停 |
| 远端可信排空/导出撤销协议完成，或进程终止证据 | 在该协议/信任边界内，旧 fd/指针不再访问 | 仅 socket 已关、REM 消息已发送就足够 |

#### 11.2 软件类型能与不能做到什么

**能**：约束谁可以取得新权限；让未完成的副作用继续拥有资源；把不可完成的等待局限到受影响资源。

**不能**：强迫既有租约在有限时间归还；让软件 MappingId 出现在硬件请求里。

这份设计可争取的是安全保留与可诊断撤销，不是无条件、有限时间的回收。

#### 11.3 逻辑退插与 host 换用途不是同一请求

virtio-mem 的请求建立在“guest 不再访问已 unplug 范围”的协议上。CPU chunk 粗粒度时仍明确不承诺恶意 guest 无法重新触碰；VFIO 后续由 guest 违规提交的旧 DMA，同样不能被软件 bitmap 神奇识别。

host 要释放 grant、改变 backing 用途、迁移设备域或把同一存储交给另一个 VM 时，要求更强：软件准入永久撤销、在途访问排空、CPU 投影解除、全部外部权限解除，并在会复用 IOVA/端点时结束旧执行用途。相关证据绑定 Window、VM、域与代次，不接受一个通用的 `stopped: bool`。

unmap 失败不仅禁止 munmap，也禁止清零后重插或把内容当成新对象使用。映射已解除但旧 endpoint 尚未排空时，不允许以新 MappingId 为理由在同 IOVA 安装新用途。共享域的成员关系也必须被证据覆盖。

---

## 第五部分：恢复与迁移

### 12. 迁移不是把运行期对象序列化

#### 12.1 没有独立的 MemoryImage runtime 层

不设独立的 MemoryImage 运行期包装、管理器或序列化入口——它没有自己的运行期不变量。机器快照直接拥有内存的版本化 Layout、已接受 Population 以及内容清单。

#### 12.2 ObjectContents：内容来源与坐标

```text
ObjectContents {
    object_id,
    extents: { object_offset, length, source, source_offset },
}
source = 快照字节 / 显式零区 / 已验证的持久来源 / 按需 pager 来源
```

**内容清单的缺口不能默认解释为零。** 每个需要恢复的对象内范围必须有确定来源：稀疏文件空洞与“本次增量未发送，沿用父代内容”是不同语义，父代来源需有身份与完整性约束。未插入的 volatile 块可以按协议记为显式零或不需恢复内容，但不得把这一套用到 pmem。

普通 RAM、私有快照 CoW、持久共享文件、按需 pager 的内容来源各不相同。pmem 如果由外部文件/存储复制，必须附带不可变的来源身份与可验证的恢复要求；仅保存路径或源机 inode 不能保证跨主机恢复的是同一版本。不支持某种来源时拒绝恢复，不能回落成全零内存。

#### 12.3 快照不保存什么

不保存 HVA、host fd、KVM slot、DMA handle 或源 VM 引用。

内容以 ObjectId + 对象内 offset 标识。一个 backing 被两个 GPA Window 使用时，不把 guest 地址洞搬进内容文件，也不重复传一份数据。**source offset 不等于对象 offset，仍需明确映射。**

requested_size 属于内存设备的协议快照，Population 属于机器快照的内存字段；停机切面校验两者的身份与容量约束。CPU binding chunk 可以重编译，但不能改变已协商的 guest block size。目标机重放 GPA 与 guest NUMA，host NUMA/页尺寸可按显式恢复政策重新放置，不用目标机的偏好默默修改 guest 结构。

#### 12.4 DirtyEpoch / TransferSet 是工作状态，不是 Population

`DirtyEpoch` 代表一轮受支持的迁移写入追踪，具有布局版本与全部写入参与者。CPU、软件设备、直通设备是不同的 dirty 生产者；CPU dirty、软件写入 dirty、直通设备 dirty 经 GPA/IOVA 映射到对象内坐标，形成 `TransferSet`。

迁移必须汇合全部参与者，**不能以 KVM dirty bitmap 冒充全部写集**。“页已发送”“源再次变脏”“目的页已接收”分别属于迁移会话的工作状态，不混为一个 bitmap。

#### 12.5 写入完成决定记脏时间

反例：提交磁盘读入 guest 时 P 被标脏；迁移复制 P 并清位；内核后来才写 P；最终 KVM bitmap 不包含这次软件写入。

修订：**租约保活不等于记录软件写入。** 软件写授权包含对象范围、方向与访问代次，并把记脏责任交给实际完成 owner；同步写入结束及异步 read_into_guest 真正完成后报告所写范围。提交时标记最多是提前的保守记录，不能取代完成后记录。

软件对象的 `DirtySet` 是写入汇合点，完成路径不只引用取得租约时的旧 epoch。迁移开始前已提交、开始后才完成的写入也必须进入当前追踪，不能让换 epoch 丢掉其责任。**采集并清位发生在相应字节复制之前**，复制期间与之后完成的写入保留到下一轮；最终排空全部写授权后才取得不再变化的最后写集。页复制与清位的先后不能随意颠倒。

具体实现仍需证明 bitmap 发布/清位与完成之间的内存序，不能把这些句子当成已实现的同步。此项也不能靠“租约在结束时 drop”自动成立，除非该 drop 确实与外部 I/O 完成绑定；等待 future 被取消、或 io_uring cancel 请求已发出，均不足以证明。具体适配器的完成/取消契约是必须验证的边界。

裸指针 escape 只能发生在有拥有者的 I/O 资源中；完成前不能提前提交 quiesce、释放 lease 或销毁记脏责任。无法确认取消的请求同样属于在途写入。

#### 12.6 最终切面

最终切面需要：暂停 vCPU；设备在自己的循环中停止新 I/O 并排空旧 I/O；排空已接收的内存变更；同步全部 dirty 生产者；采集一致的内存/设备状态。内存服务不等待一个已经在等它的设备给自己发送新视图 ack。

VFIO 必须分别具备设备状态迁移、DMA 写入追踪与最终停止的支持，只有 dirty logging 不够。缺任一能力时拒绝 live migration；stop-and-copy 也不能凭“vCPU 已停”代替设备 DMA 已停。

跨进程 handoff 的源/目标运行权由机器迁移协议决定，不属于内存分配器。目标只在状态/内容/设备恢复完整且获得运行权后运行，内存恢复数据不是避免 split-brain 的凭据。

#### 12.7 初版的明确取舍

初版 live precopy 期间固定 Layout 与 Population，拒绝新热插与 block 插拔，普通 guest/设备 I/O 仍运行。这是明确的能力取舍，不声称支持拓扑变化与迁移无缝并发。guest 内存请求使用协议允许的 busy/retry 响应；不支持该表达的消费者使迁移无法进入该模式。

---

## 第六部分：检验与未决

### 13. 用具体场景检查形状

| 场景 | 应改变的事实 | 不应改变的事实 |
|---|---|---|
| 低/高 RAM 绕过 guest 地址洞 | 两个 Window 的 GPA 放置 | 可以仍用一个连续 backing |
| 添加一个现代 DIMM 类区域 | Layout、Store、CPU/DMA 投影与读索引 | 其他设备的对象所有权 |
| 拔掉仍有异步 I/O 的区域 | 关闭准入、等待租约、撤销外部注册 | 不因请求超时释放 backing |
| virtio-mem 拔一个 block | Population、相应 CPU chunk/DMA、回收记录 | Object 身份、固定容量与 guest 放置 |
| guest IOMMU 改 IOVA 映射 | DomainMap 与该外部域的投影 | host backing 与 GPA 分配 |
| VFIO 加入已插部分块的窗口 | consumer 登记、回放、实际 pin 集合 | 不把整个容量窗口视作全部可 pin |
| VFIO 撤销失败 | StalledChange 与保留资源 | 不释放/重用 claim 与 backing |
| private 快照恢复后 balloon | 页内容来源与回收 | 不让旧快照文件内容再被 fault 回来 |
| 同进程换一个 VM 恢复 | VM 身份、准入、CPU/DMA runtime handles | 可复用唯一移交的存储对象与已选布局 |
| live precopy 的第二次写入 | dirty/TransferSet 工作状态 | 不把“已经传过”当成“以后不再变化” |

这些是设计反例，不是已经跑过的测试结果。后续实现应把每条资源释放前提做成可观察证据，尤其是失败路径；不能用表格打勾代替运行验证。

### 14. 迭代简史

每轮记录原假设、反例、改动与剩余风险。保留否决历史是为了让后续评判知道哪些路已经走过。

#### 14.1 第 1 轮被否决的假设

这些是历史反例，**不是当前保证**：

| 原假设 | 反例 |
|---|---|
| Arc 保活足以处理撤销 | 只保证不发生 use-after-unmap，不能阻止旧设备继续写入已退出 guest 的区域 |
| 发送新视图并等 ack 足够 | 设备可能在等内存操作回复，内存操作又等该设备 ack；异步 I/O 也可能已保存裸指针 |
| Population 与设备计数自然一致 | 两个 owner 间成功响应丢失、取消或部分失败会打破这一点 |
| 按反序调用就能回滚 | VFIO unmap、KVM delete、文件 discard 都可能失败，且有些动作不可恢复原内容 |
| 一个范围粒度适用于所有消费者 | guest block、host hugepage、KVM slot 与 VFIO 可撤销粒度并不相同 |

保留下来的是：Object/Window 分离、纯值 Layout、软件视图是派生索引、外部登记不是 backing 所有权。

#### 14.2 第 2 轮：拒绝“旧视图安全＝旧访问已撤销”

**改动**：增加每 Window 的访问准入与真实在途 MemoryLease；旧索引可以存在但不保留新访问权；Population 权威移入 WindowState，设备只拥有协议事实；异步变更由独立内存服务接收后拥有。

**代价**：软件设备必须遵守租约边界，异步指针缓存不能直接沿用；整个 Window 的屏障可能扰动较多 I/O。暂不引入细粒度锁消除这项代价。

#### 14.3 第 3 轮：外部投影与部分失败必须拥有实际资源

**否决**：通用 listener 广播给所有软件设备；一份 GPA 范围代表所有 DMA 地址域；“失败后反序 drop”；迁移就是序列化 runtime manager。

**增加**：受作用域限制的 DmaProjection、实际副作用拥有记录、StalledChange、对象内内容坐标、完整 dirty 参与者。

**区别声明**：逻辑已拔出不是驻留页消失；跨后端成功不是原子；安全保活不是撤销证据。

#### 14.4 第 4 轮：收缩边界

| 反例/建议 | 本轮处理 |
|---|---|
| 同一可写共享文件可以绕过 ObjectId 的不重叠检查 | 增加 MemoryStore 内真实来源+offset 检查；不声称检测全宿主 alias |
| block bitmap 缺少设备身份与粒度 | Window 描述加入 owner/MemoryDeviceId，块级 kind 包含 block_size |
| host NUMA 不应原样跨主机重放 | 拆出 HostPolicy；guest 布局稳定、host 物化策略显式替换 |
| pmem/共享/可丢弃混成同一种 backing 属性 | 拆分 content_contract 与 backing 能力，禁止破坏持久来源的 discard |
| Runtime 可能接管所有设备/IOMMU/迁移 | 正面列出它拥有的内存事实；DMA 域、设备协议、机器暂停各保留 owner |
| 单版本号不能代表外部映射已同步 | 分来源 revision、实际成功登记、MappingId 代次；禁止迟到清理跨新映射 |
| MemoryImage 没有独立运行期不变量 | 删除独立包装/服务概念，快照直接拥有内存字段与对象内容清单 |
| 只保留 Arc 的旧设备仍可写恢复后的 backing | 准入永久撤销与新 VM 身份；导出 fd 的撤销另外要求真实证据 |
| payload 完成又要取得新 used-ring 访问，可能自锁 | 外部 I/O 前取得完整租约集合；不能跨关闭窗口无限等待 |

暂不采纳：任意 alias、通用区间权限树、多来源 discard 注册框架、运行期设备对象共享锁。

#### 14.5 第 5 轮：时间边界不能藏在代次号里

**改动**：撤销证据按执行者分类（第 11 节）；逻辑退插与 host 换用途分成不同请求与不同证据要求；记脏责任移到写入的实际完成 owner，并固定“采集清位在复制之前”的顺序。

**剩余风险**：软件 lease 的队列完成边界；异步控制循环的饥饿与互等；DMA domain 两个输入的一致性；保留资源的增长。文档变长不等于这些风险已解决。

### 15. 仍待专家裁决

#### 15.1 未决取舍

1. Object/Window 分离有真实需求，但非重叠切片限制是否足够？当前不以通用 alias 解决所有后续问题。
2. 唯一 MemoryRuntime 的职责是否恰当？它协调同一内存操作的资源，不拥有 IOMMU 页表、设备协议或普通 I/O。
3. Window 级访问屏障能否接受？更细粒度屏障不是免费优化，必须用测得的负担和明确消费者来推动。
4. 迁移期间固定 Population 是否可接受？若业务要求连续 resize，必须再设计变化记录与迁移 epoch 的关系。
5. 哪些 vhost-user backend 真能撤销导出/排空 DMA，哪些只能固定生命周期？需按协议与实际 backend 验证，不用统一 trait 名字掩盖差异。
6. KVM slot、host pin、回收粒度的预算应如何选？数据模型保留差别，具体默认值不在没有工作负载证据时拍板。
7. 机密内存接入时 AllowedRam 需要叠加 shared/private 来源；现在只保留明确权限边界，不预先实现任意 source registry。
8. 整区可插拔 Window（ACPI DIMM 类）的移除路径未定。移除由 guest 确认弹出触发，协议状态在热插控制器设备手里；但移除 Window 是拓扑变更，要永久关闭准入、解除 CPU/DMA 投影，并把 GpaGrant 交还机器地址 owner。待定的是：这条请求走该设备的 WindowControl（需要能移除整个 Window 的变体），还是由控制器通知机器控制面、由持有 grant 返还路径的一方发起；以及 guest 迟迟不确认弹出时 host 如何报告（不能因超时释放）。参考：QEMU 在 guest 写入弹出位时由控制器回调机器的 hotplug handler 执行移除（`.references/qemu/hw/acpi/memory_hotplug.c:179`、`.references/qemu/hw/i386/pc.c:1257`）；Cloud Hypervisor 尚不支持内存弹出（`.references/cloud-hypervisor/vmm/src/memory_manager.rs:638`）。

#### 15.2 最需要被反驳的结论

- **租约是否值得？** 它比依赖永久映射或可信 guest 协议更严格，但会影响第三方 queue、异步 buffer 与 I/O batching。若它需要设备四处额外持锁、复制数据或维护隐蔽状态机，应当否决它，或把在线撤销缩小到能够满足契约的消费者。必须用无额外拷贝的最小原型证明可组合性。
- **MemoryRuntime 是否仍太宽？** 若其操作必须不断调用机器/设备方法才能完成，说明 owner 边界尚未成立；不能只凭“它是专门服务”免责。
- **一个 Window 的屏障是否太粗？** 性能需求足够真实时，可以沿独立撤销粒度拆 Window/屏障；未测量之前不增加每 block 原子状态。
- **外部 DMA 统一到了合适层吗？** AllowedRam/DomainMap 分离有必要，但 fd backend 的能力弱于硬件 pin。若统一接口掩盖撤销证据，应宁可保留不同 endpoint 变体。
- **拒绝并发迁移与热插是否符合 scope？** 初期拒绝有利于保持切面可证明；若专家认为这是核心能力而非优化，需要实质扩展状态模型，而不是删掉限制一句话。
- **失败保留是否可操作？** 每个 StalledChange 应指出哪个外部状态未确认、保留哪些字节/grant、怎样隔离。若只能“留着以后再说”，仍是未完成的设计。

我目前最愿意捍卫的是：Object/Window 分离、按事实唯一 owner、布局与 host 物化分开、失败操作继续拥有实际资源。最不愿提前拍板的是：软件访问租约的代价与 vhost-user 撤销边界。

本稿没有性能、协议适配或故障注入的实证，不据此宣称可直接落地。

#### 15.3 分期检查顺序

依赖顺序只用于检查抽象能否逐步存在，**不是已批准的开发计划**：

1. Object/Window 与纯布局、backing 生命周期、唯一 CPU 注册权；固定视图也可以运行。
2. RamReader/MemoryLease 的真实 I/O 适配与撤销屏障原型；未支持的消费者明确保持固定生命周期。
3. 单一 WindowState 的 virtio-mem，分别验证逻辑插入、CPU chunk 与物理回收，不先引入所有高级 adapter。
4. 首个真实外部 DMA 域接入 replay、实际登记与失败保留；随后才支持相关区域的在线撤销。
5. 快照内容来源与 dirty 会话；先停机切面，再在已支持设备集合上开放 precopy。

不为后期能力写空 trait 实现或无作用的 ack。每一步必须能独立运行，不能为了“最终统一”让现有功能先全部失效。

### 16. 参考锚点

参考代码提供真实需求与反例，**不证明本草稿的同步机制已经成立**。

| 需求证据 | 位置 |
|---|---|
| region / KVM slot / guest block 三种粒度并存 | `.references/firecracker/src/vmm/src/vstate/memory.rs:403`、`.references/firecracker/src/vmm/src/devices/virtio/mem/device.rs:514` |
| 解析后的 slice 不保活映射；写入在提交时汇合记脏 | `.references/firecracker/src/vmm/src/vstate/memory_slice.rs:30`、`:111` |
| zone/区域/登记/发布分离，政策单位不是连续 GPA | `.references/cloud-hypervisor/vmm/src/memory_manager.rs:108`、`:140`、`:190`、`:247` |
| 软件数据面与外部 DMA 的不同接入方式与回放 | `.references/cloud-hypervisor/virtio-devices/src/block.rs:168`、`.references/cloud-hypervisor/virtio-devices/src/mem.rs:857` |
| 地址解码节点与 backing 身份分开 | `.references/qemu/include/system/memory.h:560`、`.references/qemu/include/system/ramblock.h:25`、`.references/qemu/hw/i386/pc.c:795` |
| 区域内/地址域内两套 offset；多来源 populated 取 AND；VFIO 按可撤销粒度 map | `.references/qemu/include/system/memory.h:97`、`.references/qemu/system/ram-discard-manager.c:415`、`.references/qemu/hw/vfio/listener.c:220` |
| 索引版本发布不是字节快照，也不是旧使用者退出屏障 | `vm-memory-0.18.0/src/atomic.rs:28` |

---

本稿没有把“整理完成”视作设计通过。它把可捍卫的关系、已否决的假设与仍需专家裁决的代价放在同一份草稿里；下一轮可以删除本稿的核心抽象，而不以保全既有文字为目标。
