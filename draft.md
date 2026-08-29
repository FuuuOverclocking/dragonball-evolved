- 开源反推闭源重构, 作为新一代闭源的骨架

核心目标:
- 异步的主循环, 异步的设备
- api 的反射
- 展示 logger 怎么写

次要目标:
- TUI 和 GUI
- 版本显示问题
    - main 上, 如果当前 commit 没有与 cargo 版本一致的 tag, 那么 minor + 1, -nightly
    - vXX.XX 分支上, 如果不一致, patch + 1, -nightly
