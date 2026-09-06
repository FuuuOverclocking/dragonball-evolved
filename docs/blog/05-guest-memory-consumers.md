# 五：客户内存有三个使用者

最小实现里的内存代码只有十几行：`mmap` 一块匿名内存，填好 `kvm_userspace_memory_region`，调用 `KVM_SET_USER_MEMORY_REGION`。
加入第一个 vhost-user 设备之后，这十几行需要重写，因为客户 RAM 不只有 KVM 在用。

```text
RAM
├─ KvmVm       KVM_SET_USER_MEMORY_REGION
├─ VFIO        VFIO_IOMMU_MAP_DMA / iommufd
└─ vhost-user  VHOST_USER_SET_MEM_TABLE + backing fd
```

三个使用者对同一块内存有不同要求，其中一条必须在分配的时刻就满足。

## backing 是启动期决定

vhost-user 把 RAM 交给另一个进程直接访问：VMM 通过 unix socket 传递 backing fd，以及“fd 的哪个 offset 对应哪段 GPA”的映射表，后端自行 `mmap`。
匿名 `mmap` 没有 fd，传不过去。所以只要这台 VM 有可能挂载 vhost-user 设备，RAM 从一开始就要建立在 memfd、shmem 或 hugetlb 上：

```rust
enum MemoryBacking {
    Anonymous,
    File { fd: Arc<OwnedFd>, offset: u64 },
}
```

这是启动期决定，不是挂载设备时的决定：热插拔一个 vhost-user 设备无法把已经在使用的匿名 RAM 变成 memfd。
所以 backing 的选择必须出现在 `validate(config, caps)` 中，由宿主能力（memfd 是否可用、有没有 hugetlb）和配置（是否使用 vhost-user、是否启用 DAX）共同决定，不满足就拒绝启动，而不是等到挂载设备时失败。

VFIO 的要求不同但同样要提前确定：设备 DMA 经过 IOMMU 映射，相关页必须能长期 pin，这与 balloon、THP 以及能否 punch hole 的策略相互约束。

## 普通 RAM 与设备窗口分开管理

第二类内存是设备共享窗口，例如 virtio shared memory region 和 virtio-fs 的 DAX 窗口。它们位于 GPA 空间中，也占用 KVM slot，但内容由设备决定。

```rust
struct MachineMemory {
    ram: GuestMemoryMmap,
    device_regions: HashMap<DeviceId, DeviceMemoryRegion>,
    slots: KvmSlotAllocator,
}
```

一种想法是让设备自己管理这段窗口，理由是只有它知道窗口内容。三点决定不这么做：

- KVM slot 数量有上限，是全局资源，允许每个设备自行申请就无法约束总量。
- GPA 重叠是静默错误。两个设备各自算出一段“空闲”地址并注册，KVM 不会拒绝，客户机会在运行一段时间后出现难以定位的故障。
- 快照要求布局稳定。恢复时窗口必须落在原来的 GPA，只有中央分配器能保证。

所以分工是：`MachineMemory` 分配 GPA 和 KVM slot 并向 KVM 注册；设备负责窗口内容和访问协议。设备不能自行分配 GPA 或 slot，[第八篇](08-dax-window.md)会说明这条规则在 DAX 的 map/unmap 上如何落实。

## 注册在三处，注销也在三处

释放顺序是硬性的：任何还能写客户内存的组件仍在运行时，都不能释放它可以写入的内存。
停机时的顺序因此是先停 vCPU，再停外部后端和 VFIO DMA，然后注销 ioeventfd、irqfd 和 slot，最后释放 `MachineMemory`（[第十篇](10-ordering.md)）。
顺序错误的后果不是客户机崩溃，而是宿主内存被一个已经停止的设备继续写入。

还有一项容易忽略的要求：脏页跟踪。增量快照依赖 KVM 的 dirty log，但 VFIO 设备的 DMA 写不在其中，vhost-user 后端的写也不一定在。
缺少设备侧 dirty tracking 时，增量快照的结果是不可靠而不是不够精确，所以能力不足时的正确行为是退回全量快照。

下一篇：内存就绪之后，设备如何运行 → [设备是异步任务](06-device-as-task.md)
