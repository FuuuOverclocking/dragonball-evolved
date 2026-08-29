crates/logger
- rate-limited static per-callsite
    - Generic Cell Rate Algorithm
    - 参考 Firecracker /fuu.code/firecracker
        - 拷贝的代码要有版权注释

crates/logger-backend
- 普通日志 与 signal crash 日志分开两个路径
- 普通日志
    - 异步日志, `logger-backend` 后台线程, 从 mpsc 接收日志, 负责写入
    - FlushGuard: drop 时执行 logger_backend::flush(), 可随意生成多个该结构
    - 运行时可更换配置, 包括
        - log target: stderr, file(path)
        - log format: json, text
        - log fields:
            - show_tid
            - show_thread_name
            - show_target
            - show_file_line
            - show_id (实例 id)
- crash log
    - async signal safe
    - 允许用户更换写入的目标, 通过 dup2 更换 fd 背后的文件
    - 预存时区偏移, 始终使用 text 风格输出
    - nanosleep, 给后台日志线程宽限期

logger 被其他所有 crates 使用, 而 logger-backend 只被 bin crate 使用.
