import { state } from './state.js';
import { loadI18n, applyI18n, t } from './i18n.js';
import {
    renderPage, updateRequestCount, clearAllTables,
    showRequestDetail, updateFilterOptions, getSessionGroups,
    setSessionPanelFns, applyFiltersAndRender, updatePagination,
    toggleSession, selectSession, expandAllSessions, collapseAllSessions,
} from './inspector.js';
import {
    toggleSummaryPanel, openSummaryPanel, openRequestSummaryPanel,
    prepareRequestSummaryPanel, renderSummaryFromCache, normalizeRequestBody,
} from './session.js';
import {
    applyUpstreamState, updateCaptureButton,
    openUpstreamTableEdit, closeUpstreamTableEdit, activateUpstream, deleteUpstream,
    deleteModelPricing,
} from './settings.js';
import { loadCosts, updateInspectorCostStats, refreshInspectorCostStatsNow, applyCostStats } from './cost.js';import { loadArchiveList, loadArchiveFile, startArchiveRename } from './archive.js';
import {
    renderTimeline, updateConvFilter,
} from './timeline.js';

// Optional dashboard auth. Open `/?token=...` once; keep the token only for
// this browser tab, remove it from the address bar, and attach it to API/WS calls.
const dashboardUrl = new URL(window.location.href);
const tokenFromUrl = dashboardUrl.searchParams.get('token');
if (tokenFromUrl) {
    sessionStorage.setItem('cc-proxy-auth-token', tokenFromUrl);
    dashboardUrl.searchParams.delete('token');
    history.replaceState(null, '', dashboardUrl.pathname + dashboardUrl.search + dashboardUrl.hash);
}
const dashboardAuthToken = sessionStorage.getItem('cc-proxy-auth-token') || '';
const nativeFetch = window.fetch.bind(window);
window.fetch = (input, init = {}) => {
    const url = typeof input === 'string' || input instanceof URL ? new URL(input, location.href) : new URL(input.url);
    if (dashboardAuthToken && url.origin === location.origin && url.pathname.startsWith('/api/')) {
        const headers = new Headers(init.headers || (input instanceof Request ? input.headers : undefined));
        headers.set('Authorization', `Bearer ${dashboardAuthToken}`);
        init = { ...init, headers };
    }
    return nativeFetch(input, init);
};

// ── Wire up circular-dep bridge for inspector.js ──
// inspector.js needs openSummaryPanel/openRequestSummaryPanel from session.js
setSessionPanelFns(
    openSummaryPanel,
    openRequestSummaryPanel,
    renderSummaryFromCache,
    prepareRequestSummaryPanel,
);

// Also expose updateInspectorCostStats via window bridge (used by inspector.js and settings.js)
window._updateInspectorCostStats = updateInspectorCostStats;
window._renderTimeline = renderTimeline;

// ── WebSocket ──

export function connect() {
    const wsUrl = new URL(`${state.protocol}//${location.host}/ws`);
    if (dashboardAuthToken) wsUrl.searchParams.set('token', dashboardAuthToken);
    state.ws = new WebSocket(wsUrl);

    state.ws.onopen = () => {
        state._reconnectDelay = 1000;
        state._lastMsgTime = Date.now();
        const el = document.getElementById('connection-status');
        el.className = 'connected';
        el.textContent = t('status.connected');
        console.debug('[ws] connected at', new Date().toISOString());
        startSilentCheck();
        // Run resync after initial connection and every reconnect
        const reason = state.requestRows.size === 0 ? 'init' : 'reconnect';
        resyncState(reason);
    };

    state.ws.onclose = (ev) => {
        clearInterval(state._silentTimer);
        clearTimeout(state._updateFilterTimer);
        clearTimeout(state._renderPageTimer);
        state._updateFilterTimer = null;
        state._renderPageTimer = null;
        const el = document.getElementById('connection-status');
        el.className = 'disconnected';
        const label = ev.code === 1000 ? t('status.disconnected')
                    : ev.code === 1005 ? t('status.disconnected_timeout')
                    : t('status.disconnected_code', { code: ev.code });
        el.textContent = label;
        console.warn(`[ws] closed at ${new Date().toISOString()} — code=${ev.code} reason="${ev.reason}" clean=${ev.wasClean}, retry in ${state._reconnectDelay}ms`);
        setTimeout(connect, state._reconnectDelay);
        state._reconnectDelay = Math.min(state._reconnectDelay * 2, state._RECONNECT_MAX);
    };

    state.ws.onerror = (e) => {
        console.error('[ws] error', e);
        state.ws.close(); // triggers onclose → retry
    };

    state.ws.onmessage = (event) => {
        try { handleMessage(JSON.parse(event.data)); }
        catch (e) { console.error('Failed to parse WS message:', e); }
    };
}

