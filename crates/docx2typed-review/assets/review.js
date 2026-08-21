/* DOCX2TYPED review console — static vanilla frontend for the
   `docx2typed-review-frame-1` contract.

   Layering (per repo constraints):
   - ReviewClient : hydration via GET /api/review-frame only; every
     frame-dependent POST carries expected_generation (+
     expected_generation_manifest_sha256); Idempotency-Key is a retry
     identity generated once per logical mutation and reused on transient
     network retries.
   - ReviewStore  : current frame, generation, history target; partial
     results merge only when their generation equals the current frame,
     otherwise they are discarded and a full frame reload is triggered;
     a stale-review-frame mutation is never auto-retried — the frame
     refreshes, paragraph/review targets are kept, and the user must
     re-confirm before resubmitting.
   - DocumentRenderer: ordered blocks + registries + normalized regions
     rendered with DOM APIs and textContent only. No innerHTML /
     outerHTML / insertAdjacentHTML / document.write / DOMParser /
     createContextualFragment / eval / new Function, and no filesystem,
     XML, or DOCX reads.
   - DOM selections round-trip through region data-pid/data-start/
     data-end as Unicode scalar offsets (position_contract
     unicode-scalar-1). selection.toString() and full-text search are
     never used.

   Frame contract (docx2typed-review-frame-1): identity.generation is the
   canonical identity; current_snapshot / review_base / staged_snapshot
   are coordination coordinates; document.blocks only orders rendering,
   document.paragraphs / tables / styles are registries; paragraph regions
   are the only renderable body source. The console is a static file
   served from the review server origin; the capability token rides only
   in the URL fragment and is stripped from the address bar immediately. */
