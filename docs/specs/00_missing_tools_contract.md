# 13 个缺失 MCP 工具完整契约（实施规格书 v1）

> 规则：每个工具必须给出 **输入结构、输出结构、正例、拒绝例、误用例、错误码、副作用、幂等性、边界条件**。Rust实现必须逐项对号入座；任何偏差都是bug。

---

## 1. `document_read`

| 维度 | 内容 |
|---|---|
| **输入** | `{ anchor?: string, before?: int=5, after?: int=8, view?: "auto" \| "content" \| "outline" \| "spans" \| "issues" \| "capabilities" }` |
| **输出** | `{ view, revision, state, paragraphs, content, first_id, last_id }` — `content` 是完整投影文本，可直接作为 `document_patch` 的 diff base |
| **正例** | `anchor="P12", before=3, after=3, view="content"` → 返回 P9–P15 的完整投影文本 |
| **拒绝例** | `anchor="P999"` → `paragraph-not-found`；`view="spans"` 无 `anchor` → `document-read-invalid-view` |
| **误用例** | `anchor="P1", before=-1` → `invalid-arguments`；`view="invalid"` → `document-read-invalid-view` |
| **错误码** | `paragraph-not-found`、`document-read-invalid-view`、`invalid-arguments` |
| **副作用** | 无（只读） |
| **幂等性** | 是，相同参数多次调用结果完全相同 |
| **边界** | `before/after` 超出范围自动截断；`view="auto"` 大文档自动降级为 outline |

> **Rust实现要点**：必须复用 `document_projection.rs` 的投影逻辑；`view="spans"` 需要返回 `span_map` 供上层 `document_patch` 做边界检查。

---

## 2. `document_search`

| 维度 | 内容 |
|---|---|
| **输入** | `{ query: string, scope?: "body" \| "comments" \| "all" \| "header1"..., match_ref?: boolean, occurrences?: integer, expected_matches?: integer }` |
| **输出** | `{ occurrences: [{ match_ref, paragraph_id, context, index }], total }` |
| **正例** | `query="Cu2+", scope="body"` → 返回所有匹配段落的 `match_ref`、`paragraph_id`、上下文片段、在段落内的索引 |
| **拒绝例** | `expected_matches=3` 但实际只找到 2 个 → `text-ambiguous` 或 `expected-matches-mismatch` |
| **误用例** | `query=""` → `invalid-arguments`；`scope="invalid"` → `invalid-arguments` |
| **错误码** | `text-not-found`、`text-ambiguous`、`expected-matches-mismatch`、`invalid-arguments` |
| **副作用** | 无 |
| **幂等性** | 是 |
| **边界** | 大文档需分页；`occurrences` 指定第 N 个匹配 |

> **Rust实现要点**：必须复用 `govern.rs` 的搜索索引；`expected_matches` 是强约束，0 也是合法（表示"确定不存在"）。

---

## 3. `document_replace`

| 维度 | 内容 |
|---|---|
| **输入** | `{ find: string, replace: string, scope?: "body" \| "comments" \| "header1"..., regex?: boolean, case_sensitive?: boolean, expected_matches?: integer, base_revision?: string }` |
| **输出** | `{ changed: boolean, matches: integer, payload: { checks: [{name: "document-replace", status: "pass"}] } }` |
| **正例** | `find="Cu2+", replace="Cu²⁺", scope="body", expected_matches=12` → 全文 12 处替换，返回 `changed=true, matches=12` |
| **拒绝例** | `expected_matches=5` 实际匹配 3 个 → `expected-matches-mismatch`；`regex=true` 但 pattern 非法 → `invalid-arguments` |
| **误用例** | `find=""` → `invalid-arguments`；`replace=""` 且 `expected_matches>0` → `invalid-arguments` |
| **错误码** | `text-not-found`（scope 内 0 匹配且 `expected_matches` 未设或为 0 时不报错）、`text-ambiguous`、`expected-matches-mismatch`、`invalid-arguments` |
| **副作用** | **直接修改 canonical 文档树**，触发 `draft_dirty=true`，需 `commit_sync` 持久化 |
| **幂等性** | 相同参数重复调用 → 第二次 `changed=false, matches=0`（因为第一次已替换完） |
| **边界** | `scope="comments"` 仅修改批注文本；`regex=true` 支持捕获组 `$1` 回引用；跨修订边界拒绝 |

