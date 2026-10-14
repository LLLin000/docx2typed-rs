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
extract 后记录的 source/template fingerprint 与当前 DOCX 不一致 —— 即源文件在 workdir 之外被改过（别人编辑、Word 另存、投稿系统回传），workdir 落后于源。
_Avoid_: 文件过期, 版本冲突

**Baseline drift policy**:
一个 workdir 是这份文档**固定**的编辑住所，永不因漂移而新建。同步 = `extract <源.docx> -o <workdir>` 一次，把源的最新状态拉进同一个 workdir。

但 `extract` 是**基线构造器**，不是合并：它从源重新推导 `_template.docx` 与投影，因此对任何**已经越过基线**的 workdir 都是一次重置。**判断条件是「workdir 是否停在基线上」，不是「源有没有变」** —— 实测：一个已提交一版、且源一个字节都没变的 workdir，再 extract 一次同样会丢掉已提交正文。

两条守卫各指明不同的出路：
- `workdir-has-pending-edit` —— 有未提交草稿：先 `commit_sync`。
- `workdir-not-at-baseline` —— 有源不携带的已提交版本：先 `build_docx` 导出，或用 `--discard-committed-versions` 明示「以后者为准」。**"历史里还有它"不等于"它仍是当前版本"。**
一次同步只发布**一个**变更集：新基线与随它一起必须清除的东西同时落地。分两步（写基线，再清旧状态）在中断时正好留下所要消除的撕裂。清除的是**针对旧基线表达的一切**：编辑 sidecar、投影与其状态、版本指针、以及 store 车道（有 store 目录而没有它的指针，是 store 拒绝打开的 workdir）。

**保证的准确边界**：拒绝路径无副作用、成功路径一致（两者都有测试）；**发布中途的文件错误或崩溃不是原子的**（实测：变更集里靠前的资产已写入、靠后的没有，无回滚）。不要把它说成磁盘事务。

`.review/` 不清除也无需清除：会话以 `_template.docx` 的哈希为锚，基线被替换时 `ensure_session` 自行重建会话并追加 `session-rebound` 事件。
_Avoid_: 新建 workdir, fork 同步, 三方合并, 用源哈希判断是否安全, 分两步重置

**Sync**:
用源文件的当前状态刷新一个既有 workdir 的动作。它跟上"文档被外部改过"，不是"重开一个文档"。同步之后，workdir 与源再次一致，而它的历史与身份不变。只对**停在基线上**的 workdir 成立。
_Avoid_: re-extract 到新目录, reimport, re-fork
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
批注决策操作：`comment-delete <id>` 删除一个批注（comments.xml 条目 + 文档内 `commentRangeStart/End` 锚点 + `commentReference`，其余批注不动）；accept-all 同时清空全部批注。

**Table structure operation**:
表格结构操作：行/列增删、单元格横向合并（gridSpan）与拆分；新结构字节由模板合成（插入行/列保持格式但文字为空），单元格文字永不重写。经 `decide table-*` 进入新基线。

**Content control paragraph**:
`w:sdt` 内容中的段落，ID `S{index}.P{p}`；文字可编辑，`sdtPr`（alias/lock/tag）结构字节保真回放，控件结构本身不可编辑。

**Freestanding anchor**:
位于段落/单元格之间的 bookmark/comment 锚点（Word 常把 range end 放在 `</w:p>` 之后或 tc 之间）；按字节位置归属其前一段，保持锚点配对完整。

**Recursive prose surface**:
由文档正文、页眉、页脚、普通脚注和尾注 entry 出发，经 Known container 与 Prose bridge 递归到达的 Prose leaf 集合；comments 和 separator entry 不属于正文编辑面。
_Avoid_: 容器支持矩阵, 所有 w:p

**Known container**:
具有完整 typed path、leaf 枚举、字节补丁、结构锁定、修订/锚点语义、独立验证和对抗样本契约，可在递归 prose surface 中安全组合的容器类型；当前包括 table/row/cell、content control 和 text box。
_Avoid_: 已解析容器, 支持标签

**Prose bridge**:
穿过 otherwise opaque wrapper 到已知文字容器的窄、可验证通道；当前只允许从 drawing 到标准 text-box content，wrapper 的其余字节仍 opaque。
_Avoid_: drawing 支持

