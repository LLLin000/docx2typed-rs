# docx2typed

DOCX ⇄ 可编辑 Markdown 的双向转换工具，格式锁定：只改文本，重建后格式保持一致。当前语境是 typed-mode (v2) 的设计。

## Language

**Paragraph**:
一个 `w:p`，文档中最小可寻址单位。v1 中以 `<!-- Pn -->` 标记。
_Avoid_: 段, block

**Run**:
一个 `w:r`：带格式的文本切片。v1 中每 run 一行 `[n] 文本`。
_Avoid_: 文本行

**Elem**:
段落内 run 之外的所有 XML 片段（批注锚点、书签、域、绘图、修订包装），锁定不可编辑。
_Avoid_: 元素（含义过宽）

**Template**:
extract 时从原 docx 复制的副本，提供 styles.xml / sectPr / 关系；build 以其为底，仅替换 body 段落。

**Strict-run-mode (legacy)**:
旧 v1 的 `[n]` 逐 run 格式；不属于 typed-mode 实验的兼容范围。只有原始 DOCX 可用时，旧工作目录才能通过重新 extract 迁移。
_Avoid_: 旧模式, run mode

**Typed mode**:
本项目的新唯一格式：自然段落 + 样式 span + 受限结构 token；extract、build、verify 都只处理这一种格式。

**Hybrid fidelity**:
typed mode 的保真契约：未编辑段落走 byte replay，编辑过的段落走 span 合成，verify 分别用字节对比和规范形对比。已定（2026-08-04）。

**Touched paragraph**:
md 内容与 extract 时快照不一致、需要 span 合成重建的段落。
_Avoid_: 改过的段
**Typed workdir**:
一次 extract 产生的完整编辑项目，包含 typed.md、format.json、styles.json 和 `_template.docx`；build/validate/verify 以目录为输入单位。

**Untouched paragraph**:
md 内容与快照一致、走 v1 字节回放的段落。
_Avoid_: 没动的段

**Canonical form**:
段落的规范形 = 合并相邻同样式 run 后的 `(style-id, text)` 序列。touched 段落的 verify 基准。已定（2026-08-04）。

**Canonical rPr**:
用于合并判断的 rPr 等价形式：去 rsid、`<w:b/>`≡`w:val="true"`≡`"1"`（false/0/off 删元素）、属性排序、子元素排序。`w:rFonts` 的 asciiTheme 与显式 ascii 不等价，不作跨形式合并。

**Merge predicate**:
两个相邻 run 可合并 ⟺ 规范形 rPr 相等且 mid 为空（有 tab/br/绘图等中间元素绝不合并）。已定（2026-08-04）。
**Paragraph ID**:
段落在 typed.md 与 format.json 之间的稳定查找键。ID 必须唯一但不要求连续；md 中的块顺序决定输出顺序。
_Avoid_: 行号, 位置索引

**Orphan record**:
format.json 中存在、但当前 typed.md 没有对应段落块的 XML 记录，通常由删段产生；build 报警告并不输出它。
_Avoid_: 孤儿段

**Base style**:
段落中按可见文字字符数加权、取占比最高的 style；并列取最先出现者。无 span 的新文字使用它；空段落继承前一段，首个空段落使用模板 Normal。
_Avoid_: default style, paragraph default
**Typed node allowlist**:
touched 段落的合成路径只接受已定义的文字、样式 span、结构 token 和有限容器；未知节点必须拒绝 build，而不是静默保留。

**Unsupported node**:
出现在 touched 段落中的白名单外 DOCX 结构（例如域、修订、绘图或未知容器）；unchanged 段落可以原样回放。

**Typed markup**:
Markdown 风格的 AI 编辑中间格式；正文按字面处理，只允许白名单的段落标记、样式 span、结构 token 和范围容器。
_Avoid_: CommonMark, HTML document

**Restricted grammar**:
typed.md 的专用窄语法。必须由 docx2typed 自己解析和验证，不交给通用 HTML parser 或 Markdown AST 重写。

**Style registry**:
跨段落的字符格式注册表；每个 style ID 映射一份完整 canonical rPr XML，`label` 仅供阅读。

**Content-addressed style ID**:
由 canonical rPr 的 SHA-256 截断值生成的稳定 ID，例如 `s_31a8f2c004be91d7`；不依赖段落顺序。

