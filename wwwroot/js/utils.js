// ── Utilities ──

export function esc(str) {
    if (!str) return '—';
    const div = document.createElement('div');
    div.textContent = String(str);
    return div.innerHTML;
}

export function truncate(str, maxLen) {
    if (!str) return '—';
    return str.length <= maxLen ? str : str.substring(0, maxLen) + '…';
}

// Return last segment of a UUID-style session id (e.g. "abc12345" from "xxxx-xxxx-xxxx-abc12345").
// Falls back to the full id when not a UUID.
export function shortSid(sid) {
    if (!sid) return '—';
    const parts = sid.split('-');
    return parts.length > 1 ? parts[parts.length - 1] : sid.substring(0, 8);
}

export function formatTime(ts) {
    if (!ts) return '—';
    const d = new Date(ts);
    return d.toLocaleTimeString('en-US', { hour12: false });
}

export function formatHeaders(headers) {
    if (!headers) return '';
    return Object.entries(headers).map(([k, v]) => `${k}: ${v}`).join('\n');
}

// ── Archive utils ──

export function escHtml(s) {
    return s.replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;')
            .replace(/'/g, '&#39;');
}

export function fmtBytes(b) {
    return b < 1024 ? b + ' B' : (b / 1024).toFixed(1) + ' KB';
}

export function fmtRelTime(iso) {
    if (!iso) return '';
    const diff = Date.now() - new Date(iso).getTime();
    const m = Math.floor(diff / 60000);
    if (m < 1)   return '< 1m ago';
    if (m < 60)  return `${m}m ago`;
    const h = Math.floor(m / 60);
    if (h < 24)  return `${h}h ago`;
    const d = Math.floor(h / 24);
    return `${d}d ago`;
}

export function localDateStr(d) {
    return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

// ── Cost utils ──

export function formatCostShort(v) {
    if (v === 0) return '¥0';
    if (v >= 1)  return `¥${v.toFixed(1)}`;
    if (v >= 0.01) return `¥${v.toFixed(2)}`;
    return `¥${v.toFixed(4)}`;
}

export function formatAxisDate(dateStr, totalDays) {
    if (!dateStr || dateStr.length < 10) return dateStr;
    const mm = dateStr.slice(5, 7);
    const dd = dateStr.slice(8, 10);
    return totalDays > 60 ? mm : `${mm}/${dd}`;
}

export function formatTokens(n) {
    if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M';
    if (n >= 1e3) return Math.round(n / 1e3) + 'K';
    return String(n);
}

// ── JSON Tree Viewer ──

export const IMPORTANT_KEYS = ['role', 'type', 'name', 'id', 'model', 'status', 'stop_reason', 'index'];
export const COLLAPSED_KEYS = new Set(['tools', 'request_headers', 'response_headers']);

export function tryParseJson(str) {
    if (!str || typeof str !== 'string') return null;
    const s = str.trim();
    if (s.startsWith('{') || s.startsWith('[')) {
        try { return JSON.parse(s); } catch { return null; }
    }
    return null;
}

export function jsonTreeHTML(value, depth, key) {
    if (value === null) return '<span class="jt-null">null</span>';
    if (typeof value === 'boolean') return `<span class="jt-bool">${value}</span>`;
    if (typeof value === 'number') return `<span class="jt-number">${value}</span>`;
    if (typeof value === 'string') return `<span class="jt-string">"${esc(value)}"</span>`;
    const shouldCollapse = COLLAPSED_KEYS.has(key);
    if (Array.isArray(value)) {
        if (value.length === 0) return '<span class="jt-bracket">[]</span>';
        const collapsed = depth >= 2 || shouldCollapse;
        const preview = `[${value.length} item${value.length > 1 ? 's' : ''}]`;
        const children = value.map((item, i) => `<div class="jt-item"><span class="jt-index">${i}: </span>${jsonTreeHTML(item, depth + 1)}</div>`).join('');
        return `<span class="jt-node jt-array"><span class="jt-toggle ${collapsed ? '' : 'expanded'}">${collapsed ? '+' : '-'}</span><span class="jt-bracket">[</span><span class="jt-preview ${collapsed ? '' : 'hidden'}">${esc(preview)}</span><span class="jt-children ${collapsed ? 'hidden' : ''}">${children}</span><span class="jt-bracket">]</span></span>`;
    }
    if (typeof value === 'object') {
        const keys = Object.keys(value);
        if (keys.length === 0) return '<span class="jt-bracket">{}</span>';
        const collapsed = depth >= 2 || shouldCollapse;
        const previewParts = IMPORTANT_KEYS.filter(k => k in value).map(k => {
            const v = value[k];
            if (typeof v === 'string') return `${k}: "${esc(truncate(v, 40))}"`;
            if (typeof v === 'number' || typeof v === 'boolean') return `${k}: ${v}`;
            if (Array.isArray(v)) return `${k}: [${v.length}]`;
            if (v === null) return `${k}: null`;
            return `${k}: {...}`;
        });
        // For message objects, show a snippet of the last meaningful content
        if (value.role !== undefined) {
            // Case 1: content is a plain string (e.g. user message)
            if (typeof value.content === 'string' && value.content.trim()) {
                const clean = value.content.replace(/\s+/g, ' ').trim();
                previewParts.push(`"${esc(truncate(clean, 80))}"`);
            }
            // Case 2: content is an array of blocks (e.g. assistant message)
            else if (Array.isArray(value.content) && value.content.length > 0) {
            const lastBlock = value.content[value.content.length - 1];
            const lastText = lastBlock && (
                (typeof lastBlock.text === 'string' && lastBlock.text.trim()) ||
                (typeof lastBlock.content === 'string' && lastBlock.content.trim())
            );

            if (lastText) {
                // Last block has text or content (e.g. tool_result) — show it directly
                const text = (typeof lastBlock.text === 'string' && lastBlock.text.trim())
                    || (typeof lastBlock.content === 'string' && lastBlock.content.trim());
                const clean = text.replace(/\s+/g, ' ').trim();
                previewParts.push(`"${esc(truncate(clean, 80))}"`);
            } else if (lastBlock) {
                // Last block is not text (tool_use / thinking / etc.) — show action label first
                const actionLabel = typeof lastBlock.name === 'string' ? `[tool:${lastBlock.name}]`
                    : typeof lastBlock.type === 'string' ? `[${lastBlock.type}]`
                    : null;
                if (actionLabel) previewParts.push(actionLabel);

                // Then look backwards for context: text → content → tool_use → thinking
                let textTarget = null;
                // 1. Prefer text blocks
                for (let i = value.content.length - 2; i >= 0; i--) {
                    const b = value.content[i];
                    if (b && typeof b.text === 'string' && b.text.trim()) {
                        textTarget = b;
                        break;
                    }
                }
                // 2. Fallback: blocks with "content" field (e.g. tool_result)
                if (!textTarget) {
                    for (let i = value.content.length - 2; i >= 0; i--) {
                        const b = value.content[i];
                        if (b && typeof b.content === 'string' && b.content.trim()) {
                            textTarget = b;
                            break;
                        }
                    }
                }
                // 3. Fallback: tool_use blocks — show parameters
                if (!textTarget) {
                    for (let i = value.content.length - 2; i >= 0; i--) {
                        const b = value.content[i];
                        if (b && b.type === 'tool_use' && b.input && typeof b.input === 'object') {
                            textTarget = b;
                            break;
                        }
                    }
                }
                // 4. Last resort: thinking blocks
                if (!textTarget) {
                    for (let i = value.content.length - 2; i >= 0; i--) {
                        const b = value.content[i];
                        if (b && typeof b.thinking === 'string' && b.thinking.trim()) {
                            textTarget = b;
                            break;
                        }
                    }
                }

                if (textTarget) {
                    let snippet;
                    if (typeof textTarget.text === 'string') {
                        snippet = textTarget.text.trim();
                    } else if (typeof textTarget.content === 'string') {
                        snippet = textTarget.content.trim();
                    } else if (textTarget.type === 'tool_use' && textTarget.input) {
                        snippet = JSON.stringify(textTarget.input);
                    } else if (typeof textTarget.thinking === 'string') {
                        snippet = textTarget.thinking.trim();
                    }
                    if (snippet) {
                        const clean = snippet.replace(/\s+/g, ' ').trim();
                        previewParts.push(`"${esc(truncate(clean, 80))}"`);
                    }
                }
            }
        }
    }
        const remaining = keys.filter(k => !IMPORTANT_KEYS.includes(k)).length;
        const preview = previewParts.length > 0 ? previewParts.join(', ') + (remaining > 0 ? ` +${remaining}` : '') : `${keys.length} key${keys.length > 1 ? 's' : ''}`;
        const children = keys.map(k => `<div class="jt-pair"><span class="jt-key">"${esc(k)}": </span>${jsonTreeHTML(value[k], depth + 1, k)}</div>`).join('');
        return `<span class="jt-node jt-object"><span class="jt-toggle ${collapsed ? '' : 'expanded'}">${collapsed ? '+' : '-'}</span><span class="jt-bracket">{</span><span class="jt-preview ${collapsed ? '' : 'hidden'}">${preview}</span><span class="jt-children ${collapsed ? 'hidden' : ''}">${children}</span><span class="jt-bracket">}</span></span>`;
    }
    return String(value);
}

// JSON tree toggle handler — runs when the module is imported (DOM is ready for module scripts)
document.addEventListener('click', function(e) {
    const toggle = e.target.closest('.jt-toggle');
    if (!toggle) return;
    const node = toggle.parentElement;
    if (!node?.classList.contains('jt-node')) return;
    const children = node.querySelector('.jt-children');
    const preview = node.querySelector('.jt-preview');
    if (!children) return;
    if (toggle.textContent === '+') {
        node.querySelectorAll('.jt-toggle').forEach(t => { t.textContent = '-'; t.classList.add('expanded'); });
        node.querySelectorAll('.jt-children').forEach(c => c.classList.remove('hidden'));
        node.querySelectorAll('.jt-preview').forEach(p => p.classList.add('hidden'));
    } else {
        children.classList.add('hidden');
        if (preview) preview.classList.remove('hidden');
        toggle.textContent = '+';
        toggle.classList.remove('expanded');
    }
});
