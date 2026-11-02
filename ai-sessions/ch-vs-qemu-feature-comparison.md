# Cloud Hypervisor 与 QEMU 功能交集 / 差集调研

- 日期：2026-11-02
- 范围：x86-64 + KVM + Linux guest（与 dragonball-evolved 当前阶段一致）；AArch64 / RISC-V / TCG / 其他加速器只在必要处提及
- 参考树：
  - `.references/cloud-hypervisor`：`8740d14`（2026-10-25），Cargo 版本 `55.0.0`（master，文档对应 v54）
  - `.references/qemu`：`d731492`（2026-10-24），`VERSION = 11.1.50`（master）
- 方法：以两边 `docs/` 与源码目录为准，关键结论用源码 grep 复核；**不凭记忆**。文中路径均相对 `.references/`。

---

## 0. 结论先行

1. **功能交集 = "云负载 virtio 主干"**：KVM、virtio-pci 设备族、PCI/CPU/内存热插拔、VFIO/vfio-user/vDPA 直通、virtio-iommu、ACPI/fw_cfg/SMBIOS、UEFI 与直接内核启动、precopy/postcopy 迁移、快照、TPM、balloon/virtio-mem、seccomp。这一层两家语义高度对齐，但实现与兼容策略差异很大。
2. **差集呈极度不对称**：
   - **QEMU 是功能超集**，多出的部分主要是三类：① 传统/桌面设备（VGA、USB、IDE/AHCI、声卡、PS/2、软驱、串并口）；② 企业级平台特性（机器类型版本化、命名 CPU 模型矩阵、DIMM/NVDIMM/CXL、Intel VT-d/AMD-Vi 仿真、SR-IOV 仿真、vhost-net、9p、virtio-gpu/input/sound/scsi）；③ 工具/运维面（QMP/HMP、QEMU GA、record/replay、RDMA 迁移、桌面显示后端）。
   - **Cloud Hypervisor 真正独有的很少**：REST/JSON 控制面、Landlock 沙箱、MSHV 后端、CPU 绑核与 core scheduling、CPU profile、memory zone 模型、offload daemon、通用 vhost-user 设备直通、`virtio-watchdog`。其中只有前几项是"能力"，其余多为"形态选择"。
3. **同名功能、不同语义**才是迁移/设计时最需要关注的：热插拔机制、CPU 模型、迁移兼容、PCI 拓扑、控制面协议。见第 5 节。
4. 对 dragonball-evolved：**MVP 取交集里的 virtio + ACPI/hotplug 子集即可；不要复制 QEMU 的传统设备面积；应从 QEMU 借 bootindex/PXE、packed ring、dirty ring、SR-IOV 等真实缺口；从 CH 借"配置即数据"的 REST 控制面、Landlock、memory zone 与 offload 思路。**

---

## 1. 两家的定位差异（解释差集的根因）

| 维度 | Cloud Hypervisor | QEMU |
|---|---|---|
| 目标 | 只跑现代 64 位云负载；最小仿真、小攻击面（`cloud-hypervisor/README.md:35-58`） | 通用机器仿真器：云、桌面、嵌入式、跨架构（`qemu/docs/system/introduction.rst`） |
| 架构 | x86-64 / AArch64 主力，RISC-V 实验；**Rust** | 21 个 target 架构；C |
| 加速器 | KVM、MSHV | KVM、TCG、Xen、HVF、WHPX、NVMM… |
| 设备模型 | virtio-pci 为主 + 极少量 legacy（`cloud-hypervisor/docs/device_model.md`） | 数百个 device model，含完整 PC 兼容机 |
| 兼容策略 | 无机器类型版本化；迁移/快照仅保证同版本（README 明确 n-2 迁移窗口） | versioned machine type（`pc-i440fx-*`、`pc-q35-*`）保证跨版本 ABI |
| 控制面 | REST/JSON over UNIX socket；`ch-remote` | QMP（JSON-RPC over socket）、HMP、QEMU GA |

---

## 2. 功能交集

图例：✅ 支持；⚠️ 支持但有限制/实验性；❌ 不支持；— 不适用。

### 2.1 平台与 CPU