**Text-only typed mode**:
v2.0 只允许修改文字内容；styles.json、`data-s` 样式结构和结构 token 不可编辑。新增或改变格式留待后续显式功能。
**Template package integrity**:
除明确允许修改的 `word/document.xml` 段落区域外，DOCX ZIP 部件内容哈希必须保持不变；模板指纹不匹配时拒绝 build。

**Layout reflow**:
文字长度变化引起的自动换行、分页和页数变化；不属于模板格式损坏，v2.0 不承诺阻止它。
**Structure skeleton**:
段落中独立于文字内容的 style ID、结构 token、范围容器及其属性和顺序；已有段落的骨架不可编辑。

**Text-driven normalization**:
文字删除可导致空 span 删除，相邻等价 style 可合并；这些是文字编辑的规范化结果，不算格式编辑。
**File-first editing**:
typed.md 是 v2.0 唯一的修改入口；命令只负责解析、验证、构建和投影，不提供第二套修改语义。

**Validator gate**:
build 写出 DOCX 前必须通过的 typed 结构、骨架、style 引用和模板完整性检查。
**Projection view**:
由同一 restricted parser 生成的只读文本视图；不产生新的编辑语义，也不回写 typed.md。

**Clean view**:
隐藏 typed 标签、只显示连续正文的投影视图。

**Style view**:
显示 style 边界和 label 的诊断投影视图，不模拟 Word 的真实排版。
**In-memory normalization**:
build 前对 AST 做空 span 删除、相邻等价 style 合并和相同 style 嵌套扁平化；不修改 typed.md。

**Validation error**:
必须阻止 build 的 typed 语法、结构骨架、模板指纹或 DOCX 包完整性违规。

**Validation warning**:
不影响 build 的可解释状态，例如 orphan record、未引用 style 或非连续段落 ID。
**Structural token**:
typed.md 中代表非普通文字的受限节点；分为零宽 anchor、原子 inline 和有内容的 range。

**Anchor**:
不占文字宽度的结构边界，例如 comment/bookmark 的起止点；必须位于文字边界。

**Range container**:
包住可见内容的受限结构容器，例如已有 hyperlink；允许合法嵌套但不允许交叉。
**Editable surface**:
当前编辑面包含 `w:body` 直系 `w:p`、表格单元格段落（`T0.R0.C0.P0`）、文本框段落（`B0.P0`）、内容控件段落（`S0.P0`）以及 header/footer/footnote/endnote 部件段落（`header1.P0` 等，见 ADR 0038）。嵌套容器内的文字可编辑；容器结构（表格结构、控件结构、容器本身）不可在 typed.md 中编辑。

**Opaque container**:
editable surface 之外的 DOCX 容器（field/math/drawing 内部、sdtPr、tblPr 等）；作为模板内容原样保留，文字编辑面不可触碰；其内部修订只通过字节级落定处理，interior bytes 原样复制。
**Paragraph inheritance**:
新增段落通过 `inherit="P11"` 显式复制已有段落的 pPr、段落属性和 base；不允许隐式继承或在 typed.md 写 pPr XML。
**Deletion tombstone**:
typed.md 中明确表示段落删除意图的 `<!--@delete id="P5"-->` 标记；没有段落块或 tombstone 的原始 ID 是 validation error。
**Conservative rPr canonicalization**:
只消除确定的 XML 词法差异；canonical 结果用于 hash/合并/比较，原始 rPr 仍用于生成 XML。不解析完整 Word 样式继承，不确定等价时保持不同。
**Text escaping**:
typed.md 普通文字中的 `&`、`<`、`>` 使用 XML entity；parser 只还原文字节点，不把未知 entity 或未知标签静默当正文。Markdown 标点按字面处理，Unicode 空白保留。
**Logical paragraph line**:
一个 typed paragraph block 的正文使用一条逻辑行；文件换行不代表 Word 换行，真实换行必须使用 `docx-inline kind="br"`。
**Whitespace preservation**:
typed AST 不 trim 正文；生成 `w:t` 时根据文字首尾普通空格自动设置 `xml:space="preserve"`，tab/br 不混入普通文字。
**Cross-boundary rewrite**:
跨 style span、range 或 anchor 的可见文本重写；v2.0 不提供自动重分配样式的算法，validator 拒绝此类骨架变化。
**Immutable hyperlink range**:
已有 hyperlink range 的可见文字可编辑，但 `rid`、内部 anchor、目标和关系属性不可变；不能新建或改写关系。
**Immutable anchor**:
已有 comment/bookmark anchor 的 ID、名称、配对关系和顺序不可变；锚点范围内的普通文字可编辑，锚点本身不可新增、删除或移动。
**Opaque token**:
typed.md 中对 unsupported DOCX 节点的只读占位符；sidecar 保存原始 XML，clean view 显示诊断占位符，包含它的段落不能进入 touched 合成路径。
**Template-derived baseline**:
build 以 fingerprint 匹配的 `_template.docx` 重新生成原始 typed model，作为 touched 判定和结构骨架比较基线；不在 format.json 重复存正文快照。