**Prose leaf**:
递归 prose surface 中可寻址的既有 Paragraph；即使为空或带复杂 pPr/sectPr 仍是 leaf，但段落属性和结构不属于文字编辑面。
_Avoid_: leaf paragraph, editable paragraph

**Container path**:
由 package part/entry、相对父容器的 typed steps 和 leaf ordinal 组成、并由 baseline skeleton/fingerprint 证明的 Prose leaf locator；Paragraph ID 只在一个 baseline 内稳定。
_Avoid_: XPath, 永久段落 ID

**Text island**:
Prose leaf 中不跨越 opaque、inline、anchor、range/hyperlink 或 revision ancestry/visibility 边界的最大连续可编辑文字区；style span 不形成 island 边界。
_Avoid_: style region, 文本片段

**Leaf fragment**:
对应一个可证明 Text island byte slice 的最小生成和补丁单位；同一 Prose leaf 的 shell、其他 islands、opaque 内容和外层容器保持 byte replay。
_Avoid_: touched paragraph

**Leaf migration map**:
结构操作生成新 baseline 时，对可证明延续的 Prose leaf 记录 old Paragraph ID/container path 到 new ID/path 的证据映射；未映射项不靠文字相似度猜测身份。
_Avoid_: 永久身份映射

**Leaf and island inventory**:
面向 Agent 的递归编辑面只读清单，包含 leaf ID、container path、part、文字摘要、islands、hard boundaries、修订/锚点上下文和锁定原因，不暴露原始 XML。
_Avoid_: XML inventory

**Review capability**:
一次 review server 进程拥有的 256-bit 随机 bearer secret；持有者获得该文档会话的全部浏览器读写能力，但不代表用户身份，进程停止即撤销。
_Avoid_: 登录令牌, 用户 token

**Review bootstrap**:
唯一无需 Review capability 的静态空壳页面；从 URL fragment 读取 capability、立即清除地址栏，并仅在页面内存中用于加载受保护的审阅内容。
_Avoid_: 登录页, review page

**Advertised origin**:
review server 启动时确定并打印的唯一浏览器 origin；受保护请求的 Host 和写请求 Origin 必须与其精确匹配。
_Avoid_: 允许网段, 代理 origin

**Workdir manifest**:
Rust workdir 的权威入口 `workdir.json`，记录 workdir 身份与 lineage、能力集，以及每项持久资产的 schema、角色、状态、哈希和 required features；引擎不靠目录猜测版本。
_Avoid_: 全局 schema 整数, 扩展 format.json

**Persistent asset**:
workdir 中承载 canonical state、baseline、审阅状态、决策、审计、provenance 或恢复证据，并须跨引擎迁移的版本化资产；生成视图和运行中 secret 不属于 Persistent asset。
_Avoid_: workdir 文件

**Opaque attachment**:
schema-1 迁移时由用户显式保留的未知文件；隔离、哈希绑定且只读封存，不参与引擎解析、DOCX 构建或能力协商。
_Avoid_: 未知资产

**Migration lineage**:
新 workdir 对只读 legacy source 的来源关系，绑定双端 manifest、逐资产转换、领域身份映射和验证证据；不表示可以反向降级。
_Avoid_: 原地升级记录

**Needs-recovery workdir**:
源恢复状态无法唯一 rollback 或 roll-forward 时生成的隔离迁移产物；保留原始恢复资产和诊断，但不是可构建或可发布的 Typed workdir。
_Avoid_: 迁移失败 workdir

**Result envelope**:
CLI `--json` 与 MCP structuredContent 共用的版本化操作结果，固定承载 outcome、typed data、Diagnostics、Run evidence 引用和 Engine descriptor；presentation 文本不属于该契约。
_Avoid_: 命令 JSON, MCP 返回值

**Diagnostic**:
以不可复用稳定 code 表达失败或警告类别、可重试性和 typed location/details 的机器可路由记录；message 仅供人读，不是自动化接口。
_Avoid_: 错误字符串

**Engine descriptor**:
实现对外声明的协议/schema 支持范围、capability manifest、command/tool surface 与构建身份快照；兼容性由契约交集决定，而不是由 Python 或 Rust 名称决定。
_Avoid_: 版本字符串