| 能力 | Cloud Hypervisor | QEMU | 差异备注 |
|---|---|---|---|
| KVM 加速 | ✅ | ✅ | CH 要求宿主内核 ≥ 6.8（`cloud-hypervisor/README.md:85`） |
| x86-64 / Linux guest | ✅ | ✅ | QEMU 另支持 Windows/其他 OS，CH 仅 Win10/11 + Linux |
| CPU 拓扑（socket/die/core/thread） | ✅ `--cpus topology=` | ✅ `-smp` | CH 无 `clusters/modules` 层；QEMU 有（`qemu/qemu-options.hx:348`） |
| CPU 热插拔 | ✅ x86 only，`max > boot` | ✅ | 都需 guest 侧 online CPU |
| vCPU 上限 | 8192（KVM x86-64） | 受宿主/ACPI 限制 | CH 超过 254 自动切 x2APIC（`cloud-hypervisor/arch/src/x86_64/mod.rs:1158`） |
| x2APIC | ✅ 自动 | ✅ | |
| 嵌套虚拟化 | ✅ `nested=on`（显式开关） | ✅ 依赖宿主 KVM + `-cpu host` 暴露 VMX/SVM | QEMU 无独立开关 |
| Hyper-V enlightenments | ✅ `kvm_hyperv=on` | ✅ `hv-*` | 二者覆盖子集不同 |
| CPU 特性开关 | ⚠️ 仅 `amx` + 实验性 `x-cpuid-overrides` | ✅ 完整 `-cpu model,+feat,-feat` | 见 5.3 |
| 命名 CPU 模型 | ❌ 只有 `host` + profile | ✅ `qemu64/kvm64/Skylake/EPYC/...`（`qemu/target/i386/cpu.c:3544+`） | QEMU 独有 |
| 机密计算 | ⚠️ SEV-SNP（KVM 经 IGVM + fw_cfg；MSHV 原生） | ✅ SEV/SEV-ES/SNP、TDX、SGX | CH 无 TDX/SGX（SGX 已移除） |

### 2.2 启动 / 固件

| 能力 | Cloud Hypervisor | QEMU | 差异备注 |
|---|---|---|---|
| 直接内核启动 | ✅ bzImage 或 PVH vmlinux + initrd/cmdline | ✅ `-kernel/-initrd/-append`（含 multiboot、PVH ROM） | |
| UEFI | ✅ edk2 `CLOUDHV.fd` | ✅ OVMF | |
| 轻量固件 | ✅ Rust Hypervisor Firmware | ⚠️ qboot（仅 microvm） | |
| fw_cfg | ✅（feature `fw_cfg`，QEMU 兼容） | ✅ | |
| ACPI 表生成 | ✅ DSDT/FACP/MCFG/SRAT/SLIT/TPM2/SPCR/DBG2/IORT | ✅ 完整（含 DMAR/HEST 等） | CH 无 DMAR：IOMMU 是 virtio，不是 VT-d |
| SMBIOS | ✅ `--platform` | ✅ `-smbios` | |
| 启动顺序 / PXE | ❌ 单一 payload，无 `bootindex` | ✅ `-boot`、`bootindex`、iPXE | **CH 的真实缺口** |
| 多启动项 | ❌ | ✅ | |

### 2.3 内存

| 能力 | Cloud Hypervisor | QEMU | 差异备注 |
|---|---|---|---|
| 静态内存 + hugepages + shared + memfd | ✅ | ✅ | |
| 预缺页 / THP / KSM | ✅ `prefault` / `thp` / `mergeable` | ✅ | |
| 内存热插拔 | ✅ ACPI memory device（仅增）+ virtio-mem（增删） | ✅ ACPI DIMM + virtio-mem | CH **没有 DIMM device**（`cloud-hypervisor/docs/memory.md`） |
| NVDIMM | ❌（用 virtio-pmem） | ✅ | |
| virtio-pmem | ✅ | ✅ | |
| virtio-mem | ✅ | ✅ | |
| balloon + free page reporting + 统计 | ✅ | ✅ | |
| NUMA（SRAT/SLIT） | ✅ + ACPI Type-5 Generic Initiator（VFIO 设备亲和） | ✅ + HMAT | CH 无 HMAT |
| memory zone（多后端分区） | ✅ `--memory-zone` | ❌（用多个 memory-backend + NUMA 表达） | CH 独有模型 |
| CXL | ❌ | ⚠️ CXL type-3（演进中） | |