**Schema version gate**:
typed model、canonicalizer 和 sidecar schema 版本不兼容时拒绝 build，不用新规则猜旧数据。
**Byte-preserving document patch**:
从模板原始 `word/document.xml` bytes 中只替换 body 直系段落的明确 slice；untouched 段落和其他 XML bytes 原样复制，touched 段落才由 typed AST 生成。
**Independent typed verify**:
verify 从 workdir 和 output DOCX 独立重建 baseline/typed AST，校验文字、骨架、模板部件和 opaque container；不信任 build 的中间结果。
**Transactional build**:
build 只写临时 DOCX；typed validator、XML patch、package manifest 和 independent verify 全部通过后，才原子替换最终 output。

**Baseline drift**:
extract 后记录的 source/template fingerprint 与当前 DOCX 不一致；当前 workdir 不能继续 build，必须重新建立基线或显式合并。
**Baseline drift policy**:
source/template fingerprint 变化时拒绝当前 workdir；不自动覆盖或三方合并未完成的 typed 修改，必须从最新 DOCX 建立新 workdir。
**Fixture corpus**:
用于端到端验证的真实 DOCX 样本集合，覆盖文字、样式、空白、结构 token、关系、opaque 节点、表格、section 和失败路径；主证据是 package/XML 不变量。
**Textual superscript/subscript**:
正文中的 Unicode 上下标 code point（例如 `₂`、`⁺`）；属于文字内容，原样保存，不生成或引用 vertAlign style。

**Styled superscript/subscript**:
普通文字配合 style rPr 的 `w:vertAlign`（`superscript` / `subscript`）；属于格式骨架，必须通过 style span 保留。

**Representation orthogonality**:
Unicode 上下标与 Word vertAlign 是两种独立表示；typed mode 不在二者之间自动转换、折叠或判等。
**Normalization profile**:
显式的 vertical-align 预处理策略；默认 `preserve`，可选 `all` 或 selective。转换生成新的 DOCX baseline，随后必须重新 extract。

**Normalized baseline**:
经过显式 normalization profile 处理后、作为新 typed workdir 原始模板的 DOCX；不与未转换的 source baseline 混用。
**Unicode vertical catalog**:
按固定 Unicode 数据版本生成的上下标映射表；覆盖数字、运算符、正负号、等号、括号、字母、modifier/ordinal 等类别，并为非分解字符保留 manual/ambiguous 分类。

**Cataloged conversion**:
`all` 只转换 catalog 中有明确 mapping 的 code point；catalog 外的视觉相似字符不猜测，按 `preserve` 或 `error` 策略处理。
**Vertical candidate**:
agent 可见的上下标候选记录，包含 paragraph ID、occurrence ID、code point、Unicode name、类别、建议 base/vertAlign 映射和正文上下文；候选本身不改变 DOCX。

**Normalization decision policy**:
agent 对候选做出的显式 convert/preserve 决策；写入 policy 和 audit log，不能由 build 隐式推断。
**Occurrence-level decision**:
针对单个 paragraph 中单次 Unicode vertical candidate 的 convert/preserve 决策；同一 code point 在不同上下文可有不同结果。

**Normalization audit**:
记录 source/template fingerprint、Unicode catalog 版本、policy hash、每个 occurrence 的旧字符/新字符/style 和最终决定。
**Complete policy gate**:
normalization 只有在所有要求审阅的 occurrence 都有最终 convert/preserve 决定后才可提交；未决项阻止生成 normalized baseline。

