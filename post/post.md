案例一: 建模内存管理: 拥有和访问控制

案例二: 热升级热迁移中 VM 的运行权的证明

案例的设计, 要在更小的 vmm codebase 里做 demo.

类型分类
- 纯数据
    - 如何避免不同领域数据, 错误地互操作? (e.g. 12cm + 30kg) 类型包装
- 所有权
- 证明/能力

内存建模:

概念:
- Object
    - 描述
        - 长度
        - 内容: RAM, pmem
    - Host 策略
        - 后端: 匿名, shared memfd, file (private/shared)
            - 映射后: 页大小, 对齐
            - 可交出 fd
            - 可打洞
        - numa
            - 只允许/prefer/交错
- Window
    - object 切片
    - GPA 基址, 长度 = 切片长度
    - 所有者
        - 启动内存
        - 设备 ID
    - 类型
        - 启动内存
        - 可插拔 (整区/按块)
        - pmem
    - 窗口控制 - 给内存设备
    - 访问控制, 租约
        - 给软件设备
        - 给外部 DMA 域 owner
- 内存 slot
    - 分配
- address claim
- 内存后端
    - 可访问范围
- 未绑定内存
- 已绑定内存