### 2.4 PCI / IOMMU / 直通

| 能力 | Cloud Hypervisor | QEMU | 差异备注 |
|---|---|---|---|
| PCIe 配置空间 / ECAM / MCFG | ✅ | ✅ | |
| 多 PCI segment | ✅（≤96） | ✅（pxb/pxb-pcie） | CH 用 segment 扩展，QEMU 用 expander bridge |
| Root port / switch 模型 | ❌ 设备直挂 root complex | ✅ `pcie-root-port`、`xio3130` | 影响 AER/热插拔语义与 guest 拓扑感知 |
| PCI 热插拔 | ✅ ACPI-GED | ✅ 原生 PCIe + ACPI + SHPC | |
| MSI / MSI-X | ✅ | ✅ | |
| SR-IOV 能力仿真 | ❌（只透传 VF） | ✅ `pcie_sriov.c` | |
| virtio-iommu | ✅ | ✅ | |
| 仿真 vIOMMU（VT-d / AMD-Vi） | ❌ | ✅ `intel-iommu`（含中断重映射）、`amd-iommu` | CH 无 DMA remapping 仿真，故无 DMAR ACPI 表 |
| VFIO 直通 | ✅ + iommufd | ✅ + iommufd | |
| VFIO mdev | ❌ | ✅ | |
| vfio-user | ⚠️ 实验性 | ✅ `vfio-user-pci` | |
| vDPA | ⚠️ 实验性 | ✅ | |
| ATS/PRI/PASID | ❌ | ✅ | |

### 2.5 Virtio 设备族

| 设备 | Cloud Hypervisor | QEMU | 备注 |
|---|---|---|---|
| virtio-blk | ✅ | ✅ | CH 另有 vhost-user-blk；QEMU 另 vhost-user-blk/scsi |
| virtio-net | ✅（control queue、MQ、GUEST_ANNOUNCE、MTU、offload） | ✅ + failover | CH 无 vhost-net |
| virtio-console | ✅ | ✅ | |
| virtio-rng | ✅ | ✅ |
| virtio-balloon | ✅ | ✅ |
| virtio-fs | ✅（仅 vhost-user-fs + virtiofsd） | ✅（仅 vhost-user-fs） | 两家都无 DAX |
| virtio-pmem | ✅ | ✅ |
| virtio-vsock | ✅ 原生实现 | ⚠️ `vhost-vsock`（内核 vhost） | 实现路径不同 |
| virtio-mem | ✅ | ✅ |
| virtio-iommu | ✅ | ✅ |
| virtio-rtc | ✅ | ✅（`virtio-rtc-pci.c`） | v54 新增 / QEMU 同名 |
| virtio-watchdog | ✅ | ❌（只有 i6300esb/ib700） | **CH 独有设备** |
| virtio-scsi | ❌ | ✅ | |
| virtio-gpu / input / sound / crypto | ❌（明确不支持 GPU/声音，`docs/generic-vhost-user.md:3-15`） | ✅ | 桌面/富设备方向 |
| 9p | ❌（仅作为通用 vhost-user 类型出现） | ✅ `virtio-9p` | |
| 传输层 | 仅 virtio-pci | virtio-pci + virtio-mmio（microvm） | |
| packed ring | ❌（只声明 split-ring 特性位） | ✅ | 已 grep 复核 |
| 多队列 / queue affinity | ✅ | ✅（多队列）/ ❌（亲和性由外部绑核） | |
| 通用 vhost-user 设备 | ✅ `add-user-device`（任意 virtio id） | ❌（只有 gpio/i2c/rtc/fs/gpu/input 等专用） | **CH 独有形态** |

### 2.6 存储与网络