**Normalized workdir**:
由 normalization 生成的新编辑项目；拥有新的 template fingerprint、typed model、style registry 和完整 audit，不与源 workdir 原地混用。
**Style delta composition**:
vertical normalization 在原有 rPr 上叠加或替换 `w:vertAlign`，保留 bold、font、color 等其他属性；冲突的 vertAlign/position 进入 manual/error。
**AST-based normalization**:
vertical candidate 扫描和转换都基于 typed AST；保留 paragraph/style/range 上下文，再由 byte patch 生成 normalized DOCX。
**Transformation recipe**:
vertical catalog 中描述 source 文本到 target 文本 + vertical style delta 的完整规则；允许多字符结果，并标记 approved/ambiguous/manual/reversible。
**Pinned Unicode catalog**:
由固定 Unicode 数据版本生成并提交的 vertical catalog；runtime 只读取 catalog，不使用本机 `unicodedata` 重新推导候选。
**Catalog coverage gate**:
固定 Unicode 候选全集中的每个字符都必须归类为 approved、ambiguous、manual 或 unsupported；未分类候选使 catalog 生成失败。
**Typed AST**:
typed.md 的受限语法解析结果；Paragraph 由带有效 style ID 的 Text 节点和 anchor/inline/range/opaque 结构节点组成。

**Text node**:
带有效 style ID 和原始文字的 AST 节点；style 等于 paragraph base 时序列化为普通文字，否则投影为 span。

**Span projection**:
`<span data-s="...">` 只是 typed.md 的序列化形式，不是独立的领域节点；parser/build/normalize 以 Text(style_id) 为准。

**Tracked revision**:
Word 修订模式产生的文档变更记录，OOXML 中为 `w:ins`/`w:del`/`w:rPrChange`/`w:pPrChange` 等容器，携带唯一 `w:id`、author、date。
_Avoid_: 修订痕迹, 批改记录

**RevisionNode**:
typed AST 中的一等递归容器节点，`kind` 为 insert/delete（预留 move_from/move_to），携带修订身份（ooxml_id、author、date、date_utc）、属性和子节点；渲染时按上下文决定 `w:t` 还是 `w:delText`。
_Avoid_: 修订标记节点（含义过窄）

**Insertion revision**:
`w:ins` 容器：修订中插入的新文本，最终视图下可见、可编辑。

**Deletion revision**:
`w:del` 容器：修订中删除的旧文本，文本存于 `w:delText`；最终视图下隐藏，v1 锁定不可编辑，只能通过接受/拒绝操作。

**Final view**:
编辑面视图（= Word No Markup）：insert 生效、delete 隐藏；edit.md 采用此视图，删除位置以 revision gap 保留。
_Avoid_: 显示修订视图

**Revision gap**:
edit.md 中代表隐藏删除位置的零宽不可编辑占位符 `⟦revision-gap id="R…" kind="delete"⟧`；保证删除前后插入位置确定，sync 禁止编辑跨越。
_Avoid_: 删除占位（含义过宽）

**Track mode**:
修订式编辑模式：插入/删除/替换分别生成新的 insert/delete 修订（replace = delete + insert）；mutation 必须保留并理解 revision ancestry。
_Avoid_: 修订模式（与 Word 术语混淆时用全称）

**Direct mode**:
非修订式编辑模式：修改直接生效，不生成修订；direct 模式下修改修订内文字被拒绝（`revision-text-mutated-in-direct-mode`）。

**Ambiguous review state**:
存在待审修订但 `settings.xml` 无 `w:trackChanges`（或相反）的状态；extract 仍成功，但生成修订的调用被拒绝，必须显式选择 track/direct。
_Avoid_: 修订冲突

**Revision ancestry**:
修订节点的嵌套关系（如插入修订内的删除修订）；接受/拒绝外层修订时决定内层修订的去留，必须随节点记录父级关系。

**Revision key**:
工具层的稳定修订身份：part + 容器路径 + kind + `w:id` + 内容指纹；`w:id` 仅是本地方言字段，不作为全局主键。

**Revision inventory**:
`revisions.json`/`revisions.md`：全包只读修订清单（类型/作者/日期/文本/位置/可编辑性），含 nested-container 中不可编辑修订的标记与原因。
_Avoid_: 修订列表（与视图区分）

