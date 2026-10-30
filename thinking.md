
- core::range::Range 似乎不是一个很好的选择, 一方面 {start,len} 总体上在 vmm 更常见, 另一方面不能表示 <num>::MAX 是个问题, 构建自己的 Range 类型, 并参考 std 补全周围的实现
- page size 要更严谨, 区分 host/guest 侧的, 并考虑不同来源的内存 (如 hugetlbfs) 可能要求更大的对齐