| 能力 | Cloud Hypervisor | QEMU | 备注 |
|---|---|---|---|
| 磁盘格式 | raw、qcow2、vhd、vhdx、vmdk | raw/qcow2/qed/vdi/vmdk/vhdx/vpc/luks/nbd/rbd/iscsi/ssh… | CH 格式面窄但覆盖云镜像；QEMU 有些后端（gluster 11.1 已移除） |
| io_uring | ✅（默认 feature） | ✅ | |
| discard / write-zeroes / multiqueue | ✅ | ✅ | |
| 块限流 / 限流组 | ✅ | ✅ throttle-group | |
| 网络后端 | tap、macvtap、vhost-user-net、offload daemon | tap、user/slirp、passt、bridge、socket、vde、netmap、af-xdp、vhost-user、vhost-vdpa | QEMU 无 AF_PACKET 后端；**CH 无 vhost-net** |
| 网卡模型 | 仅 virtio-net | e1000/e1000e/igb/rtl8139/ne2k/pcnet/vmxnet3/virtio-net… | |
| 网络限流 | ✅ per-queue bw/ops | ❌ 未找到 | QEMU 限流只在 block 层 |
| 网卡热迁移 failover | ❌ | ✅ `failover=on` | |

### 2.7 迁移 / 快照 / 模板

| 能力 | Cloud Hypervisor | QEMU | 备注 |
|---|---|---|---|
| precopy 迁移 | ✅ UNIX/TCP + 可选 mTLS | ✅ + TLS/RDMA/exec/多通道压缩 | |
| postcopy | ✅（UFFD；排除 VFIO/vDPA/vhost-user） | ✅ | |
| 脏页跟踪 | ✅ **KVM dirty log** | ✅ dirty bitmap **+ dirty ring** | 已复核 CH 无 dirty ring |
| 多连接迁移 | ✅ `connections=1..128` | ✅ multifd | |
| 迁移取消 | ✅ `/vm.cancel-migration` | ✅ | |
| VFIO 设备迁移 | ✅ v2（同版本） | ✅ | |
| 版本兼容 | 仅 n-2，且非同版本不保证 | 机器类型版本化 + 设备兼容 | 见 5.4 |
| 快照/恢复 | ✅ 目录式（config/state/memory-ranges），支持 ondemand/CoW | ✅ savevm/qcow2 快照 + migrate-to-file | |
| VM 模板 / 快速克隆 | ✅ 基于 CoW restore | ✅ `docs/system/vm-templating.rst` | |
| COLO / record-replay | ❌ | ⚠️ COLO 实验；record/replay 稳定 | |
| guest coredump | ✅ `/vm.coredump` | ✅ `dump-guest-memory` | |

### 2.8 安全 / 可观测 / 管理

| 能力 | Cloud Hypervisor | QEMU | 备注 |
|---|---|---|---|
| seccomp | ✅ 每线程过滤器，默认开 | ✅ `-sandbox`（全局） | 粒度不同 |
| Landlock | ✅ | ❌ | CH 独有 |
| 控制面 | REST/JSON + OpenAPI，`ch-remote` | QMP/HMP | 都可用，协议不同 |
| 事件 | JSON event monitor | QMP events | |
| 指标 | `/vm.counters`、performance-metrics | `query-stats`、block stats | |
| tracing | ✅ `tracing`/`tracer` | ✅ trace events | |
| gdb stub | ✅（`guest_debug`） | ✅ | |
| 调试端口 | ✅ 0x80（带时间戳日志）、0xE9（可重定向 console）、0x402（固件 stdout） | ⚠️ 仅 0xE9 `isa-debugcon`（iobase 可改）；0x80 为空实现吞写；无 0x402 | 见第 5.9 节 |
| TPM | ✅ 2.0 CRB + swtpm socket | ✅ CRB/TIS + emulator/passthrough/swtpm | |
| VMGenID | ✅ | ✅ | |
| pvpanic | ✅ | ✅ | |
| ivshmem | ✅ plain only | ✅ plain/doorbell/flat | |
| RNG 后端 | ✅ `--rng src=` | ✅ builtin/random/egd | |
| QEMU guest agent | ❌ | ✅ | |
| 宿主侧 vCPU 绑核 | ✅ `affinity=` + core scheduling | ❌（依赖 taskset/cgroup） | CH 独有 |

---

## 3. 仅 Cloud Hypervisor 支持

严格按"QEMU 没有对应能力"筛选，数量很少：

