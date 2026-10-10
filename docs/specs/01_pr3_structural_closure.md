# PR 3 问题陈述：结构编辑的真实 DOCX 构建闭环

> 来自对 `rs/main @ 2a23b40` 的**实测复现**，不是推测。

## 1. 结论（一句话）

Rust 的 `insert_paragraph` + `commit_sync` 能把新段落写进 `typed.md`，
但 `build_docx` **100% 拒绝构建**——新段落没有建立到 DOCX 的通道。
删除段落同理。

## 2. 实测复现（可重跑）

```bash
# 环境：rs/main @ 2a23b40, target/debug/docx2typed.exe
# 源：一个含单段落 "第一段落 ABC" 的 docx

docx2typed --json extract src.docx -o wd --operation-id e1     # success
# MCP: workdir_open(wd)                                         # success, paragraphs=1
# MCP: insert_paragraph(after_id=P0, text="新插入的段落 XYZ", inherit=P0)
#                                                               # success, temp_id=N1
# MCP: commit_sync                                              # success, changed=[N1]
# MCP: build_docx(output=out.docx)                              # ❌ FAIL
```

`build_docx` 的失败回执：

```json
{
  "outcome": "failure",
  "diagnostics": [{
    "schema": "docx2typed-diagnostic-1",
    "code": "edits-not-implemented",
    "severity": "error",
    "category": "domain",
    "retriable": false,
    "message": "this workdir has committed typed edits that are not recorded as island
                edits; replaying the template would drop them — re-commit the draft or
                build with the Python reference engine"
  }]
}
```

**注意错误信息自己承认：** 「build with the Python reference engine」——即 Rust 侧做不到。

## 3. 根因（已定位到行）

构建守门在 `src/mcp.rs:1235`：

```rust
let islands_cover_edits = root.join("islands.json").is_file();
...
if has_committed_edits && !islands_cover_edits {
    return self.failure("build_docx", "edits-not-implemented", ...);
}
```

`islands.json` 为何不存在——`crates/docx2typed-review/src/draft.rs` 的记录循环：

```rust
let Some(old_body) = before.get(id) else {
    continue;                        // ← 新段落在这里被丢弃
};
```

`before` 是 **extract 时的段落表**。新段落 `N1` 在 extract 时不存在
→ `before.get("N1")` = `None` → `continue` → 不记录 island edit
→ `recorded == 0` → `islands.json` 从不落盘 → build 拒绝。

commit 后的 workdir 实况：

```
typed.md:
  <!--@p id="P0"-->
  第一段落 ABC
  <!--@p id="N1" base=""-->          ← base 为空：无基线字节
  新插入的段落 XYZ

islands.json: 不存在
```

## 4. 为什么这是「结构性」而非「实现遗漏」

Island edit 的模型是 **`IslandEdit { paragraph_id, leaf_index, old, new, ... }`**，
语义 = 「在**已存在**的叶子字节范围内，把 `old` 换成 `new`」。

新段落**没有基线字节范围**可替换（`base=""` 就是证据），所以：

- 它无法表达为一个 island edit；
- `apply_edits`（`core/prose.rs:1239`）的构建路径依赖
  `resolve_leaf(inventory, ...)`，而 inventory 来自 **template bytes**——
  新段落不在其中，会直接 `invalid-edit: leaf not found`。

即：**需要一条「合成新 `w:p`」的路径**，而不是把 island edit 的模型硬套上去。

## 5. Python 对照

Python 的 `insert_paragraph`（`mcp_server.py:4063`）与 `delete_paragraph`（`4144`）
走的是**编辑 canonical typed 树 + build 时重建段落**的路径，
不是 island-byte-replace。这正是两侧模型差异所在：

| | Python | Rust（现状） |
|---|---|---|
| 已有文字替换 | typed 树 + span 合成 | island byte replace ✅ |
| 新建段落 | typed 树 + 段落重建 ✅ | **无路径** ❌ |
| 删除段落 | typed 树 + 删除标记 ✅ | **无路径** ❌ |

## 6. PR 3 的验收标准

必须端到端真实通过（全部）：

1. `insert_paragraph` → `commit_sync` → `build_docx` **成功产出 DOCX**
2. 产出的 DOCX 中**确实存在**新的 `<w:p>`（用独立手段验证：解 zip 看 `word/document.xml`）
3. `verify_output` 通过
4. 新段落可被**再次编辑**（再 patch 一次仍能构建）
5. `delete_paragraph` 真正从产物中移除该 `<w:p>`
6. 原始源 DOCX **字节不变**
7. 未授权区域（页眉/样式/关系/其他 part）**字节不变**
8. 不支持的删除（节属性、修订、锚点、表格）**明确拒绝**，不静默丢内容

## 7. 不在 PR 3 范围

- 修订模式的段落级增删（分别覆盖 insert/delete/同作者/异作者/段落标记修订）——拆到后续 PR
- 格式引擎（第二阶段）
- Python oracle 的行为对齐（新能力 oracle 里没有）

## 8. 设计要冻结的点（动手前）

1. **新段落的 ID 分配**：现在 `@new temp="N1"` → `<!--@p id="N1" base=""-->`。
   正式 ID 是保留 `N` 前缀，还是分配 `P<n>`？两者对 `history`/`blame` 的稳定寻址影响不同。
2. **新段落的格式来源**：`inherit="P0"` 现在只是记录。合成 `<w:p>` 时，
   `pPr`/`rPr` 从哪个基线取？需要显式定义，不能猜。
3. **删除的表示**：`<!--@delete id="X"-->` 在 build 时如何转成「不输出该 `w:p`」，
   同时保证 `sectPr`、bookmark、comment range 等不被破坏。
4. **两条路径的关系**：新段落走「合成」，已有段落走「island replace」。
   同一段落既有改动又是新建时如何取优先级；`islands.json` 是否仍需要。

## 9. 复现清单

| 现象 | 路径 |
|---|---|
| 守门代码 | `src/mcp.rs:1235` `edits-not-implemented` |
| 丢新段落 | `crates/docx2typed-review/src/draft.rs` 记录循环的 `continue` |
| 合成路径缺失 | `crates/docx2typed-core/src/prose.rs:1239 apply_edits` → `resolve_leaf` |
| Python 对照 | `mcp_server.py:4063` `insert_paragraph` / `4144` `delete_paragraph` |
