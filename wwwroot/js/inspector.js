import { state } from './state.js';
import { t } from './i18n.js';
import { esc, shortSid, formatTime, formatHeaders, tryParseJson, jsonTreeHTML, truncate } from './utils.js';

function formatDate(ts) {
    if (!ts) return '';
    const d = new Date(ts);
    return `${d.getMonth() + 1}/${d.getDate()}`;
}

// Forward reference — set by main.js after all modules are loaded
let _openSummaryPanel = null;
let _renderSummaryFromCache = null;

export function setSessionPanelFns(openSummary, _openReqSummary, renderSummaryCache) {
    _openSummaryPanel = openSummary;
    _renderSummaryFromCache = renderSummaryCache;
}

// ── Price lookup ──

export function lookupProviderPrice(model) {
    if (!model) return null;

    // 1. Provider-level override: check if any provider explicitly defines this model with pricing
    for (const p of state.providerList) {
        if (p.models && Array.isArray(p.models)) {
            const match = p.models.find(m => m.id === model);
            if (match && (match.price_per_million_input != null || match.price_per_million_output != null)) {
                const inp = match.price_per_million_input ?? 0;
                return {
                    in: inp,
                    cacheWrite: match.price_per_million_cache_write ?? inp * 1.25,
                    cacheRead: match.price_per_million_cache_read ?? inp * 0.1,
                    out: match.price_per_million_output ?? 0,
                };
            }
        }
    }

    // 2. Global model_pricing table: match by id or any provider model name
    for (const mp of state.modelPricingList) {
        const providerNames = mp.providers ? Object.values(mp.providers).flat() : [];
        if (mp.id === model || providerNames.includes(model)) {
            const p = mp.price || [];
            const inp = p[0] ?? 0;
            return {
                in: inp,
                out: p[1] ?? 0,
                cacheWrite: p[2] ?? inp * 1.25,
                cacheRead: p[3] ?? inp * 0.1,
            };
        }
    }

    // 3. Fallback: any provider that lists this model (without explicit pricing)
    for (const p of state.providerList) {
        if (p.models && Array.isArray(p.models)) {
            const match = p.models.find(m => m.id === model);
            if (match) {
                return { in: 0, cacheWrite: 0, cacheRead: 0, out: 0 };
            }
        }
    }

    return null;
}

export function formatCost(req) {
    if (req.priced === false) return t('inspector.unpriced');
    // Prefer pre-computed cost from backend
    if (req.cost != null && req.cost > 0) {
        const total = req.cost;
        if (total < 0.001) return `¥${total.toFixed(5)}`;
        return `¥${total.toFixed(4)}`;
    }
    if (req.cost === 0) return '¥0.00';
    // Fallback: compute from pricing table
    const price = lookupProviderPrice(req.model);
    if (!price) return '—';
    const inTok  = req.input_tokens ?? 0;
    const outTok = req.output_tokens ?? 0;
    const ccTok  = req.cache_creation_input_tokens ?? 0;
    const crTok  = req.cache_read_input_tokens ?? 0;
    const total = inTok  * price.in  / 1_000_000
                + outTok * price.out / 1_000_000
                + ccTok  * price.cacheWrite  / 1_000_000
                + crTok  * price.cacheRead  / 1_000_000;
    if (total === 0) return '¥0.00';
    if (total < 0.001) return `¥${total.toFixed(5)}`;
    return `¥${total.toFixed(4)}`;
}

export function formatCostNum(req) {
    if (req.priced === false) return 0;
    if (req.cost != null) return req.cost;
    const price = lookupProviderPrice(req.model);
    if (!price) return 0;
    const inTok   = req.input_tokens ?? 0;
    const outTok  = req.output_tokens ?? 0;
    const ccTok   = req.cache_creation_input_tokens ?? 0;
    const crTok   = req.cache_read_input_tokens ?? 0;
    return inTok * price.in / 1_000_000
         + outTok * price.out / 1_000_000
         + ccTok * price.cacheWrite / 1_000_000
         + crTok * price.cacheRead / 1_000_000;
}

// ── Filters & groups ──

export function getFilteredRequests() {
    let result = [];
    for (const req of state.requestRows.values()) {
        if (state.filterModel === '__has_model__' && !req.model) continue;
        if (state.filterModel && state.filterModel !== '__has_model__' && req.model !== state.filterModel) continue;
        if (!req.session_id) continue;  // always show only session-grouped requests
        if (state.filterTimeFrom && new Date(req.timestamp) < new Date(state.filterTimeFrom)) continue;
        if (state.filterTimeTo && new Date(req.timestamp) > new Date(state.filterTimeTo)) continue;
        result.push(req);
    }
    result.sort((a, b) => new Date(b.timestamp) - new Date(a.timestamp));
    return result;
}

export function getSessionGroups() {
    const filtered = getFilteredRequests();
    const groupsMap = new Map();

    // Build groups from live requests
    for (const req of filtered) {
        const sid = req.session_id || '__no_session__';
        if (!groupsMap.has(sid)) {
            groupsMap.set(sid, {
                session_id: sid,
                label: state.sessionCache[sid] || shortSid(sid),
                requests: [],
                totalIn: 0, totalOut: 0, totalCost: 0,
                unpricedCount: 0,
                firstTime: req.timestamp, lastTime: req.timestamp,
                models: new Set(),
                archived: false,
                request_count: 0,
            });
        }
        const g = groupsMap.get(sid);
        g.requests.push(req);
        if (req.model) g.models.add(req.model);
        if (new Date(req.timestamp) < new Date(g.firstTime)) g.firstTime = req.timestamp;
        if (new Date(req.timestamp) > new Date(g.lastTime)) g.lastTime = req.timestamp;
    }

    // Merge sessionMeta into groupsMap
    for (const [sid, meta] of Object.entries(state.sessionMeta)) {
        if (!groupsMap.has(sid)) {
            // No live requests in current filter → archived row
            groupsMap.set(sid, {
                session_id: sid,
                label: state.sessionCache[sid] || shortSid(sid),
                requests: [],
                totalIn: meta.total_input_tokens || 0,
                totalOut: meta.total_output_tokens || 0,
                totalCost: meta.total_cost || 0,
                unpricedCount: 0,
                firstTime: meta.started_at,
                lastTime: meta.ended_at || meta.started_at,
                models: [],
                archived: true,
                request_count: meta.request_count || 0,
            });
        } else {
            // Live group exists: backfill started_at so sort order is stable
            const g = groupsMap.get(sid);
            if (meta.started_at && new Date(meta.started_at) < new Date(g.firstTime)) {
                g.firstTime = meta.started_at;
            }
            g._metaRequestCount = meta.request_count || 0;
        }
    }

    const result = Array.from(groupsMap.values());
    result.forEach(g => {
        if (!g.archived) {
            g.requests.sort((a, b) => new Date(b.timestamp) - new Date(a.timestamp));
            g.totalIn = g.requests.reduce((s, r) => s + (r.input_tokens || 0), 0);
            g.totalOut = g.requests.reduce((s, r) => s + (r.output_tokens || 0), 0);
            g.totalCost = g.requests.reduce((s, r) => s + formatCostNum(r), 0);
            g.unpricedCount = g.requests.filter(r => r.priced === false).length;
            g.models = Array.from(g.models);
            g.request_count = Math.max(g.requests.length, g._metaRequestCount || 0);
        }
    });
    result.sort((a, b) => new Date(b.lastTime) - new Date(a.lastTime));
    return result;
}