| 能力 | 价值 | 依据 |
|---|---|---|
| REST/JSON 控制面（OpenAPI + `ch-remote`） | 配置即数据，易做声明式编排；与 QMP 的命令式风格不同 | `cloud-hypervisor/docs/api.md` |
| Landlock 文件系统沙箱 | 在 seccomp 之外再做路径级约束 | `docs/landlock.md` |
| MSHV 后端 | 非 KVM 的 Windows/Hyper-V 宿主路径 | `docs/mshv.md` |
| vCPU 绑定 + core scheduling（`PR_SCHED_CORE`） | 延迟敏感负载的确定性 | `docs/cpu.md` |
| CPU profile（JSON CPUID/MSR 调整） | 实验性；无命名 CPU 模型时的替代 | `docs/cpu.md:260` |
| `--memory-zone` 多后端分区模型 | 同一 guest 混合 hugepage/共享/文件后端 | `docs/memory.md:245` |
| offload daemon（TSO/UFO/CSUM 与快照 offload 到独立进程） | 数据面减负、故障隔离 | `offload_daemon/` |
| 通用 vhost-user 设备（任意 virtio device-id） | 可外接 CH 未原生实现的 virtio 设备 | `docs/generic-vhost-user.md` |
| `virtio-watchdog` | guest 看门狗 | `virtio-devices/src/watchdog.rs` |
| 0x80 POST 码日志（带时间戳、按 Firmware/Bootloader/Kernel/Userspace/Custom 分类） | 无侵入的 boot 耗时埋点 | `devices/src/legacy/debug_port.rs` |
| 0x402 固件调试端口（写→stdout，读回 0xE9 作为存在性探测） | 串口驱动就绪前的固件输出 | `devices/src/legacy/fwdebug.rs` |
| pvmemcontrol（实验性） | virtio-mem 的精细控制 | `pvmemcontrol` feature |

> 注意：`vfio-user`、`vDPA`、`CPU profile`、`x-cpuid-overrides` 在 CH 中标注为实验性（`README.md:317-318`）；QEMU 侧 vfio-user/vDPA 已可用。

---

## 4. 仅 QEMU 支持

### 4.1 与"云负载"相关、值得认真评估的差异

| 能力 | 为什么重要 | 依据 |
|---|---|---|
| **启动顺序 / bootindex / PXE** | CH 无此能力，多磁盘/网络启动需固件侧解决 | `docs/system/bootindex.rst` |
| **命名 CPU 模型 + 模型版本化** | 跨宿主迁移的 CPU 兼容基线；CH 只有 host passthrough | `target/i386/cpu.c:3544+`、`docs/system/cpu-models-x86-abi.csv` |
| **机器类型版本化** | 长期 ABI 稳定；CH 明确不保证跨版本 | `hw/i386/pc_piix.c`、`pc_q35.c` |
| **dirty ring** | 迁移脏页跟踪开销更低 | `accel/kvm/kvm-all.c`、`dirty-ring-size` |
| **hotplug 的 DIMM/NVDIMM** | 细粒度内存扩容、持久内存 | `hw/mem/pc-dimm.c`、`hw/mem/nvdimm.c` |
| **仿真 vIOMMU（VT-d/AMD-Vi）** | 需要 DMA remapping 的 guest/直通组合；CH 只有 virtio-iommu | `hw/i386/intel_iommu.c` |
| **SR-IOV 能力仿真** | 在 guest 内创建 VF 的标准路径 | `hw/pci/pcie_sriov.c` |
| **vhost-net** | 内核态网络数据面，成熟稳定 | `hw/net/vhost_net.c` |
| **vhost-scsi / virtio-scsi** | SCSI 语义、多 LUN、热插拔 | `hw/scsi/`、`hw/virtio/vhost-scsi.c` |
| **postcopy + 压缩 + RDMA + multifd** | 迁移工具箱更完整 | `migration/` |
| **vhost-user-gpu / virtio-gpu（含 3D/venus）** | GPU 虚拟化（若未来需要） | `hw/display/` |
| **CXL type-3** | 内存扩展方向（尚早） | `hw/cxl/` |
| **SEV/SEV-ES/TDX/SGX** | 机密计算组合更全 | `target/i386/sev.c`、`hw/i386/sgx-epc.c` |
| **netdev failover** | 热迁移时 virtio-net 与 VF 切换 | `docs/system/virtio-net-failover.rst` |
| **QEMU guest agent** | guest 内运维通道 | `qga/` |
| **vmcoreinfo** | 崩溃分析 | `hw/misc/vmcoreinfo.c` |

