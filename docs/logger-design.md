# logger 设计

日志拆成两个 crate:`logger` 是所有 crate 都用的前端,`logger-backend` 只由 bin crate 使用,取代现在的 `src/logger.rs`。

```
所有 crate ─→ logger           宏 + per-callsite 限流,只依赖 log
bin crate  ─→ logger-backend   log::Log 实现 + 写入线程 + crash handler
                   └─→ logger
```

库 crate 不该知道日志写到哪里,所以 `logger` 保持叶子地位,不引入文件、线程、序列化依赖。后端由 bin crate 在启动时 install 一次;它自身的日志也走 `logger` 的宏,单向依赖不成环。

## logger

### 宏

| 宏                                                              | 限流 | 用途                               |
| --------------------------------------------------------------- | ---- | ---------------------------------- |
| `error!` `warn!` `info!`                                        | 有   | 默认。guest 可触发的路径必须用这组 |
| `error_unrestricted!` `warn_unrestricted!` `info_unrestricted!` | 无   | 仅 host 路径:启动、配置、快照      |
| `debug!` `trace!`                                               | 无   | 透传 `log`                         |

`clippy.toml` 的 `disallowed-macros` 禁止直接调用 `log::error!` 等,把这张表变成强制的。

宏体先看 `log_enabled!`,级别关闭时不碰限流器。限流发生在记录进入后端之前,顺带保护了后端队列。

### per-callsite 限流

宏体内声明 `static LIMITER`,每个展开点独立一份,刷爆一处不会压制别处。

状态是单个 `AtomicU64`,算法是 Generic Cell Rate Algorithm:

```
bit 63                                             bit 0
┌──────────────────┬─────────────────────────────────────┐
│  suppressed (24) │            tat_ms (40)              │
└──────────────────┴─────────────────────────────────────┘
```

- `tat_ms`:theoretical arrival time,进程 epoch 起算的毫秒,40 bit ≈ 34 年。
- `suppressed`:待报告的拒绝计数,饱和于 2^24-1。

每次调用取 `earliest = max(tat, now)`、`new_tat = earliest + REFILL_MS / BURST`,若 `new_tat - now > REFILL_MS` 则拒绝,否则 CAS 推进 `tat`。等价于容量 `BURST`、每 `REFILL_MS` 补满的 token bucket,但状态只有一个字,无锁、无分配。

- 拒绝时饱和自增 `suppressed`;下一次放行时清零,并用 unrestricted warn 报出压制条数,不会递归回限流器。
- CAS 重试上限 16 次,超出即拒绝,不在病态争用下自旋。
- `BURST`、`REFILL_MS` 是 const 泛型,const-eval 断言 `REFILL_MS / BURST >= 1`。默认 10 条 / 5 s。

算法与状态编码抄自 Firecracker `src/vmm/src/logger/rate_limited.rs`,拷贝的文件保留 Apache-2.0 头与出处注释。

## logger-backend

普通日志和 crash 日志走两条完全独立的路径:前者可以分配、可以排队,后者什么都不能做。

### 普通日志

`Log::log` 在调用线程上只做三件事:取时间戳、把 `Arguments` 渲染成 `String`、把 owned record 送进 bounded mpsc。拼字段和选 json/text 都在后台线程,配置由它独占,不需要锁。

record 携带 `timestamp` `level` `target` `file` `line` `tid` `dropped` `message`。

message 必须在调用线程落地 —— `Record` 借的是调用方栈上的 `fmt::Arguments`,活不过这一帧。目标是每条记录恰好一次堆分配,三条规则守住这个「一次」:

- `target` 和 `file` 跟在 message 后面共用同一块 buffer,记下各自长度,后台切片取回。`Record` 只承诺 `&'a str`,不承诺 `&'static`,把它们当静态借用送去另一个线程就是 use-after-free;共用 buffer 的代价是一次 memcpy,而不是两次分配。其余字段是 Copy,有界通道又是预分配数组,没有 per-message 节点。
- 不用 `format!`。它按字面量长度估容量,`{e:?}` 这类插值一超就 realloc。改用 `String::with_capacity(256)` + `write!`。
- record 里不放 thread name。`Thread::name()` 给的是 `&str`,送走就得 `to_string()` 或 `Arc<str>`,每条记录一次分配或一次原子 refcount。改成只发 `tid`,渲染时后台在 cache miss 上读 `/proc/self/task/<tid>/comm`,缓存 tid→name。
    - 名字因此是内核的 `comm`,不是 `Thread::name()`:上限 15 字符,`virtio-block-queue-0` 会截断成 `virtio-block-qu`;匿名线程显示可执行文件名。换来的是与 `top -H`、`ps -L`、perf 看到的一致。
    - 唯一的例外是主线程:它的 `comm` 就是进程名,`pkill`、`killall`、journald 的 `_COMM` 都读它,不该为了日志好看去 `prctl` 改掉。线程组 leader 的 tid 恒等于 pid,后台据此直接报 `main`,一次整数比较,也与 `Thread::name()` 一致。
    - 线程已退出就读不到,回落成只打 `tid`。tid 复用会把旧名字贴到新线程上,概率极小且线程集合是静态的,接受。

