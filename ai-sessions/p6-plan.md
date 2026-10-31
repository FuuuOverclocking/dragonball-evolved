# P6 virtio-mem：计划提交与实现启动

## Context

P5c3（`265fe39`）已具备真实 Linux 根盘启动、同宿主同进程停机快照及失败清理所有权。P6 增加第一个运行期内存消费者：Linux 能通过 virtio-mem 插入/拔出内存，控制面改变目标容量，内存服务独占 Population 和 KVM chunk 注册。

先将本计划保存到 `ai-sessions/p6-plan.md`，更新 `ai-sessions/work-plan.md` 的 P6 状态，并仅提交这两个文档；随后启动下面的 P6a 编码分工。保留用户的根 `build.rs` 修改，不 push，不改 `.references/`、`crates/vmm.old/`、权限或工具配置。

## 确定的范围与决定

- 一台机器至多一个 virtio-mem 设备，稳定 ID `virtio-mem0`，与磁盘 ID 冲突时拒绝配置。增加一个 Object 和一个固定 Window，Window 使用 `WindowOwner::VirtioMem` / `WindowKind::VirtioMemRam`；ObjectId/WindowId 记录在 plan，不按恢复顺序推断。CPU chunks 是运行期绑定单位，不把 layout 拆成每 chunk 一个 Window。
- 默认 block 2 MiB、chunk 128 MiB。区域非零、为 chunk 整数倍；block/chunk 为二次幂，chunk 为 block 整数倍，block 不小于 backing page。记录 GPA、MMIO、GSI grant；replay 校验而非重新分配。
- 区域位于既有 x86 device aperture，起点至少 1 GiB 对齐并满足 chunk/block 对齐，上限受宿主 GPA 和 1 TiB aperture 约束。不放进启动 e820，初始 requested/usable/plugged 都为零。
- 同一个 `MemoryStore` 映射全部容量，保持固定软件 DMA view；没有租约。部分 chunk 含 unplugged block 时仍整体 CPU-mapped。`UNPLUGGED_INACCESSIBLE` 是有效 guest 的访问义务，不宣称此阶段能拦截所有非法软件 DMA 或部分 chunk 内的非法 CPU 访问。
- 不做 P7 持久化、磁盘内容快照、dirty tracking、memfd handoff、NUMA、外部 DMA/租约或通用 memory manager。

## P6a：内存基础与协议纯核

### 内存模型及 slice 注册

修改 `crates/vmm/src/memory/{layout,store,policy}.rs`、`vm.rs`；新增 `memory/hotplug.rs`。

- 复用 `MemoryLayout::new/boot` 和 `MemoryStore::materialize`；BootRam/e820 过滤保持不变。以 `MemGeometry` 记录块/chunk几何，以不可变 `Population` 记录唯一已插块集合，转换生成 proposal 和精确资源 effect，plugged_size 从 Population 派生。
- 新增 `SlotPermit::bind_slice(WindowId, Range<ObjectOffset>)`：参数是对象坐标中的切片，必须非空、包含于该 Window 的对象切片并按 host page 对齐。GPA/HVA 从 Window 起点和切片相对偏移推导；保留真实 mapping Arc，不能生成无主 raw region。`Binding` 记录实际注册范围；既有 whole-window bind 复用同一路径。
- `MachinePlan::new/replay` 检查 `boot_windows + region_size/chunk_size <= host.max_memslots`，装配用现有 `MemSlots::allocate` 一次预留所有 chunk permits。成功 unbind 归还并复用原 permit，不因每次拔插消耗新 slot。
- 内存模块提供一个仅服务使用、校验 VirtioMem Window/切片/页对齐的 discard 入口；AnonymousPrivate 用 `MADV_DONTNEED`，SharedMemfd 用 `MADV_REMOVE`。不把破坏性入口挂到 `MemoryView`。

### virtio-mem 协议

新增 `devices/virtio/mem/protocol.rs`，最终 worker 放在 `devices/virtio/mem.rs`。

依据只读本地 `.references/specs/virtio-1.4/tex/device-types/mem/description.tex` 与 `.references/linux/include/uapi/linux/virtio_mem.h`：device ID 24，单 guest-request queue；request 24 字节、response 10 字节、config 56 字节，显式小端编码，避免 Rust 结构 padding。