// ── Session header HTML builders ──

export function buildArchivedSessionHTML(group) {
    const tokens = group.totalIn > 0 || group.totalOut > 0 ? `${group.totalIn}/${group.totalOut}t` : '—';
    const reqCount = group.request_count || 0;
    const dateStr = group.firstTime ? formatDate(group.firstTime) : '';
    const timeStr = group.lastTime ? formatTime(group.lastTime) : '—';
    const cost = group.totalCost > 0 ? `¥${group.totalCost.toFixed(4)}` : '—';
    const checked = state.selectedSessionIds.has(group.session_id) ? 'checked' : '';
    return `
        <td class="col-chk"><input type="checkbox" class="session-chk" data-session-id="${esc(group.session_id)}" ${checked}></td>
        <td class="col-req">
            <div class="session-header-inner session-archived-inner">
                ${dateStr ? `<span class="session-date">${esc(dateStr)}</span>` : ''}
                <span class="session-label">${esc(group.label)}</span>
                <span class="session-summary">
                    <span class="session-summary-item">${reqCount} ${reqCount !== 1 ? t('common.req_plural', { n: reqCount }) : t('common.req_singular', { n: reqCount })}</span>
                    <span class="session-summary-item archived-badge">${t('common.archived')}</span>
                    <span class="session-summary-item">${timeStr}</span>
                    <span class="session-summary-item">${tokens}</span>
                    <span class="session-summary-item cost">${cost}</span>
                </span>
            </div>
        </td>`;
}

export function buildSessionHeaderHTML(group, isExpanded) {
    const reqCount = group.requests.length;
    const dateStr = group.firstTime ? formatDate(group.firstTime) : '';
    const timeRange = formatTime(group.lastTime);
    const tokens = group.totalIn > 0 || group.totalOut > 0 ? `${group.totalIn}/${group.totalOut}` : '—';
    const cost = group.totalCost > 0 ? `¥${group.totalCost.toFixed(4)}` : '—';
    const pricingHint = group.unpricedCount > 0
        ? `<span class="session-summary-item unpriced-badge">${group.unpricedCount} ${t('inspector.unpriced')}</span>`
        : '';
    const models = group.models.join(', ');
    const checked = state.selectedSessionIds.has(group.session_id) ? 'checked' : '';
    return `
        <td class="col-chk"><input type="checkbox" class="session-chk" data-session-id="${esc(group.session_id)}" ${checked}></td>
        <td class="col-req">
            <div class="session-header-inner">
                <span class="session-expand-icon${isExpanded ? ' expanded' : ''}">▶</span>
                ${dateStr ? `<span class="session-date">${esc(dateStr)}</span>` : ''}
                <span class="session-label">${esc(group.label)}</span>
                <span class="session-summary">
                    <span class="session-summary-item">${reqCount} ${reqCount > 1 ? t('common.req_plural', { n: reqCount }) : t('common.req_singular', { n: reqCount })}</span>
                    ${models ? `<span class="session-summary-item">${esc(models)}</span>` : ''}
                    <span class="session-summary-item">${timeRange}</span>
                    <span class="session-summary-item">${tokens}t</span>
                    <span class="session-summary-item cost">${cost}</span>
                    ${pricingHint}
                </span>
            </div>
        </td>`;
}

// ── Session toggle & select ──

export function toggleSession(sessionId) {
    if (state.expandedSessions.has(sessionId)) {
        state.expandedSessions.delete(sessionId);
    } else {
        state.expandedSessions.add(sessionId);
    }
    renderPage();
}

export function selectSession(sid) {
    state.currentSelectedSession = sid;
    state.expandedSessions.add(sid);
    renderPage();
    if (_openSummaryPanel) _openSummaryPanel(sid);
}

export function expandAllSessions() {
    const groups = getSessionGroups();
    groups.forEach(g => state.expandedSessions.add(g.session_id));
    renderPage();
}

export function collapseAllSessions() {
    state.expandedSessions.clear();
    renderPage();
}

// ── Request summary for list display ──

// Tool icon map for common Claude Code tools
const TOOL_ICONS = {
    Read: '📄', Write: '💾', Edit: '✏️', Bash: '⚡', Glob: '🔍',
    Grep: '🔎', Agent: '🤖', WebFetch: '🌐', WebSearch: '🌐',
    NotebookEdit: '📓', LSP: '🔗',
};

// Render a single tool chip (compact, for inline display)
function renderToolChip({ name, arg, ok, out }) {
    const icon = TOOL_ICONS[name] || '🔧';
    const okClass = ok === false ? 'req-tool-chip--err' : '';
    const statusIcon = ok === false ? '✗' : '✓';
    const outPart = out ? `<span class="req-tool-out">${esc(out)}</span>` : '';
    const title = `${name}${arg ? ': ' + arg : ''}${out ? '\n→ ' + out : ''}`;
    return `<span class="req-tool-chip ${okClass}" title="${esc(title)}">`
        + `${icon} ${esc(arg || name)}`
        + `<span class="req-tool-status">${statusIcon}</span>`
        + outPart
        + `</span>`;
}