### 4.2 传统 / 桌面 / 兼容性面积（CH 有意不做）

- 显示：std VGA、Cirrus、QXL、VMware SVGA、ATI VGA、bochs-display、ramfb；后端 GTK/SDL/SPICE/VNC/curses/DBus（`hw/display/`、`ui/`）。
- USB：UHCI/EHCI/XHCI/OHCI、`usb-host` 透传、`usb-redir`、USB 存储/HID/串口/网卡/音频/CCID（`hw/usb/`）。
- 存储控制器：PIIX IDE、ICH9 AHCI、lsi53c895a、megasas、pvscsi、mptsas、NVMe（`hw/ide/`、`hw/scsi/`、`hw/nvme/`）；软驱（`hw/block/fdc.c`）。
- 网络：e1000/e1000e/igb/rtl8139/ne2k/pcnet/vmxnet3/eepro100/tulip（`hw/net/`）。
- 声音：AC97、ES1370、SB16、AdLib、GUS、Intel HDA、PC speaker（`hw/audio/`）。
- 输入/串并口：完整 PS/2、ISA 串口、并口、HPET。
- PC 固件/机器：SeaBIOS、`pc`/`q35`/`isapc`、多代 machine type、`microvm`。
- 9p、IPMI、SPICE。
- TCG 与 21 个 target 架构（跨架构仿真、插件、record/replay）。

### 4.3 平台与工具

- QMP/HMP 全套命令与 schema、QEMU GA、`qemu-img`/`qemu-nbd` 生态。
- 迁移压缩算法（zstd/qatzip/qpl/uadk）、RDMA、exec 通道、CPR。
- CXL、NVDIMM、SGX EPC、HMAT。
- `-sandbox`、命名空间、9p xattr 等安全选项。

---

## 5. 同名功能的不同实现（比清单更重要）

### 5.1 PCI 拓扑
- CH：所有设备作为 root complex 下的 PCIe endpoint，按 segment 扩展；**不建模 root port/switch**。
- QEMU：`q35` + `pcie-root-port` + expander bridge，guest 能看到真实拓扑，AER/热插拔语义更完整。
- 影响：CH 更简单、设备地址可预测；QEMU 更接近真实服务器，但拓扑配置复杂。

### 5.2 内存热插拔
- CH：二选一——ACPI memory device（只增，128 MiB 粒度）或 virtio-mem（增删）。
- QEMU：DIMM 热插拔 + virtio-mem + NVDIMM，可组合。
- 影响：CH 的"两套机制互斥"是刻意的简化；QEMU 的 DIMM 需要 guest 侧 memory block 驱动配合。

### 5.3 CPU 模型
- CH：`host` passthrough 为主；`profile` 与 `x-cpuid-overrides` 是实验性补丁式方案。
- QEMU：命名模型矩阵 + `-vN` 版本 + ABI 表，为跨宿主迁移服务。
- 影响：集群内异构宿主迁移时，CH 需要自己维护 profile；QEMU 有成熟基线。

### 5.4 迁移/快照兼容
- CH：同版本内保证；跨版本仅 n-2 的有限窗口，快照不跨版本。
- QEMU：machine type + device 兼容属性 + 迁移流版本，追求长期 ABI。
- 影响：热升级场景下，CH 必须停机重建或做状态转换；QEMU 可原地升级。对 dragonball 的 handover 设计，这是必须自证的规约问题（参见 AGENTS.md 的 TLA+ 要求）。

### 5.5 控制面
- CH：REST + OpenAPI，资源导向（`/vm.add-disk` 等），适合声明式编排。
- QEMU：QMP 命令式 + 事件；HMP 面向人。
- 影响：CH 的"配置即数据"更容易与项目偏好的不可变状态/重放模型对齐。

