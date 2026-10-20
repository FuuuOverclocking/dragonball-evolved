这是一个 rust vmm 项目, 是 dragonball 开源版本的重构, 我已经大致搭建了代码的框架, 需要你参考

/fuu.code/firecracker (以该项目为主要参考)
/fuu.code/cloud-hypervisor

继续设计实现 vmm. 要点:

- 异步的主循环, 异步的设备
    - vmm 主线程使用 tokio (local runtime), 运行在 vmm 主线程或独立线程的 IO 设备也使用异步语法
- 以能够运行 x86-64 + kvm + linux 为当前目标, 这会包含一些边界和取舍的选择, 例如只支持 PVH 启动等, 提供一个 `- [x]` markdown 任务列表让我选择, 你可以提供建议
- 代码要具有良好的品味, 注重可组合性, 偏好函数式的、纯的实现方式