其余约束:

- 时间戳必须在调用线程取。后台线程会滞后,记录的是事件时间而不是写入时间。
- `tid` 只有本线程读得到,thread-local 缓存一次 `gettid`,之后是一次 TLS 读。
- 队列满则丢弃并计数,下一条写成功时把丢弃数报出来。日志不能吃光内存,更不能阻塞 vcpu 和设备线程。

### 运行时配置

配置变更作为控制消息走同一条 mpsc,顺序因此天然确定:变更前入队的记录仍按旧配置渲染。

| 项     | 取值                                                                   |
| ------ | ---------------------------------------------------------------------- |
| target | `stderr`、`file(path)`                                                 |
| format | `text`、`json`                                                         |
| fields | `show_tid` `show_thread_name` `show_target` `show_file_line` `show_id` |

`show_id` 指实例 id,由 bin crate 在 init 时一次性写入 `OnceLock`。打开文件在调用线程完成,失败要能返回给调用方(API),送进队列的是已打开的 `File`。

级别是唯一由前端读的配置项,所以它留在 `log` 的全局原子里(`log::set_max_level`),不走队列。

### FlushGuard

`logger_backend::flush()` 送一个带 ack 的控制消息并等待,后台线程排空队列、flush 底层 writer 后回 ack。`log::logger().flush()` 走同一条路径。

`FlushGuard` 是零大小类型,`Drop` 里调 `flush()`。flush 是幂等的 barrier,所以多个 guard 可以随便建 —— main 一个、测试一个、线程里一个都行。

### crash 日志

处理 `SIGSEGV` `SIGBUS` `SIGILL` `SIGFPE` `SIGABRT` `SIGSYS`。这时进程已不可信,不能等后台线程,handler 必须 async-signal-safe:绕开 mpsc、锁和分配,用栈上定长 buffer 手写十进制,直接 `write(2)`。

- 目标 fd 在 init 时固定成一个预留编号(stderr 的一份私有副本),换目标是打开新文件后 `dup3(new, CRASH_FD, O_CLOEXEC)`。handler 只认这个常量,不读任何可变状态。
- crash 目标可以独立设置:`CrashTarget::SameAsLog` 跟随普通日志,`Own(target)` 自己一份。默认跟随 —— supervisor 下 stderr 常是 `/dev/null`,默认写去那里等于默认丢掉。
    - "是否跟随"这个状态属于调用线程(`dup3` 在调用线程做),不进写入线程的 `Settings`,用一个 `AtomicBool` 存,handler 不读它。
    - 另外在调用线程留一份当前普通日志 fd 的副本,好让"重新跟随"这种不带 `target` 的调用也知道该指向哪里;写入线程持有的原件随时可能被换掉,所以存副本而不是编号。
    - 所有文件先全部打开成功再动 fd,任一路径打不开就整体返回错误、什么都不改。
- 记录格式与普通文本行对齐:`<ts> ERRO [tname:tid] crashed on SIGSEGV, code=1, addr=0x0, id=<id>`。信号打名字而不是编号;`si_addr` 只在内核自己抛出(`si_code > 0`)且该信号确实带地址时才打,否则那个 union 里装的是发送方 pid/uid,当地址打出来是误导。字段集是固定的,不受 `show_*` 影响 —— 那些配置由写入线程独占,handler 不能加锁去读。
- 线程名只有 `tid == pid` 时能给出 `main`,其余线程报 `-`:读 `comm` 要开文件,handler 不做。拿 crash 行里的 tid 去翻同线程早先的普通日志即可。
- `clock_gettime` 是 async-signal-safe,`localtime_r` 不是。init 时预存 UTC 偏移秒数,handler 用整数运算算出本地时间。
- 始终 text。json 转义要额外的分支和缓冲,收益为零。
- 写完后 `nanosleep` 一段 grace period,给后台线程机会把已入队的普通日志刷出去;然后恢复默认 handler 并 re-raise,保住 core dump 和退出码。

panic 不是信号,panic hook 可以分配、可以取 backtrace,走普通路径。

## 非目标

- 不做日志轮转。target 是文件或 named pipe,轮转交给外部。
- 限流参数不做运行时可配。需要不同额度时在 callsite 上写死 const 泛型参数。
