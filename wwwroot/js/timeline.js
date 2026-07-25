import { state } from './state.js';
import { t } from './i18n.js';
import { esc, shortSid, formatTime, truncate } from './utils.js';

// ── Timeline derived from requestRows ──

const MAX_ITEMS = 100;

function statusClass(status) {
    switch (status) {
        case 'recording': return 'tl-recording';
        case 'failed': return 'tl-failed';
        case 'interrupted': return 'tl-interrupted';
        case 'completed': return 'tl-completed';
        default: return '';
    }
}

function statusLabel(status) {
    switch (status) {
        case 'recording': return '⏳';
        case 'completed': return '✓';
        case 'failed': return '✗';
        case 'interrupted': return '⊘';
        case 'cancelled': return '✕';
        default: return '';
    }
}

export function renderTimeline() {
    const timeline = document.getElementById('conversation-timeline');
    const filterSid = document.getElementById('conv-filter')?.value || '';

    // Collect all items from requestRows, sorted by timestamp DESC
    const items = [];
    for (const req of state.requestRows.values()) {
        if (!req.session_id) continue;
        if (filterSid && req.session_id !== filterSid) continue;
        items.push(req);
    }
    items.sort((a, b) => (b.timestamp || 0) - (a.timestamp || 0));
    const visible = items.slice(0, MAX_ITEMS);

    // Rebuild DOM
    timeline.innerHTML = '';
    for (const item of visible) {
        const div = document.createElement('div');
        div.className = 'timeline-item ' + statusClass(item.status);
        div.dataset.requestId = item.id;
        div.dataset.session = item.session_id || '';
        div.setAttribute('tabindex', '0');
        div.setAttribute('role', 'button');

        const promptText = item.prompt
            || (item.request_body ? tryParsePrompt(item.request_body) : null)
            || `${item.method} ${item.path}`;
        const truncated = truncate(promptText, 80);
        const time = formatTime(item.timestamp);
        const statusCode = item.status_code != null ? `HTTP ${item.status_code}` : '';
        const model = item.model || '—';
        const tokens = item.input_tokens != null
            ? `${item.input_tokens}→${item.output_tokens || 0}t`
            : '';
        const dur = item.duration_ms != null ? `${item.duration_ms}ms` : '';

        div.innerHTML = `
            <div class="timeline-row1">${esc(truncated)}</div>
            <div class="timeline-row2">
                <span class="tl-status">${statusLabel(item.status)}</span>
                <span>${esc(time)}</span>
                ${statusCode ? `<span>${esc(statusCode)}</span>` : ''}
                <span>${esc(model)}</span>
                ${tokens ? `<span>${esc(tokens)}</span>` : ''}
                ${dur ? `<span>${esc(dur)}</span>` : ''}
            </div>`;
        timeline.appendChild(div);
    }
}

function tryParsePrompt(body) {
    if (typeof body !== 'string') return null;
    try {
        const parsed = JSON.parse(body);
        if (parsed.messages && Array.isArray(parsed.messages)) {
            for (let i = parsed.messages.length - 1; i >= 0; i--) {
                const m = parsed.messages[i];
                if (m.role === 'user' && m.content) {
                    if (typeof m.content === 'string') return m.content;
                    if (Array.isArray(m.content)) {
                        const textParts = m.content.filter(c => c.type === 'text').map(c => c.text);
                        if (textParts.length) return textParts.join(' ');
                    }
                }
            }
        }
    } catch (_) {}
    return null;
}

// ── Session filter ──

export function updateConvFilter() {
    const select = document.getElementById('conv-filter');
    const current = select.value;
    select.innerHTML = '<option value="">All</option>';
    const sorted = [...state.convSessions].sort();
    sorted.forEach(s => {
        const label = state.sessionCache[s] || shortSid(s);
        select.innerHTML += `<option value="${esc(s)}">${esc(label)}</option>`;
    });
    select.value = current;
    applyConvFilter();
}

export function applyConvFilter() {
    renderTimeline();
}

// Event delegation for timeline clicks (including fullscreen)
document.getElementById('conversation-timeline').addEventListener('click', (e) => {
    const item = e.target.closest('.timeline-item');
    if (item && item.dataset.requestId) {
        if (typeof window._navigateToRequest === 'function') {
            window._navigateToRequest(item.dataset.requestId);
        }
    }
});
document.getElementById('fullscreen-content').addEventListener('click', (e) => {
    const item = e.target.closest('.timeline-item');
    if (item && item.dataset.requestId) {
        if (typeof window._navigateToRequest === 'function') {
            window._navigateToRequest(item.dataset.requestId);
        }
    }
});

document.getElementById('conv-filter').addEventListener('change', applyConvFilter);