### 5.6 安全边界
- CH：每线程 seccomp + Landlock，vhost-user 后端天然进程隔离（含 offload daemon）。
- QEMU：全局 seccomp sandbox + 可选命名空间；vhost-user 同样进程隔离。
- 影响：CH 的 per-thread 过滤器更细；QEMU 的 sandbox 覆盖面更广但粒度粗。

### 5.7 设备直通形态
- CH：`--device`（vfio）、`--user-device`（vfio-user）、`--vdpa`、通用 vhost-user。
- QEMU：`vfio-pci`（含 mdev）、`vfio-user-pci`、`vhost-vdpa-device`、专用 vhost-user-*。
- 影响：CH 的通用 vhost-user 更灵活；QEMU 的 mdev 支持是 CH 缺失项。

### 5.8 中断控制器
- CH：IOAPIC（split irqchip，且与 serial 绑定）+ 动态 x2APIC；无 8259 PIC、无 HPET。
- QEMU：完整 8259 + IOAPIC + HPET + `kernel-irqchip=on/off/split`。
- 影响：CH 只能跑现代 ACPI guest；QEMU 可跑老 OS。

### 5.9 调试端口（0x80 / 0xE9 / 0x402）
- CH：三个独立设备、可同时使用。
  - **0x80**：写 1 字节即按 code 区间打 `Debug I/O port: <类别> code 0xNN` 日志并附相对 VMM 启动的时间戳；只读会报错。始终存在。
  - **0xE9**：Bochs/QEMU 风格的字符输出，可重定向到 tty/file/pty（`--debug-console`，默认关闭）。
  - **0x402**：写 1 字节直接 `print!` 到 stdout，读回 `0xE9` 作为"调试端口存在"的 magic；供自家 Rust Hypervisor Firmware 在串口驱动就绪前输出。
- QEMU：
  - **0xE9**：`isa-debugcon`（`hw/char/debugcon.c`），写字节转发到 chardev（默认 vc/stdio），`-debugcon dev`，`iobase` 属性默认 0xE9 可改（因此可把第二个实例放到 0x402）。
  - **0x80**：`hw/i386/pc.c:210-218` 只注册了空实现的 `ioport80_write`（丢弃）和返回全 1 的 read，**不记录、不打印**——只是避免 guest 写 POST 码时触发未映射 I/O。
  - 无内置 0x402 处理。
- 影响：CH 的 0x80/0x402 是"有语义的调试设施"，QEMU 的 0x80 只是兼容占位。需要 guest 早期日志时，QEMU 依靠 `isa-debugcon` + 改 iobase 或串口，CH 开箱即用。



### 6.1 建议的 MVP 基线（取交集）
- 传输/设备：virtio-pci + blk/net/console/rng/balloon/vsock/pmem/mem/iommu/fs(vhost-user)。
- 平台：ACPI（DSDT/FACP/MCFG/SRAT）、fw_cfg、SMBIOS、UEFI + 直接内核启动。
- 热插拔：CPU（x86）、PCI（ACPI-GED）、内存（virtio-mem）。
- 直通：VFIO + virtio-iommu。
- 迁移/快照：precopy + 本地快照；postcopy 后置。
- 安全：seccomp。
这一集合两家语义一致，参考资料最充分，且与项目"x86-64 + KVM + Linux、最小仿真"的定位吻合。

### 6.2 明确不做（至少当前阶段）
- QEMU 的传统设备面积：VGA/USB/IDE/AHCI/声卡/PS2/软驱/串并口/9p/e1000 系列。
- 桌面显示后端（GTK/SDL/SPICE/VNC）。
- TCG 与跨架构 target。
- 机器类型版本化的完整矩阵（成本极高；CH 用"不保证跨版本"换取了实现简化）。

### 6.3 值得从 QEMU 借鉴的真实缺口
1. **bootindex/PXE**：CH 没有，实际部署会碰到；实现成本不高。
2. **packed virtqueue**：CH 缺失；若追求 virtio 规范完整度应补。
3. **dirty ring**：比 dirty log 更适合高脏页率迁移。
4. **SR-IOV 能力仿真**：若要在 guest 内管理 VF。
5. **命名 CPU 模型/兼容基线**：若目标包含跨宿主迁移，建议用"类型化的 CPU 能力集"表达，而不是 CH 的 JSON profile 补丁。
6. **postcopy 的失败语义**（UFFD 页错误、源端释放时机）值得对照实现。