(function () {
'use strict';

/* ---------------------------------------------------------------- constants */

var FRAME_SCHEMA = 'docx2typed-review-frame-1';
var POLL_MS = 2200;
var MAX_RETRY = 2;
var MAX_TEXT_CLIP = 44;
var WORKFLOW_STEPS = ['load', 'agent', 'review', 'handoff', 'deliver'];

var ERROR_COPY = {
  'stale-review-frame': ['文档已在后台更新', '已刷新并保留你的选择，请确认后重新提交'],
  'generation-conflict': ['文档已在后台更新', '已刷新并保留你的选择，请确认后重新提交'],
  'current-snapshot-drift': ['当前文档已经变化', '已刷新，请确认后重新操作'],
  'staged-parent-mismatch': ['版本已经前进', '已刷新当前版本，请确认后重新发送'],
  'patch-parent-mismatch': ['版本已经前进', '已刷新当前版本，请确认后重新发送'],
  'patch-context-mismatch': ['正文已经变化', '请重新选择当前版本中的文字'],
  'patch-precondition': ['选中文本已经变化', '请重新选择当前版本中的文字'],
  'patch-fingerprint-mismatch': ['段落已经变化', '请重新读取当前版本后再操作'],
  'patch-style-mismatch': ['格式区域已经变化', '请重新读取当前版本后再选择'],
  'patch-overlap': ['本轮草稿选区重叠', '请重新选择不重叠的正文'],
  'queued-human-patch': ['还有人工调整待处理', '请先处理本轮人工草稿'],
  'writer-busy': ['另一个写入正在进行', '请稍后重试，不要重复提交'],
  'writer-timeout': ['写入超时', '请稍后重试'],
  'needs-recovery': ['存储需要恢复', '请检查 review server 日志'],
  'reserve-depleted': ['存储空间不足', '请释放空间后重试'],
  'store-invalid': ['存储状态无效', '请检查 review server'],
  'operation-id-reused': ['请求标识冲突', '请勿重复提交相同请求'],
  'session-invalid': ['会话已失效', '请重新打开启动链接'],
  'history-readonly': ['历史版本为只读', '切回当前版本后才能操作'],
  'frame-unavailable': ['审阅帧尚未就绪', '请等待载入完成'],
  'backed-not-atomic': ['该工作目录不是原子审阅工作区', '无法在线写入，请改用支持原子存储的工作目录'],
};

var KIND_LABELS = {
  delete: '删除',
  insert: '插入',
  move_from: '移出',
  move_to: '移入',
};

var HIGHLIGHT_HEX = {
  black: '#000000', blue: '#0000FF', cyan: '#00FFFF', green: '#00FF00',
  magenta: '#FF00FF', red: '#FF0000', yellow: '#FFFF00', white: '#FFFFFF',
  darkBlue: '#00008B', darkCyan: '#00808B', darkGreen: '#006400',
  darkMagenta: '#8B008B', darkRed: '#8B0000', darkYellow: '#808000',
  darkGray: '#A9A9A9', lightGray: '#D3D3D3',
};

var UNDERLINE_STYLES = {
  single: 'solid', words: 'solid', double: 'double', dotted: 'dotted',
  dottedHeavy: 'dotted', dash: 'dashed', dashed: 'dashed',
  dashLong: 'dashed', dashLongHeavy: 'dashed', dashDotHeavy: 'dashed',
  dashDotDotHeavy: 'dashed', wave: 'wavy', wavyHeavy: 'wavy', wavyDouble: 'wavy',
};

var ALLOWED_CSS_KEYS = [
  'font-family', 'font-size', 'font-weight', 'font-style', 'color',
  'background-color', 'text-decoration', 'text-decoration-style',
  'vertical-align', 'text-transform', 'font-variant-caps', 'letter-spacing',
  'direction', 'unicode-bidi', 'text-emphasis', 'text-shadow', 'opacity',
  '-webkit-text-stroke', 'font-stretch',
];

/* ------------------------------------------------------------ capability */

var sessionToken = (function () {
  var match = location.hash.match(/[#&]token=([A-Za-z0-9_-]+)/);
  var token = match ? decodeURIComponent(match[1]) : '';
  if (token && history.replaceState) {
    history.replaceState(null, '', location.pathname + location.search);
  }
  return token;
})();

/* ------------------------------------------------------------- utilities */

function scalarLength(text) { return Array.from(text).length; }

function scalarSlice(text, start, end) {
  return Array.from(text).slice(start, end).join('');
}

function toNumber(value, fallback) {
  var parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

function clampInt(value, min, max) {
  return Math.max(min, Math.min(max, Math.round(value)));
}

function pad2(value) { return String(value).padStart(2, '0'); }

function setText(node, text) {
  if (node) node.textContent = text == null ? '' : String(text);
}

function clearChildren(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

function el(tag, className, attrs) {
  var node = document.createElement(tag);
  if (className) node.className = className;
  if (attrs) {
    for (var key in attrs) {
      if (!Object.prototype.hasOwnProperty.call(attrs, key)) continue;
      var value = attrs[key];
      if (key === 'text') node.textContent = value;
      else if (key === 'dataset') Object.assign(node.dataset, value);
      else node.setAttribute(key, String(value));
    }
  }
  return node;
}

function delay(ms) {
  return new Promise(function (resolve) { setTimeout(resolve, ms); });
}

function clipText(text, length) {
  var value = String(text || '').replace(/\s+/g, ' ').trim();
  if (value.length <= length) return value;
  return value.slice(0, length - 1) + '…';
}

function dateLabel(value) {
  if (!value) return '';
  var text = String(value);
  if (text.length >= 10) return text.slice(0, 10).replaceAll('-', '.');
  return text;
}

function itemMeta(item) {
  return [item.author, item.date ? dateLabel(item.date) : ''].filter(Boolean).join(' · ') || '未标注作者';
}

function kindLabel(kind) {
  return KIND_LABELS[kind] || '修订';
}

function pidLabel(pid) {
  return String(pid || '').replace(/^P/, '') || '未知';
}

function newEventId() {
  return 'local-' + Date.now() + '-' + Math.random().toString(36).slice(2, 9);
}

function friendlyError(error, fallback) {
  var code = String((error && error.code) || '');
  var detail = String((error && error.detail) || (error && error.message) || error || '');
  if (!code) {
    var match = detail.match(/^([a-z][a-z0-9-]*):\s*(.*)$/);
    if (match) { code = match[1]; detail = match[2]; }
  }
  var copy = ERROR_COPY[code];
  if (copy) return copy[0] + ' · ' + copy[1];
  return fallback + (detail ? ' · ' + detail : '');
}

function isStaleCode(code) {
  return [
    'stale-review-frame', 'generation-conflict', 'current-snapshot-drift',
    'staged-parent-mismatch', 'patch-parent-mismatch',
    'patch-context-mismatch', 'patch-precondition',
    'patch-fingerprint-mismatch', 'patch-style-mismatch',
  ].indexOf(code) >= 0;
}

function responseError(response, fallback) {
  return response.text().then(function (raw) {
    var payload = {};
    try { payload = raw ? JSON.parse(raw) : {}; } catch (_) { /* non-JSON body */ }
    var error = new Error(String(payload.error || raw || fallback));
    error.code = String(payload.code || '');
    error.detail = String(payload.error || raw || fallback);
    return error;
  });
}

function networkError(cause) {
  var error = new Error('网络连接失败 · ' + String((cause && cause.message) || ''));
  error.network = true;
  return error;
}

/* ------------------------------------------------------------- ReviewClient */

function ReviewClient() {
  this.token = sessionToken;
}

ReviewClient.prototype.request = function (url, options) {
  var headers = new Headers((options && options.headers) || {});
  if (this.token) headers.set('Authorization', 'Bearer ' + this.token);
  return fetch(url, Object.assign({}, options || {}, { headers: headers, cache: 'no-store' }))
    .then(function (response) {
      if (response.status === 404) {
        var error = new Error('会话已失效或已过期');
        error.code = 'session-invalid';
        throw error;
      }
      return response;
    })
    .catch(function (cause) {
      if (cause && cause.code === 'session-invalid') throw cause;
      throw networkError(cause);
    });
};

/* Full hydration happens ONLY through GET /api/review-frame. A history
   view is requested with ?history=<id>; the frame then declares
   history_id so the store can stay in read-only mode. */
ReviewClient.prototype.getFrame = function (historyId) {
  var url = '/api/review-frame';
  if (historyId) url += '?history=' + encodeURIComponent(historyId);
  var client = this;
  return client.request(url).then(function (response) {
    if (!response.ok) throw responseError(response, '审阅帧读取失败');
    return response.json();
  });
};

/* A mutating POST. `key` is the retry identity for this logical mutation:
   retries of the SAME mutation reuse it so the server replays the original
   committed data instead of double-applying. */
ReviewClient.prototype.post = function (path, body, key) {
  var client = this;
  return client.request(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'Idempotency-Key': key },
    body: JSON.stringify(body),
  }).then(function (response) {
    if (!response.ok) throw responseError(response, '保存失败');
    return response.json();
  });
};

ReviewClient.prototype.newKey = function () {
  if (globalThis.crypto && typeof crypto.randomUUID === 'function') {
    return 'k' + crypto.randomUUID().replace(/-/g, '');
  }
  var alphabet = 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789';
  var text = '';
  for (var i = 0; i < 24; i += 1) {
    text += alphabet.charAt(Math.floor(Math.random() * alphabet.length));
  }
  return 'k' + Date.now().toString(36) + text;
};

/* Retry ONLY transient network failures, reusing the same Idempotency-Key.
   HTTP errors (including stale-frame 409s) are final and never retried. */
function postWithRetry(client, path, body, key) {
  var attempt = 0;
  function run() {
    return client.post(path, body, key).catch(function (error) {
      if (!error.network || attempt >= MAX_RETRY) throw error;
      attempt += 1;
      return delay(400 * attempt).then(run);
    });
  }
  return run();
}

/* -------------------------------------------------------- frame normalization */

function normalizeFrame(raw) {
  var source = (raw && typeof raw === 'object') ? raw : {};
  var identity = source.identity && typeof source.identity === 'object' ? source.identity : {};
  var generation = identity.generation || source.generation || null;
  var generationManifestSha256 = identity.generation_manifest_sha256
    || identity.manifest_sha256
    || source.generation_manifest_sha256
    || source.manifest_sha256
    || null;
  var backed = source.backed !== false;
  var historyId = source.history_id || source.snapshot || null;
  var positionContract = source.position_contract || null;

  /* Session coordinates: `state` carries the session object plus
     filesystem_typed_sha256 / current_matches_filesystem; the frame also
     exposes current_snapshot / review_base / staged_snapshot at top
     level. Merge both so either shape works. */
  var state = source.state && typeof source.state === 'object' ? source.state : {};
  var session = {};
  Object.keys(state).forEach(function (key) { session[key] = state[key]; });
  if (!session.current_snapshot) session.current_snapshot = source.current_snapshot || null;
  if (!session.review_base) session.review_base = source.review_base || null;
  if (!session.staged_snapshot) session.staged_snapshot = source.staged_snapshot || null;
  if (!session.writer || typeof session.writer !== 'object') {
    session.writer = { state: 'idle', batch_id: null };
  }
  if (!('current_matches_filesystem' in session)) session.current_matches_filesystem = true;

  var doc = source.document && typeof source.document === 'object' ? source.document : {};
  var review = source.review && typeof source.review === 'object' ? source.review : {};
  var queue = review.events ? review : (source.queue && typeof source.queue === 'object' ? source.queue : {});
  var rawHistory = source.history;
  var history = rawHistory && Array.isArray(rawHistory.records)
    ? rawHistory.records
    : (Array.isArray(rawHistory) ? rawHistory : []);
  var diagnostics = Array.isArray(source.diagnostics) ? source.diagnostics : [];

  var revRegistry = Array.isArray(doc.revisions) ? doc.revisions : [];
  var comRegistry = Array.isArray(doc.comments) ? doc.comments : [];

  /* paragraphs: id-keyed object, or a FLAT ARRAY (Core projection) whose
     canonical text is derived by joining segment texts. */
  var rawParagraphs = doc.paragraphs && typeof doc.paragraphs === 'object' ? doc.paragraphs : {};
  var paragraphs = {};
  if (Array.isArray(rawParagraphs)) {
    rawParagraphs.forEach(function (record) {
      var pid = record && (record.paragraph_id || record.id);
      if (pid) paragraphs[pid] = normalizeParagraph(pid, record, revRegistry, comRegistry);
    });
  } else {
    Object.keys(rawParagraphs).forEach(function (pid) {
      paragraphs[pid] = normalizeParagraph(pid, rawParagraphs[pid], revRegistry, comRegistry);
    });
  }

  var tables = normalizeTables(doc, paragraphs);
  var styles = normalizeStyles(doc);

  /* blocks: flat list, or a NESTED per-part tree (Core projection):
     paragraph / table / box / sdt entries; containers flatten in reading
     order. */
  var blocks = [];
  var rawBlocks = Array.isArray(doc.blocks) ? doc.blocks : [];
  if (!rawBlocks.length && Array.isArray(doc.parts)) {
    /* Core projection: per-part block trees. Only the main document part
       belongs to the reading path; headers/footers/notes are side data. */
    doc.parts.forEach(function (part) {
      if (!part || !Array.isArray(part.blocks)) return;
      var partName = String(part.part || '');
      if (partName && partName !== 'document' && partName.indexOf('document.xml') < 0 && partName !== 'body') return;
      rawBlocks = rawBlocks.concat(part.blocks);
    });
  }
  rawBlocks.forEach(function (entry) {
    flattenBlock(entry, paragraphs, tables, blocks);
  });

  return {
    schema: source.schema,
    generation: generation,
    generationManifestSha256: generationManifestSha256,
    backed: backed,
    historyId: historyId,
    positionContract: positionContract,
    session: session,
    queue: queue,
    history: history,
    diagnostics: diagnostics,
    paragraphs: paragraphs,
    tables: tables,
    styles: styles,
    blocks: blocks.filter(Boolean),
    images: Array.isArray(doc.images) ? doc.images : [],
  };
}

function flattenBlock(entry, paragraphs, tables, blocks) {
  if (!entry || typeof entry !== 'object') return;
  if (entry.kind === 'paragraph') {
    blocks.push({ kind: 'paragraph', id: entry.paragraph_id || entry.id || '' });
  } else if (entry.kind === 'table') {
    blocks.push({ kind: 'table', id: 'T' + entry.table_index });
  } else if (entry.kind === 'box' || entry.kind === 'sdt') {
    /* Containers render their children in reading order; the container
       itself adds no prose. */
    if (Array.isArray(entry.blocks)) {
      entry.blocks.forEach(function (child) { flattenBlock(child, paragraphs, tables, blocks); });
    }
  } else if (typeof entry === 'string') {
    blocks.push(paragraphs[entry] ? { kind: 'paragraph', id: entry }
      : tables[entry] ? { kind: 'table', id: entry } : null);
  } else if (entry.id || entry.paragraph_id || entry.table_id) {
    var blockId = entry.id || entry.paragraph_id || entry.table_id;
    blocks.push({ kind: entry.kind || 'paragraph', id: blockId });
  } else if (entry.Paragraph && typeof entry.Paragraph === 'object') {
    /* Rust Core projection: serde externally-tagged enum entries. */
    var paragraph = entry.Paragraph;
    blocks.push({ kind: 'paragraph', id: paragraph.paragraph_id || paragraph.id || '' });
    if (Array.isArray(paragraph.nested)) {
      paragraph.nested.forEach(function (child) { flattenBlock(child, paragraphs, tables, blocks); });
    }
  } else if (entry.Table && typeof entry.Table === 'object') {
    blocks.push({ kind: 'table', id: 'T' + entry.Table.table_index });
  } else if (entry.Box && typeof entry.Box === 'object') {
    if (Array.isArray(entry.Box.blocks)) {
      entry.Box.blocks.forEach(function (child) { flattenBlock(child, paragraphs, tables, blocks); });
    }
  } else if (entry.Sdt && typeof entry.Sdt === 'object') {
    if (Array.isArray(entry.Sdt.blocks)) {
      entry.Sdt.blocks.forEach(function (child) { flattenBlock(child, paragraphs, tables, blocks); });
    }
  }
}

function normalizeParagraph(pid, rec, revRegistry, comRegistry) {
  var record = rec && typeof rec === 'object' ? rec : {};
  var baseStyle = typeof record.style_id === 'string' ? record.style_id
    : (typeof record.base_style === 'string' ? record.base_style : '');
  var paragraphFingerprint = typeof record.paragraph_fingerprint === 'string'
    ? record.paragraph_fingerprint : null;

  /* Regions/segments are the only renderable body source: contiguous
     [start, end) ranges in Unicode scalar offsets per paragraph. The
     canonical paragraph text is derived by joining segment texts. */
  var rawRegions = Array.isArray(record.regions) ? record.regions
    : (Array.isArray(record.segments) ? record.segments : []);
  var text = typeof record.text === 'string' ? record.text : '';
  if (!text && rawRegions.length) {
    text = rawRegions.map(function (region) {
      return typeof region.text === 'string' ? region.text : '';
    }).join('');
  }
  var textLength = scalarLength(text);

  function revisionByKey(key) {
    for (var i = 0; i < revRegistry.length; i += 1) {
      if (revRegistry[i] && revRegistry[i].key === key) return revRegistry[i];
    }
    return null;
  }
  function commentById(id) {
    for (var i = 0; i < comRegistry.length; i += 1) {
      if (comRegistry[i] && (comRegistry[i].id === id || comRegistry[i].cid === id)) return comRegistry[i];
    }
    return null;
  }

  var regions = [];
  var cursor = 0;
  var sorted = rawRegions.slice().sort(function (left, right) {
    return toNumber(left.start, 0) - toNumber(right.start, 0);
  });
  sorted.forEach(function (region) {
    if (!region || typeof region !== 'object') return;
    var start = clampInt(toNumber(region.start, cursor), 0, textLength);
    var end = clampInt(toNumber(region.end, start), start, textLength);
    if (end < start) end = start;

    /* Resolve per-segment revision metadata: an object, a revision key
       string (looked up in the registry), or revision_* fields. */
    var revisionMeta = null;
    var meta = region.revision;
    if (meta && typeof meta === 'object') {
      revisionMeta = Object.assign({}, meta);
    } else if (typeof meta === 'string') {
      revisionMeta = revisionByKey(meta);
      if (!revisionMeta) revisionMeta = { key: meta, kind: 'insert' };
    } else if (region.revision_id || region.revision_key) {
      var keyCandidate = region.revision_key || region.revision_id;
      revisionMeta = revisionByKey(keyCandidate) || {
        key: keyCandidate,
        kind: region.revision_kind || 'insert',
        author: region.revision_author,
        date: region.revision_date,
      };
    }
    var kind = revisionMeta ? (revisionMeta.kind || 'insert') : null;

    /* Visibility: trust projection flags when present; otherwise derive
       from the revision kind (final hides deletes, original hides
       inserts). */
    var vis = region.visibility && typeof region.visibility === 'object' ? region.visibility : {};
    var hasFinalView = Object.prototype.hasOwnProperty.call(vis, 'final_view');
    var visibility = {
      original: Object.prototype.hasOwnProperty.call(vis, 'original') ? !!vis.original : kind !== 'insert',
      tracked: Object.prototype.hasOwnProperty.call(vis, 'tracked') ? !!vis.tracked : true,
      final: Object.prototype.hasOwnProperty.call(vis, 'final') ? !!vis.final
        : (hasFinalView ? !!vis.final_view : kind !== 'delete'),
    };

    regions.push({
      id: typeof region.id === 'string' ? region.id : '',
      start: start,
      end: end,
      text: typeof region.text === 'string' ? region.text : scalarSlice(text, start, end),
      style_id: typeof region.style_id === 'string' ? region.style_id : '',
      region_fingerprint: typeof region.region_fingerprint === 'string' ? region.region_fingerprint : null,
      revision_meta: revisionMeta,
      comment_ids: Array.isArray(region.comment_ids) ? region.comment_ids : [],
      visibility: visibility,
    });
    cursor = Math.max(cursor, end);
  });
  if (!regions.length && text) {
    regions.push({
      id: '', start: 0, end: textLength, text: text, style_id: baseStyle,
      region_fingerprint: null, revision_meta: null, comment_ids: [],
      visibility: { original: true, tracked: true, final: true },
    });
  }

  var revisions = [];
  var seenRevisions = {};
  function addRevision(meta) {
    if (!meta || typeof meta !== 'object') return;
    var rid = meta.rid || meta.id || meta.key || meta.revision_key || '';
    if (!rid || seenRevisions[rid]) return;
    seenRevisions[rid] = true;
    var kind = meta.kind || 'insert';
    var start = toNumber(meta.start, null);
    var end = toNumber(meta.end, null);
    revisions.push({
      rid: rid,
      key: meta.key || meta.revision_key || rid,
      kind: kind,
      author: typeof meta.author === 'string' ? meta.author : '',
      date: typeof meta.date === 'string' ? meta.date : '',
      text: typeof meta.text === 'string' ? meta.text : '',
      start: start === null ? null : clampInt(start, 0, textLength),
      end: end === null ? null : clampInt(end, 0, textLength),
    });
  }

  regions.forEach(function (region) {
    if (region.revision_meta) {
      addRevision(Object.assign({}, region.revision_meta, { start: region.start, end: region.end }));
    }
  });
  revRegistry.forEach(function (meta) {
    if (!meta || typeof meta !== 'object' || meta.paragraph_id !== pid) return;
    addRevision(meta);
  });

  var comments = [];
  var seenComments = {};
  function addComment(meta) {
    if (!meta || typeof meta !== 'object') return;
    var cid = meta.cid || meta.id || '';
    if (!cid || seenComments[cid]) return;
    seenComments[cid] = true;
    var start = toNumber(meta.start, null);
    var end = toNumber(meta.end, null);
    comments.push({
      cid: cid,
      author: typeof meta.author === 'string' ? meta.author : '',
      date: typeof meta.date === 'string' ? meta.date : '',
      text: typeof meta.text === 'string' ? meta.text : '',
      source: typeof meta.source === 'string' ? meta.source : 'word',
      start: start === null ? null : clampInt(start, 0, textLength),
      end: end === null ? null : clampInt(end, 0, textLength),
    });
  }
  regions.forEach(function (region) {
    var inline = Array.isArray(region.comments) ? region.comments : [];
    inline.forEach(function (meta) {
      addComment(Object.assign({}, meta, { start: region.start, end: region.end }));
    });
    region.comment_ids.forEach(function (cid) {
      var resolved = commentById(cid);
      if (resolved) addComment(Object.assign({}, resolved, { cid: resolved.id || resolved.cid, start: region.start, end: region.end }));
    });
  });
  /* Comments anchor to the paragraph via `anchors` (Core projection) or a
     flat paragraph_id + offsets. */
  comRegistry.forEach(function (meta) {
    if (!meta || typeof meta !== 'object') return;
    var anchors = Array.isArray(meta.anchors) ? meta.anchors
      : (meta.paragraph_id ? [{ paragraph_id: meta.paragraph_id, start: meta.start, end: meta.end }] : []);
    anchors.forEach(function (anchor) {
      if (!anchor || typeof anchor !== 'object') return;
      if ((anchor.paragraph_id || anchor.pid || '') !== pid) return;
      /* Frame anchors carry paragraph_id but no scalar offsets (the Core
         CommentEntry has only kind/part/paragraph_id). Without offsets
         addComment stores start=null and the units filter drops the
         comment entirely - the "undefined" marker bug. Fall back to the
         whole-paragraph span so the comment attaches to its paragraph. */
      var start = toNumber(anchor.start, null);
      var end = toNumber(anchor.end, null);
      if (start === null || end === null) { start = 0; end = textLength; }
      addComment({
        cid: meta.cid || meta.id,
        author: meta.author,
        date: meta.date,
        text: meta.text,
        start: start,
        end: end,
      });
    });
  });

  revisions.sort(function (a, b) { return offsetRank(a) - offsetRank(b); });
  comments.sort(function (a, b) { return offsetRank(a) - offsetRank(b); });

  return {
    id: pid,
    base_style: baseStyle,
    paragraph_fingerprint: paragraphFingerprint,
    text: text,
    regions: regions,
    revisions: revisions,
    comments: comments,
  };
}

function offsetRank(item) {
  return item.start === null ? Number.MAX_SAFE_INTEGER : item.start;
}

var TABLE_PARAGRAPH_RE = /^T(\d+)\.R(\d+)\.C(\d+)\.P(\d+)$/;

function normalizeTables(doc, paragraphs) {
  var tables = {};
  if (Array.isArray(doc.tables)) {
    /* Core projection: flat array of { part, table_index, rows, start, end }. */
    doc.tables.forEach(function (rec) {
      if (!rec || typeof rec !== 'object') return;
      var index = toNumber(rec.table_index, null);
      var tid = index !== null ? 'T' + index
        : (typeof rec.id === 'string' && rec.id ? rec.id : '');
      if (tid) tables[tid] = normalizeTable(tid, rec, paragraphs);
    });
  } else if (doc.tables && typeof doc.tables === 'object') {
    Object.keys(doc.tables).forEach(function (tid) {
      tables[tid] = normalizeTable(tid, doc.tables[tid], paragraphs);
    });
  }
  /* Derive a table grid for tables referenced by blocks but missing a
     registry entry, from cell paragraph ids (T<n>.R<r>.C<c>.P<p>). */
  var derived = {};
  Object.keys(paragraphs).forEach(function (pid) {
    var match = TABLE_PARAGRAPH_RE.exec(pid);
    if (!match) return;
    var tid = 'T' + match[1];
    (derived[tid] = derived[tid] || []).push({ row: Number(match[2]), col: Number(match[3]), pid: pid });
  });
  Object.keys(derived).forEach(function (tid) {
    if (tables[tid]) return;
    var rows = {};
    derived[tid].forEach(function (entry) {
      (rows[entry.row] = rows[entry.row] || {})[entry.col] = (rows[entry.row][entry.col] || []).concat(entry.pid);
    });
    tables[tid] = { id: tid, rows: tableRowsFromMap(rows) };
  });
  return tables;
}

function tableRowsFromMap(byRow) {
  var rows = [];
  Object.keys(byRow).map(Number).sort(function (a, b) { return a - b; }).forEach(function (rowIndex) {
    var cells = [];
    Object.keys(byRow[rowIndex]).map(Number).sort(function (a, b) { return a - b; }).forEach(function (colIndex) {
      cells.push({ index: colIndex, paragraph_ids: byRow[rowIndex][colIndex] });
    });
    rows.push({ index: rowIndex, cells: cells });
  });
  return rows;
}

function normalizeTable(tid, rec, paragraphs) {
  var record = rec && typeof rec === 'object' ? rec : {};
  /* Projection shape: { id, rows, columns } with numeric dimensions. */
  var rowsCount = toNumber(record.rows, null);
  var columnsCount = toNumber(record.columns, null);
  if (rowsCount !== null || columnsCount !== null) {
    var byRC = {};
    Object.keys(paragraphs).forEach(function (pid) {
      var match = TABLE_PARAGRAPH_RE.exec(pid);
      if (!match || 'T' + match[1] !== tid) return;
      byRC[Number(match[2]) + ':' + Number(match[3])] = (byRC[Number(match[2]) + ':' + Number(match[3])] || []).concat(pid);
    });
    var rows = [];
    var height = Math.max(rowsCount || 0, rowsCount === null ? 1 : 0);
    var width = Math.max(columnsCount || 0, columnsCount === null ? 1 : 0);
    for (var r = 0; r < height; r += 1) {
      var cells = [];
      for (var c = 0; c < width; c += 1) {
        cells.push({ index: c, paragraph_ids: byRC[r + ':' + c] || [] });
      }
      rows.push({ index: r, cells: cells });
    }
    return { id: tid, rows: rows };
  }
  /* Tolerant fallback for richer shapes: rows as array/map of cells. */
  var rows = [];
  var rawRows = record.rows;
  if (Array.isArray(rawRows)) {
    rawRows.forEach(function (rawRow, rowIndex) {
      if (Array.isArray(rawRow)) {
        var cells = rawRow.map(function (ids, colIndex) {
          return {
            index: colIndex,
            paragraph_ids: Array.isArray(ids) ? ids : (typeof ids === 'string' ? [ids] : []),
          };
        });
        rows.push({ index: toNumber(record.index, rowIndex), cells: cells });
      } else if (rawRow && typeof rawRow === 'object') {
        var rawCells = Array.isArray(rawRow.cells) ? rawRow.cells : [];
        var normalized = rawCells.map(function (rawCell, colIndex) {
          if (typeof rawCell === 'string') return { index: colIndex, paragraph_ids: [rawCell] };
          if (Array.isArray(rawCell)) {
            return { index: toNumber(rawCell.index, colIndex), paragraph_ids: rawCell };
          }
          if (!rawCell || typeof rawCell !== 'object') return { index: colIndex, paragraph_ids: [] };
          var ids = rawCell.paragraph_ids || rawCell.paragraphs || (rawCell.pid ? [rawCell.pid] : []);
          return {
            index: toNumber(rawCell.index, toNumber(rawCell.col_index, colIndex)),
            paragraph_ids: Array.isArray(ids) ? ids : [ids],
          };
        });
        rows.push({ index: toNumber(rawRow.index, toNumber(rawRow.row_index, rowIndex)), cells: normalized });
      }
    });
  } else if (rawRows && typeof rawRows === 'object') {
    var byRow = {};
    Object.keys(rawRows).forEach(function (rowKey) {
      var rawRow = rawRows[rowKey];
      var cells = [];
      if (rawRow && typeof rawRow === 'object') {
        Object.keys(rawRow).forEach(function (colKey) {
          var ids = rawRow[colKey];
          cells.push({
            index: toNumber(colKey, 0),
            paragraph_ids: Array.isArray(ids) ? ids : (typeof ids === 'string' ? [ids] : []),
          });
        });
      }
      byRow[toNumber(rowKey, 0)] = cells;
    });
    rows = tableRowsFromMap(byRow);
  }
  rows.sort(function (a, b) { return a.index - b.index; });
  rows.forEach(function (row) { row.cells.sort(function (a, b) { return a.index - b.index; }); });
  return { id: tid, rows: rows };
}

function normalizeStyles(doc) {
  var styles = {};
  function normalizeStyle(sid, rec) {
    return {
      id: sid,
      label: rec && typeof rec.label === 'string' ? rec.label
        : (rec && typeof rec.name === 'string' ? rec.name : ''),
      features: rec && rec.features && typeof rec.features === 'object' ? rec.features : {},
      css: rec && rec.css && typeof rec.css === 'object' ? rec.css : null,
      mapped: Array.isArray(rec && rec.mapped) ? rec.mapped : [],
      unmapped: Array.isArray(rec && rec.unmapped) ? rec.unmapped : [],
    };
  }
  if (Array.isArray(doc.styles)) {
    /* Core projection: flat array of { style_id, label, features }. */
    doc.styles.forEach(function (rec) {
      if (!rec || typeof rec !== 'object') return;
      var sid = rec.style_id || rec.id || '';
      if (sid) styles[sid] = normalizeStyle(sid, rec);
    });
  } else {
    var rawStyles = doc.styles && typeof doc.styles === 'object' ? doc.styles : {};
    Object.keys(rawStyles).forEach(function (sid) {
      styles[sid] = normalizeStyle(sid, rawStyles[sid]);
    });
  }
  return styles;
}

/* ------------------------------------------------------------ DocumentRenderer */

function styleDeclarations(styleRec) {
  var features = styleRec.features;
  var css = {};
  var mapped = new Set(styleRec.mapped || []);
  var unmapped = new Set(styleRec.unmapped || []);

  function addDecoration(value) {
    var existing = css['text-decoration'] || '';
    css['text-decoration'] = existing ? existing + ' ' + value : value;
  }

  var fonts = [];
  ['font:ascii', 'font:hAnsi', 'font:eastAsia', 'font:cs'].forEach(function (key) {
    var value = features[key];
    if (!value) return;
    var cleaned = String(value).replace(/['"]/g, '').trim();
    if (cleaned) fonts.push("'" + cleaned + "'");
    mapped.add(key);
  });
  if (fonts.length) {
    fonts.push('Arial', 'sans-serif');
    css['font-family'] = Array.from(new Set(fonts)).join(', ');
  }

  var size = toNumber(features.sz, null);
  if (size !== null) { css['font-size'] = (size / 2) + 'pt'; mapped.add('sz'); }
  var sizeCs = toNumber(features.szCs, null);
  if (size === null && sizeCs !== null) { css['font-size'] = (sizeCs / 2) + 'pt'; }
  if (sizeCs !== null) mapped.add('szCs');

  if (features.b) { css['font-weight'] = '700'; mapped.add('b'); }
  if (features.i) { css['font-style'] = 'italic'; mapped.add('i'); }
  if (features.strike) { addDecoration('line-through'); mapped.add('strike'); }
  if (features.dstrike) { addDecoration('line-through'); css['text-decoration-style'] = 'double'; mapped.add('dstrike'); }
  if (features.smallCaps) { css['font-variant-caps'] = 'small-caps'; mapped.add('smallCaps'); }
  if (features.caps) { css['text-transform'] = 'uppercase'; mapped.add('caps'); }
  if (features.outline) { css['-webkit-text-stroke'] = '0.25px currentColor'; mapped.add('outline'); }
  if (features.shadow) { css['text-shadow'] = '1px 1px 0 rgba(17,17,17,.24)'; mapped.add('shadow'); }
  if (features.emboss) { css['text-shadow'] = '1px 1px 0 rgba(255,255,255,.75), -1px -1px 0 rgba(17,17,17,.22)'; mapped.add('emboss'); }
  if (features.imprint) { css['text-shadow'] = '-1px -1px 0 rgba(255,255,255,.75), 1px 1px 0 rgba(17,17,17,.22)'; mapped.add('imprint'); }
  if (features.vanish || features.webHidden) {
    css['opacity'] = '.42';
    addDecoration('underline dotted');
    if (features.vanish) mapped.add('vanish');
    if (features.webHidden) mapped.add('webHidden');
  }

  var vertical = String(features.vertAlign || '');
  if (vertical === 'superscript' || vertical === 'subscript') {
    css['vertical-align'] = vertical === 'superscript' ? 'super' : 'sub';
    css['font-size'] = '.72em';
    mapped.add('vertAlign');
  }
  var position = toNumber(features.position, null);
  if (position !== null) {
    mapped.add('position');
    if (!vertical) css['vertical-align'] = (position / 2) + 'pt';
  }

  var color = String(features.color || '');
  if (color) {
    var hex = color.replace(/^#/, '');
    if (/^[0-9A-Fa-f]{6}$/.test(hex)) { css.color = '#' + hex; mapped.add('color'); }
    else if (color.toLowerCase() === 'auto') { mapped.add('color'); }
    else unmapped.add('color=' + color);
  }

  var highlight = String(features.highlight || '');
  if (highlight) {
    if (Object.prototype.hasOwnProperty.call(HIGHLIGHT_HEX, highlight)) {
      css['background-color'] = HIGHLIGHT_HEX[highlight];
      mapped.add('highlight');
    } else if (highlight === 'none') { mapped.add('highlight'); }
    else unmapped.add('highlight=' + highlight);
  }

  var underline = String(features.u || '');
  if (underline) {
    if (underline === 'none' || underline === 'false') mapped.add('u');
    else if (Object.prototype.hasOwnProperty.call(UNDERLINE_STYLES, underline)) {
      addDecoration('underline');
      css['text-decoration-style'] = UNDERLINE_STYLES[underline];
      mapped.add('u');
    } else unmapped.add('u=' + underline);
  }

  var kern = toNumber(features.kern, null);
  if (kern !== null) { css['letter-spacing'] = (kern / 2) + 'pt'; mapped.add('kern'); }
  var spacing = toNumber(features.spacing, null);
  if (spacing !== null) { css['letter-spacing'] = (spacing / 20) + 'pt'; mapped.add('spacing'); }
  var width = toNumber(features.w, null);
  if (width !== null) {
    css['font-stretch'] = Math.max(50, Math.min(200, width)) + '%';
    mapped.add('w');
  }

  if (features.rtl) { css.direction = 'rtl'; css['unicode-bidi'] = 'embed'; mapped.add('rtl'); }
  var emphasis = String(features.em || '');
  if (emphasis) {
    css['text-emphasis'] = (emphasis === 'true' || emphasis === '1') ? 'filled dot' : 'filled ' + emphasis;
    mapped.add('em');
  }
  ['font:hint', 'rStyle', 'cs'].forEach(function (key) {
    if (key in features) mapped.add(key);
  });

  Object.keys(features).forEach(function (key) {
    if (!mapped.has(key) && key.indexOf('font:') !== 0) unmapped.add(key);
  });

  return { css: css, mapped: Array.from(mapped).sort(), unmapped: Array.from(unmapped).sort() };
}

var CSS_VALUE_RE = /^[A-Za-z0-9 .,#%()'"/:-]+$/;

function allowedCssObject(styleRec) {
  var result = {};
  var source = styleRec.css || {};
  Object.keys(source).forEach(function (key) {
    var cssKey = key.replace(/([a-z0-9])([A-Z])/g, '$1-$2').toLowerCase();
    if (ALLOWED_CSS_KEYS.indexOf(cssKey) < 0) return;
    var value = String(source[key]);
    if (!CSS_VALUE_RE.test(value)) return;
    result[cssKey] = value;
  });
  return result;
}

function cssRuleFor(styleRec) {
  if (!/^[A-Za-z0-9_-]+$/.test(styleRec.id)) return '';
  var derived = styleDeclarations(styleRec);
  var css = derived.css;
  if (!Object.keys(css).length) css = allowedCssObject(styleRec);
  var declarations = Object.keys(css).map(function (key) {
    return key + ': ' + css[key];
  }).join('; ');
  if (!declarations) return '';
  return '.s-' + styleRec.id + ' { ' + declarations + ' }';
}

function DocumentRenderer(store, paper, styleHost, auditBody, auditStatus, diagnosticsHost, hooks) {
  this.store = store;
  this.paper = paper;
  this.styleHost = styleHost;
  this.styleSheet = null;
  this.auditBody = auditBody;
  this.auditStatus = auditStatus;
  this.diagnosticsHost = diagnosticsHost;
  this.hooks = hooks || {};
}

DocumentRenderer.prototype.render = function (frame) {
  this.applyStyleRules(frame.styles);
  this.renderPaper(frame);
  this.renderAudit(frame.styles);
  this.renderDiagnostics(frame.diagnostics);
};

DocumentRenderer.prototype.applyStyleRules = function (styles) {
  var rules = [];
  Object.keys(styles).forEach(function (sid) {
    var rule = cssRuleFor(styles[sid]);
    if (rule) rules.push(rule);
  });
  if (!this.styleSheet && 'CSSStyleSheet' in window && 'adoptedStyleSheets' in document) {
    this.styleSheet = new CSSStyleSheet();
    document.adoptedStyleSheets = document.adoptedStyleSheets.concat([this.styleSheet]);
  }
  if (this.styleSheet) this.styleSheet.replaceSync(rules.join('\n'));
};

DocumentRenderer.prototype.renderPaper = function (frame) {
  clearChildren(this.paper);
  var rendered = 0;
  var self = this;
  frame.blocks.forEach(function (block) {
    if (block.kind === 'table') {
      var table = self.buildTable(block.id, frame);
      if (table) { self.paper.appendChild(table); rendered += 1; }
      return;
    }
    var paragraph = self.buildParagraph(block.id, frame);
    if (paragraph) { self.paper.appendChild(paragraph); rendered += 1; }
  });
  if (!rendered) {
    var empty = el('p', 'paper-empty');
    empty.textContent = '当前文档没有可渲染的正文段落。';
    this.paper.appendChild(empty);
  }
};

DocumentRenderer.prototype.buildParagraph = function (pid, frame) {
  var paragraph = frame.paragraphs[pid];
  if (!paragraph) return null;
  var root = el('p', 'document-paragraph');
  root.dataset.pid = pid;
  var self = this;
  /* Embedded images of this paragraph render first: drawing runs are
     opaque (no text), so the figure sits at the paragraph top. */
  (frame.images || []).forEach(function (image) {
    if (image.paragraph_id !== pid) return;
    var figure = el('span', 'doc-image');
    figure.dataset.pid = pid;
    var img = el('img', 'doc-image-img');
    img.src = image.data_uri;
    img.alt = '';
    img.width = image.width_px;
    img.height = image.height_px;
    figure.appendChild(img);
    root.appendChild(figure);
  });
  paragraphUnits(paragraph).forEach(function (unit) {
    unit.comments.forEach(function (comment) {
      root.appendChild(self.buildCommentAnchor(comment));
    });
    var holder = root;
    if (unit.revision) {
      holder = self.buildRevisionMark(unit.revision, unit);
      root.appendChild(holder);
    }
    var seg = el('span', 'doc-seg');
    seg.dataset.pid = pid;
    seg.dataset.start = String(unit.start);
    seg.dataset.end = String(unit.end);
    seg.dataset.finalVis = unit.visibility && unit.visibility.final === false ? '0' : '1';
    seg.dataset.originalVis = unit.visibility && unit.visibility.original === false ? '0' : '1';
    if (unit.style_id) seg.classList.add('s-' + unit.style_id);
    seg.textContent = unit.text;
    holder.appendChild(seg);
  });
  return root;
};

DocumentRenderer.prototype.buildRevisionMark = function (revision, unit) {
  var isDelete = revision.kind === 'delete' || revision.kind === 'move_from';
  var mark = el('span', 'revision-mark ' + (isDelete ? 'revision-delete' : 'revision-insert'));
  mark.dataset.rid = revision.rid;
  mark.dataset.revkey = revision.key || revision.rid;
  mark.dataset.revkind = revision.kind;
  mark.dataset.finalVis = unit.visibility && unit.visibility.final === false ? '0' : '1';
  mark.dataset.originalVis = unit.visibility && unit.visibility.original === false ? '0' : '1';
  mark.tabIndex = 0;
  mark.setAttribute('role', 'button');
  mark.setAttribute('aria-label', kindLabel(revision.kind) + '修订：' + clipText(unit.text, 40));
  var self = this;
  mark.addEventListener('click', function (event) {
    event.stopPropagation();
    if (self.hooks.onRevisionClick) self.hooks.onRevisionClick(revision.rid);
  });
  mark.addEventListener('keydown', function (event) {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      if (self.hooks.onRevisionClick) self.hooks.onRevisionClick(revision.rid, true);
    }
  });
  return mark;
};

DocumentRenderer.prototype.buildCommentAnchor = function (comment) {
  var anchor = el('button', 'comment-anchor' + (comment.source === 'agent' ? ' comment-anchor--agent' : ''));
  anchor.type = 'button';
  anchor.dataset.cid = comment.cid;
  anchor.textContent = pad2(comment.order);
  anchor.title = '批注 ' + comment.order;
  anchor.setAttribute('aria-label', '批注 ' + comment.order);
  var self = this;
  anchor.addEventListener('click', function () {
    if (self.hooks.onCommentClick) self.hooks.onCommentClick(comment.cid);
  });
  return anchor;
};

DocumentRenderer.prototype.buildTable = function (tid, frame) {
  var table = frame.tables[tid];
  if (!table || !table.rows.length) return null;
  var root = el('table', 'document-table');
  root.dataset.tableId = tid;
  var caption = el('caption', 'sr-only');
  caption.textContent = '正文表格 ' + tid;
  root.appendChild(caption);
  var body = el('tbody');
  var firstRowIndex = table.rows[0].index;
  var self = this;
  table.rows.forEach(function (row) {
    var tr = el('tr', row.index === firstRowIndex ? 'document-table-row document-table-row--first' : 'document-table-row');
    tr.dataset.row = String(row.index);
    row.cells.forEach(function (cell) {
      var td = el('td', 'document-table-cell');
      td.dataset.column = String(cell.index);
      var ids = Array.isArray(cell.paragraph_ids) ? cell.paragraph_ids : [];
      if (!ids.length) {
        var empty = el('p', 'document-paragraph table-cell-empty');
        empty.setAttribute('aria-hidden', 'true');
        empty.textContent = '\u00a0';
        td.appendChild(empty);
      }
      ids.forEach(function (cpid) {
        var paragraph = self.buildParagraph(cpid, frame);
        if (paragraph) {
          paragraph.classList.add('table-cell-paragraph');
          td.appendChild(paragraph);
        }
      });
      tr.appendChild(td);
    });
    body.appendChild(tr);
  });
  root.appendChild(body);
  return root;
};

DocumentRenderer.prototype.renderAudit = function (styles) {
  var rows = [];
  var warningCount = 0;
  Object.keys(styles).sort().forEach(function (sid) {
    var style = styles[sid];
    var derived = styleDeclarations(style);
    var unmapped = derived.unmapped.length ? derived.unmapped : (style.unmapped || []);
    var mapped = derived.mapped.length ? derived.mapped : (style.mapped || []);
    if (unmapped.length) warningCount += 1;
    rows.push({ sid: sid, label: style.label || '—', mapped: mapped, unmapped: unmapped });
  });
  clearChildren(this.auditBody);
  rows.forEach(function (row) {
    var tr = el('tr');
    tr.appendChild(el('td', 'mono', { text: row.sid }));
    tr.appendChild(el('td', null, { text: row.label }));
    tr.appendChild(el('td', row.mapped.length ? 'audit-ok' : null, { text: row.mapped.join(', ') || '—' }));
    tr.appendChild(el('td', row.unmapped.length ? 'audit-warning' : 'audit-ok', { text: row.unmapped.join(', ') || '—' }));
    this.auditBody.appendChild(tr);
  }, this);
  var status = warningCount ? warningCount + ' 个样式待核对' : '完整映射';
  setText(this.auditStatus, status);
  this.auditStatus.classList.toggle('has-warning', warningCount > 0);
};

DocumentRenderer.prototype.renderDiagnostics = function (diagnostics) {
  clearChildren(this.diagnosticsHost);
  if (!diagnostics || !diagnostics.length) {
    this.diagnosticsHost.hidden = true;
    return;
  }
  this.diagnosticsHost.hidden = false;
  diagnostics.forEach(function (item) {
    if (!item || typeof item !== 'object') return;
    var row = el('p', 'diagnostic-entry');
    var code = el('span', 'mono', { text: String(item.code || '') });
    row.appendChild(code);
    row.appendChild(document.createTextNode(' ' + String(item.message || '')));
    this.diagnosticsHost.appendChild(row);
  }, this);
};

function paragraphUnits(paragraph) {
  var length = scalarLength(paragraph.text);
  var cuts = new Set([0, length]);
  /* Region boundaries are cut points too: a unit must fall inside one
     region so multi-segment paragraphs (per-run segments from the Rust
     frame) render every segment instead of being dropped because no
     single region covers the whole paragraph. */
  (paragraph.regions || []).forEach(function (region) {
    if (!region) return;
    cuts.add(clampInt(region.start, 0, length));
    cuts.add(clampInt(region.end, 0, length));
  });
  paragraph.revisions.forEach(function (revision) {
    if (revision.start === null || revision.end === null) return;
    cuts.add(clampInt(revision.start, 0, length));
    cuts.add(clampInt(revision.end, 0, length));
  });
  var points = Array.from(cuts).sort(function (a, b) { return a - b; });
  var units = [];
  for (var i = 0; i + 1 < points.length; i += 1) {
    var start = points[i];
    var end = points[i + 1];
    if (end <= start) continue;
    var region = null;
    for (var s = 0; s < paragraph.regions.length; s += 1) {
      var candidate = paragraph.regions[s];
      if (candidate.start <= start && end <= candidate.end) { region = candidate; break; }
    }
    if (!region) continue;
    var revision = null;
    for (var r = 0; r < paragraph.revisions.length; r += 1) {
      var rev = paragraph.revisions[r];
      if (rev.start !== null && rev.start <= start && end <= rev.end) { revision = rev; break; }
    }
    var comments = paragraph.comments.filter(function (comment) {
      if (comment.start === null) return false;
      return comment.start >= start && (comment.start < end || (end === length && comment.start === end));
    });
    units.push({
      start: start,
      end: end,
      text: scalarSlice(paragraph.text, start, end),
      style_id: (region.style_id || paragraph.base_style || ''),
      revision: revision,
      comments: comments,
      visibility: region.visibility || { original: true, tracked: true, final: true },
    });
  }
  return units;
}

/* --------------------------------------------------------------- ReviewStore */

function ReviewStore(client) {
  this.client = client;
  this.frame = null;
  this.generation = null;
  this.generationManifestSha256 = null;
  this.historyTarget = null;
  this.status = 'loading'; /* loading | ready | stale | conflict | writer-busy | error */
  this.errorMessage = '';
  this.requiresConfirmation = false;
  this.listeners = [];
  this.refreshPending = false;
  this.polling = false;
}

ReviewStore.prototype.subscribe = function (listener) {
  this.listeners.push(listener);
};

ReviewStore.prototype.notify = function (reason) {
  this.listeners.forEach(function (listener) { listener(reason); });
};

ReviewStore.prototype.setStatus = function (status, message) {
  this.status = status;
  this.errorMessage = message || '';
  this.notify('status');
};

ReviewStore.prototype.hydrate = function () {
  var store = this;
  this.setStatus('loading', '正在载入审阅帧…');
  return this.client.getFrame().then(function (raw) {
    store.adoptFrame(raw);
  }).catch(function (error) {
    store.setStatus('error', friendlyError(error, '载入失败'));
  });
};

ReviewStore.prototype.adoptFrame = function (raw) {
  var frame = normalizeFrame(raw);
  if (!frame.schema || frame.schema !== FRAME_SCHEMA) {
    this.setStatus('error', '审阅帧格式无法识别');
    return;
  }
  this.frame = frame;
  this.generation = frame.generation;
  this.generationManifestSha256 = frame.generationManifestSha256;
  this.historyTarget = frame.historyId || null;
  if (frame.backed === false) {
    this.setStatus('error', '该工作目录不是原子审阅工作区（backed=false），仅可只读查看。');
    this.notify('frame');
    return;
  }
  /* A session that has never seen a write has no current snapshot, so
     there is nothing to drift from — only a present-but-mismatched
     snapshot is a real conflict. */
  var hasCurrentSnapshot = frame.session && frame.session.current_snapshot;
  if (hasCurrentSnapshot && frame.session.current_matches_filesystem === false) {
    this.status = 'conflict';
    this.errorMessage = '当前文档与快照不一致（文件系统已变化），写入已被阻止。';
  } else if (frame.session.writer && frame.session.writer.state && frame.session.writer.state !== 'idle') {
    this.status = 'writer-busy';
    this.errorMessage = '另一个写入正在进行，请稍候，不要重复提交。';
  } else {
    this.status = 'ready';
    this.errorMessage = '';
  }
  this.notify('frame');
};

/* Partial polling results merge ONLY when their generation equals the
   current frame's generation; anything else is discarded and a full frame
   reload is triggered. */
ReviewStore.prototype.mergeCandidate = function (raw) {
  if (!raw || typeof raw !== 'object') { this.refreshSoon(); return; }
  var generation = (raw.identity && raw.identity.generation) || raw.generation || null;
  if (raw.schema === FRAME_SCHEMA) {
    if (generation && this.generation && generation === this.generation) {
      this.mergePartial(raw);
    } else {
      this.adoptFrame(raw);
    }
    return;
  }
  if (generation && this.generation && generation === this.generation) {
    this.mergePartial(raw);
  } else {
    this.refreshSoon();
  }
};

ReviewStore.prototype.mergePartial = function (raw) {
  if (!this.frame) return;
  if (raw.state && typeof raw.state === 'object') this.frame.session = normalizeSession(raw.state, this.frame.session);
  if (raw.current_snapshot) this.frame.session.current_snapshot = raw.current_snapshot;
  if (raw.review_base) this.frame.session.review_base = raw.review_base;
  if (raw.staged_snapshot) this.frame.session.staged_snapshot = raw.staged_snapshot;
  if (raw.review && typeof raw.review === 'object') this.frame.queue = raw.review;
  if (raw.queue && typeof raw.queue === 'object') this.frame.queue = raw.queue;
  if (raw.history && typeof raw.history === 'object') {
    this.frame.history = Array.isArray(raw.history.records) ? raw.history.records : [];
  }
  this.notify('partial');
};

function normalizeSession(state, fallback) {
  var session = {};
  var source = state && typeof state === 'object' ? state : {};
  Object.keys(source).forEach(function (key) { session[key] = source[key]; });
  if (!session.current_snapshot && fallback) session.current_snapshot = fallback.current_snapshot || null;
  if (!session.review_base && fallback) session.review_base = fallback.review_base || null;
  if (!session.staged_snapshot && fallback) session.staged_snapshot = fallback.staged_snapshot || null;
  if (!session.writer || typeof session.writer !== 'object') {
    session.writer = { state: 'idle', batch_id: null };
  }
  if (!('current_matches_filesystem' in session)) session.current_matches_filesystem = true;
  return session;
}

ReviewStore.prototype.refreshSoon = function () {
  var store = this;
  if (this.refreshPending) return;
  this.refreshPending = true;
  setTimeout(function () {
    store.refreshPending = false;
    store.refresh();
  }, 0);
};

ReviewStore.prototype.refresh = function () {
  var store = this;
  return this.client.getFrame().then(function (raw) {
    store.adoptFrame(raw);
  }).catch(function (error) {
    store.setStatus('error', friendlyError(error, '刷新失败'));
  });
};

ReviewStore.prototype.poll = function () {
  var store = this;
  /* While a history snapshot is being reviewed the page is read-only and
     the frame is served with ?history=<id>; polling the current frame
     would yank the reader back to the live version. */
  if (this.polling || !this.frame || this.historyTarget) return Promise.resolve();
  this.polling = true;
  return this.client.getFrame().then(function (raw) {
    store.mergeCandidate(raw);
  }).catch(function () {
    if (!store.frame) store.setStatus('error', '连接失败');
  }).then(function () {
    store.polling = false;
  }, function () {
    store.polling = false;
  });
};

/* One frame-dependent mutation. `payload` gets the CAS coordinates of the
   current frame attached (expected_generation + the optional manifest
   hash). A stale-review-frame failure is NEVER retried: the frame
   refreshes (targets preserved) and the caller must re-confirm. The
   Idempotency-Key is generated once per logical mutation and reused on
   transient network retries only. */
ReviewStore.prototype.mutate = function (path, payload) {
  var store = this;
  if (this.historyTarget) {
    var readonlyError = new Error('历史版本为只读快照');
    readonlyError.code = 'history-readonly';
    return Promise.reject(readonlyError);
  }
  if (this.frame && this.frame.backed === false) {
    var backedError = new Error('该工作目录不是原子审阅工作区');
    backedError.code = 'backed-not-atomic';
    return Promise.reject(backedError);
  }
  if (this.status === 'writer-busy' || this.status === 'conflict') {
    var blockedError = new Error('当前有写入正在进行或文档已变化');
    blockedError.code = this.status === 'writer-busy' ? 'writer-busy' : 'current-snapshot-drift';
    return Promise.reject(blockedError);
  }
  if (!this.generation || !this.generationManifestSha256) {
    var notReady = new Error('审阅帧尚未就绪');
    notReady.code = 'frame-unavailable';
    return Promise.reject(notReady);
  }
  var body = Object.assign({}, payload, {
    expected_generation: this.generation,
    expected_generation_manifest_sha256: this.generationManifestSha256,
  });
  var key = this.client.newKey();
  return postWithRetry(this.client, path, body, key).catch(function (error) {
    if (isStaleCode(error.code)) {
      store.requiresConfirmation = true;
      return store.refresh().then(function () {
        var stale = new Error('文档已在后台更新，请确认后重新提交');
        stale.code = 'stale-review-frame';
        throw stale;
      });
    }
    if (error.code === 'writer-busy' || error.code === 'writer-timeout') {
      store.setStatus('writer-busy', '另一个写入正在进行，请稍候，不要重复提交');
    }
    throw error;
  });
};

/* ------------------------------------------------------------ ReviewConsole */

function ReviewConsole() {
  this.client = new ReviewClient();
  this.store = new ReviewStore(this.client);
  this.refs = {};
  this.tab = 'revisions';
  this.filter = 'all';
  this.view = 'markup';
  this.currentRid = null;
  this.currentCid = null;
  this.action = null;
  this.decisions = {};
  this.pendingSelection = null;
  this.dismissedComposer = null;
  this.queueEvents = [];
  this.lastFocus = null;
  this.pollTimer = 0;
  this.selectionTimer = 0;
  this.selectionCaptureTimer = 0;
  this.revisions = new Map();
  this.comments = new Map();
  this._collectRefs();
  this._bind();
  this.store.subscribe(this._onStoreChange.bind(this));
  this.store.hydrate();
  var self = this;
  this.pollTimer = setInterval(function () { self.store.poll(); }, POLL_MS);
}

ReviewConsole.prototype._collectRefs = function () {
  var ids = [
    'topbar', 'server-status', 'send-agent', 'export', 'history-picker',
    'history-select', 'state-banner', 'state-banner-label', 'state-banner-text',
    'workflow-strip', 'workflow-kicker', 'workflow-title', 'workflow-description',
    'document-title', 'global-status', 'stage-title', 'stage-summary',
    'document-paper', 'staged-patches', 'staged-patch-items',
    'format-diagnostics', 'diagnostic-status', 'audit-body', 'diagnostic-entries',
    'revision-list', 'comment-list', 'word-comment-items', 'agent-comment-items',
    'revision-count', 'comment-count', 'revision-filter',
    'decided-count', 'rail-progress', 'review-rail',
    'decision-panel', 'decision-empty', 'decision-content', 'decision-kicker',
    'decision-title', 'decision-quote', 'decision-meta', 'decision-note',
    'decision-error', 'decision-apply',
    'comment-compose', 'comment-quote', 'comment-meta', 'comment-note',
    'comment-error', 'comment-cancel', 'comment-save',
    'comment-detail', 'comment-detail-kicker', 'comment-detail-title',
    'comment-detail-quote', 'comment-detail-meta', 'comment-detail-text',
    'comment-detail-replies', 'comment-detail-note', 'comment-detail-error',
    'comment-detail-close', 'comment-detail-save',
    'adjust-compose', 'adjust-quote', 'adjust-meta', 'adjust-text',
    'adjust-error', 'adjust-cancel', 'adjust-save',
    'adjust-selection', 'comment-selection',
    'staged-patch-rail', 'staged-patch-rail-items',
    'selection-tools', 'selection-count', 'selection-highlight',
    'review-jump-controls', 'review-jump-status', 'previous-review', 'next-review',
  ];
  var self = this;
  ids.forEach(function (id) {
    var key = id.replace(/-([a-z])/g, function (_, letter) { return letter.toUpperCase(); });
    self.refs[key] = document.getElementById(id);
  });
  this.refs.docStyleHost = document.getElementById('doc-style-rules');
  this.refs.viewButtons = Array.prototype.slice.call(document.querySelectorAll('.view-button'));
  this.refs.railTabs = Array.prototype.slice.call(document.querySelectorAll('.rail-tab'));
  this.refs.filterButtons = Array.prototype.slice.call(document.querySelectorAll('.filter-button'));
  this.refs.actionButtons = Array.prototype.slice.call(document.querySelectorAll('.decision-action'));
  this.refs.workflowSteps = Array.prototype.slice.call(document.querySelectorAll('.workflow-step'));
};

ReviewConsole.prototype._bind = function () {
  var self = this;
  this.refs.viewButtons.forEach(function (button) {
    button.addEventListener('click', function () { self.setView(button.dataset.view); });
  });
  this.refs.railTabs.forEach(function (button) {
    button.addEventListener('click', function () { self.setTab(button.dataset.tab); });
  });
  this.refs.filterButtons.forEach(function (button) {
    button.addEventListener('click', function () {
      self.filter = button.dataset.filter;
      self.applyFilter();
    });
  });
  this.refs.historySelect.addEventListener('change', function () {
    self.openHistory(this.value);
  });
  this.refs.sendAgent.addEventListener('click', function () { self.dispatchToAgent(); });
  this.refs.export.addEventListener('click', function () { self.exportDecisions(); });
  this.refs.actionButtons.forEach(function (button) {
    button.addEventListener('click', function () {
      self.action = button.dataset.action;
      self.refs.actionButtons.forEach(function (other) {
        other.classList.toggle('is-selected', other === button);
      });
      setText(self.refs.decisionError, '');
    });
  });
  this.refs.decisionApply.addEventListener('click', function () { self.applyDecision(); });
  this.refs.commentSave.addEventListener('click', function () { self.saveComment(); });
  this.refs.commentCancel.addEventListener('click', function () { self.cancelComment(); });
  this.refs.commentDetailClose.addEventListener('click', function () { self.dismissReviewSurface(); });
  this.refs.commentDetailSave.addEventListener('click', function () { self.saveCommentReply(); });
  this.refs.adjustSave.addEventListener('click', function () { self.saveAdjustment(); });
  this.refs.adjustCancel.addEventListener('click', function () { self.cancelAdjustment(); });
  this.refs.previousReview.addEventListener('click', function () { self.jumpRevision(-1); });
  this.refs.nextReview.addEventListener('click', function () { self.jumpRevision(1); });
  this.refs.adjustSelection.addEventListener('click', function () { self.openAdjustmentComposer(); });
  this.refs.commentSelection.addEventListener('click', function () { self.openCommentComposer(); });
  this.refs.adjustSelection.addEventListener('mousedown', function (event) { event.preventDefault(); });
  this.refs.commentSelection.addEventListener('mousedown', function (event) { event.preventDefault(); });

  document.addEventListener('selectstart', function (event) {
    var target = event.target && (event.target.nodeType === 1 ? event.target : event.target.parentElement);
    if (target && !target.closest('.document-paper')) event.preventDefault();
  }, true);
  document.addEventListener('selectionchange', function () {
    window.clearTimeout(self.selectionCaptureTimer);
    self.selectionCaptureTimer = setTimeout(function () { self.captureSelection(); }, 32);
  });
  document.addEventListener('pointerdown', function (event) {
    var hasSurface = Boolean(self.currentRid || self.currentCid
      || !self.refs.commentCompose.hidden || !self.refs.adjustCompose.hidden
      || !self.refs.commentDetail.hidden || !self.refs.selectionTools.hidden);
    if (hasSurface && !self._isReviewSurfaceTarget(event.target)) self.dismissReviewSurface();
  }, true);
  document.addEventListener('keydown', function (event) {
    if (event.key !== 'Escape') return;
    var hasSurface = Boolean(self.currentRid || self.currentCid
      || !self.refs.commentCompose.hidden || !self.refs.adjustCompose.hidden
      || !self.refs.commentDetail.hidden || !self.refs.selectionTools.hidden);
    if (hasSurface) {
      event.preventDefault();
      self.dismissReviewSurface();
    }
  });
  window.addEventListener('resize', function () { self.syncTopbarHeight(); });
  window.addEventListener('scroll', function () { self.syncTopbarHeight(); }, { passive: true });

  this.syncTopbarHeight();
  if ('ResizeObserver' in window && this.refs.topbar) {
    new ResizeObserver(function () { self.syncTopbarHeight(); }).observe(this.refs.topbar);
  }
  this.setView('markup');
  this.setTab('revisions');
  this.setHistoryControls(true);
};

/* ------------------------------------------------------------- store events */

ReviewConsole.prototype._onStoreChange = function (reason) {
  if (reason === 'status') {
    this.renderStatus();
    this.updateQueueStatus();
    this.updateWorkflow();
    return;
  }
  if (reason === 'partial') {
    this.queueEvents = (this.store.frame.queue && this.store.frame.queue.events) || [];
    this.hydrateDecisions();
    this.renderRevisionRail();
    this.renderComments();
    this.renderStagedPatches();
    this.updateStats();
    this.updateQueueStatus();
    this.updateWorkflow();
    return;
  }
  var anchor = this.captureViewportAnchor();
  this.queueEvents = (this.store.frame.queue && this.store.frame.queue.events) || [];
  this.hydrateDecisions();
  this.revisions = this.frameRevisions();
  this.comments = this.frameComments();
  this.renderer().render(this.store.frame);
  this.renderRevisionRail();
  this.renderComments();
  this.renderStagedPatches();
  this.updateHistoryOptions();
  this.updateStats();
  this.renderStatus();
  this.updateQueueStatus();
  this.updateWorkflow();
  this.setHistoryControls(Boolean(this.store.historyTarget));
  this.restoreViewportAnchor(anchor);
  this.restoreReviewTarget();
};

ReviewConsole.prototype.renderer = function () {
  if (!this._renderer) {
    this._renderer = new DocumentRenderer(this.store, this.refs.documentPaper,
      this.refs.docStyleHost, this.refs.auditBody, this.refs.diagnosticStatus,
      this.refs.diagnosticEntries,
      {
        onRevisionClick: this._onRevisionMarkClick.bind(this),
        onCommentClick: this._onCommentAnchorClick.bind(this),
      });
  }
  return this._renderer;
};

ReviewConsole.prototype._onRevisionMarkClick = function (rid, fromKeyboard) {
  this.setTab('revisions');
  this.filter = 'all';
  this.applyFilter();
  this.lastFocus = document.activeElement;
  this.setCurrentRevision(rid, false);
  if (fromKeyboard) this.refs.decisionNote.focus({ preventScroll: true });
};

ReviewConsole.prototype._onCommentAnchorClick = function (cid) {
  this.setTab('comments');
  this.lastFocus = document.activeElement;
  this.setCurrentComment(cid, false);
};

/* -------------------------------------------------------------- frame data */

ReviewConsole.prototype.frameRevisions = function () {
  var map = new Map();
  var order = 1;
  var frame = this.store.frame;
  if (!frame) return map;
  Object.keys(frame.paragraphs).forEach(function (pid) {
    var paragraph = frame.paragraphs[pid];
    paragraph.revisions.forEach(function (revision) {
      var text = '';
      if (revision.start !== null && revision.end !== null) {
        text = scalarSlice(paragraph.text, revision.start, revision.end);
      } else {
        text = revision.text || '';
      }
      map.set(revision.rid, {
        rid: revision.rid,
        key: revision.key,
        kind: revision.kind,
        author: revision.author,
        date: revision.date,
        pid: pid,
        start: revision.start,
        end: revision.end,
        order: String(order),
        text: text,
      });
      order += 1;
    });
  });
  return map;
};

ReviewConsole.prototype.frameComments = function () {
  var map = new Map();
  var order = 1;
  var frame = this.store.frame;
  if (!frame) return map;
  Object.keys(frame.paragraphs).forEach(function (pid) {
    frame.paragraphs[pid].comments.forEach(function (comment) {
      map.set(comment.cid, {
        cid: comment.cid,
        author: comment.author,
        date: comment.date,
        text: comment.text,
        pid: pid,
        start: comment.start,
        end: comment.end,
        order: String(order),
        source: comment.source || 'word',
      });
      order += 1;
    });
  });
  return map;
};

ReviewConsole.prototype.agentCommentRecords = function () {
  return this.queueEvents
    .filter(function (event) {
      return event.type === 'comment' && event.event_id
        && !event.reply_to && event.kind !== 'comment-reply';
    })
    .sort(function (left, right) {
      return String(left.created_at || '').localeCompare(String(right.created_at || ''));
    })
    .map(function (event, index) {
      return {
        cid: 'event:' + event.event_id,
        author: event.author || event.client_id || '人工审阅',
        date: event.created_at || '',
        text: event.note || event.selected_text || '',
        selected_text: event.selected_text || '',
        pid: event.paragraph_id || '',
        order: String(index + 1),
        source: 'agent',
      };
    });
};

ReviewConsole.prototype.paragraphText = function (pid) {
  var frame = this.store.frame;
  if (!frame || !frame.paragraphs[pid]) return '';
  return frame.paragraphs[pid].text;
};

ReviewConsole.prototype.patchParentSnapshot = function () {
  var session = this.store.frame ? this.store.frame.session : {};
  var staged = session.staged_snapshot;
  if (staged && Array.isArray(staged.patch_ids) && staged.patch_ids.length) {
    return staged.id || '';
  }
  var current = session.current_snapshot;
  return (current && current.id) || '';
};

ReviewConsole.prototype.hydrateDecisions = function () {
  this.decisions = {};
  var self = this;
  this.queueEvents.forEach(function (event) {
    if (event.type !== 'decision') return;
    var rid = event.revision_id && self.revisions.has(event.revision_id) ? event.revision_id : null;
    if (!rid) {
      self.revisions.forEach(function (revision) {
        if (revision.key === event.revision_key) rid = revision.rid;
      });
    }
    if (rid) {
      self.decisions[rid] = {
        revision_key: event.revision_key,
        decision: event.decision,
        comment: event.comment || null,
      };
    }
  });
};

/* ------------------------------------------------------------- rail + stats */

ReviewConsole.prototype.setTab = function (tab) {
  this.tab = tab;
  this.refs.railTabs.forEach(function (button) {
    button.setAttribute('aria-selected', String(button.dataset.tab === tab));
  });
  this.refs.revisionList.hidden = tab !== 'revisions';
  this.refs.commentList.hidden = tab !== 'comments';
  this.refs.revisionFilter.hidden = tab !== 'revisions';
};

ReviewConsole.prototype.applyFilter = function () {
  var self = this;
  var items = this.refs.revisionList.querySelectorAll('.review-item');
  items.forEach(function (item) {
    item.hidden = self.filter !== 'all' && item.dataset.status !== self.filter;
  });
  this.refs.filterButtons.forEach(function (button) {
    button.setAttribute('aria-pressed', String(button.dataset.filter === self.filter));
  });
};

ReviewConsole.prototype.setView = function (view) {
  this.view = view;
  document.body.dataset.view = view;
  this.refs.viewButtons.forEach(function (button) {
    button.setAttribute('aria-pressed', String(button.dataset.view === view));
  });
};

ReviewConsole.prototype.renderRevisionRail = function () {
  var list = this.refs.revisionList;
  clearChildren(list);
  var self = this;
  var items = Array.from(this.revisions.values());
  if (!items.length) {
    list.appendChild(el('div', 'rail-empty', { text: '当前文档没有可审阅修订。' }));
  }
  items.forEach(function (item) {
    var button = el('button', 'review-item');
    button.type = 'button';
    button.dataset.rid = item.rid;
    var decided = Boolean(self.decisions[item.rid]);
    button.dataset.status = decided ? 'decided' : 'pending';
    if (self.currentRid === item.rid) {
      button.classList.add('is-active');
      button.setAttribute('aria-current', 'true');
    }
    if (decided) button.classList.add('is-decided');
    button.setAttribute('aria-label', kindLabel(item.kind) + '：' + clipText(item.text, 44));

    var index = el('span', 'review-index', { text: pad2(item.order) });
    var copy = el('span', 'review-item-copy');
    copy.appendChild(el('span', 'review-item-label', { text: kindLabel(item.kind) }));
    copy.appendChild(el('span', 'review-item-quote', { text: clipText(item.text) || '无文本修订' }));
    copy.appendChild(el('span', 'review-item-meta', {
      text: (itemMeta(item) || '未标注作者') + ' · 段落 ' + pidLabel(item.pid),
    }));
    button.appendChild(index);
    button.appendChild(copy);
    button.addEventListener('click', function () {
      self.lastFocus = button;
      self.setCurrentRevision(item.rid, true);
    });
    list.appendChild(button);
  });
  this.applyFilter();
};

ReviewConsole.prototype.renderComments = function () {
  var self = this;
  var wordItems = this.refs.wordCommentItems;
  var agentItems = this.refs.agentCommentItems;
  clearChildren(wordItems);
  clearChildren(agentItems);
  var wordList = Array.from(this.comments.values()).filter(function (item) { return item.source !== 'agent'; });
  var agentList = this.agentCommentRecords();
  var allComments = new Map(this.comments);
  agentList.forEach(function (record) { allComments.set(record.cid, record); });
  this.comments = allComments;

  if (!wordList.length) {
    wordItems.appendChild(el('div', 'rail-empty', { text: '当前文档没有批注。' }));
  }
  wordList.forEach(function (item) {
    wordItems.appendChild(self.buildCommentItem(item));
  });
  if (!agentList.length) {
    agentItems.appendChild(el('div', 'rail-empty', { text: '尚无新增批注' }));
  }
  agentList.forEach(function (item) {
    agentItems.appendChild(self.buildCommentItem(item));
  });
  setText(this.refs.commentCount, String(this.comments.size));
  setText(this.refs.revisionCount, String(this.revisions.size));
};

ReviewConsole.prototype.buildCommentItem = function (item) {
  var self = this;
  var button = el('button', 'comment-item');
  button.type = 'button';
  button.dataset.cid = item.cid;
  if (this.currentCid === item.cid) {
    button.classList.add('is-active');
    button.setAttribute('aria-current', 'true');
  }
  button.setAttribute('aria-label', '批注 ' + item.order + '：' + clipText(item.text, 40));
  var index = el('span', 'review-index', { text: pad2(item.order) });
  var copy = el('span', 'review-item-copy');
  copy.appendChild(el('span', 'review-item-label', { text: item.source === 'agent' ? '审阅批注' : '批注' }));
  copy.appendChild(el('span', 'review-item-quote', { text: clipText(item.text) || '空批注' }));
  copy.appendChild(el('span', 'review-item-meta', {
    text: (itemMeta(item) || '未标注作者') + (item.pid ? ' · 段落 ' + pidLabel(item.pid) : ''),
  }));
  button.appendChild(index);
  button.appendChild(copy);
  button.addEventListener('click', function () {
    self.lastFocus = button;
    self.setCurrentComment(item.cid, true);
  });
  return button;
};

ReviewConsole.prototype.updateStats = function () {
  if (!this.store.frame) return;
  var decided = Object.keys(this.decisions).length;
  var total = this.revisions.size;
  setText(this.refs.decidedCount, String(decided).padStart(2, '0'));
  setText(this.refs.railProgress, decided + ' / ' + total + ' 已决策');
  var paragraphs = 0;
  var tables = 0;
  this.store.frame.blocks.forEach(function (block) {
    if (block.kind === 'table') tables += 1;
    else paragraphs += 1;
  });
  var comments = this.comments.size;
  setText(this.refs.globalStatus,
    paragraphs + ' 段 · ' + tables + ' 张表 · ' + total + ' 处修订 · ' + comments + ' 条批注');
  this.updateStageSummary(paragraphs, tables, total, comments);
};

ReviewConsole.prototype.updateStageSummary = function (paragraphs, tables, revisions, comments) {
  setText(this.refs.stageSummary,
    '共 ' + paragraphs + ' 段 · ' + tables + ' 张表 · ' + revisions + ' 处修订 · ' + comments + ' 条批注；字体、字号、上下标、修订层级与批注位置按源文档保留。');
};

/* ---------------------------------------------------------------- decisions */

ReviewConsole.prototype.setCurrentRevision = function (rid, shouldScroll) {
  var item = this.revisions.get(rid);
  if (!item) return;
  this.currentRid = rid;
  this.currentCid = null;
  this.refs.commentCompose.hidden = true;
  this.refs.adjustCompose.hidden = true;
  this.refs.commentDetail.hidden = true;

  document.querySelectorAll('.revision-mark').forEach(function (mark) {
    mark.classList.toggle('is-active', mark.dataset.rid === rid);
  });
  document.querySelectorAll('.review-item').forEach(function (itemEl) {
    var active = itemEl.dataset.rid === rid;
    itemEl.classList.toggle('is-active', active);
    if (active) itemEl.setAttribute('aria-current', 'true');
    else itemEl.removeAttribute('aria-current');
  });

  this.refs.decisionEmpty.hidden = true;
  this.refs.decisionContent.hidden = false;
  setText(this.refs.decisionKicker, pad2(item.order) + ' / ' + kindLabel(item.kind));
  setText(this.refs.decisionTitle, item.text || '无文本修订');
  setText(this.refs.decisionQuote, '“' + (item.text || '无文本修订') + '”');
  setText(this.refs.decisionMeta, itemMeta(item) + ' · 段落位置 ' + pidLabel(item.pid));
  this.refs.decisionNote.value = (this.decisions[rid] && this.decisions[rid].comment) || '';
  setText(this.refs.decisionError, '');
  this.action = (this.decisions[rid] && this.decisions[rid].decision) || null;
  this.updateActionButtons();
  this.updateApplyButton();
  this.updateJumpControls();
  if (this.isMobileViewport()) this.setMobileSheet('decision');
  if (shouldScroll) {
    this.scrollToElement(this.activeRevisionElement(rid), item.pid);
  }
};

ReviewConsole.prototype.setCurrentComment = function (cid, shouldScroll) {
  var item = this.comments.get(cid);
  if (!item) return;
  this.currentCid = cid;
  this.currentRid = null;
  this.refs.commentCompose.hidden = true;
  this.refs.adjustCompose.hidden = true;
  this.refs.decisionContent.hidden = true;
  this.refs.decisionEmpty.hidden = true;
  this.refs.commentDetail.hidden = false;

  setText(this.refs.commentDetailKicker, item.source === 'agent' ? 'AGENT NOTE' : 'WORD COMMENT');
  setText(this.refs.commentDetailTitle, item.source === 'agent' ? '新增审阅批注' : '原文批注');
  setText(this.refs.commentDetailQuote, item.source === 'agent' ? 'Agent 追加 · 待处理' : '原始 Word · 只读');
  setText(this.refs.commentDetailMeta, itemMeta(item) + ' · P' + pidLabel(item.pid));
  setText(this.refs.commentDetailText, item.text || '（空批注）');
  this.refs.commentDetailNote.value = '';
  setText(this.refs.commentDetailError, '');
  this.renderCommentReplies(item);
  this.updateJumpControls();

  document.querySelectorAll('.comment-item').forEach(function (itemEl) {
    itemEl.classList.toggle('is-active', itemEl.dataset.cid === cid);
  });
  document.querySelectorAll('.comment-anchor').forEach(function (anchor) {
    anchor.classList.toggle('is-active', anchor.dataset.cid === cid);
  });
  if (this.isMobileViewport()) this.setMobileSheet('comment-detail');
  if (shouldScroll) {
    var target = this.activeCommentAnchor(cid);
    this.scrollToElement(target || null, item.pid);
  }
};

ReviewConsole.prototype.updateActionButtons = function () {
  this.refs.actionButtons.forEach(function (button) {
    button.classList.toggle('is-selected', button.dataset.action === this.action);
  }, this);
};

ReviewConsole.prototype.updateApplyButton = function () {
  var needsConfirm = this.store.requiresConfirmation;
  setText(this.refs.decisionApply, needsConfirm ? '重新确认并保存' : '保存本项决策');
};

ReviewConsole.prototype.activeRevisionElement = function (rid) {
  var result = null;
  document.querySelectorAll('.revision-mark').forEach(function (mark) {
    if (mark.dataset.rid === rid) result = mark;
  });
  return result;
};

ReviewConsole.prototype.activeCommentAnchor = function (cid) {
  var result = null;
  document.querySelectorAll('.comment-anchor').forEach(function (anchor) {
    if (anchor.dataset.cid === cid) result = anchor;
  });
  return result;
};

ReviewConsole.prototype.scrollToElement = function (element, pid) {
  var target = element;
  if (!target && pid) {
    document.querySelectorAll('.document-paragraph').forEach(function (paragraph) {
      if (paragraph.dataset.pid === pid) target = paragraph;
    });
  }
  if (!target) return;
  var reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  target.scrollIntoView({ behavior: reduced ? 'auto' : 'smooth', block: 'center' });
};

ReviewConsole.prototype.renderCommentReplies = function (item) {
  var self = this;
  var replies = [];
  if (item) {
    replies = this.queueEvents.filter(function (event) {
      return event.type === 'comment' && event.event_id
        && String(event.reply_to || event.comment_id || '') === String(item.cid);
    }).sort(function (left, right) {
      return String(left.created_at || '').localeCompare(String(right.created_at || ''));
    });
  }
  var container = this.refs.commentDetailReplies;
  clearChildren(container);
  container.hidden = replies.length === 0;
  replies.forEach(function (reply) {
    var line = el('p', 'comment-detail-reply');
    line.appendChild(el('span', 'comment-detail-reply-meta', {
      text: '给 agent · ' + itemMeta({ author: reply.author || '人工审阅', date: reply.created_at || '' }),
    }));
    line.appendChild(document.createTextNode(reply.note || ''));
    container.appendChild(line);
  });
};

ReviewConsole.prototype.updateJumpControls = function () {
  var sequence = Array.from(this.revisions.values());
  this.refs.reviewJumpControls.hidden = sequence.length === 0;
  var index = this.currentRid ? sequence.findIndex(function (item) { return item.rid === this.currentRid; }, this) : -1;
  setText(this.refs.reviewJumpStatus, (index >= 0 ? index + 1 : 0) + ' / ' + sequence.length);
  this.refs.previousReview.disabled = index <= 0;
  this.refs.nextReview.disabled = index >= sequence.length - 1;
};

ReviewConsole.prototype.jumpRevision = function (offset) {
  var sequence = Array.from(this.revisions.values());
  if (!sequence.length) return;
  var index = this.currentRid ? sequence.findIndex(function (item) { return item.rid === this.currentRid; }, this) : -1;
  var targetIndex = Math.min(sequence.length - 1, Math.max(0, (index >= 0 ? index : offset > 0 ? -1 : 0) + offset));
  var target = sequence[targetIndex];
  this.setTab('revisions');
  this.filter = 'all';
  this.applyFilter();
  this.setCurrentRevision(target.rid, true);
};

ReviewConsole.prototype.clearReviewSelection = function () {
  this.currentRid = null;
  this.currentCid = null;
  this.action = null;
  this.refs.commentDetail.hidden = true;
  this.refs.decisionContent.hidden = true;
  this.refs.decisionEmpty.hidden = false;
  setText(this.refs.decisionEmpty, '从右侧索引选择一条修订，正文会自动定位到对应句子。');
  document.querySelectorAll('.revision-mark, .review-item, .comment-item, .comment-anchor').forEach(function (element) {
    element.classList.remove('is-active');
    element.removeAttribute('aria-current');
  });
  this.updateJumpControls();
};

/* ------------------------------------------------------------------ actions */

ReviewStore.prototype.mergePostResponse = function (data) {
  if (!data || typeof data !== 'object') return this.refresh();
  if (data.schema === FRAME_SCHEMA) {
    this.adoptFrame(data);
    return data;
  }
  /* A committed mutation advances the generation; the partial payload no
     longer matches the current frame, so a full frame reload hydrates the
     new generation. */
  if (data.committed_generation || data.session || data.counts) {
    this.mergeCandidate(data);
    return data;
  }
  return this.refresh().then(function () { return data; });
};

ReviewConsole.prototype.applyDecision = function () {
  var self = this;
  if (!this.currentRid) return;
  var item = this.revisions.get(this.currentRid);
  var note = this.refs.decisionNote.value.trim();
  var decision = this.action || (note ? 'comment' : null);
  if (!decision) {
    setText(this.refs.decisionError, '请选择接受、拒绝或暂缓，或先留下意见。');
    return;
  }
  return this.store.mutate('/api/reviews', {
    type: 'decision',
    client_id: 'decision:' + item.rid,
    revision_id: item.rid,
    revision_key: item.key,
    paragraph_id: item.pid,
    selected_text: item.text,
    decision: decision,
    comment: note || '',
  }).then(function (data) {
    self.store.requiresConfirmation = false;
    self.updateApplyButton();
    self.store.mergePostResponse(data);
    setText(self.refs.decisionError, '已暂存 · 点击“发送给 agent”后回传');
  }).catch(function (error) {
    self.restoreReviewTarget();
    setText(self.refs.decisionError, friendlyError(error, '保存失败'));
  });
};

ReviewConsole.prototype.dispatchToAgent = function () {
  var self = this;
  var drafts = this.queueEvents.filter(function (event) { return event.status === 'draft'; });
  if (!drafts.length) return;
  this.store.mutate('/api/reviews/dispatch', {}).then(function (data) {
    self.store.mergePostResponse(data);
    var count = data && Array.isArray(data.events) ? data.events.length : 0;
    self.updateQueueStatus('已发送 ' + count + ' 条 · 等待 agent 读取');
  }).catch(function (error) {
    self.updateQueueStatus(friendlyError(error, '发送失败'));
  });
};

ReviewConsole.prototype.exportDecisions = function () {
  var payload = { schema: 'docx2typed-review-decisions-1', decisions: Object.values(this.decisions) };
  var blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' });
  var link = document.createElement('a');
  link.href = URL.createObjectURL(blob);
  link.download = 'review-decisions.json';
  link.click();
  setTimeout(function () { URL.revokeObjectURL(link.href); }, 1000);
};

/* ------------------------------------------------------------ staged patches */

ReviewConsole.prototype.stagedPatchEvents = function () {
  return this.queueEvents
    .filter(function (event) {
      return event.type === 'patch' && event.event_id
        && ['applied', 'acknowledged'].indexOf(event.delivery_state) < 0
        && event.status !== 'applied';
    })
    .sort(function (left, right) {
      return String(left.created_at || '').localeCompare(String(right.created_at || ''));
    });
};

ReviewConsole.prototype.renderStagedPatches = function () {
  var records = this.store.historyTarget ? [] : this.stagedPatchEvents();
  var self = this;
  clearChildren(this.refs.stagedPatchItems);
  clearChildren(this.refs.stagedPatchRailItems);
  this.refs.stagedPatches.hidden = records.length === 0;
  this.refs.stagedPatchRail.hidden = records.length === 0;
  records.forEach(function (record) {
    self.refs.stagedPatchItems.appendChild(self.buildPatchCard(record, false));
    self.refs.stagedPatchRailItems.appendChild(self.buildPatchCard(record, true));
  });
};

ReviewConsole.prototype.buildPatchCard = function (record, interactive) {
  var card = el(interactive ? 'button' : 'article', 'staged-patch-card' + (interactive ? ' staged-patch-card--button' : ''));
  card.dataset.pid = record.paragraph_id || '';
  var delivery = record.status === 'queued' ? '已发送 · 等待 agent 读取' : '草稿 · 尚未发送';
  card.appendChild(el('span', 'staged-patch-label', { text: '正文调整 · ' + delivery }));
  card.appendChild(el('p', 'staged-patch-meta', {
    text: (record.paragraph_id || '未知段落') + ' · ' + itemMeta({ author: record.author || '人工审阅', date: record.created_at || '' }),
  }));
  var diff = el('p', 'staged-patch-diff');
  diff.appendChild(el('span', 'staged-patch-before', { text: '− ' + (record.before || '') }));
  diff.appendChild(el('span', 'staged-patch-after', { text: '＋ ' + (record.after || '') }));
  card.appendChild(diff);
  if (interactive) {
    var self = this;
    card.type = 'button';
    card.addEventListener('click', function () {
      var target = null;
      document.querySelectorAll('.document-paragraph').forEach(function (paragraph) {
        if (paragraph.dataset.pid === card.dataset.pid) target = paragraph;
      });
      if (!target) return;
      document.querySelectorAll('.document-paragraph.is-draft-target').forEach(function (paragraph) {
        paragraph.classList.remove('is-draft-target');
      });
      target.classList.add('is-draft-target');
      self.scrollToElement(target, null);
      setTimeout(function () { target.classList.remove('is-draft-target'); }, 900);
    });
  }
  return card;
};

/* ------------------------------------------------------------ selection tools */

ReviewConsole.prototype.paragraphAtPoint = function (node) {
  if (!node) return null;
  var holder = node.nodeType === 1 ? node : node.parentElement;
  return holder ? holder.closest('.document-paragraph') : null;
};

ReviewConsole.prototype.segAtPoint = function (node) {
  if (!node) return null;
  if (node.nodeType === Node.TEXT_NODE) {
    return node.parentElement ? node.parentElement.closest('.doc-seg') : null;
  }
  if (node.nodeType === Node.ELEMENT_NODE) return node.closest('.doc-seg');
  return null;
};

/* Unicode scalar offset of a boundary point inside a paragraph, derived only
   from region data-start/data-end and counted prefixes — never from
   selection text or full-text search. */
ReviewConsole.prototype.scalarOffsetAt = function (paragraph, node, offset) {
  var seg = this.segAtPoint(node);
  if (seg && paragraph.contains(seg)) {
    try {
      var range = document.createRange();
      range.setStart(seg, 0);
      range.setEnd(node, offset);
      return Number(seg.dataset.start) + scalarLength(range.toString());
    } catch (_) { /* boundary outside the region */ }
  }
  var cursor = 0;
  var segs = paragraph.querySelectorAll('.doc-seg');
  for (var i = 0; i < segs.length; i += 1) {
    if (this.pointBefore(node, offset, segs[i])) return cursor;
    cursor = Number(segs[i].dataset.end);
  }
  return cursor;
};

ReviewConsole.prototype.pointBefore = function (node, offset, candidate) {
  if (node.nodeType === Node.TEXT_NODE) {
    var holder = node.parentElement;
    if (!holder) return false;
    return (holder.compareDocumentPosition(candidate) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
  }
  if (node.nodeType === Node.ELEMENT_NODE) {
    if (node === candidate) return offset <= 0;
    if (node.contains(candidate)) {
      var index = 0;
      var children = node.childNodes;
      for (var i = 0; i < children.length; i += 1) {
        var child = children[i];
        if (child === candidate || (child.contains && child.contains(candidate))) {
          return offset <= index;
        }
        index += 1;
      }
      return false;
    }
    return (node.compareDocumentPosition(candidate) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
  }
  return false;
};

ReviewConsole.prototype.captureSelection = function () {
  var self = this;
  if (this.store.historyTarget) { this.deferClearSelection(true); return; }
  /* A compose panel is anchored to the pending selection; focusing its
     textarea collapses the document selection, so new selections are
     ignored until the panel closes. */
  if (!this.refs.commentCompose.hidden || !this.refs.adjustCompose.hidden) return;
  var selection = window.getSelection();
  if (!selection || selection.isCollapsed) { this.deferClearSelection(); return; }
  var paper = this.refs.documentPaper;
  if (!paper || !paper.contains(selection.anchorNode) || !paper.contains(selection.focusNode)) {
    this.deferClearSelection(true);
    return;
  }
  var anchorParagraph = this.paragraphAtPoint(selection.anchorNode);
  var focusParagraph = this.paragraphAtPoint(selection.focusNode);
  if (!anchorParagraph || anchorParagraph !== focusParagraph) {
    this.deferClearSelection(true);
    return;
  }
  var pid = anchorParagraph.dataset.pid || '';
  var start = this.scalarOffsetAt(anchorParagraph, selection.anchorNode, selection.anchorOffset);
  var end = this.scalarOffsetAt(anchorParagraph, selection.focusNode, selection.focusOffset);
  var low = Math.min(start, end);
  var high = Math.max(start, end);
  if (high <= low) { this.deferClearSelection(true); return; }
  var paragraphText = this.paragraphText(pid);
  var text = scalarSlice(paragraphText, low, high);
  if (!text) { this.deferClearSelection(true); return; }
  window.clearTimeout(this.selectionTimer);
  var range = selection.getRangeAt(0);
  var rects = Array.prototype.slice.call(range.getClientRects());
  var lastRect = rects.length ? rects[rects.length - 1] : range.getBoundingClientRect();
  this.pendingSelection = {
    range: range.cloneRange(),
    anchorRect: { left: lastRect.left, top: lastRect.top, right: lastRect.right, bottom: lastRect.bottom },
    text: text,
    pid: pid,
    start: low,
    end: high,
    beforeContext: scalarSlice(paragraphText, Math.max(0, low - 100), low),
    afterContext: scalarSlice(paragraphText, high, high + 100),
  };
  setText(this.refs.selectionCount, scalarLength(text) + ' 字');
  var rect = range.getBoundingClientRect();
  var tools = this.refs.selectionTools;
  tools.style.left = Math.max(12, Math.min(window.innerWidth - tools.offsetWidth - 12, rect.left + rect.width / 2 - 80)) + 'px';
  tools.style.top = Math.max(12, Math.min(window.innerHeight - tools.offsetHeight - 12, rect.top - 48)) + 'px';
  tools.hidden = false;
  this.renderSelectionHighlight();
  if (this.isMobileViewport()) {
    requestAnimationFrame(function () { self.positionMobileComposer(tools); });
  }
};

ReviewConsole.prototype.deferClearSelection = function (force) {
  var self = this;
  window.clearTimeout(this.selectionTimer);
  this.selectionTimer = setTimeout(function () {
    var current = window.getSelection();
    if (force || !current || current.isCollapsed) self.clearSelectionSurface();
  }, 120);
};

ReviewConsole.prototype.clearSelectionSurface = function () {
  /* While a compose panel is open its target selection must survive the
     collapse caused by focusing the textarea. */
  if (this.refs.commentCompose.hidden && this.refs.adjustCompose.hidden) {
    this.pendingSelection = null;
  }
  this.refs.selectionTools.hidden = true;
  if (this.refs.commentCompose.hidden && this.refs.adjustCompose.hidden) this.clearSelectionHighlight();
};

ReviewConsole.prototype.clearSelectionHighlight = function () {
  var host = this.refs.selectionHighlight;
  host.hidden = true;
  clearChildren(host);
};

ReviewConsole.prototype.renderSelectionHighlight = function () {
  var range = this.pendingSelection && this.pendingSelection.range;
  var host = this.refs.selectionHighlight;
  if (!range) { this.clearSelectionHighlight(); return; }
  clearChildren(host);
  var rects = Array.prototype.slice.call(range.getClientRects()).filter(function (rect) {
    return rect.width > 0 && rect.height > 0;
  });
  rects.forEach(function (rect) {
    var box = el('span', 'selection-highlight-box');
    box.style.left = (rect.left - 2) + 'px';
    box.style.top = (rect.top - 2) + 'px';
    box.style.width = (rect.width + 4) + 'px';
    box.style.height = (rect.height + 4) + 'px';
    host.appendChild(box);
  });
  host.hidden = rects.length === 0;
};

ReviewConsole.prototype.openAdjustmentComposer = function () {
  var item = this.pendingSelection;
  if (!item) return;
  var dismissed = this.dismissedComposer && this.dismissedComposer.type === 'adjust'
    && this.samePendingSelection(this.dismissedComposer.selection, item)
    ? this.dismissedComposer.value : item.text;
  this.refs.commentDetail.hidden = true;
  this.dismissedComposer = null;
  this.setTab('revisions');
  if (this.isMobileViewport()) this.setMobileSheet('adjust');
  this.refs.decisionEmpty.hidden = true;
  this.refs.decisionContent.hidden = true;
  this.refs.commentCompose.hidden = true;
  this.refs.adjustCompose.hidden = false;
  setText(this.refs.adjustQuote, '“' + item.text + '”');
  setText(this.refs.adjustMeta, '段落位置 ' + pidLabel(item.pid) + ' · 生成一条带前置条件的文本 patch');
  this.refs.adjustText.value = dismissed;
  setText(this.refs.adjustError, '');
  this.refs.selectionTools.hidden = true;
  this.renderSelectionHighlight();
  this.positionMobileComposer(this.refs.adjustCompose);
  this.refs.adjustText.focus();
};

ReviewConsole.prototype.saveAdjustment = function () {
  var self = this;
  var item = this.pendingSelection;
  var after = this.refs.adjustText.value;
  if (!item) return;
  if (!item.pid) {
    setText(this.refs.adjustError, '无法确定正文锚点，请重新选择文本。');
    return;
  }
  if (after === item.text) {
    setText(this.refs.adjustError, '调整后的文本不能与原文相同。');
    return;
  }
  this.store.mutate('/api/reviews', {
    type: 'patch',
    client_id: 'patch:' + newEventId(),
    review_item_id: 'patch:' + item.pid + ':' + newEventId(),
    origin: 'human_ui',
    author: 'human_ui',
    paragraph_id: item.pid,
    kind: 'replace',
    parent_snapshot: this.patchParentSnapshot(),
    target: {
      start_offset: item.start,
      end_offset: item.end,
      expected_text: item.text,
      left_context: item.beforeContext,
      right_context: item.afterContext,
    },
    before: item.text,
    after: after,
  }).then(function (data) {
    self.store.mergePostResponse(data);
    setText(self.refs.adjustError, 'patch 已暂存 · 点击“发送给 agent”后回传');
    self.pendingSelection = null;
    self.dismissedComposer = null;
    self.clearSelectionHighlight();
    self.refs.adjustText.value = '';
  }).catch(function (error) {
    if (error.code === 'stale-review-frame') self.restoreReviewTarget();
    setText(self.refs.adjustError, friendlyError(error, '保存失败'));
  });
};

ReviewConsole.prototype.cancelAdjustment = function () {
  this.pendingSelection = null;
  this.dismissedComposer = null;
  this.clearSelectionHighlight();
  this.refs.adjustCompose.hidden = true;
  if (this.isMobileViewport()) this.setMobileSheet(null);
  if (!this.currentRid) this.refs.decisionEmpty.hidden = false;
  this.returnFocus();
};

ReviewConsole.prototype.openCommentComposer = function () {
  var item = this.pendingSelection;
  if (!item) return;
  var dismissed = this.dismissedComposer && this.dismissedComposer.type === 'comment'
    && this.samePendingSelection(this.dismissedComposer.selection, item)
    ? this.dismissedComposer.value : '';
  this.dismissedComposer = null;
  this.refs.commentDetail.hidden = true;
  this.setTab('comments');
  if (this.isMobileViewport()) this.setMobileSheet('comment');
  this.refs.decisionEmpty.hidden = true;
  this.refs.decisionContent.hidden = true;
  this.refs.commentCompose.hidden = false;
  this.refs.adjustCompose.hidden = true;
  setText(this.refs.commentQuote, '“' + item.text + '”');
  setText(this.refs.commentMeta, '段落位置 ' + pidLabel(item.pid) + ' · 这是一条发给 agent 的新批注');
  this.refs.commentNote.value = dismissed;
  setText(this.refs.commentError, '');
  this.refs.selectionTools.hidden = true;
  this.renderSelectionHighlight();
  this.positionMobileComposer(this.refs.commentCompose);
  this.refs.commentNote.focus({ preventScroll: true });
};

ReviewConsole.prototype.saveComment = function () {
  var self = this;
  var item = this.pendingSelection;
  var note = this.refs.commentNote.value.trim();
  if (!item) return;
  if (!note) {
    setText(this.refs.commentError, '请先写下批注内容。');
    return;
  }
  this.store.mutate('/api/reviews', {
    type: 'comment',
    client_id: 'comment:' + newEventId(),
    paragraph_id: item.pid,
    selected_text: item.text,
    before_context: item.beforeContext,
    after_context: item.afterContext,
    note: note,
  }).then(function (data) {
    self.store.mergePostResponse(data);
    setText(self.refs.commentError, '已暂存 · 点击“发送给 agent”后回传');
    self.pendingSelection = null;
    self.dismissedComposer = null;
    self.clearSelectionHighlight();
    self.refs.commentNote.value = '';
  }).catch(function (error) {
    if (error.code === 'stale-review-frame') self.restoreReviewTarget();
    setText(self.refs.commentError, friendlyError(error, '保存失败'));
  });
};

ReviewConsole.prototype.cancelComment = function () {
  this.pendingSelection = null;
  this.dismissedComposer = null;
  this.clearSelectionHighlight();
  this.refs.commentCompose.hidden = true;
  if (this.isMobileViewport()) this.setMobileSheet(null);
  if (!this.currentRid) this.refs.decisionEmpty.hidden = false;
  this.refs.commentDetail.hidden = true;
  this.returnFocus();
};

ReviewConsole.prototype.saveCommentReply = function () {
  var self = this;
  var cid = this.currentCid;
  var item = cid ? this.comments.get(cid) : null;
  var note = this.refs.commentDetailNote.value.trim();
  if (!item) return;
  if (!note) {
    setText(this.refs.commentDetailError, '请先写下要告诉 agent 的处理意见。');
    return;
  }
  var eventId = newEventId();
  this.store.mutate('/api/reviews', {
    type: 'comment',
    client_id: 'comment-reply:' + eventId,
    review_item_id: 'comment-reply:' + cid + ':' + eventId,
    origin: 'human_ui',
    author: 'human_ui',
    kind: 'comment-reply',
    reply_to: cid,
    comment_id: cid,
    source_comment: item.text || '',
    source_comment_author: item.author || '',
    paragraph_id: item.pid || '',
    selected_text: item.text || '原文批注',
    before_context: item.before_context || '',
    after_context: item.after_context || '',
    note: note,
  }).then(function (data) {
    self.store.mergePostResponse(data);
    self.refs.commentDetailNote.value = '';
    self.renderCommentReplies(item);
    setText(self.refs.commentDetailError, '已暂存给 agent · 点击“发送给 agent”后回传');
  }).catch(function (error) {
    if (error.code === 'stale-review-frame') self.restoreReviewTarget();
    setText(self.refs.commentDetailError, friendlyError(error, '保存失败'));
  });
};

ReviewConsole.prototype.samePendingSelection = function (left, right) {
  return Boolean(left && right && left.pid === right.pid && left.start === right.start && left.end === right.end);
};

/* -------------------------------------------------------------- dismissal */

ReviewConsole.prototype._isReviewSurfaceTarget = function (target) {
  return target instanceof Element && Boolean(target.closest('#review-rail, #selection-tools, #review-jump-controls'));
};

ReviewConsole.prototype.dismissReviewSurface = function () {
  var editor = null;
  if (!this.refs.commentCompose.hidden) {
    editor = { type: 'comment', selection: this.pendingSelection, value: this.refs.commentNote.value };
  } else if (!this.refs.adjustCompose.hidden) {
    editor = { type: 'adjust', selection: this.pendingSelection, value: this.refs.adjustText.value };
  }
  this.dismissedComposer = editor && editor.selection ? editor : null;
  if (!editor) this.pendingSelection = null;
  this.refs.commentCompose.hidden = true;
  this.refs.adjustCompose.hidden = true;
  this.refs.selectionTools.hidden = true;
  this.clearSelectionHighlight();
  this.clearReviewSelection();
  if (this.isMobileViewport()) this.setMobileSheet(null);
  this.returnFocus();
};

ReviewConsole.prototype.returnFocus = function () {
  var target = this.lastFocus;
  this.lastFocus = null;
  if (target && document.contains(target)) target.focus({ preventScroll: true });
};

/* ------------------------------------------------------------- mobile sheet */

ReviewConsole.prototype.isMobileViewport = function () {
  return window.matchMedia('(max-width: 600px)').matches;
};

ReviewConsole.prototype.setMobileSheet = function (sheet) {
  if (!this.refs.reviewRail || !this.isMobileViewport()) return;
  if (sheet) this.refs.reviewRail.dataset.mobileSheet = sheet;
  else delete this.refs.reviewRail.dataset.mobileSheet;
};

ReviewConsole.prototype.positionMobileComposer = function (element) {
  if (!this.isMobileViewport() || !element || !this.pendingSelection || !this.pendingSelection.anchorRect) return;
  var anchor = this.pendingSelection.anchorRect;
  var maxTop = Math.max(8, window.innerHeight - element.offsetHeight - 12);
  var top = Math.max(8, Math.min(maxTop, anchor.bottom + 8));
  element.style.setProperty('--mobile-compose-top', top + 'px');
};

/* ---------------------------------------------------------------- history */

ReviewConsole.prototype.updateHistoryOptions = function () {
  var frame = this.store.frame;
  var records = frame ? frame.history.filter(function (record) { return record && (record.history_id || record.id); }) : [];
  var picker = this.refs.historyPicker;
  var select = this.refs.historySelect;
  picker.hidden = records.length < 2;
  if (picker.hidden) return;
  clearChildren(select);
  var currentId = (frame.session.current_snapshot && frame.session.current_snapshot.id) || 'C0';
  select.appendChild(el('option', null, { value: '', text: '当前版本 · ' + currentId }));
  records.forEach(function (record) {
    var id = record.history_id || record.id;
    if (String(record.current_snapshot && record.current_snapshot.id) === String(currentId)) return;
    select.appendChild(el('option', null, { value: id, text: historyLabel(record) }));
  });
  select.value = this.store.historyTarget || '';
};

function historyOriginLabel(origin) {
  return {
    source: '源文档',
    agent: 'Agent 修改',
    human_ui: '人工调整',
    human_external: '外部导入',
    settlement: '审阅结算',
    'session-bootstrap': '初始基线',
  }[origin] || '版本快照';
}

function historyLabel(record) {
  var snapshotId = record && record.current_snapshot && record.current_snapshot.id;
  var id = String(record && (record.history_id || record.id)) || '';
  if (!id && snapshotId) return '基线 · ' + snapshotId;
  return (String(snapshotId || '') + ' · ' + historyOriginLabel(record && record.origin)) || id;
}

ReviewConsole.prototype.openHistory = function (id) {
  var self = this;
  if (this.store.historyTarget === id) return;
  this.lastFocus = this.refs.historySelect;
  if (!id) {
    return this.store.refresh().then(function () {
      self.updateQueueStatus('已回到当前版本 · 保留阅读位置');
    }).catch(function (error) {
      self.updateQueueStatus(friendlyError(error, '当前版本读取失败'));
    });
  }
  return this.client.getFrame(id).then(function (raw) {
    if (raw && raw.schema === FRAME_SCHEMA) {
      self.store.adoptFrame(raw);
      if (!self.store.historyTarget) {
        self.updateQueueStatus('历史版本读取失败 · 该版本没有可读快照');
      } else {
        self.updateQueueStatus('历史版本 ' + id + ' · 只读');
      }
      return;
    }
    var record = raw && (raw.document ? raw : (raw.records ? raw.records[0] : null));
    if (record && record.document) {
      self.store.adoptFrame({
        schema: FRAME_SCHEMA,
        identity: { generation: self.store.generation, generation_manifest_sha256: self.store.generationManifestSha256 },
        history_id: id,
        state: self.store.frame.session,
        review: { events: [] },
        history: self.store.frame.history,
        document: record.document,
      });
      self.updateQueueStatus('历史版本 ' + id + ' · 只读');
      return;
    }
    self.updateQueueStatus('历史版本尚未生成可读快照');
  }).catch(function (error) {
    self.updateQueueStatus(friendlyError(error, '历史版本读取失败'));
  });
};

/* ---------------------------------------------------------- read-only mode */

ReviewConsole.prototype.setHistoryControls = function (readOnly) {
  document.body.dataset.history = readOnly ? 'true' : 'false';
  var controls = [
    this.refs.decisionApply, this.refs.commentSave, this.refs.commentDetailSave,
    this.refs.adjustSave, this.refs.sendAgent, this.refs.export,
  ];
  controls.forEach(function (control) { if (control) control.disabled = readOnly; });
  this.refs.actionButtons.forEach(function (button) { button.disabled = readOnly; });
  [this.refs.decisionNote, this.refs.commentDetailNote, this.refs.commentNote, this.refs.adjustText]
    .forEach(function (field) { if (field) field.readOnly = readOnly; });
  if (readOnly) {
    this.pendingSelection = null;
    this.refs.selectionTools.hidden = true;
    this.refs.commentCompose.hidden = true;
    this.refs.adjustCompose.hidden = true;
    this.refs.commentDetail.hidden = true;
    this.refs.decisionContent.hidden = true;
    this.refs.decisionEmpty.hidden = false;
    this.clearSelectionHighlight();
  }
};

/* ------------------------------------------------------------ target restore */

ReviewConsole.prototype.captureViewportAnchor = function () {
  var top = this.refs.topbar ? this.refs.topbar.getBoundingClientRect().bottom : 0;
  var target = null;
  document.querySelectorAll('.document-paragraph').forEach(function (paragraph) {
    if (!target && paragraph.getBoundingClientRect().bottom >= top) target = paragraph;
  });
  if (!target) target = document.querySelector('.document-paragraph');
  if (!target) return null;
  return { pid: target.dataset.pid || '', offset: target.getBoundingClientRect().top - top };
};

ReviewConsole.prototype.restoreViewportAnchor = function (anchor) {
  if (!anchor || !anchor.pid) return;
  var target = null;
  document.querySelectorAll('.document-paragraph').forEach(function (paragraph) {
    if (paragraph.dataset.pid === anchor.pid) target = paragraph;
  });
  if (!target) return;
  var top = this.refs.topbar ? this.refs.topbar.getBoundingClientRect().bottom : 0;
  var currentOffset = target.getBoundingClientRect().top - top;
  window.scrollBy({ top: currentOffset - Number(anchor.offset || 0), behavior: 'auto' });
};

/* Keep the paragraph/review target across a frame refresh: re-select the
   revision/comment if it still exists, re-open the pending selection when
   the paragraph and offsets are still valid, and leave the decision draft
   (action + note) intact so the user can re-confirm. */
ReviewConsole.prototype.restoreReviewTarget = function () {
  if (this.currentRid) {
    if (this.revisions.has(this.currentRid)) {
      this.setCurrentRevision(this.currentRid, false);
      return;
    }
    this.currentRid = null;
  }
  if (this.currentCid) {
    if (this.comments.has(this.currentCid)) {
      this.setCurrentComment(this.currentCid, false);
      return;
    }
    this.currentCid = null;
  }
  if (this.pendingSelection) {
    var paragraph = this.store.frame.paragraphs[this.pendingSelection.pid];
    var valid = paragraph && this.pendingSelection.end <= scalarLength(paragraph.text);
    if (valid && this.pendingSelection.end > this.pendingSelection.start) {
      this.renderSelectionHighlight();
      return;
    }
    this.pendingSelection = null;
    this.dismissedComposer = null;
    this.updateQueueStatus('正文已变化，请重新选择后再操作');
  }
  this.clearReviewSelection();
};

/* ---------------------------------------------------------------- statuses */

ReviewConsole.prototype.renderStatus = function () {
  var banner = this.refs.stateBanner;
  var label = this.refs.stateBannerLabel;
  var text = this.refs.stateBannerText;
  var status = this.store.status;
  banner.dataset.state = status;
  if (status === 'loading') {
    setText(label, '载入中');
    setText(text, this.store.errorMessage || '正在载入审阅帧…');
  } else if (status === 'stale') {
    setText(label, '文档已更新');
    setText(text, '你的选择已保留，请确认后重新提交。');
  } else if (status === 'conflict') {
    setText(label, '版本冲突');
    setText(text, this.store.errorMessage || '当前文档与快照不一致，操作已被阻止。');
  } else if (status === 'writer-busy') {
    setText(label, '写入进行中');
    setText(text, this.store.errorMessage || '另一个写入正在进行，请稍候，不要重复提交。');
  } else if (status === 'error') {
    setText(label, '连接错误');
    setText(text, this.store.errorMessage || '与 review server 的连接已断开。');
  } else {
    banner.dataset.state = '';
    setText(label, '');
    setText(text, '');
  }
  this.refs.serverStatus.dataset.state = status === 'error' || status === 'conflict' ? 'error' : '';
};

ReviewConsole.prototype.updateQueueStatus = function (message) {
  var drafts = this.queueEvents.filter(function (event) { return event.status === 'draft'; }).length;
  var queued = this.queueEvents.filter(function (event) {
    return event.status === 'queued' && ['applied', 'acknowledged'].indexOf(event.delivery_state) < 0;
  }).length;
  var frame = this.store.frame;
  var snapshot = (frame && frame.session.current_snapshot && frame.session.current_snapshot.id) || '';
  var serverStatus = this.refs.serverStatus;
  var sendButton = this.refs.sendAgent;

  if (this.store.historyTarget) {
    serverStatus.dataset.state = '';
    setText(serverStatus, '历史版本 ' + this.store.historyTarget + ' · 只读');
    setText(sendButton, '历史版本只读');
    sendButton.disabled = true;
    return;
  }
  var isError = Boolean(message && /SERVER ERROR|失败|无法|已变化|冲突|占用|不匹配|重新/.test(String(message)));
  serverStatus.dataset.state = isError ? 'error' : '';
  if (message) setText(serverStatus, message);
  else setText(serverStatus, 'LOCAL SERVER' + (snapshot ? ' · ' + snapshot : '') + ' · 草稿 ' + drafts + ' · 待 agent ' + queued);
  setText(sendButton, '发送给 agent' + (drafts ? ' (' + drafts + ')' : ''));
  sendButton.disabled = drafts === 0 || Boolean(this.store.historyTarget);
  this.refs.export.disabled = Boolean(this.store.historyTarget);
};

ReviewConsole.prototype.updateWorkflow = function () {
  if (!this.refs.workflowStrip) return;
  var drafts = this.queueEvents.filter(function (event) { return event.status === 'draft'; }).length;
  var queued = this.queueEvents.filter(function (event) {
    return event.status === 'queued' && ['applied', 'acknowledged'].indexOf(event.delivery_state) < 0;
  }).length;
  var total = this.revisions.size;
  var decided = Object.keys(this.decisions).length;
  var frame = this.store.frame;
  var snapshot = (frame && frame.session.current_snapshot && frame.session.current_snapshot.id) || 'C0';
  var round = Math.max(1, Number(String(snapshot).replace(/^C/, '')) + 1);

  var active = 'agent';
  var kicker = 'NEXT ACTION';
  var title = '等待 agent 开始本轮修改';
  var description = '文档已载入，原始 DOCX 不会被直接覆盖。';

  if (this.store.historyTarget) {
    active = 'review';
    kicker = 'HISTORICAL ROUND';
    title = '查看历史版本 ' + this.store.historyTarget;
    description = '这是只读历史快照；切回当前版本后才能继续决策或发送给 agent。';
  } else if (this.store.status === 'error' || this.store.status === 'conflict') {
    active = 'handoff';
    kicker = 'ACTION BLOCKED';
    title = '连接 review server 后再继续';
    description = '当前草稿仍在浏览器中，未发送的内容不会丢失。';
  } else if (drafts) {
    active = 'handoff';
    kicker = 'NEXT ACTION';
    title = '发送 ' + drafts + ' 条意见给 agent';
    description = '发送后 agent 才会读取本轮决策、批注或正文调整。';
  } else if (queued) {
    active = 'agent';
    kicker = 'AGENT WORKING';
    title = 'agent 正在处理第 ' + round + ' 轮';
    description = '当前版本 ' + snapshot + '，页面会保留你的审阅位置并自动刷新。';
  } else if (total > decided) {
    active = 'review';
    kicker = 'NEXT ACTION';
    title = '逐项审阅 ' + (total - decided) + ' 处修订';
    description = '点击右侧项目定位正文；原文批注保留，处理意见会回传给 agent。';
  } else if ((total && decided === total) || (!total && round > 1)) {
    active = 'deliver';
    kicker = 'NEXT ACTION';
    title = '本轮决策已完成，交给 agent 构建验证';
    description = '最终视图只改变阅读方式；只有 build、verify 检查通过才算交付。';
  }
  this.refs.workflowStrip.dataset.state = (this.store.status === 'error' || this.store.status === 'conflict') ? 'error' : active;
  setText(this.refs.workflowKicker, kicker);
  setText(this.refs.workflowTitle, title);
  setText(this.refs.workflowDescription, description);
  var activeIndex = WORKFLOW_STEPS.indexOf(active);
  this.refs.workflowSteps.forEach(function (step) {
    var index = WORKFLOW_STEPS.indexOf(step.dataset.flowStep);
    step.classList.toggle('is-complete', index < activeIndex);
    step.classList.toggle('is-active', index === activeIndex);
  });
};

/* ------------------------------------------------------------------ layout */

ReviewConsole.prototype.syncTopbarHeight = function () {
  if (!this.refs.topbar) return;
  document.documentElement.style.setProperty('--topbar-height',
    this.refs.topbar.getBoundingClientRect().height + 'px');
};

/* ------------------------------------------------------------------- boot */

var consoleInstance = null;
function boot() {
  if (consoleInstance) return;
  consoleInstance = new ReviewConsole();
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', boot);
} else {
  boot();
}

})();