const INLINE_CHIP_LIMIT = 8;

// Render a list of tools: first N chips inline + overflow badge with popover
function renderToolChips(tools, reqId) {
    if (!tools.length) return '';
    const visible = tools.slice(0, INLINE_CHIP_LIMIT);
    const hidden  = tools.slice(INLINE_CHIP_LIMIT);
    const inlineHtml = visible.map(t => renderToolChip(t)).join('');

    if (!hidden.length) return inlineHtml;

    // Build popover rows (all tools, full detail)
    const popoverRows = tools.map(t => {
        const icon = TOOL_ICONS[t.name] || '🔧';
        const okClass = t.ok === false ? 'tpop-row--err' : '';
        const statusIcon = t.ok === false ? '✗' : '✓';
        return `<div class="tpop-row ${okClass}">`
            + `<span class="tpop-status">${statusIcon}</span>`
            + `<span class="tpop-icon">${icon}</span>`
            + `<span class="tpop-name">${esc(t.name)}</span>`
            + (t.arg ? `<span class="tpop-arg">${esc(t.arg)}</span>` : '<span></span>')
            + (t.out ? `<div class="tpop-out">${esc(t.out)}</div>` : '')
            + `</div>`;
    }).join('');

    const popId = `tpop-${CSS.escape(reqId)}`;
    return `${inlineHtml}`
        + `<span class="req-tool-more" data-popid="${popId}">+${hidden.length}</span>`
        + `<div class="tool-popover" id="${popId}">${popoverRows}</div>`;
}

// Extract the short argument label for a tool_use block
function toolArgLabel(tu) {
    if (!tu.input) return tu.name;
    const v = tu.input.file_path || tu.input.command || tu.input.pattern || tu.input.prompt || '';
    return v ? truncate(String(v), 60) : tu.name;
}

// Extract a short result string from a tool_result content block
function toolResultSnippet(tr) {
    const raw = typeof tr.content === 'string' ? tr.content
        : Array.isArray(tr.content) ? (tr.content.find(b => b.type === 'text')?.text || '') : '';
    if (!raw.trim()) return '';
    // First non-empty line, truncated
    const first = raw.trim().split('\n')[0].trim();
    return truncate(first, 24);
}

// Find the last user message that contains real text (not a tool_result message)
function findLastUserText(messages) {
    for (let i = messages.length - 1; i >= 0; i--) {
        const m = messages[i];
        if (m.role !== 'user') continue;
        if (typeof m.content === 'string' && m.content.trim()) {
            return m.content.replace(/\s+/g, ' ').trim();
        }
        if (Array.isArray(m.content)) {
            // Skip messages that are entirely tool_result blocks
            if (m.content.every(b => b.type === 'tool_result')) continue;
            // Take the last text block
            for (let j = m.content.length - 1; j >= 0; j--) {
                const b = m.content[j];
                if (b.type === 'text' && b.text?.trim()) {
                    return b.text.replace(/\s+/g, ' ').trim();
                }
            }
        }
    }
    return '';
}

function renderPromptSummary(prompt) {
    return `<span class="req-summary-prompt" title="${esc(prompt)}">`
        + `💬 ${esc(truncate(prompt, 40))}</span>`;
}

function buildRequestSummary(req) {
    const bodyJson = req.request_body ? tryParseJson(req.request_body) : null;
    const msgCount = req.messages_count
        ?? (bodyJson && Array.isArray(bodyJson.messages) ? bodyJson.messages.length : null);

    const countChip = (msgCount != null && msgCount > 0)
        ? `<span class="req-msg-count">[${msgCount}]</span> `
        : '';

    // ── Fast path: prompt is loaded directly without the full request body ──
    if (req.prompt && !bodyJson) {
        return `${countChip}${renderPromptSummary(req.prompt)}`;
    }

    // ── Full path: body available (after row click) ──
    if (bodyJson && Array.isArray(bodyJson.messages) && bodyJson.messages.length > 0) {
        const msgs = bodyJson.messages;
        const last = msgs[msgs.length - 1];

        // Case A: last user message is pure text → user's real question
        if (last.role === 'user') {
            const text = typeof last.content === 'string' ? last.content
                : Array.isArray(last.content) ? (last.content.find(b => b.type === 'text')?.text || '') : '';
            if (text.trim()) {
                const clean = text.replace(/\s+/g, ' ').trim();
                return `${countChip}<span class="req-summary-text">${esc(truncate(clean, 80))}</span>`;
            }
        }

        // Case B: last user message is all tool_result blocks
        if (last.role === 'user' && Array.isArray(last.content)) {
            const toolResults = last.content.filter(b => b.type === 'tool_result');
            if (toolResults.length > 0 && toolResults.length === last.content.length) {
                let prevAssistant = null;
                for (let i = msgs.length - 2; i >= 0; i--) {
                    if (msgs[i].role === 'assistant') { prevAssistant = msgs[i]; break; }
                }
                const toolUses = (prevAssistant && Array.isArray(prevAssistant.content))
                    ? prevAssistant.content.filter(b => b.type === 'tool_use')
                    : [];

                const toolData = toolUses.map(tu => {
                    const arg = toolArgLabel(tu);
                    const tr = toolResults.find(r => r.tool_use_id === tu.id);
                    return {
                        name: tu.name,
                        arg,
                        ok: tr ? tr.is_error !== true : true,
                        out: tr ? toolResultSnippet(tr) : '',
                    };
                });

                const lastUserText = findLastUserText(msgs.slice(0, msgs.length - 1));
                const promptPart = lastUserText
                    ? `<span class="req-summary-prompt" title="${esc(lastUserText)}">💬 ${esc(truncate(lastUserText, 40))}</span> `
                    : '';

                const chipsHtml = toolData.length > 0
                    ? renderToolChips(toolData, req.id)
                    : `<span class="req-tool-chip">🔧 ${toolResults.length} results</span>`;

                return `${countChip}${promptPart}${chipsHtml}`;
            }
        }
    }

    // Fallback: count + tokens
    const inTok = req.input_tokens;
    const tokStr = (inTok != null && inTok > 0)
        ? `<span class="req-summary-tokens">${inTok}t</span>`
        : '';
    if (countChip || tokStr) return `${countChip}${tokStr}`;

    return `<span class="req-summary-dim">${esc(req.method)} ${esc(req.path)}</span>`;
}

