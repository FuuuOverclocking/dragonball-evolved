# 八：DAX 窗口既不是内存也不是设备

virtio-fs 的 DAX 做的是这样一件事：客户机访问共享目录中的文件时，不走请求、拷贝、响应的流程，而是直接对一段客户物理地址做 load 和 store，由宿主把文件页映射到那里。
省掉的是每次 I/O 的数据拷贝，代价是 VMM 里多出一类对象：一段 GPA 空间，内容由文件系统后端在运行时修改。

第一个要回答的是它归到哪一类：RAM 还是设备。两种归类都不成立。

## 归为 RAM 的后果

- 快照会把整个窗口当内存导出。窗口可能是几十 GB 的稀疏预留，导出的内容是宿主文件的副本，而文件本身还在宿主上。
- 它会随 RAM 一起被映射进 VFIO 设备的 DMA domain。
- 客户机看到的内存容量会包含它。

## 归为设备的后果

设备需要自己找一段空闲 GPA、自己申请 KVM slot。[第五篇](05-guest-memory-consumers.md)已经说明这条路的三个问题：slot 是全局资源、GPA 重叠是静默错误、快照要求布局稳定。

## 所以它是第三类：设备内存区域

```rust
struct DeviceMemoryRegion {
    guest_range: GuestRange,
    host_mapping: MmapRegion,
    kvm_slot: u32,
    kind: DeviceMemoryKind,   // VirtioSharedMemory | DaxWindow
}

struct DaxWindow {
    region: DeviceMemoryRegion,
    writable: bool,
    mappings: BTreeMap<u64, DaxMapping>,   // window_offset -> (file_offset, len)
}
```

生命周期分成窗口和窗口内的映射两层，这是整个设计的关键：

```text
ResourceManager 分配 GPA + KVM slot
-> 预留 PROT_NONE / MAP_NORESERVE 的宿主窗口
-> KVM 注册整个 window
-> transport 暴露 virtio shared-memory region

FUSE_SETUPMAPPING / vhost-user FS_MAP
-> 在 window 内 MAP_FIXED 映射文件区间
FUSE_REMOVEMAPPING / FS_UNMAP
-> 恢复为 PROT_NONE 匿名映射
```

窗口在启动时一次建好，之后 GPA 和 slot 不再变化；变化的只是窗口内部某些 offset 上映射了哪个文件的哪一段。
`PROT_NONE` 预留的作用是：客户机访问尚未映射的窗口地址会得到明确的错误，而不是读到上一次映射残留的内容。

## map 与 unmap 由谁执行

后端提交请求，`MachineMemory` 执行，且在 vmm-main 上串行执行：

```text
guest FUSE_SETUPMAPPING -> backend -> FS_MAP(fd, file_offset, window_offset, len)
-> vmm-main：校验边界、对齐、权限 -> mmap(MAP_FIXED) -> 回复
```

进程内 virtio-fs 可以省掉 backend-request socket，但仍要走同一个串行入口。
原因是 `MAP_FIXED` 是覆盖操作：两个并发的 map 落在重叠区间上时，结果取决于执行顺序，而且不会有任何错误返回。串行点只能有一个，放在拥有窗口的组件里最直接。

同样地，设备不能自行分配 GPA 或 KVM slot，它只能在分配给它的窗口内、通过 `MachineMemory` 执行的 `MAP_FIXED` 修改内容。

## 三条默认值

**默认只读。** 可写 DAX 意味着客户机的 store 直接落到宿主文件页上，绕过文件系统的一致性检查以及配额、审计逻辑。它必须显式启用，并明确约束文件一致性与安全策略。

**不自动加入 VFIO DMA domain。** 见[第七篇](07-data-plane-outside.md)。

**teardown 有固定顺序。** 先停后端，使其不再提交 map 与 unmap，然后清除映射、注销 KVM slot，最后释放窗口。
反序执行会在后端仍可能提交请求时移除它正在使用的窗口。

## 快照如何处理

不导出页面。保存的是窗口布局和映射元数据，或者只保存空窗口加重建策略，让恢复后的客户机重新发起 `SETUPMAPPING`。

这里有一个边界要承认：DAX 的语义依赖宿主文件的内容。如果 backing 可写，或者恢复时文件已经改变，精确恢复做不到。
此时正确的行为是拒绝恢复或显式使映射失效，而不是把旧的映射元数据套到新的文件上。

另外说明：本项目和参考的 Firecracker 目前都没有实现 DAX，上面写的是目标边界。先确定边界的原因是，按“它就是 RAM”实现会同时影响快照和 VFIO 两处，改起来涉及面很大。

下一篇：前面的决定在快照上会被集中检验 → [快照的前提](09-snapshot-quiesce.md)
