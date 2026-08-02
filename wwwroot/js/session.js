import { state, getLru, setLru } from './state.js';
import { t } from './i18n.js';
import { esc, shortSid } from './utils.js';
import { renderPage, updateFilterOptions, updateRequestCount } from './inspector.js';

// ── Summary panel controls ──

export function toggleSummaryPanel() {
    state.summaryCollapsed = !state.summaryCollapsed;
    const panel = document.getElementById('summary-panel');
    const inspector = document.getElementById('view-inspector');
    panel.classList.toggle('collapsed', state.summaryCollapsed);
    inspector.classList.toggle('summary-collapsed', state.summaryCollapsed);
    // Arrow direction: › when collapsed (expand), ‹ when expanded (collapse)
    document.getElementById('btn-summary-toggle').textContent = state.summaryCollapsed ? '\u203a' : '\u2039';
}

export function toggleSummaryMaximize() {
    const panel = document.getElementById('summary-panel');
    const inspector = document.getElementById('view-inspector');
    const maximized = panel.classList.toggle('maximized');
    inspector.classList.toggle('summary-maximized', maximized);
    document.getElementById('btn-summary-maximize').textContent = maximized ? '\u2715' : '\u26f6';
}

export function bindSummarySidebarActions(sid) {
    const renameBtn = document.getElementById('btn-summary-rename');
    const exportBtn = document.getElementById('btn-summary-export');
    const exportJsonBtn = document.getElementById('btn-summary-export-json');
    const exportYamlBtn = document.getElementById('btn-summary-export-yaml');
    const deleteBtn = document.getElementById('btn-summary-delete');
    const maximizeBtn = document.getElementById('btn-summary-maximize');

    [renameBtn, exportBtn, deleteBtn, maximizeBtn].forEach(b => b.classList.remove('hidden'));

    renameBtn.onclick = async () => {
        const current = state.sessionCache[sid] || shortSid(sid);
        const label = prompt('New name:', current);
        if (!label || label.trim() === current) return;
        const resp = await fetch(`/api/session/${encodeURIComponent(sid)}`, {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ label: label.trim() }),
        });
        if (resp.ok) {
            state.sessionCache[sid] = label.trim();
            document.getElementById('summary-title').textContent = t('summary.summary_of', { label: label.trim().slice(-20) });
            updateFilterOptions();
            renderPage();
        } else {
            const error = await resp.json().catch(() => ({}));
            alert(error.error || 'Rename failed');
        }
    };

    exportJsonBtn.onclick = () => {
        window.open(`/api/session/${encodeURIComponent(sid)}/export?format=json`, '_blank');
        document.getElementById('summary-export-menu').classList.add('hidden');
    };
    exportYamlBtn.onclick = () => {
        window.open(`/api/session/${encodeURIComponent(sid)}/export?format=yaml`, '_blank');
        document.getElementById('summary-export-menu').classList.add('hidden');
    };

    deleteBtn.onclick = async () => {
        const label = state.sessionCache[sid] || shortSid(sid);
        if (!confirm(t('summary.confirm_delete_session', { label }))) return;
        const resp = await fetch(`/api/session/${encodeURIComponent(sid)}`, { method: 'DELETE' });
        if (resp.ok) {
            delete state.sessionMeta[sid];
            delete state.sessionCache[sid];
            if (state.currentSelectedSession === sid) {
                state.currentSelectedSession = null;
                document.getElementById('summary-title').textContent = t('summary.session_summary');
                document.getElementById('summary-content').innerHTML = '<div class="summary-empty"><div class="summary-empty-icon">&#9776;</div><div>' + t('summary.click_view_summary') + '</div></div>';
                [renameBtn, exportBtn, deleteBtn].forEach(b => b.classList.add('hidden'));
            }
            renderPage(); updateFilterOptions(); updateRequestCount();
        } else {
            const error = await resp.json().catch(() => ({}));
            alert(error.error || 'Delete failed');
        }
    };
}