// ── Request row HTML ──

export function buildRequestRowHTML(req, hideSession) {
    let statusClass = '';
    if (req.status_code) {
        if (req.status_code < 400) statusClass = 'status-200';
        else if (req.status_code < 500) statusClass = 'status-4xx';
        else statusClass = 'status-5xx';
    }
    const checked = state.selectedIds.has(req.id) ? 'checked' : '';

    const inOut = (req.input_tokens != null || req.output_tokens != null)
        ? `${req.input_tokens ?? 0}/${req.output_tokens ?? 0}`
        : '—';
    const costStr = formatCost(req);
    const summary = buildRequestSummary(req);
    const dur = req.duration_ms != null ? req.duration_ms + 'ms' : '—';
    const ttft = req.time_to_first_token_ms != null ? req.time_to_first_token_ms + 'ms' : '—';

    // Line 2: compact metadata separated by dots
    const meta = [
        `<span class="rq-meta-time">${formatTime(req.timestamp)}</span>`,
        `<span class="rq-meta-status ${statusClass}">${req.status_code || '—'}</span>`,
        `<span class="rq-meta-model" title="${esc(req.model || '')}">${esc(req.model || '—')}</span>`,
        `<span class="rq-meta-inout">${inOut}t</span>`,
        `<span class="rq-meta-cost${req.priced === false ? ' unpriced' : ''}">${costStr}</span>`,
        `<span class="rq-meta-dur">${dur}</span>`,
        `<span class="rq-meta-ttft">${ttft}</span>`,
    ].join('<span class="rq-meta-sep">·</span>');

    return `
        <td class="col-chk"><input type="checkbox" class="row-chk" data-id="${req.id}" ${checked}></td>
        <td class="col-req">
            <div class="rq-cell">
                <div class="rq-line1 col-summary">${summary}</div>
                <div class="rq-line2">
                    ${meta}
                    <button class="btn-delete-row rq-delete" data-id="${req.id}" title="${t('common.delete')}">×</button>
                </div>
            </div>
        </td>
    `;
}

// ── Render page ──

export function renderPage() {
    const groups = getSessionGroups();
    const totalPages = Math.max(1, Math.ceil(groups.length / state.pageSize));
    if (state.currentPage > totalPages) state.currentPage = totalPages;
    const start = (state.currentPage - 1) * state.pageSize;
    const tbody = document.getElementById('requests-tbody');
    tbody.innerHTML = '';

    const pageGroups = groups.slice(start, start + state.pageSize);
    pageGroups.forEach((group, idx) => {
        if (group.session_id === '__no_session__') {
            // No-session requests: plain rows, no folding
            group.requests.forEach(req => {
                const tr = document.createElement('tr');
                tr.id = `req-${req.id}`;
                tr.innerHTML = buildRequestRowHTML(req);
                tr.addEventListener('click', (e) => {
                    if (!req.id || e.target.closest('.row-chk') || e.target.closest('.btn-delete-row')) return;
                    showRequestDetail(req);
                });
                tbody.appendChild(tr);
            });

        } else if (group.requests.length === 0) {
            // Case 0: archived session — show compact summary row, no expand
            const archivedTr = document.createElement('tr');
            archivedTr.className = 'session-header session-archived' + (state.currentSelectedSession === group.session_id ? ' session-selected' : '');
            archivedTr.dataset.session = group.session_id;
            archivedTr.innerHTML = buildArchivedSessionHTML(group);
            archivedTr.addEventListener('click', (e) => {
                if (e.target.closest('.session-chk')) return;
                selectSession(group.session_id);
            });
            tbody.appendChild(archivedTr);

        } else if (group.requests.length === 1) {
            // Single request — render as foldable session header + child row
            const isExpanded = state.expandedSessions.has(group.session_id);
            const req = group.requests[0];

            const headerTr = document.createElement('tr');
            headerTr.className = 'session-header' + (state.currentSelectedSession === group.session_id ? ' session-selected' : '');
            headerTr.dataset.session = group.session_id;
            headerTr.innerHTML = buildSessionHeaderHTML(group, isExpanded);
            headerTr.addEventListener('click', (e) => {
                if (e.target.closest('.session-chk')) return;
                if (e.target.closest('.session-expand-icon')) {
                    toggleSession(group.session_id);
                    return;
                }
                selectSession(group.session_id);
                if (req.id) showRequestDetail(req);
            });
            tbody.appendChild(headerTr);

            if (isExpanded) {
                const tr = document.createElement('tr');
                tr.id = `req-${req.id}`;
                tr.className = 'session-child';
                tr.dataset.session = group.session_id;
                tr.innerHTML = buildRequestRowHTML(req, true);
                tr.addEventListener('click', (e) => {
                    if (!req.id || e.target.closest('.row-chk') || e.target.closest('.btn-delete-row')) return;
                    showRequestDetail(req);
                });
                tbody.appendChild(tr);
            }

        } else {
            // Case 2: multiple requests — expandable session group
            const isExpanded = state.expandedSessions.has(group.session_id);

            const headerTr = document.createElement('tr');
            headerTr.className = 'session-header' + (state.currentSelectedSession === group.session_id ? ' session-selected' : '');
            headerTr.dataset.session = group.session_id;
            headerTr.innerHTML = buildSessionHeaderHTML(group, isExpanded);
            headerTr.addEventListener('click', (e) => {
                if (e.target.closest('.session-chk')) return;
                if (e.target.closest('.session-expand-icon')) {
                    toggleSession(group.session_id);
                    return;
                }
                selectSession(group.session_id);
            });
            tbody.appendChild(headerTr);

            if (isExpanded) {
                group.requests.forEach(req => {
                const tr = document.createElement('tr');
                tr.id = `req-${req.id}`;
                tr.className = 'session-child';
                tr.dataset.session = group.session_id;
                tr.innerHTML = buildRequestRowHTML(req, true);
                tr.addEventListener('click', (e) => {
                    if (!req.id || e.target.closest('.row-chk') || e.target.closest('.btn-delete-row')) return;
                    showRequestDetail(req);
                });
                tbody.appendChild(tr);
            });
            }
        }

        // Spacer between groups
        if (idx < pageGroups.length - 1) {
            const spacerTr = document.createElement('tr');
            spacerTr.className = 'session-group-spacer';
            spacerTr.innerHTML = '<td colspan="2"></td>';
            tbody.appendChild(spacerTr);
        }
    });

    // Highlight selected row
    if (state.selectedRequestId) {
        const row = document.getElementById(`req-${state.selectedRequestId}`);
        if (row) row.classList.add('selected');
    }

    updatePagination(groups.length, totalPages);
    updateSelectionUI();
    // updateInspectorCostStats is called from cost.js — imported by main.js
    if (typeof window._updateInspectorCostStats === 'function') window._updateInspectorCostStats();
}

