接着, 让 model 通过(伪)代码抽象思考, 类型驱动设计, 对 VMM 进行建模.

读取太多已有材料会严重干扰/诱导 model 思考, 所以我已经整理了代码和文档.

- 做个阶段性的分享
    - 关于 VMM 的开发, 函数式 core + 异步 runtime 是什么样的
    - 类型驱动建模
    - 形式化验证
    - 发挥模型擅长交互式网页的特长

告诉 AI, InstanceInfo 中的 VmState 可以随意修改.

当前 codebase 已提供 vmm 外壳. 考虑 scope 隐含的需求, 让我们来调研和设计 VMM 的生命周期和内存管理, 注意两方面:
1. 类型驱动建模
2. 设计有真实需求支撑
3. 从简到繁, 分阶段做, 用户来推动阶段

期望的产出内容:
- crates/vmm/src/{lifecycle.rs, memory.rs}: 纯类型无过程, 接受伪代码, 允许细节待定
- ai-sessions/{lifecycle.md, memory.md}: 文档, 还可以用网页来**辅助**说明状态变迁

可以自由使用 git 分支 `ai-explore/` 和 `.qoder/worktrees/` 来探索, 但不要查看仓库的历史.

InstanceInfo 中的 VmState 可以随意修改.