export function renderSummaryFromCache(summaryJson, sid) {
    const content = document.getElementById('summary-content');
    let data;
    try { data = JSON.parse(summaryJson); } catch (e) {
        content.innerHTML = `<div class="summary-error">${esc(String(e))}</div>`;
        return;
    }
    document.getElementById('summary-title').textContent =
        t('summary.summary_of', { label: (state.sessionCache[sid] || sid).slice(-20) });
    content.innerHTML = renderSummaryHTML(data);
    bindSummaryEvents(content);
    document.getElementById('summary-panel').classList.remove('hidden');
    document.getElementById('view-inspector').classList.add('summary-open');
    bindSummarySidebarActions(sid);
    if (state.summaryCollapsed) toggleSummaryPanel();
}

export function prepareRequestSummaryPanel(sid) {
    const content = document.getElementById('summary-content');
    document.getElementById('summary-title').textContent = t('summary.loading');
    content.innerHTML = '<div class="summary-loading">' + t('summary.loading') + '</div>';
    document.getElementById('summary-panel').classList.remove('hidden');
    document.getElementById('view-inspector').classList.add('summary-open');
    bindSummarySidebarActions(sid);
    if (state.summaryCollapsed) toggleSummaryPanel();
}

export async function openRequestSummaryPanel(reqId, sid) {
    prepareRequestSummaryPanel(sid);
    const content = document.getElementById('summary-content');
    const cached = getLru(state.requestSummaryCache, reqId);
    if (cached) {
        renderRequestSummary(cached, sid);
        return;
    }

    let promise = state.requestSummaryFetches.get(reqId);
    if (!promise) {
        promise = fetchRequestSummary(reqId);
        state.requestSummaryFetches.set(reqId, promise);
    }

    try {
        const data = await promise;
        setLru(state.requestSummaryCache, reqId, data);
        renderRequestSummary(data, sid);
    } catch (e) {
        content.innerHTML = `<div class="summary-error">${esc(String(e))}</div>`;
        document.getElementById('summary-title').textContent = t('summary.error');
    } finally {
        state.requestSummaryFetches.delete(reqId);
    }
}

async function fetchRequestSummary(reqId) {
    const resp = await fetch(`/api/request/${encodeURIComponent(reqId)}/summary`);
    if (resp.ok) return resp.json();
    const error = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error(error.error || t('summary.failed_load_summary'));
}

function renderRequestSummary(data, sid) {
    const content = document.getElementById('summary-content');
    if (typeof data === 'string') {
        try {
            data = JSON.parse(data);
        } catch (e) {
            content.innerHTML = `<div class="summary-error">${esc(String(e))}</div>`;
            return;
        }
    }
    document.getElementById('summary-title').textContent =
        t('summary.summary_of', { label: (state.sessionCache[sid] || sid).slice(-20) });
    content.innerHTML = renderSummaryHTML(data);
    bindSummaryEvents(content);
}

export async function openSummaryPanel(sid) {
    const content = document.getElementById('summary-content');
    document.getElementById('summary-title').textContent = t('summary.loading');
    content.innerHTML = '<div class="summary-loading">' + t('summary.loading') + '</div>';
    document.getElementById('summary-panel').classList.remove('hidden');
    document.getElementById('view-inspector').classList.add('summary-open');

    // Show action buttons immediately
    bindSummarySidebarActions(sid);

    // If collapsed, auto-expand
    if (state.summaryCollapsed) toggleSummaryPanel();

    try {
        const resp = await fetch(`/api/session/${encodeURIComponent(sid)}/timeline`);
        if (resp.ok) {
            const data = await resp.json();
            // If the timeline has real content, render it; otherwise fall back
            // to the classic summary for archived/empty sessions.
            if (data.interactions && data.interactions.length > 0) {
                document.getElementById('summary-title').textContent =
                    t('summary.summary_of', { label: (state.sessionCache[sid] || sid).slice(-20) });
                content.innerHTML = renderSessionTimeline(data);
                return;
            }
        }
        const summaryResp = await fetch(`/api/session/${encodeURIComponent(sid)}/summary`);
        if (!summaryResp.ok) {
            // Archived session with no summary_json — offer link to Archive tab
            const shortId = sid.slice(-8);
            content.innerHTML = `<div class="summary-archived-notice">
                <div class="summary-archived-icon">📦</div>
                <div>${t('summary.archived_no_summary')}</div>
                <button class="summary-archive-link btn-link" data-sid="${esc(sid)}">${t('summary.open_in_archive')}</button>
            </div>`;
            document.getElementById('summary-title').textContent = shortId;
            // Wire up the link to switch to Archive tab and load the file
            content.querySelector('.summary-archive-link').addEventListener('click', () => {
                document.querySelector('[data-view="summaries"]')?.click();
                // Slight delay so the view switches before loading
                setTimeout(() => {
                    import('./archive.js').then(m => m.loadArchiveFile(`${sid}.yaml`));
                }, 100);
            });
            return;
        }
        const data = await summaryResp.json();
        document.getElementById('summary-title').textContent =
            t('summary.summary_of', { label: (state.sessionCache[sid] || sid).slice(-20) });
        content.innerHTML = renderSummaryHTML(data);
        bindSummaryEvents(content);
    } catch (e) {
        content.innerHTML = `<div class="summary-error">${esc(String(e))}</div>`;
        document.getElementById('summary-title').textContent = 'Error';
    }
}