> **Rust实现要点**：这是**最复杂**的一个。必须复用 `_replace_in_body` 的逐段落替换逻辑，逐段验证 `old` 不跨越修订边界（`_span_crosses_boundary`）；`expected_matches` 是原子性门控，全部成功或全部回滚。

---

## 4. `document_patch`

| 维度 | 内容 |
|---|---|
| **输入** | `{ hunks: [{ paragraph_id, old, new }], base_revision?: string, operation_id?: string }` |
| **输出** | `{ changed: boolean, payload: { checks: [{name: "document-patch", status: "pass"}] } }` |
| **正例** | 单段落单 hunk：`{ "paragraph_id": "P12", "old": "Cu2+", "new": "Cu²⁺" }` → 成功 |
| **拒绝例** | `old` 跨修订边界 → `edit-span-crosses-revision-boundary`；`old` 在段落中不存在 → `text-not-found`；`old` 在段落中出现多次 → `text-ambiguous`；hunk 间重叠 → `patch-hunks-invalid` |
| **误用例** | `old == new` → `patch-noop`；`base_revision` 过期 → `stale-document-view`；hunk 顺序错误 → `patch-hunks-invalid` |
| **错误码** | `text-not-found`、`text-ambiguous`、`edit-span-crosses-revision-boundary`、`stale-document-view`、`patch-hunks-invalid`、`patch-noop`、`operation-id-reused` |
| **副作用** | 修改 canonical 树，`draft_dirty=true` |
| **幂等性** | 相同 `operation_id` 重复 → replay 原结果；不同 `operation_id` 同参数 → 第二次 `text-not-found`（已被修改） |
| **边界** | 单次调用支持多 hunk，**按顺序原子应用**，任一失败全盘回滚；跨修订边界、跨样式区间、跨受保护标记均拒绝 |

> **Rust实现要点**：**核心编辑原语**。必须复用 `govern.rs` 的 `_apply_batch_to_body` 与 `_span_crosses_boundary`；所有 hunk 先全量校验再落盘；`base_revision` 用 `document_state.revision_after` 链式传递。

---

## 5. `format_span`

| 维度 | 内容 |
|---|---|
| **输入** | `{ match_ref: string, attributes: { vertAlign?: "superscript" \| "subscript" \| "baseline", bold?: boolean, italic?: boolean, ... } }` |
| **输出** | `{ changed: boolean, payload: { checks: [{name: "format-span", status: "pass"}] } }` |
| **正例** | `match_ref="P12:r3", attributes: { vertAlign: "superscript" }` → 将第 3 个 run 设为上标 |
| **拒绝例** | `match_ref` 指向已删除文本 → `paragraph-not-found`；`attributes` 含不支持键 → `format-invalid-attribute` |
| **错误码** | `paragraph-not-found`、`format-invalid-attribute` |
| **副作用** | 修改 run properties，`draft_dirty=true` |
| **幂等性** | 相同属性重复设置 → `changed=false` |
| **边界** | 仅支持 `vertAlign`、`bold`、`italic`、`underline`、`strike`、`color`、`sz` 等 run-level 属性；段落级格式（对齐、缩进）不在本工具范围 |

> **Rust实现要点**：复用 `typed_core` 的 `_rpr_with_overrides` + `_RPR_ORDER`，**必须**按 schema 顺序插入/移除 `w:vertAlign` 等子元素，不能 `append`。`match_ref` 来自 `document_search`/`document_read`，不可手造。

---

## 6-11. `history_*` (list/diff/blame/restore/gc/verify)

| 维度 | 内容 |
|---|---|
| **输入/输出** | 详见 `scripts/mcp_server.py` 对应函数签名；返回结构见 `history_*` 返回值 |
| **核心语义** | - `list`：返回版本链（最新在前），含 `version`、`label`、`created_at`、`label`、`pin`、`dirty`<br>- `diff`：对比两版本（默认 vs HEAD），返回段落级增删改<br>- `blame`：给定段落 ID，返回最后修改该段的版本<br>- `restore`：从旧版本**向前**复制树产生新版本（绝不回滚 HEAD）<br>- `gc`：`list` 统计、`prune` 删除被裁剪版本的内容 blobs（元数据保留）<br>- `verify`：校验保留内容完整性，报告被裁剪版本 |
| **错误码** | `version-not-found`、`version-trimmed`、`partial-restore-needs-dependent-state`、`history-verify-failed` |
| **副作用** | `restore`/`gc prune` 写入新版本/删除 blobs；其余只读 |
| **幂等性** | 读操作是；`restore` 同版本重复 → 新版本但内容相同；`gc prune` 幂等 |
| **边界** | 选择性恢复仅限无依赖段落；GC 仅裁剪内容 blobs，版本元数据永不删 |