- 强制 VERSION_1 和 `UNPLUGGED_INACCESSIBLE`；不提供 ACPI_PXM、PERSISTENT_SUSPEND。
- PLUG/UNPLUG/STATE 均要求对齐、正 block count、无溢出且完整位于 usable 区域；混入已插 PLUG/未插 UNPLUG 返回 ERROR；插入总量超过 captured requested limit 返回 NACK；STATE 返回 plugged/unplugged/mixed。
- UNPLUG_ALL 只看 type；成功后 plugged=0，可缩 usable。所有错误先完成纯验证，再做任何资源效果。
- requested 减小允许暂时小于 plugged；usable 不能随目标缩小，只有成功 UNPLUG_ALL 可缩。推荐增长为 `max(old_usable, round_up(requested, chunk))`，ALL 后可缩为 rounded requested（不超过 region）。

## P6b：单所有者内存服务

新增 `memory/service.rs`，通过既有 `DeviceRuntime::spawn_on(Placement::Io)` 放在 I/O LocalRuntime，future 可 !Send。

- 服务独占 Population、revision、chunk 的 `SlotPermit/Binding` 和一个具体未完成变更；设备仅持有窗口限定的 `WindowControl` 与只读 `WindowStatus`。不保存另一份 requested_size 或 plugged bitmap。
- owned 请求携带地址/长度/操作和当时目标上限。服务接收后拥有操作，与 reply receiver 的生死无关；返回 `PendingMemory { operation_id, reply }` 供设备 select，不嵌套 await。
- PLUG：先绑定新变为非空的 chunks，全部成功后提交 Population、发布状态、完成应答。
- UNPLUG/ALL：先解绑新变为空的 chunks，再 discard 精确移除的 block 切片，最后提交/发布/应答；非空 chunk 保留绑定，但移除的块仍可 discard。
- Unbind 失败保留 live Binding，禁止对其未撤销范围 discard。破坏性效果开始前失败可完整补偿回旧注册；补偿未完成则保留真实资源和待办步骤。discard 开始后不能承诺内容回滚，保留具体未完成变更并向前完成，不返回假 ACK，也不以 BUSY 掩盖半完成变更。只有未实施效果或完整回滚的请求可以按协议拒绝。
- 服务阻塞/失败时禁止新变更及快照，控制/完成/停止仍可被轮询；超时保留所有者。关机先停止 CPU，再排空/终止请求及释放真实注册；停止服务不 discard 仍插入的内容。最终通过 P5c3 PendingCleanup 判断资源确已释放。

## P6c：设备循环与动态配置

修改 `devices/virtio/{mmio,mod,interrupt}.rs`，新增窄的配置发布类型和 virtio-mem worker。

- worker 先校验完整 descriptor 方向、guest 地址、请求与响应空间，复制 owned 请求，保留 queue head 与 PendingMemory；同时轮询完成、quiesce、reset、resize 和通知。每次只接受一个内存变更，不改变既有 block worker 的单所有者形状。
- 已接收 guest 请求与目标更新串行处理；resize 不改变已接收请求的目标上限。quiesce 关闭新请求准入，但必须继续排空完成；无 pending 工作才 ACK。reset 等旧请求退出队列/RAM访问后释放 session，同时保留 Population、requested/usable 和 plugged 内容。
- 配置 reader/publisher 分离；publisher 不 Clone，单一设备 owner 发布完整固定长度 image，transport 只读。读配置与 generation 使用一致的同步边界，generation 在可能出现不一致读值的配置变化时更新。
- 保留 `DeviceDesc` 的不可变身份验证，增加独立配置 image/generation 快照；block 配置仍严格与当前后端容量匹配，mem 动态字段由设备状态与 Population 派生并校验。不通过忽略整个 descriptor/config 比较来放宽恢复。
- required_features 在 FEATURES_OK/activation/restore 均校验。requested/usable 改变发 Config IRQ；plugged 更新 generation，但通常不发 Config IRQ；ALL 的 usable 调整遵守规范通知例外。恢复配置只安装状态，不在 CPU/LAPIC/clock 前注入 IRQ。

## P6d：API、装配和生命周期

修改 `crates/api/src/{machine,lib}.rs`，`crates/vmm/src/spec.rs`，`machine/{plan,build,mem,state,cleanup}.rs`，`main_loop.rs` / `main_loop/controller.rs`。

