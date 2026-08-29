# API 代码生成

> 声明: 本文为古法制造!

这个 crate 将展示通过 API 的编译时反射, 生成 1) 自描述的 metadata, 2) 配置文件 schema, 3) RESTful API 和 4) CLI/REPL.

## 0. 基本

**请求和响应一对一**

VMM API 的基本单元是 `enum VmmOp`, 枚举每一种变体是对 VMM 的一种操作. 在老 Dragonball 里我们会看到各种操作的响应被一股脑塞进了

```rs
pub enum VmmData {
    Empty,
    VmmStatus(HashMap<String, CommonStatus>),
    // ..
}
```

跑是能跑, 但你会多出一些恼人的 `.unwrap()`, `_ => return Err(XXXError::InvalidResponse)`, 以及更多的人工确认步骤.

在新 Dragonball 中, 我们无疑需要更优写法, 让请求和响应类型一对一, 让编译器给我们打辅助, 类似于

```rs
VmmOp {
    Op1 -> Resp1,
    Op2 -> Option<Resp2>,
}
```

**高阶类型**

此外, 我们遇到了这样的两难情况:

- **场景 A (RPC/请求响应)**: 客户端发送一个 GetStatus 操作给后台线程, 需要等待后台线程回复状态. 这时候必须带上一个通信管道 (比如 `oneshot::Sender<VmmStatus>`).
- **场景 B (事件流/命令回放/持久化)**: 我们只关心“发生了什么操作”, 不需要回复. 比如系统内部记录日志, 或者从配置文件反序列化出操作列表. 这时候带个 `oneshot::Sender` 既不合理也不能被序列化.

常规做法要把 `enum` 写两份, 产出大量重复的 match 代码. 幸好 Rust 1.65 稳定了 GAT (泛型关联类型), 我们现在可以用它来表达高阶类型:

```rs
enum VmmOp<M: OpMode> {
    GetStatus(GetStatus, M::Reply<VmmStatus>),
    AddDisk(AddDisk, M::Reply<Result<()>>),
    // ..
}

type VmmRequest = VmmOp<op_mode::Request>; // 发出请求者关注响应
type VmmCommand = VmmOp<op_mode::Command>; // 只管发出命令, 哪管水火滔天

mod op_mode {
    pub trait OpMode {
        type Reply<T>;
    }

    pub struct Request;
    impl OpMode for Request {
        type Reply<T> = oneshot::Sender<T>; // 一次性的通道, 用来发回响应.
    }

    pub struct Command;
    impl OpMode for Command {
        type Reply<T> = (); // 单元类型, 这让 VmmCommand 可以序列化.
    }
}
```

如此一来, `VmmOp` 可以专注描述 `请求 -> 响应` 的映射关系, 不需要预先假定发回响应的方式.

## 1. 自描述的 metadata

自描述的 metadata 是这个 crate 的核心. 它通过对 API 的定义进行反射 (Reflect), 生成关于自身定义的运行时信息, 可以大大提升往上编程的 **灵活性**.

**什么地方需要这种灵活性呢?**

1) 允许用户查询 API 的支持程度

VMM 这样复杂的生产项目很快会迭代许多版本, 有时版本还并非线性的, 分叉为场景特供. 指望在客户端维护一个庞大的 `版本-特性` 矩阵不切实际且负担过重.

让 API 自身的定义成为可查询的数据, 解放了用户记忆版本的负担, 可以精准查询特性的支持程度.

2) 生成一致的 Schema 文件

新 Dragonball 允许通过 config file 和 RESTful API 去配置它, 而相应的 JSON Schema 和 OpenAPI YAML 就成为了维护负担 (后者还将用于生成 SDK).

传统方法依赖人肉纠错, 把代码上的变更统一到 schema 文件上, 这不仅费时费力, 还存在写错的风险. 让 AI 来同步效果很不错, 但 运行速度 / token 成本 / 正确同步的稳定性 仍有不足.

正统方法是代码生成, 通过反射得到 API 定义的元数据, 然后直接产生 schema 定义.

**宏 + Crate schemars**

```rs
define_schema! {
    VmmOp {
        UpdateLogger -> Result<()>;
    }
}
```

## 2. 配置文件 schema

## 3. RESTful API

## 4. CLI/REPL