export function updatePagination(total, totalPages) {
    document.getElementById('page-info').textContent = t('pagination.count', { sessions: total, requests: state.requestRows.size });
    document.getElementById('page-num').textContent = t('pagination.page_of', { curr: state.currentPage, total: totalPages });
    document.getElementById('btn-page-prev').disabled = state.currentPage <= 1;
    document.getElementById('btn-page-next').disabled = state.currentPage >= totalPages;
}

// ── Upsert request row ──

export function upsertRequestRow(req) {
    const isNew = !state.requestRows.has(req.id);
    const existing = state.requestRows.get(req.id);
    state.requestRows.set(req.id, { ...(existing || {}), ...req });
    // Every task update can change session totals, ordering, model filters and
    // folding layout. Re-render the group (debounced), not just the task row.
    if (!state._renderPageTimer) {
        state._renderPageTimer = setTimeout(() => {
            renderPage();
            state._renderPageTimer = null;
        }, isNew ? 0 : 100);
    }
    // Debounce filter updates — streaming can fire 10-20x/sec
    clearTimeout(state._updateFilterTimer);
    state._updateFilterTimer = setTimeout(updateFilterOptions, 500);
}

// ── Request detail ──

export async function showRequestDetail(req) {
    state.selectedRequestId = req.id;
    // Auto-expand the session containing this request
    if (req.session_id) state.expandedSessions.add(req.session_id);

    const content = document.getElementById('detail-content');
    delete content.dataset.streamStarted;
    document.getElementById('request-detail').classList.remove('hidden');
    document.getElementById('view-inspector').classList.add('detail-open');
    document.getElementById('detail-title').textContent = `${req.method} ${req.path}`;

    // Find page containing this request's session group
    const groups = getSessionGroups();
    const groupIdx = groups.findIndex(g => g.session_id === (req.session_id || '__no_session__'));
    if (groupIdx >= 0) {
        const targetPage = Math.floor(groupIdx / state.pageSize) + 1;
        if (targetPage !== state.currentPage) state.currentPage = targetPage;
    }
    renderPage();

    // Fetch full request (with body) from API, since list_requests omits body for performance
    try {
        const resp = await fetch(`/api/request/${encodeURIComponent(req.id)}`);
        if (resp.ok) {
            const fullReq = await resp.json();
            // Write back to state so the summary column reflects the full body
            const existing = state.requestRows.get(fullReq.id);
            state.requestRows.set(fullReq.id, { ...(existing || {}), ...fullReq });
            const row = document.getElementById(`req-${fullReq.id}`);
            if (row) row.innerHTML = buildRequestRowHTML(fullReq, row.classList.contains('session-child'));
            updateDetailView(fullReq);
            // Cascade pre-computed summary to sidebar — no second API call needed
            if (fullReq.summary_json && _renderSummaryFromCache) {
                _renderSummaryFromCache(fullReq.summary_json, req.session_id);
            }
            return;
        }
    } catch (_) {}
    updateDetailView(req);
}

export function updateDetailView(req) {
    const activeTab = document.querySelector('.detail-tabs .tab.active')?.dataset.tab || 'request';
    showDetailTab(activeTab, req);
}

export function showDetailTab(tab, req) {
    const content = document.getElementById('detail-content');
    switch (tab) {
        case 'request':
            content.innerHTML = renderDetailBody(formatHeaders(req.request_headers), req.request_body);
            break;
        case 'response':
            content.innerHTML = renderDetailBody(formatHeaders(req.response_headers), req.response_body);
            break;
        case 'sse':
            content.innerHTML = `<pre style="width:100%;white-space:pre-wrap;word-break:break-all;margin:0;font:inherit;color:inherit;">${esc(formatSseContent(req))}</pre>`;
            break;
    }
}

export function renderDetailBody(headers, body) {
    const parts = [];
    if (headers) {
        const lines = headers.split('\n');
        parts.push(`<details class="foldable-section"><summary>Headers (${lines.length} lines)</summary><pre class="detail-headers">${esc(headers)}</pre></details>`);
    }
    if (body !== undefined && body !== null && body !== '') {
        // Full task responses are NormalizedResponse objects. Passing an object
        // through esc() stringifies it as "[object Object]", so render structured
        // values directly and only parse when the payload is a string.
        const parsed = typeof body === 'object' ? body : tryParseJson(body);
        if (parsed !== null) parts.push(`<div class="json-tree">${jsonTreeHTML(parsed, 0)}</div>`);
        else parts.push(`<pre class="detail-plain">${esc(body)}</pre>`);
    }
    return parts.join('');
}