- `SetMemoryHotplug(MemoryHotplugConfig { size_mib, block_size_mib: Option<u32>, chunk_size_mib: Option<u32> })`：`.machine.memory_hotplug`，PUT `/v1/machine/memory-hotplug`，只在未启动时配置。
- `ResizeMemory(HotplugSizeMib)`：PATCH 同一路径，只在 Running 接受；零有效，超区域/未配置/未对齐/错误 payload 明确拒绝且不修改状态。成功表示目标配置已发布，不等待 Linux 达到目标。
- 复用 define_schema 的枚举/路由/config/schema 派生；补所有 exhaustive matches，不手写另一套 REST 路由或兼容命令。
- DevicePlan/DeviceSnapshot 区分 Block 与 Mem；根盘继续优先，mem 放在磁盘之后，所有 MMIO 参数从记录 grant 派生。
- 用具体 `PreparedMem/AttachedMem` 和按 ID/kind 的 prepared 数据扩展共享停泊装配；不引入 positional Option 状态或通用 restore flag。
- pause：CPU → 设备排空已接收工作 → 服务；resume：服务 → 设备 → CPU。控制面转发/发布完成通过 select 处理，不堵住主循环；在途 resize 与生命周期操作不因客户端断开或 Shutdown 被取消。失败资源纳入既有 PendingCleanup。

## P6e：快照与运行验收

修改 `machine/snapshot.rs` 和现有 regression specs。

- 内存快照按 WindowId 保存 Population/revision；设备快照保存 requested/usable、queue、transport/config 状态。计数不作为第二个可变权威。
- preflight 拒绝缺失/多余/错 kind、几何/Population/usable 越界、features/queue/config 不一致；允许 plugged 超过缩小后的 requested。
- capture 取得实际设备/服务 ACK 且无未结束变更。重建先校验已有 RAM 和全部纯状态，恢复 VM/vCPU/clock，再绑定已插 chunks、装配停泊设备/服务、取得 ACK、补 pending IRQ；失败仍返回 Ready RAM 或具体 PendingCleanup。
- 更新生成 schema 和 API payload/配置回归规格；添加纯协议、chunk跨界、重复请求、reset、失败绑定/解绑/discard、超时资源保留和两种 backing round-trip 的确定性回归，不用固定 sleep 伪造完成。
- 当前 `/verify` 指导下不运行测试、typecheck、clippy 或 API feature CI来代替验收；允许构建与格式检查，新增用例留给 CI。验收通过实际公开接口与进程观察：
  1. P6a 用外部公共库示例实际创建 KVM、注册 Window slice，观察访问/解绑/permit复用、两种 backing discard 内容和驻留变化。
  2. 真实 CLI/REST：`sg kvm`、`/work/dbe-kernel/build/vmlinux`、双 CPU、只读 `rootfs-drained.ext4`、无 initrd，`init=/bin/sh rootflags=noload memhp_default_state=online_movable`。
  3. grow → touch已插页 → shrink → zero → regrow；用 guest 串口握手、MemTotal、memory block/sysfs 与宿主 aperture RSS 证明生效/回收，不把 PATCH 200 当收敛。
  4. 两种 backing 的公开库暂停/capture/teardown/rebuild/resume，保留 Population、已插页标记与 RAM 身份；检查恢复期间无提前内存/队列活动。
  5. 错误 resize、错误 feature、非法/跨界协议请求和失败清理探测；同时用不同扇区读取与 `/sys/block/vda/stat` 检查 virtio-blk 回归，最终 reboot 正常退出。
- guest 制品及驱动器放 `/work/dbe-kernel`，不修改参考 kernel 源码、不覆盖已有日志；保持普通无 hotplug CLI 可运行。

## 启动实现的编码分工

计划文档提交后立即启动 P6a，两项只写不重叠路径的前台子代理：

1. 内存基础：`memory/hotplug.rs`、layout/store/policy、`vm.rs`，实现 Population/切片注册/discard，附确定性规格；不碰 machine/API/virtio。
2. 协议纯核：`devices/virtio/mem/protocol.rs`，实现 wire layout/请求解码/响应配置编码与规范校验，附规格；不碰 memory/transport/API。

主会话审查真实 diff、协调 module wiring 并运行公共 KVM 示例。固定这些实际接口后再启动 service 与 config publisher，最后串行接 worker/API/machine/snapshot，避免代理同时修改核心文件。每个可运行子阶段验收后独立提交；本次不提前承诺整个 P6 已完成。
