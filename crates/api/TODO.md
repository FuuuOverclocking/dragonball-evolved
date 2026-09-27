- cmds/schema.rs or bin/dgb-schema.rs?
    - 谁来用?
        - 导出 schema: 入口是 make, 发版本、更新SDK
        - 检查 schema: 必要性不强, 开发和运维场景
    - 决定: 分开成独立二进制
- Config/ProcessConfig/schema.rs 要不要放进 crate api?
    - 决定: Config/ProcessConfig 放进 crate api
- clap 声明式 + 部分 impl Args impl FromArgMatches
- features
    - api: 提供反射, 是 GET /v1/metadata 和 schema 的基础

crate api 应当是叶子 crate 还是一个高层 crate?
- 作为高层 crate, 这样其他 crate 可以把自己的配置就近放置, 并且第三方来源的 crate 不支持引用 api crate

先解决绑定表的 feature 边界：当前 CONFIG_BINDINGS
   被整个 metadata 模块门控（crates/api/src/helpers/macr
   os.rs:77），搬回后仅开启 serde
   的反序列化也需要它，因此应让绑定表在 serde
   下可用、宏中 Binding.schema 单独受 metadata
   控制，并删除旧文件自己的 BINDINGS。
   建议把文件加载单独放进 load = ["serde",
   "dep:toml"]：Config、ProcessConfig
   默认可用，序列化及配置反序列化归 serde，JsonSchema 和
    config::schema 归
   metadata，文件读取、继承合并及相关测试归
   load；代价是多一个 feature，但普通 Serde
   使用者不会被迫引入 TOML。
   搬回应保持行为而非照抄旧实现：保留 dragonball
   键、对象递归合并／数组替换、循环继承检查、$schema
   仅在加载时剥离，以及不填充运行时默认值；恢复根模块导
   出，主程序启用 request + load、api-schema 启用
   metadata + load，同时注意 CLI／logger-backend
   仍需适配新的 LevelFilter，验证默认零依赖及各 feature
   组合。