### 6.4 值得从 CH 借鉴
1. **REST 资源模型 + 配置即数据**：与项目"状态转移返回新值、恢复时重放"的品味一致；比 QMP 命令式更易做 Kani/TLA+ 建模。
2. **Landlock + per-thread seccomp**：比全局 sandbox 更细粒度，契合"权限由持有的值表达"。
3. **memory zone**：把"内存后端"建模为显式资源集合，避免全局 manager。
4. **offload daemon / 通用 vhost-user**：数据面与控制面分离、故障就地隔离的现成范式。
5. **"两套内存热插拔互斥"** 这类刻意简化：用类型而非运行时检查表达（例如 `HotplugMethod::Acpi | VirtioMem` 的 sum type）。

### 6.5 风险提示
- **迁移/快照兼容**是两家最大的语义鸿沟。若 dragonball 要做热升级 handover，不能照搬 CH 的"同版本"假设，也不能直接套 QEMU 的 machine type；需要按 AGENTS.md 先用 TLA+ 定规约。
- **仿真 vIOMMU 的缺失**会限制某些直通组合；若目标客户需要 VT-d 语义，需提前评估。
- **无 root port 模型**会影响 guest 对 PCIe 拓扑的感知（AER、带宽协商），对高性能网卡直通可能有影响。

---

## 7. 证据索引与存疑项

### 7.1 主要证据
- CH 功能面：`cloud-hypervisor/README.md`、`docs/{cpu,memory,hotplug,pci,iommu,vfio,vdpa,fs,snapshot_restore,live_migration,seccomp,landlock,api,device_model}.md`、`virtio-devices/src/lib.rs:23-45`、`vmm/src/config.rs`、`arch/src/x86_64/mod.rs`。
- QEMU 功能面：`qemu/docs/system/**`、`qemu/qemu-options.hx`、`qemu/hw/{i386,virtio,net,block,display,usb,audio,scsi,nvme,mem,pci-bridge}/`、`qemu/target/i386/cpu.c`、`qemu/migration/`、`qemu/block/`。
- 复核命令（关键项）：
  - CH 无 packed ring：`virtio-devices/src` 仅有 `#[repr(C, packed)]`，无 `VIRTIO_F_RING_PACKED`。
  - CH 无 vhost-net：`vhost-kern` 仅用于 vDPA。
  - CH 无 SR-IOV：`pci/src` 无 `sriov` 符号。
  - CH 无 dirty ring：`hypervisor/src` 无 `DIRTY_RING`。
  - QEMU 无通用 vhost-user-device：`hw/virtio/` 只有 gpio/i2c/rtc/fs/gpu 等专用。
  - QEMU 无 virtio-watchdog：`hw/watchdog/` 只有 i6300esb/ib700 等。
  - QEMU 网络限流：`net/` 无 rate-limit 符号（仅 block 层 throttling）。
  - QEMU vhost-user-fs 无 DAX 属性：`include/hw/virtio/vhost-user-fs.h` 无 `dax`。

### 7.2 存疑 / 需进一步确认
1. CH 的 `x86_64` PCIe root port：源码中未发现 root port 模型，但未逐行确认所有拓扑路径。
2. CH 的 TDX 支持：当前树无 `docs/intel_tdx.md` 与 CLI 入口，推断已移除；仅 IGVM 注释提及。
3. CH 的 packed ring：依据特性位列表推断不支持，未检查底层 `virtio-queue` 依赖是否悄悄启用。
4. QEMU 的 vfio-platform 移除版本（agent 报 10.2）与 gluster 移除（11.1）以 `docs/about/removed-features.rst` 为准，建议引用前再核对行号。
5. QEMU 是否有独立的网络限流能力：本次未找到，但不排除通过 `netfilter` 或外部 tc 实现。
6. QEMU 各设备/迁移特性的"稳定/实验/弃用"状态随 master 变动，落地前应对照 `docs/about/{deprecated,removed-features}.rst` 再确认一次。