export function renderSummaryHTML(d) {
    const fmt = n => n != null ? n.toLocaleString() : '—';

    // Meta row
    const meta = `
        <div class="summary-meta">
            <span class="summary-meta-item"><strong>${esc(d.model || '—')}</strong></span>
            <span class="summary-meta-item">${esc(d.started_at ? new Date(d.started_at).toLocaleString() : '—')}</span>
            <span class="summary-meta-item">In: <strong>${fmt(d.input_tokens)}</strong> Out: <strong>${fmt(d.output_tokens)}</strong></span>
            ${d.cache_read_tokens ? `<span class="summary-meta-item">Cache-hit: <strong>${fmt(d.cache_read_tokens)}</strong></span>` : ''}
            <span class="summary-meta-item">Status: <strong>${d.status_code || '—'}</strong></span>
            ${d.stop_reason ? `<span class="summary-meta-item">Stop: <strong>${esc(d.stop_reason)}</strong></span>` : ''}
        </div>`;

    // Tool icon mapping
    const TOOL_ICONS = { Read: '📄', Edit: '✏️', Write: '💾', Bash: '▶', Agent: '🤖', Grep: '🔍', Glob: '📂', WebFetch: '🌐', WebSearch: '🌐' };
    const toolIcon = name => TOOL_ICONS[name] || '⚙';

    // Group actions by preceding user prompt using msg_index
    const prompts = d.user_prompts || [];
    const actions = d.assistant_actions || [];

    // Build segments: each segment = { prompt, actions[] }
    const segments = [];
    for (let pi = 0; pi <= prompts.length; pi++) {
        const prompt = pi > 0 ? prompts[pi - 1] : null;
        const fromIdx = pi > 0 ? prompts[pi - 1].msg_index : -1;
        const toIdx = pi < prompts.length ? prompts[pi].msg_index : Infinity;
        const segActions = actions.filter(a => a.msg_index > fromIdx && a.msg_index < toIdx);
        if (prompt || segActions.length > 0) {
            segments.push({ prompt, actions: segActions });
        }
    }

    // Render segments
    const FOLD_AT = 5;
    let conversationHtml = '<div class="summary-section-title">' + t('summary.conversation') + '</div>';
    for (const seg of segments) {
        conversationHtml += '<div class="summary-segment">';
        if (seg.prompt) {
            conversationHtml += `<div class="summary-prompt-header"><span class="summary-prompt-icon">💬</span><span class="summary-prompt-text">${esc(seg.prompt.text)}</span></div>`;
        }
        if (seg.actions.length > 0) {
            conversationHtml += '<div class="summary-actions-group">';
            const needFold = seg.actions.length > FOLD_AT + 2;
            for (let i = 0; i < seg.actions.length; i++) {
                const a = seg.actions[i];
                const hidden = (needFold && i >= FOLD_AT) ? ' summary-action-hidden' : '';
                let inner = '';
                if (a.thought) {
                    inner += `<div class="summary-action-thought">${esc(a.thought)}</div>`;
                }
                for (const tool of (a.tools || [])) {
                    inner += `<div class="summary-tool-row">
                        <span class="summary-tool-icon">${toolIcon(tool.name)}</span>
                        <span class="summary-tool-name">${esc(tool.name)}</span>
                        <span class="summary-tool-desc">${esc(tool.description)}</span>
                    </div>`;
                }
                if (inner) {
                    conversationHtml += `<div class="summary-action-item${hidden}">${inner}</div>`;
                }
            }
            if (needFold) {
                const hiddenCount = seg.actions.length - FOLD_AT;
                conversationHtml += `<button class="summary-expand-btn summary-expand-seg">▼ ${t('summary.more_actions', { n: hiddenCount })}</button>`;
            }
            conversationHtml += '</div>';
        }
        conversationHtml += '</div>';
    }
    if (segments.length === 0) {
        conversationHtml += '<div class="summary-loading">' + t('summary.no_conversation') + '</div>';
    }
    const actionsHtml = conversationHtml;

    // Touched files
    let filesHtml = '<div class="summary-section-title">' + t('summary.touched_files') + '</div>';
    const files = d.touched_files || [];
    if (files.length > 0) {
        filesHtml += `<table class="summary-files-table">
            <thead><tr><th>${t('summary.file')}</th><th>${t('summary.reads')}</th><th>${t('summary.writes')}</th><th>${t('summary.edits')}</th></tr></thead><tbody>`;
        for (const f of files) {
            filesHtml += `<tr>
                <td>${esc(f.path)}</td>
                <td class="ops">${f.reads || 0}</td>
                <td class="ops">${f.writes || 0}</td>
                <td class="ops">${f.edits || 0}</td>
            </tr>`;
        }
        filesHtml += '</tbody></table>';
    } else {
        filesHtml += '<div class="summary-loading">' + t('summary.no_file_ops') + '</div>';
    }

    // Final response
    const respText = d.final_response || '';
    let responseHtml = '<div class="summary-section-title">' + t('summary.final_response') + '</div>';
    if (respText) {
        responseHtml += `<div class="summary-final-response" id="summary-final-resp">${esc(respText)}</div>
            <div class="summary-response-toggle" id="summary-resp-toggle">${t('summary.expand')}</div>`;
    } else {
        responseHtml += '<div class="summary-loading">—</div>';
    }

    // Stats
    const s = d.stats || {};
    const byName = s.tool_call_by_name || {};
    const toolBreakdown = Object.entries(byName)
        .sort((a, b) => b[1] - a[1])
        .map(([k, v]) => `${esc(k)}: ${v}`)
        .join(', ');
    const statsHtml = `
        <div class="summary-section-title">${t('summary.stats')}</div>
        <table class="summary-stats-table">
            <tr><td>${t('summary.total_messages')}</td><td>${fmt(s.total_messages)}</td></tr>
            <tr><td>${t('summary.user_prompts')}</td><td>${fmt(s.user_prompt_count)}</td></tr>
            <tr><td>${t('summary.tool_calls')}</td><td>${fmt(s.tool_call_count)}</td></tr>
            <tr><td>${t('summary.tool_results')}</td><td>${fmt(s.tool_result_count)}</td></tr>
            <tr><td>${t('summary.thinking_blocks')}</td><td>${fmt(s.thinking_block_count)}</td></tr>
            ${toolBreakdown ? `<tr><td>${t('summary.by_tool')}</td><td style="word-break:break-all;font-size:0.72rem">${toolBreakdown}</td></tr>` : ''}
        </table>`;

    return meta + actionsHtml + filesHtml + responseHtml + statsHtml;
}