export function formatSseContent(req) {
    const parts = [];

    // New architecture: response_body is a NormalizedResponse object
    const body = typeof req.response_body === 'object' && req.response_body !== null
        ? req.response_body : null;

    if (body) {
        if (body.thinking && body.thinking.length > 0) {
            parts.push('=== Thinking ===');
            body.thinking.forEach(t => parts.push(t));
            parts.push('');
        }
        if (body.text && body.text.length > 0) {
            parts.push('=== Response ===');
            body.text.forEach(t => parts.push(t));
            parts.push('');
        }
        if (body.tool_calls && body.tool_calls.length > 0) {
            parts.push('=== Tool Calls ===');
            body.tool_calls.forEach(tc => {
                parts.push(`[${tc.name}] ${JSON.stringify(tc.input, null, 2)}`);
            });
            parts.push('');
        }
        if (body.tool_results && body.tool_results.length > 0) {
            parts.push('=== Tool Results ===');
            body.tool_results.forEach(tr => {
                parts.push(`[${tr.tool_use_id}] ${tr.content}`);
            });
        }
        if (parts.length > 0) return parts.join('\n');
    }

    // Legacy: content_text + sse_events
    if (req.content_text) {
        parts.push('=== Response Content ===');
        parts.push(req.content_text);
    }
    const structured = (req.sse_events || []).filter(e => {
        if (!e.data) return false;
        try {
            const d = JSON.parse(e.data);
            return d.type !== 'content_block_delta' && d.type !== 'ping';
        } catch { return true; }
    });
    if (structured.length > 0) {
        if (parts.length > 0) parts.push('');
        parts.push('=== Events ===');
        structured.forEach(e => parts.push(`event: ${e.event_type || '—'}\ndata: ${e.data || '—'}\n`));
    }
    return parts.join('\n');
}

export function appendSseEvent(event) {
    const activeTab = document.querySelector('.detail-tabs .tab.active')?.dataset.tab;
    if (activeTab === 'sse') {
        const content = document.getElementById('detail-content');
        const pre = content.querySelector('pre');
        if (pre) {
            if (!content.dataset.streamStarted) {
                content.dataset.streamStarted = '1';
                pre.textContent = '(streaming…)\n\n';
            }
            pre.textContent += `event: ${event.event_type || '—'}\ndata: ${event.data || '—'}\n\n`;
            content.scrollTop = content.scrollHeight;
        }
    }
}

// ── Fullscreen ──

export function renderDetailFullscreen(req, activeTab) {
    document.getElementById('fullscreen-title').textContent = `${req.method} ${req.path}`;
    const content = document.getElementById('fullscreen-content');
    content.innerHTML = `
        <div class="detail-tabs fs-detail-tabs" style="padding:0 16px;border-bottom:1px solid var(--border);background:var(--bg-panel);">
            <button class="tab${activeTab==='request'?' active':''}" data-tab="request">Request</button>
            <button class="tab${activeTab==='response'?' active':''}" data-tab="response">Response</button>
            <button class="tab${activeTab==='sse'?' active':''}" data-tab="sse">SSE Events</button>
        </div>
        <div id="fs-detail-body" class="detail-body" style="max-height:none;flex:1;overflow-y:auto;"></div>
    `;
    renderFullscreenTab(activeTab, req);
}

export function renderFullscreenTab(tab, req) {
    const body = document.getElementById('fs-detail-body');
    if (!body) return;
    switch (tab) {
        case 'request': body.innerHTML = renderDetailBody(formatHeaders(req.request_headers), req.request_body); break;
        case 'response': body.innerHTML = renderDetailBody(formatHeaders(req.response_headers), req.response_body); break;
        case 'sse': body.innerHTML = `<pre style="width:100%;white-space:pre-wrap;word-break:break-all;margin:0;font:inherit;color:inherit;">${esc(formatSseContent(req))}</pre>`; break;
    }
}

// ── Request count & clear ──

export function updateRequestCount() {
    document.getElementById('request-count').textContent = t('status.requests', { n: state.requestRows.size });
}

export function clearAllTables() {
    state.requestRows.clear();
    state.expandedSessions.clear();
    state.selectedSessionIds.clear();
    state.currentSelectedSession = null;
    state.currentPage = 1;
    state.filterModel = '__has_model__';
    state.filterTimeFrom = '';
    state.filterTimeTo = '';
    document.getElementById('filter-model').value = '__has_model__';
    document.getElementById('filter-time-from').value = '';
    document.getElementById('filter-time-to').value = '';
    document.getElementById('requests-tbody').innerHTML = '';
    document.getElementById('hooks-tbody').innerHTML = '';
    document.getElementById('summary-title').textContent = 'Session Summary';
    document.getElementById('summary-content').innerHTML = '<div class="summary-empty"><div class="summary-empty-icon">&#9776;</div><div>Click a session to view summary</div></div>';
    document.getElementById('summary-panel').classList.add('hidden');
    document.getElementById('view-inspector').classList.remove('summary-open', 'summary-collapsed');
    ['btn-summary-rename', 'btn-summary-export', 'btn-summary-delete'].forEach(id => document.getElementById(id).classList.add('hidden'));
    updatePagination(0, 1);
    updateRequestCount();
}

// ── Filter options ──

export function updateFilterOptions() {
    const models = new Set();
    state.requestRows.forEach(r => { if (r.model) models.add(r.model); });
    const modelSelect = document.getElementById('filter-model');
    modelSelect.innerHTML = '<option value="">All</option><option value="__has_model__">All Models</option>';
    models.forEach(m => { modelSelect.innerHTML += `<option value="${esc(m)}">${esc(m)}</option>`; });
    modelSelect.value = state.filterModel;
}

export function applyFiltersAndRender() {
    state.currentPage = 1;
    renderPage();
}

// ── Selection UI ──

export function updateSelectionUI() {
    const reqCount = state.selectedIds.size;
    const sessionCount = state.selectedSessionIds.size;
    const total = reqCount + sessionCount;
    const btn = document.getElementById('btn-delete-selected');

    let label;
    if (sessionCount > 0 && reqCount > 0) {
        label = t('common.delete_button_mixed', { sessions: sessionCount, reqs: reqCount });
    } else if (sessionCount > 0) {
        label = t('common.delete_button_sessions', { n: sessionCount });
    } else {
        label = t('common.delete_button_requests', { n: reqCount });
    }
    btn.innerHTML = label;
    btn.classList.toggle('hidden', total === 0);
    document.getElementById('btn-export-selected').classList.toggle('hidden', total === 0);
    // Persisted summaries are session-level documents.
    document.getElementById('btn-summary-selected').classList.toggle('hidden', sessionCount === 0);

    const allChkCount = document.querySelectorAll('.session-chk, .row-chk').length;
    const selectAll = document.getElementById('select-all');
    if (total === 0) { selectAll.checked = false; selectAll.indeterminate = false; }
    else if (total >= allChkCount) { selectAll.checked = true; selectAll.indeterminate = false; }
    else { selectAll.checked = false; selectAll.indeterminate = true; }
}