**Operation ID**:
调用方为 mutation 提供的幂等身份；相同 canonical input 可安全重取原结果，不同 input 复用同一 ID 必须拒绝。
_Avoid_: request ID, run ID

**Run evidence**:
mutation、build、verify 或 migrate 的 hash-bound 成败记录；语义 payload 可规范化差分，运行时间和主机等 provenance 不参与语义 hash。
_Avoid_: 命令日志

**Workdir generation**:
由 Workdir manifest 原子选择的不可变 Persistent asset 闭包；一次 mutation 发布完整新代，reader 固定单代读取，不能逐文件跨代观察。
_Avoid_: workdir 版本, 文件快照

**Draft ingress**:
绑定当前 Workdir generation 的 root-level 可编辑投影；sync 将其消费为新 canonical generation，本身不是 canonical state。
_Avoid_: 第二权威源

**Writer lane**:
由 OS advisory lock 串行化且在提交时执行 parent-generation CAS 的唯一 workdir 写通道；owner metadata 只用于诊断，不能用于偷锁。
_Avoid_: PID lockfile

**Transaction journal**:
按 Operation ID 保存、hash-chain 的不可变阶段记录，用于判定 prepared、external-published、generation-committed 和 completed 状态，而不是用 mtime 猜测恢复方向。
_Avoid_: 事务日志文本

**Recovery reserve**:
workdir 为磁盘耗尽预留的真实分配空间；只允许在 ENOSPC 恢复时释放，用于闭合 journal 和写最小失败证据，补足前禁止新 mutation。
_Avoid_: 空间预估

**Engine**:
以同步、无会话 `execute` 编排一个完整用例的深模块；统一执行契约协商、恢复、snapshot 固定、领域规划、独立验证、持久提交和 Result 生成，transport adapter 不得绕过顺序。
_Avoid_: backend, service

**Change set**:
Core 产出的声明式变更计划，绑定 expected generation/hash、Persistent asset 写集、外部产物、领域身份映射、required checks 和语义证据；不包含任意文件写回调。
_Avoid_: patch callback, 临时目录

**Verification request**:
Independent Verifier 的只读输入，只绑定 immutable byte sources、Workdir generation、capability 与 check profile；不能携带 Build plan 或 Core AST。
_Avoid_: build validation

**Check registry**:
由 Independent Verifier 拥有的稳定验证 check ID、profile、依赖和 verdict 规则集合；Engine 只能请求 profile，不能删除 required check。
_Avoid_: build 检查列表

**Scale profile**:
版本化支持规模的正交资源边界，按 package bytes、展开 XML、parts、Prose leaves、revisions、comments、tables/cells 与最大 leaf/island 分档；S/L 属产品契约，X 仅用于单维压力诊断。
_Avoid_: 文件大小档位

**Resource limit**:
Scale profile 的 fail-closed 安全边界；命中时必须在产生 partial workdir/output 前返回实际值、limit、profile 和稳定 Diagnostic，不能依赖 OOM 或超时兜底。
_Avoid_: 性能建议

**Qualification budget**:
绑定版本、fixture、runner 资源级别和完整 raw samples 的 wall-time、RSS、临时磁盘、输出大小、吞吐及 no-regression 发布门；硬件速度数值不进入公共 wire 协议。
_Avoid_: benchmark 最好成绩

**Release bundle**:
按 target 发布的自包含签名 archive，包含唯一 `docx2typed` binary、licenses、manifest、checksums 与供应链证据；运行不依赖 Python/Node，review 与协议资产嵌入 binary。
_Avoid_: wheel, cargo install

**Channel manifest**:
签名的 stable/prerelease 更新索引，按 target 绑定产品版本、协议/schema/capability 范围、最低 OS、artifact URL/size/hash/signature/provenance；stable 只能指向完成全资格的 Release bundle。
_Avoid_: latest 下载页

**Install receipt**:
安装器记录其创建的版本目录、current pointer 及经授权的 PATH/MCP 配置变更；update/uninstall 只能撤销仍与 receipt 一致的资产，不能删除 Workdir generation 或用户 evidence。
_Avoid_: 全局配置