// ── Session timeline (tree of interactions → runs → model calls → tools) ──

const TIMELINE_TOOL_ICONS = {
    Read: '📄', Write: '💾', Edit: '✏️', Bash: '⚡', Glob: '🔍',
    Grep: '🔎', Agent: '🤖', WebFetch: '🌐', WebSearch: '🌐',
    NotebookEdit: '📓', LSP: '🔗',
};
const tlToolIcon = name => TIMELINE_TOOL_ICONS[name] || '⚙';
const RUN_KIND_LABELS = {
    main: '💬',
    subagent: '⤷',
    title: '⚙',
    memory: '⚙',
    recap: '⚙',
    compact: '⚙',
    system: '⚙',
};

function fmtDur(ms) {
    if (ms == null) return '—';
    if (ms < 1000) return ms + 'ms';
    if (ms < 60000) return (ms / 1000).toFixed(1) + 's';
    return (ms / 60000).toFixed(1) + 'm';
}

function fmtTime(ts) {
    if (!ts) return '';
    const d = new Date(ts);
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

// Collect Task* tool operations across an interaction and aggregate them by
// task id ("Task #N" from results, or taskId from update/stop inputs).
function collectTaskRelations(interaction) {
    const tasks = new Map(); // key: "1" | "b8izd4ugh" | auto
    const ops = [];
    for (const run of (interaction.runs || [])) {
        for (const call of (run.model_calls || [])) {
            for (const op of (call.operations || [])) {
                if (!op.tool_name || !op.tool_name.startsWith('Task')) continue;
                ops.push(op);
            }
        }
    }
    for (const op of ops) {
        let id = null;
        let status = null;
        let subject = null;
        // Parse inputs from input_preview (JSON string).
        let input = {};
        try { input = JSON.parse(op.input_preview || '{}'); } catch (e) { /* not json */ }
        if (op.tool_name === 'TaskCreate') {
            subject = input.subject || '';
            // result_preview like "Task #1 created successfully: ..."
            const m = /Task #(\d+)/.exec(op.result_preview || '');
            id = m ? m[1] : `c${ops.indexOf(op)}`;
            status = 'created';
        } else if (op.tool_name === 'TaskUpdate') {
            id = String(input.taskId ?? '');
            status = input.status || '';
            if (!tasks.has(id)) { tasks.set(id, { id, subject: '' }); }
        } else if (op.tool_name === 'TaskStop') {
            id = input.task_id || '';
            status = 'stopped';
        }
        if (id === null || id === '') continue;
        if (!tasks.has(id)) tasks.set(id, { id, subject: '' });
        const t = tasks.get(id);
        if (subject) t.subject = subject;
        t.status = status || t.status;
        t.last = op;
    }
    return Array.from(tasks.values());
}

function renderTaskRelations(interaction) {
    const tasks = collectTaskRelations(interaction);
    if (tasks.length === 0) return '';
    const rows = tasks.map(t => {
        const subject = t.subject ? esc(t.subject.slice(0, 60)) : `Task #${esc(t.id)}`;
        const status = t.status ? `<span class="timeline-task-status ${esc(t.status)}">${esc(t.status)}</span>` : '';
        return `<div class="timeline-task-row">
            <span class="timeline-task-id">#${esc(t.id)}</span>
            <span class="timeline-task-subject">${subject}</span>
            ${status}
        </div>`;
    }).join('');
    return `<div class="timeline-tasks"><div class="timeline-tasks-title">Tasks</div>${rows}</div>`;
}

export function renderSessionTimeline(d) {
    const fmt = n => n != null ? n.toLocaleString() : '—';
    const icons = RUN_KIND_LABELS;
    const isUserPrompt = run => run.run_kind === 'main';

    // Header stats
    const header = `
        <div class="summary-meta">
            <span class="summary-meta-item">${fmt(d.total_model_calls)} ${t('summary.requests')}</span>
            <span class="summary-meta-item">${fmt(d.user_interactions)} ${t('summary.user_prompts_count')}</span>
            <span class="summary-meta-item">${t('summary.timeline')}</span>
        </div>`;

    let html = header + '<div class="timeline-tree">';

    for (const interaction of (d.interactions || [])) {
        const hasUserRun = (interaction.runs || []).some(isUserPrompt);
        // Interaction header: highlight when it carries a user prompt.
        const cls = hasUserRun ? 'timeline-interaction timeline-interaction-user' : 'timeline-interaction';
        html += `<div class="${cls}">`;
        const icon = hasUserRun ? '💬' : '⚙';
        html += `<div class="timeline-interaction-header">
            <span class="timeline-interaction-icon">${icon}</span>
            <span class="timeline-interaction-text">${esc(interaction.prompt_text || t('summary.no_prompt'))}</span>
        </div>`;

        html += renderTaskRelations(interaction);

        for (const run of (interaction.runs || [])) {
            const runIcon = icons[run.run_kind] || '⚙';
            const isSubagent = run.run_kind === 'subagent';
            const subCls = isSubagent ? ' timeline-run-subagent' : '';
            html += `<div class="timeline-run${subCls}">
                <div class="timeline-run-header">
                    <span class="timeline-run-icon">${runIcon}</span>
                    <span class="timeline-run-kind">${esc(run.run_kind)}</span>
                    <span class="timeline-run-meta">${fmt(run.model_calls.length)} ${t('summary.calls')} · ${fmt(run.tool_call_count)} ${t('summary.tools')}</span>
                </div>
                <div class="timeline-calls">`;
            for (const call of (run.model_calls || [])) {
                html += `<div class="timeline-call" data-call-id="${esc(call.id)}">
                    <span class="timeline-call-time">${fmtTime(call.started_at)}</span>
                    <span class="timeline-call-model">${esc(call.resolved_model)}</span>
                    <span class="timeline-call-status ${esc(call.status)}">${esc(call.status)}</span>
                    <span class="timeline-call-tokens">${fmt(call.input_tokens)}/${fmt(call.output_tokens)}</span>
                    <span class="timeline-call-dur">${fmtDur(call.duration_ms)}</span>
                    <span class="timeline-call-cost">$${(call.cost_microusd / 1e6).toFixed(4)}</span>
                    ${call.stop_reason ? `<span class="timeline-call-stop">${esc(call.stop_reason)}</span>` : ''}
                    <div class="timeline-ops">`;
                for (const op of (call.operations || [])) {
                    html += `<div class="timeline-op">
                        <div class="timeline-op-head">
                            <span class="timeline-op-icon">${tlToolIcon(op.tool_name)}</span>
                            <span class="timeline-op-name">${esc(op.tool_name)}</span>
                            <span class="timeline-op-status ${esc(op.status)}">${esc(op.status)}</span>
                        </div>
                        ${op.input_preview ? `<div class="timeline-op-input">${esc(op.input_preview)}</div>` : ''}
                        ${op.result_preview ? `<div class="timeline-op-result">${esc(op.result_preview)}</div>` : ''}
                    </div>`;
                }
                html += '</div></div>';
            }
            html += '</div></div>';
        }
        html += '</div>';
    }
    html += '</div>';

    return html;
}

export function bindSummaryEvents(container) {
    // Per-segment expand buttons
    container.querySelectorAll('.summary-expand-seg').forEach(btn => {
        btn.addEventListener('click', () => {
            const group = btn.closest('.summary-actions-group');
            group.querySelectorAll('.summary-action-hidden').forEach(el => el.classList.remove('summary-action-hidden'));
            btn.remove();
        });
    });
    // Toggle final response
    const resp = container.querySelector('#summary-final-resp');
    const toggle = container.querySelector('#summary-resp-toggle');
    if (resp && toggle) {
        toggle.addEventListener('click', () => {
            const expanded = resp.classList.toggle('expanded');
            toggle.textContent = expanded ? t('summary.collapse') : t('summary.expand');
        });
    }
}

export function normalizeRequestBody(item) {
    if (typeof item.request_body === 'string' && item.request_body) {
        try { item = { ...item, request_body: JSON.parse(item.request_body) }; } catch (e) { /* keep as string */ }
    }
    // Also normalize nested requests array (session export via API)
    if (Array.isArray(item.requests)) {
        item = { ...item, requests: item.requests.map(normalizeRequestBody) };
    }
    return item;
}

// ── Event listeners ──

document.getElementById('btn-summary-toggle').addEventListener('click', toggleSummaryPanel);
document.getElementById('summary-tab-strip').addEventListener('click', toggleSummaryPanel);
document.getElementById('btn-summary-maximize').addEventListener('click', toggleSummaryMaximize);

// Export dropdown toggle
document.getElementById('btn-summary-export').addEventListener('click', (e) => {
    e.stopPropagation();
    document.getElementById('summary-export-menu').classList.toggle('hidden');
});
document.addEventListener('click', () => {
    document.getElementById('summary-export-menu')?.classList.add('hidden');
});