function handleMessage(msg) {
    state._lastMsgTime = Date.now();
    if (state.syncing) {
        // Buffer events during resync to replay after snapshot loads
        if (msg.type === 'NewRequest' || msg.type === 'RequestUpdated'
            || msg.type === 'SessionUpdated' || msg.type === 'CostUpdated') {
            state.pendingEvents.push(msg);
        }
        return;
    }
    console.debug('[ws] msg type=' + msg.type);
    switch (msg.type) {
        case 'NewRequest':
        case 'RequestUpdated':
            applyTaskEvent(msg.payload);
            renderTimeline();
            if (msg.payload.id === state.selectedRequestId) showRequestDetail(msg.payload);
            updateRequestCount();
            break;
        case 'SessionUpdated':
            if (msg.payload && msg.payload.id) {
                const s = msg.payload;
                state.sessionMeta[s.id] = { ...(state.sessionMeta[s.id] || {}), ...s };
                state.sessionCache[s.id] = s.label || shortSid(s.id);
                if (!state.convSessions.has(s.id)) {
                    state.convSessions.add(s.id);
                    updateConvFilter();
                }
                renderPage();
            }
            break;
        case 'CostUpdated':
            if (typeof window._applyCostStats === 'function') {
                window._applyCostStats(msg.payload);
            }
            break;
        case 'Cleared':
            state.requestRows.clear();
            state.detailCache.clear();
            state.requestSummaryCache.clear();
            state.requestSummaryFetches.clear();
            state.convSessions.clear();
            state.selectedRequestId = null;
            clearAllTables();
            updateRequestCount();
            break;
        case 'Resync':
            console.warn('[ws] Resync received — running full resync');
            resyncState('lagged');
            break;
    }
}

// ── Task event reducer ──

function applyTaskEvent(payload) {
    const previous = state.requestRows.get(payload.id) || {};
    // Never downgrade a terminal status to Recording
    const prevStatus = previous.status;
    const nextStatus = payload.status;
    if (prevStatus && prevStatus !== 'recording' && nextStatus === 'recording') {
        return;
    }
    if (prevStatus && prevStatus !== nextStatus) {
        state.requestSummaryCache.delete(payload.id);
    }
    const isNew = !state.requestRows.has(payload.id);
    // Merge: WS fields overlay, but null body fields from WS don't
    // overwrite detail that was already fetched via REST.
    const next = { ...previous, ...payload };
    if (payload.request_body == null && previous.request_body != null) {
        next.request_body = previous.request_body;
    }
    if (payload.response_body == null && previous.response_body != null) {
        next.response_body = previous.response_body;
    }
    if (payload.content_text == null && previous.content_text != null) {
        next.content_text = previous.content_text;
    }
    state.requestRows.set(payload.id, next);

    // Debounced re-render — same as old upsertRequestRow
    if (!state._renderPageTimer) {
        state._renderPageTimer = setTimeout(() => {
            renderPage();
            state._renderPageTimer = null;
        }, isNew ? 0 : 100);
    }
    clearTimeout(state._updateFilterTimer);
    state._updateFilterTimer = setTimeout(updateFilterOptions, 500);
}

// ── Resync state ──

async function resyncState(reason) {
    if (state.syncing) {
        state._resyncQueued = true;
        return;
    }
    state.syncing = true;
    state.pendingEvents = [];
    console.debug('[ws] resync start — reason:', reason);

    try {
        const sessions = await fetch('/api/sessions').then(r => r.json()).catch(() => []);

        // Initial paint only needs session aggregates; tasks are loaded on expansion.
        const newRows = new Map();
        const newMeta = {};
        const newCache = {};
        sessions.forEach(s => {
            newMeta[s.id] = s;
            newCache[s.id] = s.label || shortSid(s.id);
        });

        // Replay buffered events
        for (const evt of state.pendingEvents) {
            if (evt.type === 'SessionUpdated' && evt.payload && evt.payload.id) {
                const s = evt.payload;
                newMeta[s.id] = { ...(newMeta[s.id] || {}), ...s };
                newCache[s.id] = s.label || shortSid(s.id);
            }
            if ((evt.type === 'NewRequest' || evt.type === 'RequestUpdated') && evt.payload) {
                const prev = newRows.get(evt.payload.id) || {};
                const prevStatus = prev.status;
                const nextStatus = evt.payload.status;
                if (!(prevStatus && prevStatus !== 'recording' && nextStatus === 'recording')) {
                    newRows.set(evt.payload.id, { ...prev, ...evt.payload });
                }
            }
        }

        // Atomic replacement
        state.requestRows = newRows;
        state.sessionMeta = newMeta;
        state.sessionCache = newCache;
        state.loadedSessions.clear();
        state.loadingSessions.clear();
        state.sessionTaskPages.clear();
        state.detailCache.clear();
        state.requestSummaryCache.clear();
        state.requestSummaryFetches.clear();
        state.pendingEvents = [];

        renderPage();
        updateRequestCount();
        updateFilterOptions();
        renderTimeline();
        if (typeof window._applyCostStats !== 'function' || reason !== 'init') {
            refreshInspectorCostStatsNow();
        }
    } catch (e) {
        console.error('[ws] resync failed:', e);
    } finally {
        state.syncing = false;
        if (state._resyncQueued) {
            state._resyncQueued = false;
            resyncState('queued');
        }
    }
}