**Byte-level settlement**:
accept-all/reject-all 对全文档 XML 的字节级落定：insert/move_to 解包、delete/move_from 移除（reject 反向并把 `delText` 换回 `t`）；段落标记修订双向移除；opaque interior bytes 原样复制；comment/bookmark 锚点在移除区间外重锚定。落定后 re-extract 出干净基线。

**New baseline workdir**:
settle/table 操作产出的新 DOCX + 重新 extract 的干净基线 workdir；源 workdir 永不修改，原工作目录可继续独立使用。

**Comment decision**:
批注决策操作：`comment-delete <id>` 删除一个批注（comments.xml 条目 + 文档内 `commentRangeStart/End` 锚点 + `commentReference`，其余批注不动）；accept-all/reject-all 的修订落定**保留全部批注**，清空批注只能逐条 `comment-delete`（2026-09-06 定）。

**Table structure operation**:
表格结构操作：行/列增删、单元格横向合并（gridSpan）与拆分；新结构字节由模板合成（插入行/列保持格式但文字为空），单元格文字永不重写。经 `decide table-*` 进入新基线。

**Content control paragraph**:
`w:sdt` 内容中的段落，ID `S{index}.P{p}`；文字可编辑，`sdtPr`（alias/lock/tag）结构字节保真回放，控件结构本身不可编辑。

**Freestanding anchor**:
位于段落/单元格之间的 bookmark/comment 锚点（Word 常把 range end 放在 `</w:p>` 之后或 tc 之间）；按字节位置归属其前一段，保持锚点配对完整。

**Store**:
workdir 私有的、承载不可变 generation 与 current 指针的账本；事务与 CAS 的实现处，不是用户语言。
_Avoid_: 版本库, 仓库

**Generation**:
一次 mutation 产生的不可变整份编辑状态快照，带父代与逐文件指纹；事务与 GC 的单位。
_Avoid_: 版本（用户版本是 Version）, snapshot

**Snapshot**:
每个保存边界发布的 canonical round 记录，含内容哈希、父快照与可渲染视图；协作、评审与 preflight 的基准。
_Avoid_: 版本, commit

**Save boundary**:
草稿成为 canonical 状态的时刻（`commit_sync`，以及直接改写 canonical 状态的格式/决策/表格操作）；只有它产生 Version。
_Avoid_: 提交点, checkpoint

**Version**:
用户可见的可回退保存点，指向一个 Snapshot 并锚定其内容；id 创建时确定且永不复用。
_Avoid_: 修订（该词已属于 revisions.json 的 Word 修订）, commit, checkpoint, 版本号 C{n}

**Restore**:
把某个历史 Version 的内容复制为一个新 Version 的操作；current 只向前，历史永不回拨。
_Avoid_: 回滚, revert, reset, 检出

**Export**:
把当前状态物化成可交付 DOCX；不产生 Version，也不改变 canonical 状态。
_Avoid_: 保存, 提交, save

**Workspace**:
一个逻辑文档长期复用的 workdir；用户面对的是"这个文档 + 它的版本时间线"，不是一堆并列的 wd/final(2)。
_Avoid_: 临时目录, 工作副本

**Tree**:
一个 Version 的完整状态，用内容寻址的语义对象（段落、格式记录、token 表、样式、模板）组成的 Merkle 根；两个 Version 未改动的部分共享同一批对象。
_Avoid_: 目录快照, 文件树

**Version dirty**:
canonical 状态已偏离 HEAD 所指的 Version（格式/决策类操作直接改 canonical 而不立即成版本）；与 draft dirty（投影偏离 canonical）互相独立。
_Avoid_: 未保存, 脏草稿

**Export receipt**:
一次导出的凭据（产物路径、sha256、所依据的 Version）；作为操作证据累积，不写回不可变的 Version 记录。
_Avoid_: 导出记录（写在版本里）

**Cherry-pick**:
把指定段落回到某个历史 Version 内容的语义操作；因段落会引用全局 token/锚点，只在那段落可自证无害时成立，否则 fail-closed。
_Avoid_: 局部恢复, 段落回退

**Baseline transition**:
同一文档进入新基线纪元的 Version（模板/源指纹切换、重新 extract）；用户仍只看到"下一个版本"。
_Avoid_: 新 workdir, 重抽取
