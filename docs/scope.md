# dragonball-evolved 支持的功能集

## 0. 基线

### 0.1 实现范围

- 平台: x86-64, KVM, PVH 直接内核启动
- ACPI: DSDT/FACP/MADT/MCFG
- 传输/设备: virtio-pci + blk/console
- 快照: 本地快照与恢复
- 安全: seccomp

### 0.2 设计约束

以下功能不在基线实现范围内, 但领域模型从第一天起必须能容纳它们, 不能按静态 VM 设计:

- guest 内存布局在运行时可增可减 (virtio-mem, ACPI 内存热插, balloon).
- 设备集合与 vCPU 集合在运行时可变 (PCI 热插拔, CPU 热插拔).
- 所有资源分配 (GPA, PIO, GSI, BDF 等) 作为数据记录, 快照恢复与迁移时重放而非重算.
- 设备状态可序列化, 且与 VMM 进程解耦 (快照, precopy, 热升级).
- guest 内存可能被 DMA 映射并固定 (VFIO, virtio-iommu), 内存布局变化需要同步 IOMMU 映射.
- 运行时可能需要跟踪脏页 (precopy).

### 0.3 功能组合

以下组合的语义需要在实现相关功能前明确, 当前倾向:

- VFIO + balloon: 互斥, 被固定的内存无法回收.
- VFIO + virtio-mem: 支持, 热插/热拔时同步 DMA 映射/解除映射.
- VFIO + precopy: 不支持, 设备侧脏页跟踪 (VFIO migration v2) 后置.
- virtio-mem / balloon + precopy: 迁移期间冻结内存布局调整.

### 0.4 后续里程碑

按顺序推进, 每一步保持系统可运行:

1. virtio-mem / balloon
2. PCI 热插拔 (ACPI-GED), CPU 热插拔
3. VFIO + virtio-iommu
4. precopy

达到基线后, 下文为未来需要实现的功能集合.

## 1. 平台与 CPU

- 体系结构: x86-64, aarch64
- Guest OS: Linux (6.6+), Windows (Hyper-V enlightenments)
- 加速器: 仅 KVM
- CPU 拓扑
- CPU 热插拔
- x2APIC
- CPU 特性开关 + CPU Model (CPUID, MSRs)
- 机密计算实验性支持 (初期不考虑)

## 2. 启动

- Linux 直接内核启动: bzImage, PVH
- UEFI (同样通过 PVH 启动 edk2 固件)
- fw_cfg (兼容 QEMU)
- ACPI
- SMBIOS
- 启动顺序 / PXE

## 3. 内存

- 静态内存, 大页, shared, memfd
- prefault, THP, KSM
- ACPI 热插 (不支持热拔)
- virtio-mem 热插拔
- virtio-pmem (其实更适合归到存储)
- balloon + free page reporting
- NUMA (SRAT/SLIT, vfio 亲和, HMAT)
- CXL 实验性支持

## 4. PCI / IOMMU / 直通

- PCIe 配置空间 / ECAM / MCFG
- 多 PCI segment
- Root port / switch 模型
- PCI 热插拔 (ACPI-GED)
- MSI / MSI-X
- virtio-iommu
- VFIO 直通 + iommufd
- vfio-user, vDPA 实验性支持

## 5. virtio 设备

blk, net, console, rng, balloon,
fs, pmem, vsock, mem, iommu, rtc
watchdog, gpu (实验性), 

传输层: mmio 和 pci
spec: 1.4, 支持 packed ring
支持 vhost, vhost-user

## 6. 存储与网络

- 磁盘格式: raw, qcow2
- 阻塞 / io_uring (跳过 aio)
- discard / write-zeroes / multiqueue
- 网络后端: tap, vhost-net, vhost-user-net
- 限流

## 7. 快照 / 迁移 / 热升级

- precopy, postcopy
- dirty log, dirty ring
- vfio 设备迁移 (v2)
- VMGenID

## 8. 安全 / 可观测 / 管理

- seccomp, landlock (实验性)
- tracing, metrics
- GDB stub
- TPM 2.0
- pvpanic
