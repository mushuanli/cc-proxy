import { state } from './state.js';
import { t } from './i18n.js';
import { esc, shortSid, formatTime, truncate } from './utils.js';

// ── Timeline ──

export function addToTimeline(item) {
    const timeline = document.getElementById('conversation-timeline');
    const div = document.createElement('div');
    div.className = 'timeline-item';
    if (item.session_id) {
        div.dataset.session = item.session_id;
        if (!state.convSessions.has(item.session_id)) {
            state.convSessions.add(item.session_id);
            updateConvFilter();
        }
    }
    if (item.hook_event_name) {
        div.classList.add('hook');
        div.innerHTML = `
            <div class="timeline-header"><span>Hook: ${esc(item.hook_event_name)}</span><span>${formatTime(item.timestamp)}</span></div>
            <div class="timeline-body">${esc(JSON.stringify(item.hook_input, null, 2))}</div>`;
    } else {
        const model = item.model || '—';
        const tokens = item.input_tokens != null ? `${item.input_tokens}→${item.output_tokens || 0}t` : '';
        const content = item.content_text || item.response_body || item.request_body || '';
        const formatted = esc(content)
            .replace(/\[Thinking\]/g, '<span class="tl-thinking">[Thinking]</span>')
            .replace(/\[Tool Use\]/g, '<span class="tl-tool">[Tool Use]</span>');
        div.innerHTML = `
            <div class="timeline-header">
                <span>${esc(item.method)} ${esc(item.path)} — ${item.status_code || '...'} | ${esc(model)} | ${tokens} | ${esc(shortSid(item.session_id) || '—')}</span>
                <span>${formatTime(item.timestamp)} | ${item.duration_ms || 0}ms</span>
            </div>
            <div class="timeline-body">${formatted}</div>`;
    }
    timeline.prepend(div);
    while (timeline.children.length > 100) timeline.lastChild.remove();
}

export function updateConvFilter() {
    const select = document.getElementById('conv-filter');
    const current = select.value;
    select.innerHTML = '<option value="">All</option>';
    state.convSessions.forEach(s => {
        select.innerHTML += `<option value="${esc(s)}">${esc(shortSid(s))}</option>`;
    });
    select.value = current;
    applyConvFilter();
}

export function applyConvFilter() {
    const sid = document.getElementById('conv-filter').value;
    document.querySelectorAll('#conversation-timeline .timeline-item').forEach(el => {
        el.style.display = (!sid || el.dataset.session === sid) ? '' : 'none';
    });
}

// ── MCP table ──

export function addMcpRow(req) {
    const tbody = document.getElementById('mcp-tbody');
    const tr = document.createElement('tr');
    tr.innerHTML = `<td>${formatTime(req.timestamp)}</td><td>${esc(req.model || '—')}</td><td>${req.status_code || '—'}</td><td>${esc(truncate(req.request_body, 100))}</td><td>${esc(truncate(req.response_body, 100))}</td>`;
    tbody.prepend(tr);
    while (tbody.children.length > 100) tbody.lastChild.remove();
}

export function renderMcpTable(requests) {
    document.getElementById('mcp-tbody').innerHTML = '';
    requests.forEach(req => addMcpRow(req));
}

// ── Hook table ──

export function addHookRow(event) {
    const tbody = document.getElementById('hooks-tbody');
    const tr = document.createElement('tr');
    tr.innerHTML = `<td>${formatTime(event.timestamp)}</td><td>${esc(event.hook_event_name)}</td><td>${esc(event.session_id)}</td><td>${esc(event.cwd)}</td><td>${event.exit_code}</td>`;
    tbody.prepend(tr);
    while (tbody.children.length > 200) tbody.lastChild.remove();
}

export function renderHookTable(events) {
    document.getElementById('hooks-tbody').innerHTML = '';
    events.forEach(e => addHookRow(e));
}

// Event listener for conversation filter
document.getElementById('conv-filter').addEventListener('change', applyConvFilter);