// ── Navigate to request ──

export async function navigateToRequest(id) {
    const req = state.requestRows.get(id);
    if (!req) {
        console.warn('[nav] request not found:', id);
        return;
    }
    // Activate Inspector tab
    document.querySelectorAll('nav a').forEach(a => a.classList.remove('active'));
    const inspectorLink = document.querySelector('nav a[data-view="inspector"]');
    if (inspectorLink) inspectorLink.classList.add('active');
    document.querySelectorAll('.view').forEach(v => v.classList.remove('active'));
    document.getElementById('view-inspector').classList.add('active');
    // Show summary panel
    const panel = document.getElementById('summary-panel');
    panel.classList.remove('hidden');
    document.getElementById('view-inspector').classList.add('summary-open');

    if (req.session_id) {
        state.expandedSessions.add(req.session_id);
    }
    // Paginate to the correct page
    const groups = getSessionGroups();
    const groupIdx = groups.findIndex(g => g.session_id === (req.session_id || '__no_session__'));
    if (groupIdx >= 0) {
        state.currentPage = Math.floor(groupIdx / state.pageSize) + 1;
    }
    renderPage();
    await showRequestDetail(req);
    requestAnimationFrame(() => {
        const row = document.getElementById(`req-${id}`);
        if (row) {
            row.scrollIntoView({ behavior: 'smooth', block: 'center' });
            row.classList.add('highlight-flash');
            setTimeout(() => row.classList.remove('highlight-flash'), 1500);
        }
    });
}

// Expose for conversation clicks
window._navigateToRequest = navigateToRequest;

function shortSid(sid) {
    if (!sid) return '—';
    const parts = sid.split('-');
    return parts.length > 1 ? parts[parts.length - 1] : sid.substring(0, 8);
}

// ── Silent check ──

export function startSilentCheck() {
    clearInterval(state._silentTimer);
    state._silentTimer = setInterval(() => {
        const elapsed = Math.floor((Date.now() - state._lastMsgTime) / 1000);
        const el = document.getElementById('connection-status');
        if (elapsed > 180) {
            el.textContent = t('status.silent', { sec: elapsed });
            el.className = 'connected silent';
            if (elapsed % 30 === 0) {
                console.warn(`[ws] silent for ${elapsed}s — no messages received`);
            }
        } else {
            el.textContent = t('status.connected');
            el.className = 'connected';
        }
    }, 5000);
}

// ── Navigation ──

document.querySelectorAll('nav a').forEach(link => {
    link.addEventListener('click', (e) => {
        e.preventDefault();
        const view = link.dataset.view;
        document.querySelectorAll('nav a').forEach(a => a.classList.remove('active'));
        link.classList.add('active');
        document.querySelectorAll('.view').forEach(v => v.classList.remove('active'));
        document.getElementById(`view-${view}`).classList.add('active');

        // Show summary panel only in Inspector when a session is selected
        const panel = document.getElementById('summary-panel');
        const inspector = document.getElementById('view-inspector');
        if (view === 'inspector') {
            if (state.currentSelectedSession) {
                panel.classList.remove('hidden');
                inspector.classList.add('summary-open');
                if (state.summaryCollapsed) {
                    inspector.classList.add('summary-collapsed');
                }
            } else {
                panel.classList.add('hidden');
            }
        } else {
            panel.classList.add('hidden');
            inspector.classList.remove('summary-open', 'summary-collapsed');
        }

        if (view === 'summaries') loadArchiveList();
        if (view === 'cost') loadCosts();
    });
});

// ── Capture ──
document.getElementById('btn-toggle-capture').addEventListener('click', async () => {
    state.captureEnabled = !state.captureEnabled;
    await fetch('/api/capture', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ enabled: state.captureEnabled }) });
    updateCaptureButton();
});

// Fullscreen conversation
document.getElementById('btn-fullscreen-conv').addEventListener('click', () => {
    document.getElementById('fullscreen-title').textContent = t('nav.conversation');
    const content = document.getElementById('fullscreen-content');
    content.innerHTML = document.getElementById('conversation-timeline').innerHTML;
    document.getElementById('fullscreen-overlay').classList.remove('hidden');
});