// ── Event listeners ──

// Tool popover: click badge to toggle, click outside to close
document.addEventListener('click', (e) => {
    const badge = e.target.closest('.req-tool-more');
    if (badge) {
        e.stopPropagation();
        const pop = document.getElementById(badge.dataset.popid);
        if (!pop) return;
        const isOpen = pop.classList.contains('open');
        document.querySelectorAll('.tool-popover.open').forEach(p => p.classList.remove('open'));
        if (!isOpen) {
            pop.classList.add('open');
            // Position below the badge
            const rect = badge.getBoundingClientRect();
            pop.style.top = (rect.bottom + window.scrollY + 4) + 'px';
            pop.style.left = Math.min(rect.left + window.scrollX, window.innerWidth - 280) + 'px';
        }
        return;
    }
    // Close any open popover on outside click
    if (!e.target.closest('.tool-popover')) {
        document.querySelectorAll('.tool-popover.open').forEach(p => p.classList.remove('open'));
    }
});

// Detail tab buttons
document.querySelectorAll('.tab').forEach(btn => {
    btn.addEventListener('click', () => {
        document.querySelectorAll('.tab').forEach(b => b.classList.remove('active'));
        btn.classList.add('active');
        const req = state.requestRows.get(state.selectedRequestId);
        if (req) showDetailTab(btn.dataset.tab, req);
    });
});

document.getElementById('btn-close-detail').addEventListener('click', () => {
    document.getElementById('request-detail').classList.add('hidden');
    document.getElementById('view-inspector').classList.remove('detail-open');
    state.selectedRequestId = null;
    document.querySelectorAll('#requests-tbody tr').forEach(r => r.classList.remove('selected'));
});

// Fullscreen
document.getElementById('btn-fullscreen-close').addEventListener('click', () => {
    document.getElementById('fullscreen-overlay').classList.add('hidden');
    state.fullscreenReqId = null;
});

document.getElementById('btn-fullscreen-detail').addEventListener('click', () => {
    const req = state.requestRows.get(state.selectedRequestId);
    if (!req) return;
    state.fullscreenReqId = req.id;
    const activeTab = document.querySelector('.detail-tabs .tab.active')?.dataset.tab || 'request';
    renderDetailFullscreen(req, activeTab);
    document.getElementById('fullscreen-overlay').classList.remove('hidden');
});

document.getElementById('fullscreen-content').addEventListener('click', (e) => {
    const btn = e.target.closest('.fs-detail-tabs .tab');
    if (!btn) return;
    const req = state.requestRows.get(state.fullscreenReqId);
    if (!req) return;
    document.querySelectorAll('.fs-detail-tabs .tab').forEach(t => t.classList.remove('active'));
    btn.classList.add('active');
    renderFullscreenTab(btn.dataset.tab, req);
});

// Selection checkboxes
document.addEventListener('change', (e) => {
    if (e.target.classList.contains('row-chk')) {
        const id = e.target.dataset.id;
        if (e.target.checked) state.selectedIds.add(id); else state.selectedIds.delete(id);
        updateSelectionUI();
    }
    if (e.target.classList.contains('session-chk')) {
        const sid = e.target.dataset.sessionId;
        if (e.target.checked) state.selectedSessionIds.add(sid); else state.selectedSessionIds.delete(sid);
        updateSelectionUI();
    }
    if (e.target.id === 'select-all') {
        const checked = e.target.checked;
        document.querySelectorAll('.session-chk').forEach(cb => {
            cb.checked = checked;
            if (checked) state.selectedSessionIds.add(cb.dataset.sessionId);
            else state.selectedSessionIds.delete(cb.dataset.sessionId);
        });
        document.querySelectorAll('.row-chk').forEach(cb => {
            cb.checked = checked;
            if (checked) state.selectedIds.add(cb.dataset.id);
            else state.selectedIds.delete(cb.dataset.id);
        });
        updateSelectionUI();
    }
});

// Delete single row
document.addEventListener('click', async (e) => {
    const btn = e.target.closest('.btn-delete-row');
    if (!btn) return;
    e.stopPropagation();
    const id = btn.dataset.id;
    if (!confirm(t('common.confirm_delete_request', { id: id.substring(0, 8) }))) return;
    const resp = await fetch(`/api/request/${encodeURIComponent(id)}`, { method: 'DELETE' });
    if (resp.ok) {
        state.requestRows.delete(id);
        state.selectedIds.delete(id);
        if (state.selectedRequestId === id) {
            state.selectedRequestId = null;
            document.getElementById('request-detail').classList.add('hidden');
            document.getElementById('view-inspector').classList.remove('detail-open');
        }
        renderPage(); updateFilterOptions(); updateRequestCount();
    }
});

