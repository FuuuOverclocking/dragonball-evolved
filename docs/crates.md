# Crate 依赖关系

1. `api` 是工作区的叶子 crate, 不依赖工作区内其他 crate (自身的 `api-macros` 除外).
2. 只有 main crate (`dragonball-evolved`) 可以依赖 `logger-backend`; 其他 crate 只依赖 `logger`.