> **Rust实现要点**：Rust 已有 `review/history.rs` 的 `list/read/append`，**缺的是 MCP 暴露层** + `diff`/`blame`/`restore`/`gc`/`verify` 的完整实现。版本树在 `store/store.rs`。

---

## 12. `workspace_adopt`

| 维度 | 内容 |
|---|---|
| **输入** | `{ token: string, family_id: string, operation_id?: string }` |
| **输出** | `{ status: "resolved" \| "conflict", family_id, workspace_id, workspace_path }` |
| **前置** | 必须先 `workdir_open(<docx路径>)` 返回 `workspace-adoption-required` + `adoption_token` + `candidates[]` |
| **正例** | 用户确认候选 `family_id="f_xxx"` → 返回 `resolved`，后续 `workdir_open` 直接解析到该 family |
| **拒绝例** | `token` 过期/被篡改 → `adoption-token-invalid`/`adoption-token-stale`；`family_id` 不在候选列表 → `adoption-refused` |
| **错误码** | `adoption-token-invalid`、`adoption-token-stale`、`adoption-refused`、`workdir-not-open` |
| **副作用** | 写入 `workspace.json` 绑定 `family_id`；不修改文档内容 |
| **幂等性** | 同 token 同 family_id 重复 → 成功（幂等） |
| **边界** | 文件被外部修改导致 `adoption-token-stale` → 必须重新 `workdir_open` 获取新 token |

> **Rust实现要点**：**Rust 完全缺失 workspace identity 基础设施**（无 `family_id`/`workspace_id`/`workspace.json` 读写）。必须先补 `workspace_identity` 模块（参考 Python `scripts/workspace_identity.py`），再暴露 `workspace_adopt`/`fork`。

---

## 13. `workspace_fork`

| 维度 | 内容 |
|---|---|
| **输入** | `{ path?: string, operation_id?: string }` — 可选指定源 DOCX，默认当前 workdir 导出版本 |
| **输出** | `{ family_id, workspace_id, origin_family, origin_version, workspace_path }` |
| **语义** | 从现有 family **派生**新 family：新 family 继承源版本树，但获得新 `family_id`；记录 `origin_family`/`origin_version` 供审计 |
| **正例** | 从 V12 派生新专利 family → 新 family_id `f_xxx`，`origin_version="V12"` |
| **拒绝例** | 无可用 workdir → `workdir-not-open`；源文档损坏 → `source-invalid` |
| **错误码** | `workdir-not-open`、`source-invalid` |
| **副作用** | 创建新 workdir 目录，写入新 `workspace.json`（新 family_id），拷贝模板包 |

---

## 统一验收标准（每个工具必须通过）

| 维度 | 要求 |
|---|---|
| **正例通过** | 给定规范输入，返回预期结构，字段名/类型/枚举值完全一致 |
| **拒绝例正确** | 给定规范拒绝输入，返回规定错误码，`isError=true`，诊断信息可定位 |
| **误用例拦截** | 参数类型错误、必填缺失、枚举越界 → `invalid-arguments`，不抛 panic |
| **幂等性** | 幂等操作重复调用结果一致；非幂等操作第二次按语义处理（如已替换完再替换 → `matches=0`） |
| **边界覆盖** | 空输入、最大长度、Unicode、跨修订边界、跨样式区间、空文档、单段落文档、大文档 |
| **证据完整** | 产生 `.evidence.json`，含 `operation_id`、`inputs`、`outputs`、`checks`、`verdict`、`payload_sha256`、`provenance` |
| **幂等/重放** | 相同 `operation_id` 重试 → replay 原结果；不同 `operation_id` 同参数 → 按语义处理 |

---

## 实施顺序与依赖（强制顺序）