// ── Expose inline onclick handlers to window ──
Object.assign(window, {
    openUpstreamTableEdit,
    closeUpstreamTableEdit,
    activateUpstream,
    deleteUpstream,
    deleteModelPricing,
    toggleSession,
    selectSession,
    startArchiveRename,
    loadArchiveFile,
});

// ── DOMContentLoaded / init ──
// ES modules are deferred by default, so the DOM is ready when this runs.

(async function init() {
    await loadI18n();
    applyI18n();

    // Summary panel starts hidden — only shown when a session is selected in Inspector
    document.getElementById('summary-panel').classList.add('hidden');

    connect();

    fetch('/api/upstreams')
        .then(r => r.json())
        .then(data => applyUpstreamState(data.active_upstream, data.active_proxy_upstream, data.upstreams, data.providers, data.active_effort, data.model_pricing, data.http_proxy));

    // Sessions and requests are loaded by resyncState() in WS onopen

    fetch('/api/capture/status')
        .then(r => r.json())
        .then(data => { state.captureEnabled = data.enabled; updateCaptureButton(); });

    fetch('/api/retention')
        .then(r => r.json())
        .then(data => {
            document.getElementById('ret-hours').value = data.request_retention_hours;
            document.getElementById('ret-max-sessions').value = data.session_max_count;
            document.getElementById('ret-delete-days').value = data.session_delete_after_days ?? 0;
        })
        .catch(() => {});

    // ── Retention panel ──
    document.getElementById('btn-retention-save').addEventListener('click', async () => {
        const body = {
            request_retention_hours: parseInt(document.getElementById('ret-hours').value) || 0,
            session_max_count: parseInt(document.getElementById('ret-max-sessions').value) || 0,
            session_delete_after_days: parseInt(document.getElementById('ret-delete-days').value) || 0,
        };
        const resp = await fetch('/api/retention', {
            method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body)
        });
        if (resp.ok) {
            const result = document.getElementById('ret-cleanup-result');
            result.className = 'cleanup-result success';
            result.textContent = t('settings.saved');
            result.classList.remove('hidden');
            setTimeout(() => result.classList.add('hidden'), 2000);
        }
    });

    document.getElementById('btn-cleanup-now').addEventListener('click', async () => {
        const btn = document.getElementById('btn-cleanup-now');
        btn.disabled = true;
        btn.textContent = t('settings.cleaning');
        try {
            const resp = await fetch('/api/cleanup', { method: 'POST' });
            const data = await resp.json();
            if (!resp.ok) throw new Error(data.error || 'cleanup failed');
            const result = document.getElementById('ret-cleanup-result');
            result.className = 'cleanup-result success';
            result.textContent = t('settings.cleaning_result', { reqs: data.deleted_requests, sessions: data.deleted_sessions });
            result.classList.remove('hidden');
            document.getElementById('ret-last-cleanup').textContent = new Date().toLocaleTimeString();
        } catch (e) {
            const result = document.getElementById('ret-cleanup-result');
            result.className = 'cleanup-result';
            result.textContent = t('settings.failed_clean');
            result.classList.remove('hidden');
        }
        btn.disabled = false;
        btn.textContent = t('settings.clean_up_now');
    });

    document.getElementById('btn-summary-all').addEventListener('click', async () => {
        if (!confirm(t('settings.confirm_summarize_all'))) return;
        const btn = document.getElementById('btn-summary-all');
        btn.disabled = true;
        btn.textContent = t('settings.summarizing');
        try {
            const resp = await fetch('/api/summaries/all', { method: 'POST' });
            const data = await resp.json();
            if (!resp.ok) throw new Error(data.error || 'summary generation failed');
            const result = document.getElementById('ret-cleanup-result');
            if (data.summarized && data.summarized.length > 0) {
                result.className = 'cleanup-result success';
                result.textContent = t('settings.summarized_sessions', { n: data.summarized.length });
            } else {
                result.className = 'cleanup-result success';
                result.textContent = t('settings.nothing_summarize');
            }
            if (data.errors && data.errors.length > 0) {
                result.textContent += ` (${data.errors.length} error(s))`;
            }
            result.classList.remove('hidden');
            setTimeout(() => result.classList.add('hidden'), 4000);
        } catch (e) {
            const result = document.getElementById('ret-cleanup-result');
            result.className = 'cleanup-result';
            result.textContent = t('settings.summary_failed');
            result.classList.remove('hidden');
        }
        btn.disabled = false;
        btn.textContent = t('settings.summarize_all_now');
    });

})(); // end init
