#!/bin/bash
# UI 模块拆分脚本
# 分析 src/app/ui.rs 中的所有函数并按依赖关系分组

echo "=== UI 模块拆分分析 ==="
echo ""
echo "总行数:"
wc -l src/app/ui.rs

echo ""
echo "=== 函数列表 ==="
grep -n "^fn \|^pub fn \|^impl App" src/app/ui.rs | head -50

echo ""
echo "=== 常量定义 ==="
grep -n "^const \|^pub const " src/app/ui.rs

echo ""
echo "=== 类型定义 ==="
grep -n "^struct \|^enum \|^type " src/app/ui.rs
