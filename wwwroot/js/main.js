import { state } from './state.js';
import { loadI18n, applyI18n, t } from './i18n.js';
import {
    renderPage, upsertRequestRow, updateRequestCount, clearAllTables,
    appendSseEvent, showRequestDetail, updateFilterOptions, getSessionGroups,
    setSessionPanelFns, applyFiltersAndRender, updatePagination,
    toggleSession, selectSession, expandAllSessions, collapseAllSessions,
} from './inspector.js';
import {
    toggleSummaryPanel, openSummaryPanel, openRequestSummaryPanel,
    normalizeRequestBody,
} from './session.js';
import {
    applyUpstreamState, updateCaptureButton,
    openUpstreamTableEdit, closeUpstreamTableEdit, activateUpstream, deleteUpstream,
    deleteModelPricing,
} from './settings.js';
import { loadCosts, updateInspectorCostStats, refreshInspectorCostStatsNow } from './cost.js';import { loadArchiveList, loadArchiveFile, startArchiveRename } from './archive.js';
import {
    addToTimeline,
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
setSessionPanelFns(openSummaryPanel, openRequestSummaryPanel);

// Also expose updateInspectorCostStats via window bridge (used by inspector.js and settings.js)
window._updateInspectorCostStats = updateInspectorCostStats;

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
    console.debug('[ws] msg type=' + msg.type);
    switch (msg.type) {
        case 'NewRequest':
        case 'RequestUpdated':
            upsertRequestRow(msg.payload);
            addToTimeline(msg.payload);
            if (msg.payload.id === state.selectedRequestId) showRequestDetail(msg.payload);
            updateRequestCount();
            if (msg.payload.session_id) {
                const sid = msg.payload.session_id;
                fetchSessionMeta(sid);
            }
            break;
        case 'SseEvent':
            if (msg.payload.request_id === state.selectedRequestId) appendSseEvent(msg.payload.event);
            break;
        case 'Cleared':
            clearAllTables();
            updateRequestCount();
            break;
        case 'Resync':
            // The server dropped some messages (broadcast buffer overflow).
            // Re-fetch requests via REST so our view doesn't silently miss events.
            console.warn('[ws] Resync received — refreshing request list from REST');
            fetch('/api/requests?limit=2000')
                .then(r => r.json())
                .then(requests => {
                    state.requestRows.clear();
                    requests.forEach(req => state.requestRows.set(req.id, req));
                    renderPage(); updateRequestCount(); updateFilterOptions();
                })
                .catch(() => {});
            break;
    }
}

// ── Session metadata fetch ──

export function fetchSessionMeta(sid) {
    if (state.pendingSessionFetches.has(sid)) {
        // Do not lose a newer task event while an older metadata request is in flight.
        state.queuedSessionFetches.add(sid);
        return;
    }
    state.pendingSessionFetches.add(sid);
    fetch(`/api/session/${encodeURIComponent(sid)}`)
        .then(r => r.ok ? r.json() : null)
        .then(data => {
            if (data && data.session) {
                const s = data.session;
                state.sessionMeta[s.id] = s;
                state.sessionCache[s.id] = s.label || shortSid(s.id);
                // Sync all tasks from this session into requestRows
                if (Array.isArray(data.requests)) {
                    data.requests.forEach(req => {
                        const existing = state.requestRows.get(req.id);
                        // REST list rows are intentionally lightweight. Merge them with
                        // the richer event payload instead of choosing one source and
                        // dropping fields from the other.
                        state.requestRows.set(req.id, { ...(existing || {}), ...req });
                    });
                }
                renderPage();
                updateRequestCount();
            }
        })
        .catch(() => {})
        .finally(() => {
            state.pendingSessionFetches.delete(sid);
            if (state.queuedSessionFetches.delete(sid)) fetchSessionMeta(sid);
        });
}

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

    // Pre-fill session cache and metadata in one call
    fetch('/api/sessions')
        .then(r => r.json())
        .then(sessions => {
            sessions.forEach(s => {
                state.sessionMeta[s.id] = s;
                state.sessionCache[s.id] = s.label || shortSid(s.id);
            });
            // Session and task snapshots load concurrently. Render again when
            // metadata arrives so labels, archived rows and aggregate totals
            // do not depend on which request happened to finish first.
            renderPage();
        })
        .catch(() => {});

    fetch('/api/requests?limit=2000')
        .then(r => r.json())
        .then(requests => {
            if (requests.length > 0) {
                requests.forEach(req => {
                    const existing = state.requestRows.get(req.id);
                    state.requestRows.set(req.id, { ...(existing || {}), ...req });
                });
                // All sessions start collapsed — user expands on demand
                state.currentPage = 1;
                renderPage(); updateRequestCount();
                updateFilterOptions();
            } else {
                // Ensure pagination reads correct even with no data
                updatePagination(0, 1);
            }
            // Refresh Inspector cost stats after initial data load
            refreshInspectorCostStatsNow();
        })
        .catch(err => console.error('Failed to load requests:', err));

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