**Qualification launcher**:
仅在 Python↔Rust differential 阶段由 Python package 定位 pinned Rust candidate 的临时黑盒启动器；不是 extension、生产 backend 或长期分发入口，clean cutover 后退役。
_Avoid_: Python bridge

**Interop cell**:
Consumer exact build × OS × evidence phase × corpus tier 的独立资格判定；open、render、save、reopen、semantic retention 与 repair observability 不得相互代替，阻断 cell 的 not-run/unknown 等同失败。
_Avoid_: Office 兼容

**Semantic roundtrip signature**:
Consumer 保存前后比较的版本化领域摘要；忽略 ZIP layout、timestamps、rsid 与 consumer-owned properties，严格保留可见/最终/原文视图、revision/comment graph、anchors、relationship targets 和命名 opaque-part policy。
_Avoid_: roundtrip hash

**Fixture manifest**:
以 SHA-256 寻址一份 Public、Private 或 Calibration fixture，记录 provenance/license/consent、生成器锁、dialect/features、expected signatures、适用 Interop cells、隐私/保留 owner 和 mutation lineage。
_Avoid_: corpus 文件清单

**Reference bundle**:
签名 Python oracle tag 绑定的不可变资格闭包，包含 Engine descriptor、协议/schema/能力/check/failure registries、fixture manifests、canonical inputs/results/evidence/signatures、raw provenance 和全部发布报告。
_Avoid_: CI artifact, golden files

**Semantic root**:
Reference bundle 中所有 schema-driven canonical oracle records、fixture identities 与 verdicts 的单一摘要；排除规则版本化，重建或允许的维护不得改变该 root。
_Avoid_: source commit hash

**Oracle major**:
Python reference 行为闭包的显式版本；任何 canonical output、verdict、stable failure 或 side-effect 变化都必须发布新 major，不能 patch 原冻结 bundle。
_Avoid_: Python patch release

## Agent-facing surface

这一节描述 agent 看到的那一层。上面的术语描述引擎存了什么；这一节描述调用者面对什么。
两层要分开命名：同一件事有两个名字时，agent 名字是**权威**，引擎名字是**实现细节**。

**Facade**:
`facade_locate` / `facade_edit` / `facade_resolve` / `facade_verify` —— 薄组合层，不新增编辑、历史或验证机制，只把引擎已有的证明翻译成 agent 词汇。
_Avoid_: editor API, wrapper, adapter, 工具收拢层

**Handle**:
一次定位结果的、绑定版本的引用。wire 上的名字是 `match_ref`，facade 发布为 `handle`；它钉住定位时的 revision 与所指向的原文，失配时拒绝而不是照旧套用。
_Avoid_: match_id, target id, anchor, span ref, 偏移量

**Capability verdict**:
对单个目标"这次编辑能否成功"的回答：`supported` / `unsupported` / `ambiguous` / `stale_revision`。agent 据此分派；自由文本诊断是给人看的。
_Avoid_: status, result code, 错误码

**Coverage**:
一次搜索或验证**实际扫过**的部件与范围。零结果必须连 coverage 一起返回，否则"不存在"会被读成"没找过"。
_Avoid_: scope（scope 是请求，coverage 是回答）

**Session**:
一条 MCP 连接已打开的那个 workdir。一条连接在其生命周期内只驱动一个 workdir；`workdir-already-open` 是生命周期规则，不是瞬时故障。
_Avoid_: connection, context, 会话状态

**Family** (`f_…`):
永久的逻辑文档，首次 extract 时铸造，存于 `<workdir>/workspace.json`。
_Avoid_: 项目, 文件族, 文档 ID

**Workspace** (`ws_…`):
一次提取产生的编辑状态；来自 fork 时携带 `origin_family` / `origin_version`。
_Avoid_: 工程, 项目, 目录

**Observation**:
某个 family 被看到过的一份 DOCX —— 副本、回传件、导出件都算。resolve 一个 observation 走证据分级（P1 哈希唯一 / P3 文件对象+哈希 / C1 文件对象在而内容已变 / C2 docId 仅作候选 / U 无证据），**C1、C2 与冲突档必须问用户一次**，不得自行认领。
_Avoid_: 输入文件, 源文件, instance
