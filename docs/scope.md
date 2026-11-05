# dragonball-evolved 支持的功能集

## 0. 基线

- 传输/设备：virtio-pci + blk/net/console/balloon/pmem/mem/iommu
- 平台：ACPI（DSDT/FACP/MCFG/SRAT）, PVH 直接内核启动
- 热插拔：CPU（x86）、PCI（ACPI-GED）、内存（virtio-mem）
- 直通：VFIO + virtio-iommu
- 迁移/快照：precopy + 本地快照；postcopy 后置
- 安全：seccomp

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
