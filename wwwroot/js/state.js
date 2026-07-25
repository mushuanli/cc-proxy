// Shared mutable state — imported by all modules
export const state = {
    // WebSocket
    ws: null,
    protocol: location.protocol === 'https:' ? 'wss:' : 'ws:',
    selectedRequestId: null,
    captureEnabled: false,
    _reconnectDelay: 1000,
    _RECONNECT_MAX: 30000,
    _lastMsgTime: Date.now(),
    _silentTimer: null,

    // Pagination & filter
    currentPage: 1,
    pageSize: 50,
    filterModel: '__has_model__',
    filterTimeFrom: '',
    filterTimeTo: '',

    // Selection
    selectedIds: new Set(),
    selectedSessionIds: new Set(),

    // Session folding & selection
    expandedSessions: new Set(),
    currentSelectedSession: null,
    summaryCollapsed: false,

    // Provider / upstream
    providerList: [],
    upstreamList: [],
    modelPricingList: [],
    activeUpstream: '',
    activeProxyUpstream: '',
    activeEffort: 'auto',
    EFFORT_LEVELS: ['auto', 'low', 'medium', 'high', 'xhigh', 'max', 'ultracode'],

    // Session cache
    sessionCache: {},
    sessionMeta: {},
    pendingSessionFetches: new Set(),
    queuedSessionFetches: new Set(),
    _updateFilterTimer: null,
    _renderPageTimer: null,

    // Archive
    archiveFiles: [],
    archiveSearchTimer: null,
    archiveCurrentFile: null,

    // Matrix popover
    _matrixPopover: null,

    // Settings edit modes
    upstreamEditMode: null,
    upstreamCreateKind: null,
    upstreamAccordionName: null,
    providerEditMode: null,
    providerAccordionName: null,
    mpEditMode: null,
    mpAccordionId: null,

    // Inspector
    requestRows: new Map(),
    loadedSessions: new Set(),
    loadingSessions: new Set(),
    sessionTaskPages: new Map(),

    // Detail cache & dedup
    detailCache: new Map(),      // key: `${id}:${status}`
    detailFetches: new Map(),    // key: `${id}:${status}`, value: Promise
    requestSummaryCache: new Map(),
    requestSummaryFetches: new Map(),

    // Resync state
    pendingEvents: [],
    syncing: false,
    _resyncQueued: false,

    // Fullscreen
    fullscreenReqId: null,

    // Timeline
    convSessions: new Set(),
};

export function getLru(cache, key) {
    if (!cache.has(key)) return undefined;
    const value = cache.get(key);
    cache.delete(key);
    cache.set(key, value);
    return value;
}

export function setLru(cache, key, value, limit = 20) {
    cache.delete(key);
    cache.set(key, value);
    while (cache.size > limit) {
        cache.delete(cache.keys().next().value);
    }
}
