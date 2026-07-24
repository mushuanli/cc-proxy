import { state } from './state.js';
import { t } from './i18n.js';
import { esc, shortSid, formatTime, truncate } from './utils.js';

function responsePreview(item) {
    if (item.content_text) return item.content_text;
    const body = item.response_body;
    if (!body || typeof body !== 'object') return body || item.request_body || '';

    const parts = [];
    if (Array.isArray(body.thinking)) parts.push(...body.thinking.map(v => `[Thinking] ${v}`));
    if (Array.isArray(body.text)) parts.push(...body.text);
    if (Array.isArray(body.tool_calls)) {
        parts.push(...body.tool_calls.map(call =>
            `[Tool Use] ${call.name || 'tool'} ${JSON.stringify(call.input ?? {})}`));
    }
    if (Array.isArray(body.tool_results)) {
        parts.push(...body.tool_results.map(result =>
            `[Tool Result] ${result.content ?? ''}`));
    }
    return parts.join('\n') || JSON.stringify(body, null, 2);
}

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
    } else {
        const model = item.model || '—';
        const tokens = item.input_tokens != null ? `${item.input_tokens}→${item.output_tokens || 0}t` : '';
        const content = responsePreview(item);
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
document.getElementById('conv-filter').addEventListener('change', applyConvFilter);