// Delete selected
document.getElementById('btn-delete-selected').addEventListener('click', async () => {
    const sessionCount = state.selectedSessionIds.size;
    const reqCount = state.selectedIds.size;
    if (sessionCount === 0 && reqCount === 0) return;

    let confirmMsg = '';
    if (sessionCount > 0 && reqCount > 0) {
        confirmMsg = t('common.delete_mixed', { sessions: sessionCount, reqs: reqCount });
    } else if (sessionCount > 0) {
        confirmMsg = t('common.delete_session_requests', { n: sessionCount });
    } else {
        confirmMsg = t('common.delete_requests_selected', { n: reqCount });
    }
    if (!confirm(confirmMsg)) return;

    // Delete sessions
    if (sessionCount > 0) {
        const sids = Array.from(state.selectedSessionIds);
        for (const sid of sids) {
            const resp = await fetch(`/api/session/${encodeURIComponent(sid)}`, { method: 'DELETE' });
            if (!resp.ok) {
                const error = await resp.json().catch(() => ({}));
                alert(error.error || `Failed to delete session ${sid}`);
                continue;
            }
            delete state.sessionMeta[sid];
            delete state.sessionCache[sid];
            if (state.currentSelectedSession === sid) {
                state.currentSelectedSession = null;
                document.getElementById('summary-title').textContent = t('summary.session_summary');
                document.getElementById('summary-content').innerHTML = '<div class="summary-empty"><div class="summary-empty-icon">&#9776;</div><div>' + t('summary.click_view_summary') + '</div></div>';
                ['btn-summary-rename', 'btn-summary-export', 'btn-summary-delete'].forEach(id => document.getElementById(id).classList.add('hidden'));
            }
            // Remove requests belonging to this session from local cache
            for (const [id, req] of state.requestRows.entries()) {
                if (req.session_id === sid) state.requestRows.delete(id);
            }
        }
        state.selectedSessionIds.clear();
    }

    // Delete individual requests
    if (reqCount > 0) {
        const ids = Array.from(state.selectedIds);
        const resp = await fetch('/api/requests', {
            method: 'DELETE',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ ids }),
        });
        if (resp.ok) {
            ids.forEach(id => state.requestRows.delete(id));
            state.selectedIds.clear();
            if (state.selectedRequestId && ids.includes(state.selectedRequestId)) {
                state.selectedRequestId = null;
                document.getElementById('request-detail').classList.add('hidden');
                document.getElementById('view-inspector').classList.remove('detail-open');
            }
        }
    }

    document.getElementById('select-all').checked = false;
    renderPage(); updateFilterOptions(); updateRequestCount();
});

// Generate readable summaries for selected sessions.
document.getElementById('btn-summary-selected').addEventListener('click', async () => {
    const sids = Array.from(state.selectedSessionIds);
    if (sids.length === 0) return;

    const isSingle = sids.length === 1;
    const confirmMsg = isSingle
        ? t('common.summarize_session_single')
        : t('common.summarize_session_multi', { n: sids.length });
    if (!confirm(confirmMsg)) return;

    const resp = await fetch('/api/summaries', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ session_ids: sids }),
    });
    const data = await resp.json();
    if (!resp.ok) {
        alert(data.error || t('common.summary_fail_alert'));
        return;
    }

    if (data.summarized && data.summarized.length > 0) {
        alert(t('common.summarized_sessions_alert', { n: data.summarized.length }));
    }

    if (data.errors && data.errors.length > 0) {
        alert(t('common.summary_fail_alert') + '\n' + data.errors.join('\n'));
    }
});

// Export selected
document.getElementById('btn-export-selected').addEventListener('click', async () => {
    const sessionCount = state.selectedSessionIds.size;
    const reqCount = state.selectedIds.size;
    if (sessionCount === 0 && reqCount === 0) return;

    const result = [];
    const coveredRequestIds = new Set();

    for (const sid of state.selectedSessionIds) {
        const resp = await fetch(`/api/session/${encodeURIComponent(sid)}/export?format=json`);
        if (!resp.ok) continue;
        const data = await resp.json();
        result.push(data);
        if (Array.isArray(data.requests)) {
            data.requests.forEach(r => coveredRequestIds.add(r.id));
        }
    }

    for (const id of state.selectedIds) {
        if (coveredRequestIds.has(id)) continue;
        const req = state.requestRows.get(id);
        if (req) result.push(_normalizeRequestBody(req));
    }

    const blob = new Blob([JSON.stringify(result, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    const now = new Date();
    const ts = now.getFullYear()
        + String(now.getMonth() + 1).padStart(2, '0')
        + String(now.getDate()).padStart(2, '0')
        + '_'
        + String(now.getHours()).padStart(2, '0')
        + String(now.getMinutes()).padStart(2, '0')
        + String(now.getSeconds()).padStart(2, '0');
    a.download = `ccproxy-${ts}.json`;
    a.click();
    URL.revokeObjectURL(url);
});

// Filter events
document.getElementById('filter-model').addEventListener('change', () => {
    state.filterModel = document.getElementById('filter-model').value;
    applyFiltersAndRender();
});
document.getElementById('filter-time-from').addEventListener('change', () => {
    state.filterTimeFrom = document.getElementById('filter-time-from').value;
    if (state.filterTimeFrom) state.filterTimeFrom += ':00';
    applyFiltersAndRender();
});
document.getElementById('filter-time-to').addEventListener('change', () => {
    state.filterTimeTo = document.getElementById('filter-time-to').value;
    if (state.filterTimeTo) state.filterTimeTo += ':00';
    applyFiltersAndRender();
});

// Pagination events
document.getElementById('page-size').addEventListener('change', () => {
    state.pageSize = parseInt(document.getElementById('page-size').value);
    state.currentPage = 1;
    renderPage();
});
document.getElementById('btn-page-prev').addEventListener('click', () => {
    if (state.currentPage > 1) { state.currentPage--; renderPage(); }
});
document.getElementById('btn-page-next').addEventListener('click', () => {
    const tp = Math.max(1, Math.ceil(getSessionGroups().length / state.pageSize));
    if (state.currentPage < tp) { state.currentPage++; renderPage(); }
});

// Expand/collapse toggle — expand all if any are collapsed, else collapse all
document.getElementById('btn-toggle-expand').addEventListener('click', () => {
    const groups = getSessionGroups();
    const expandableGroups = groups.filter(g => g.requests.length > 1);
    const allExpanded = expandableGroups.length > 0 && expandableGroups.every(g => state.expandedSessions.has(g.session_id));
    if (allExpanded) {
        state.expandedSessions.clear();
    } else {
        expandableGroups.forEach(g => state.expandedSessions.add(g.session_id));
    }
    renderPage();
});

// Internal helper for export (avoids circular dep with session.js)
function _normalizeRequestBody(item) {
    if (typeof item.request_body === 'string' && item.request_body) {
        try { item = { ...item, request_body: JSON.parse(item.request_body) }; } catch (e) { /* keep as string */ }
    }
    if (Array.isArray(item.requests)) {
        item = { ...item, requests: item.requests.map(_normalizeRequestBody) };
    }
    return item;
}
