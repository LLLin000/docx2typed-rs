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

Python 的 `insert_paragraph`（`mcp_server.py:4330`）与 `delete_paragraph`（`4411`）
走的是**编辑 canonical typed 树 + build 时重建段落**的路径，
不是 island-byte-replace。这正是两侧模型差异所在：

| | Python | Rust（现状） |
|---|---|---|
| 已有文字替换 | typed 树 + span 合成 | island byte replace ✅ |
| 新建段落 | typed 树 + 段落重建 ✅ | **无路径** ❌ |
| 删除段落 | typed 树 + 删除标记 ✅ | **无路径** ❌ |

## 6. 设计点已冻结（来自 Python 源码解析，非推测）

四个必须在动手前定死的点，已从 Python 源码解析出答案。

### 6.1 新段落的 ID：**提交时重新分配为下一个空闲 `P<n>`**

> ⚠️ 本节初版断言「不重新分配，保留 `N` 编号」，并据此认为
> 「commit_sync allocates the formal paragraph ID」这句提示是误导。
> **该结论是错的**：这是我从 `original_index = -1`（`typed_docx.py:2858`，
> 那是**渲染位置**标记）错误外推到了 **id** 上。实测 Python 后确认它**确实重新分配**。

权威实现（`scripts/edit_sync.py:1028`）：

```python
def _next_paragraph_id(used: set[str]) -> str:
    numbers = [int(value[1:]) for value in used if value.startswith("P") and value[1:].isdigit()]
    return f"P{max(numbers, default=-1) + 1}"
```

- 取值 = 所有在用 id 中最大的 `P<n>` 加一；`used` 在**同一次提交内共享**，
  所以插入两段会得到 `P1`、`P2`，而不是两个 `P1`。
- 草稿的临时 id（`N1`）是**草稿局部记账**，**不会**进入 `typed.md`。
- 实测对照（同一份文档，同一操作序列）：

| | `commit_sync.changed_paragraph_ids` | `typed.md` 标记 |
|---|---|---|
| Python 参考 | `["P1"]` | `<!--@p id="P1" inherit="P0"-->` |
| Rust（修复前） | `["N1"]` | `<!--@p id="N1" base="" inherit="P0"/>` |
| Rust（修复后） | `["P1"]` | `<!--@p id="P1" inherit="P0"-->` |

**标记语法**：Python 的解析器要求 `base` / `inherit` **恰好出现一个**
（`typed_core`：「paragraph marker requires exactly one of base or inherit」）。
Rust 修复前同时写了 `base=""` 与 `inherit="P0"`——按 Python 语法是**非法**的。

**删除的记账**：删除一个**非基线**段落（即本谱系里新建的那个）**不留痕迹**。
Python 的 `typed.md` 在 insert→commit→delete→commit 之后**没有** `@delete` 标记，
`structure.json` 也不存在；只有基线段落被删才写 `@delete`。
实测两端一致：产物都回到 `['第一段落 ABC']`。

> 教训：`original_index` 是**渲染位置**的标记，与**id 分配**是两件事。
> 把一处的观察外推到相邻概念，就得到了一条错误的「冻结决定」，
> 并让实现照着错的目标写了一版。

### 6.2 新段落的格式来源：**从 inherit 段落复制 `p_open` + `ppr`**

`_render_paragraph`（`typed_docx.py:1547`）：

```python
p_open = paragraph.p_open or inherited.p_open
ppr    = paragraph.ppr    or inherited.ppr
body   = _render_nodes_seq(paragraph.nodes, paragraph.base_style, styles, tokens)
return (p_open + ppr + body + "</w:p>").encode("utf-8")
```

- `base_style` ← `inherited.base_style`（`assign_default_style`）
- `p_open` ← `_new_paragraph_opening(inherited.p_open)`：复制继承段的开标签并
  **剔除 ephemeral 属性**（`_EPHEMERAL_P_ATTRS`）
- `ppr` ← `inherited.ppr`（原样复用继承段的 `<w:pPr>` 字节）

**继承段的合法性约束**（`typed_docx.py:2833-2846`，全部 fail-closed）：

| 约束 | 错误 |
|---|---|
| `inherit` 必须指向**已存在的 baseline** id | `new paragraph requires existing inherit ID` |
| 继承段不得是容器（表格单元格/文本框/页眉页脚/脚注） | `table-structure-immutable` |
| 继承段不得 `section_bearing` 或含结构节点 | `new paragraph cannot inherit protected structure` |
| 新段落不得新增结构 token（修订容器、锚点） | `new paragraph cannot add structural tokens` |

### 6.3 删除的表示：**不渲染即删除；受保护结构拒绝**

删除的 id 从 `live_paragraphs` 中移除（不进入渲染序列），产物中自然没有该 `<w:p>`。
拒绝条件（`typed_docx.py:2864-2870`）：

```python
if baseline.section_bearing or _contains_structural(baseline.nodes):
    raise ValidationError(f"paragraph with protected structure cannot be deleted: {deleted_id}")
```

（`comments.*` 的删除是 deliberate decision，单独放行。）

### 6.4 新段落的**位置**：`insert_before` 下一个存活基线段

`_paragraph_placements`（`typed_docx.py:1567`）：

- 已有段落 → `slots[i] = original_index`（保留模板槽位）
- 新段落 → `slots = None`，`insert_before = 其后第一个已有段落的 original_index`
- 若其后无已有段落 → `insert_before = template_count`（追加到末尾）

即新段落**插在下一条存活基线段之前**，不是按草稿顺序盲插。

### 6.5 两条路径的关系

Python 只有**一条**路径：`_render_paragraph` 对每个段落渲染；
未触碰的段落走字节回放，触碰/新增的走合成。

Rust 需要让「新段落合成」与「已有段落 island-replace」**共存**：

| 段落类型 | Rust 路径 |
|---|---|
| 未触碰 | 字节回放（现状）|
| 已有段落被改 | island byte-replace（现状 ✅）|
| **新段落** | **合成 `p_open + ppr + body + </w:p>`，按 §8.4 定位插入（需新增）** |
| 被删除 | 不输出（需新增）|

## 7. PR 3 的验收标准

必须端到端真实通过（全部）：

1. `insert_paragraph` → `commit_sync` → `build_docx` **成功产出 DOCX**
2. 产出的 DOCX 中**确实存在**新的 `<w:p>`（用独立手段验证：解 zip 看 `word/document.xml`）
3. `verify_output` 通过
4. 新段落可被**再次编辑**（再 patch 一次仍能构建）
5. `delete_paragraph` 真正从产物中移除该 `<w:p>`
6. 原始源 DOCX **字节不变**
7. 未授权区域（页眉/样式/关系/其他 part）**字节不变**
8. 不支持的删除（节属性、修订、锚点、表格）**明确拒绝**，不静默丢内容

## 8. 不在 PR 3 范围

- 修订模式的段落级增删（分别覆盖 insert/delete/同作者/异作者/段落标记修订）——拆到后续 PR
- 格式引擎（第二阶段）
- Python oracle 的行为对齐（新能力 oracle 里没有）

## 9. 复现清单

| 现象 | 路径 |
|---|---|
| 守门代码 | `src/mcp.rs:1235` `edits-not-implemented` |
| 丢新段落 | `crates/docx2typed-review/src/draft.rs` 记录循环的 `continue` |
| 合成路径缺失 | `crates/docx2typed-core/src/prose.rs:1239 apply_edits` → `resolve_leaf` |
| Python 对照 | `mcp_server.py:4330` `insert_paragraph` / `4411` `delete_paragraph` |