| 阶段 | 任务 | 依赖 |
|---|---|---|
| **A1** | 补全 `workspace_identity` 模块（`family_id`/`workspace_id`/`workspace.json` 读写） | — |
| **A2** | 补全 `history_*` MCP 暴露 + `history_diff/blame/restore/gc/verify` 实现 | `history.rs` 已有底层 |
| **A3** | `document_read` / `document_search`（读路径） | `document_projection.rs`、`govern.rs` 已有底层 |
| **A4** | `format_span`（字符级格式） | `_RPR_ORDER`、`insert_rpr_child` 已就绪 |
| **A5** | `document_search` + `document_read` 完善（`match_ref` 生成） | 依赖上一步 |
| **A6** | `document_patch`（核心写入原语）| 依赖 A3/A4 的 `match_ref` 与 `base_revision` 机制 |
| **A7** | `document_replace`（全局替换） | 复用 A4 的逐段落替换逻辑 |
| **A8** | `document_replace` 的批量语义 | 复用 A7 |
| **A9** | `workspace_adopt` / `workspace_fork` | 依赖 `workspace_identity` 模块 |
| **A10** | `history_diff/blame/restore/gc/verify` | 依赖 `history.rs` + `store.rs` |
| **A11** | `format_span` 完善（跨区间、受保护标记拒绝） | 依赖 A4 |

---

## 统一验收标准（每个工具必须通过）

| 维度 | 要求 |
|---|---|
| **正例通过** | 给定规范输入，返回预期结构，字段名/类型/枚举值完全一致 |
| **拒绝例正确** | 给定规范拒绝输入，返回规定错误码，`isError=true`，诊断信息可定位 |
| **误用例拦截** | 参数类型错误、必填缺失、枚举越界 → `invalid-arguments`，不抛 panic |
| **幂等性** | 幂等操作重复调用结果一致；非幂等操作第二次按语义处理（如已替换完再替换 → `matches=0`） |
| **边界覆盖** | 空输入、最大长度、Unicode、跨修订边界、跨样式区间、空文档、单段落文档、大文档 |
| **证据完整** | 产生 `.evidence.json`，含 `operation_id`、`inputs`、`outputs`、`checks`、`verdict`、`payload_sha256`、`provenance` |
| **幂等/重放** | 相同 `operation_id` 重试 → replay 原结果；不同 `operation_id` 同参数 → 按语义处理 |

---

## Rust 端实现的"禁止事项"（红线）

| 禁止项 | 理由 |
|---|---|
| ❌ 把 Python 的 bug 当 feature 复制 | Python 已修复的 bug（如 rPr 顺序）必须修正 |
| ❌ 用字符串拼接生成 XML | 必须用 `xmltree`/`xmltree::Element` 保证 well-formed |
| ❌ 直接 `append` rPr 子元素 | 必须用 `_RPR_ORDER` 按 schema 顺序插入 |
| ❌ 硬编码错误消息 | 必须用结构化 `diagnostic`（`code`/`severity`/`category`/`retriable`） |
| ❌ 静默吞异常 | 所有失败必须返回结构化 `ToolError`，`diagnostic.code` 稳定 |
| ❌ 硬编码文件路径 | 必须通过 `workdir` 解析，支持工作区迁移 |
| ❌ 绕过 `package_guard` / `verify_workdir` | build 路径必须保持内部验证 |

---

## 交付物清单（每个 PR 必须产出）

| PR | 代码变更 | 测试新增 | 文档/证据 |
|---|---|---|---|
| PR 2 | clippy 修复、migrate/differential 归一化 | 回归测试锁住旧行为 | 失败日志、修复说明 |
| PR 3 | insert/delete paragraph + build+verify 闭环 | E2E 测试：新建→改→删→build→verify | 生成 DOCX SHA、verify 证据 |
| PR 4 | `document_read/search/patch/replace` + `format_span` | 矩阵测试覆盖正/拒/误用/幂等/边界 | 输出 diff、证据、错误码对照表 |
| PR 5 | `history_*` + `workspace_adopt/fork` + identity | 版本树恢复、fork 后双 family 共存 | workspace.json schema、采纳流程图 |
| PR 6 | 证据格式、gate、e2e 验收 | 8 场景全覆盖 | 证据样本、gate 拒绝日志 |

---

> 以上即为全部契约。Rust 侧每写一个工具，就拿对应章节的表格逐项打勾。所有规格已落盘到 `docs/specs/00_missing_tools_contract.md`。