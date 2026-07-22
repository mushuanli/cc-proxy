import { state } from './state.js';
import { t } from './i18n.js';
import { esc, localDateStr, formatCostShort, formatTokens } from './utils.js';
import { lookupProviderPrice } from './inspector.js';

// Format request count with fixed 8-char width (monospace)
function formatReqCount(n) {
    const s = String(n);
    return s.padStart(8, '\u2007'); // figure space for monospace alignment
}

// Format token count with fixed 8-char width; use M suffix when >= 1M
function formatTokensFixed(n) {
    let s;
    if (n >= 1e9)      s = (n / 1e9).toFixed(2) + 'G';
    else if (n >= 1e6) s = (n / 1e6).toFixed(2) + 'M';
    else if (n >= 1e3) s = (n / 1e3).toFixed(1) + 'K';
    else               s = String(n);
    return s.padStart(8, '\u2007');
}

// ── Inspector toolbar cost stats ──

let _statRefreshTimer = null;
let _statLastRefresh = 0; // timestamp of last successful fetch
const _STAT_COOLDOWN_MS = 5 * 60 * 1000; // 5 minutes

async function _doUpdateInspectorCostStats() {
    const nowTs = Date.now();
    if (nowTs - _statLastRefresh < _STAT_COOLDOWN_MS) return;
    _statLastRefresh = nowTs;

    const now = new Date();
    const todayStr = localDateStr(now);
    const monthFirstStr = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}-01`;

    const [todayData, monthData] = await Promise.all([
        fetch(`/api/costs?from=${encodeURIComponent(todayStr)}&to=${encodeURIComponent(todayStr)}`).then(r => r.ok ? r.json() : null).catch(() => null),
        fetch(`/api/costs?from=${encodeURIComponent(monthFirstStr)}&to=${encodeURIComponent(todayStr)}`).then(r => r.ok ? r.json() : null).catch(() => null),
    ]);

    const sumCosts = (data) => {
        if (!data || !data.by_model) return { in: 0, out: 0, cost: 0 };
        let inTok = 0, outTok = 0, cost = 0;
        for (const m of calcModelCosts(data.by_model)) {
            inTok += m.input_tokens;
            outTok += m.output_tokens;
            cost += m.cost;
        }
        return { in: inTok, out: outTok, cost };
    };

    const today = sumCosts(todayData);
    const month = sumCosts(monthData);

    document.getElementById('stat-today-tokens').textContent = `${formatTokens(today.in)}/${formatTokens(today.out)}`;
    document.getElementById('stat-today-cost').textContent = today.cost > 0 ? `¥${today.cost.toFixed(3)}` : '¥0';
    document.getElementById('stat-month-tokens').textContent = `${formatTokens(month.in)}/${formatTokens(month.out)}`;
    document.getElementById('stat-month-cost').textContent = month.cost > 0 ? `¥${month.cost.toFixed(3)}` : '¥0';
}

export function updateInspectorCostStats() {
    if (_statRefreshTimer) return;
    _statRefreshTimer = setTimeout(() => {
        _statRefreshTimer = null;
        _doUpdateInspectorCostStats();
    }, 2000);
}

export function refreshInspectorCostStatsNow() {
    clearTimeout(_statRefreshTimer);
    _statRefreshTimer = null;
    _doUpdateInspectorCostStats();
}

/** Apply cost stats pushed from server via WebSocket (no API call). */
export function applyCostStats(stats) {
    const inTok = stats.today_input_tokens || 0;
    const outTok = stats.today_output_tokens || 0;
    const todayCost = (stats.today_cost_microusd || 0) / 1_000_000;
    const monthCost = (stats.month_cost_microusd || 0) / 1_000_000;

    document.getElementById('stat-today-tokens').textContent = `${formatTokens(inTok)}/${formatTokens(outTok)}`;
    document.getElementById('stat-today-cost').textContent = todayCost > 0 ? `¥${todayCost.toFixed(3)}` : '¥0';
    document.getElementById('stat-month-cost').textContent = monthCost > 0 ? `¥${monthCost.toFixed(3)}` : '¥0';

    _statLastRefresh = Date.now(); // suppress stale REST fetch within cooldown
}

window._updateInspectorCostStats = updateInspectorCostStats;
window._applyCostStats = applyCostStats;

// ── Cost view helpers ──

export function findProviderForModel(model) {
    if (!model) return null;
    for (const p of state.providerList) {
        if (p.models && p.models.some(m => m.id === model)) return p;
    }
    return null;
}

export function calcModelCosts(byModel) {
    return byModel.map(m => {
        const price = lookupProviderPrice(m.model);
        const cost = m.input_tokens          * (price ? price.in         : 5)    / 1e6
                   + m.output_tokens         * (price ? price.out        : 25)   / 1e6
                   + m.cache_creation_tokens * (price ? price.cacheWrite : 3.75) / 1e6
                   + m.cache_read_tokens     * (price ? price.cacheRead  : 0.3)  / 1e6;
        return Object.assign({}, m, { cost });
    });
}

export function groupByProvider(providerCosts) {
    return providerCosts.map(p => {
        const cost = p.input_tokens          * 5    / 1e6
                   + p.output_tokens         * 25   / 1e6
                   + p.cache_creation_tokens * 3.75 / 1e6
                   + p.cache_read_tokens     * 0.3  / 1e6;
        return { provider: p.provider, cost, input_tokens: p.input_tokens, output_tokens: p.output_tokens };
    }).sort((a, b) => b.cost - a.cost);
}

export function formatCostValue(v) {
    return `¥${v.toFixed(4)}`;
}

// ── Mode & range calculation ──

let _costMode = 'day'; // 'day' | 'week' | 'month'
let _selectedBarIdx = -1;

// Returns { from, to, bars: [{label, from, to}] }
function getCostModeRange(mode) {
    const now = new Date();
    const today = localDateStr(now);

    if (mode === 'month') {
        const bars = [];
        for (let i = 9; i >= 0; i--) {
            const d = new Date(now.getFullYear(), now.getMonth() - i, 1);
            const y = d.getFullYear();
            const mo = String(d.getMonth() + 1).padStart(2, '0');
            const from = `${y}-${mo}-01`;
            // last day of that month
            const lastDay = new Date(y, d.getMonth() + 1, 0);
            const to = i === 0 ? today : localDateStr(lastDay);
            bars.push({ label: `${y}-${mo}`, from, to });
        }
        return { from: bars[0].from, to: today, bars };
    }

    if (mode === 'week') {
        const bars = [];
        // Monday of current week
        const dow = now.getDay(); // 0=Sun
        const mondayOffset = dow === 0 ? -6 : 1 - dow;
        for (let i = 9; i >= 0; i--) {
            const mon = new Date(now.getFullYear(), now.getMonth(), now.getDate() + mondayOffset - i * 7);
            const sun = new Date(mon.getFullYear(), mon.getMonth(), mon.getDate() + 6);
            const from = localDateStr(mon);
            const to = i === 0 ? today : localDateStr(sun);
            // ISO week number
            const wn = isoWeekNumber(mon);
            bars.push({ label: `W${wn}`, from, to });
        }
        return { from: bars[0].from, to: today, bars };
    }

    // day mode: use month picker
    const picker = document.getElementById('cost-month-picker');
    const monthVal = picker && picker.value ? picker.value : `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}`;
    const [y, mo] = monthVal.split('-').map(Number);
    const from = `${y}-${String(mo).padStart(2, '0')}-01`;
    const lastDay = new Date(y, mo, 0); // last day of that month
    const to = monthVal === `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}` ? today : localDateStr(lastDay);
    const bars = [];
    const start = new Date(y, mo - 1, 1);
    const end = new Date(to + 'T00:00:00');
    for (let d = new Date(start); d <= end; d.setDate(d.getDate() + 1)) {
        const ds = localDateStr(d);
        bars.push({ label: ds, from: ds, to: ds });
    }
    return { from, to, bars };
}

function isoWeekNumber(date) {
    const d = new Date(Date.UTC(date.getFullYear(), date.getMonth(), date.getDate()));
    const dayNum = d.getUTCDay() || 7;
    d.setUTCDate(d.getUTCDate() + 4 - dayNum);
    const yearStart = new Date(Date.UTC(d.getUTCFullYear(), 0, 1));
    return Math.ceil((((d - yearStart) / 86400000) + 1) / 7);
}

// Aggregate by_day rows into per-bar segments with cost
// Returns Map<label, [{model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, request_count, cost}]>
function aggregateByPeriod(byDay, bars) {
    // Pre-compute cost for each day row
    const rows = byDay.map(d => {
        const price = lookupProviderPrice(d.model);
        const cost = d.input_tokens          * (price ? price.in         : 5)    / 1e6
                   + d.output_tokens         * (price ? price.out        : 25)   / 1e6
                   + d.cache_creation_tokens * (price ? price.cacheWrite : 3.75) / 1e6
                   + d.cache_read_tokens     * (price ? price.cacheRead  : 0.3)  / 1e6;
        return { ...d, cost };
    });

    const result = new Map();
    for (const bar of bars) result.set(bar.label, []);

    for (const row of rows) {
        for (const bar of bars) {
            if (row.date >= bar.from && row.date <= bar.to) {
                const segs = result.get(bar.label);
                const existing = segs.find(s => s.model === row.model);
                if (existing) {
                    existing.input_tokens += row.input_tokens;
                    existing.output_tokens += row.output_tokens;
                    existing.cache_creation_tokens += row.cache_creation_tokens;
                    existing.cache_read_tokens += row.cache_read_tokens;
                    existing.request_count += row.request_count;
                    existing.cost += row.cost;
                } else {
                    segs.push({ ...row });
                }
                break; // each day belongs to exactly one bar
            }
        }
    }
    return result;
}

// ── Load & render cost view ──

export async function loadCosts() {
    closeDrilldown();
    document.getElementById('cost-loading').classList.remove('hidden');
    try {
        const { from, to, bars } = getCostModeRange(_costMode);
        const resp = await fetch(`/api/costs?from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`);
        const data = await resp.json();
        renderCostView(data, bars);
    } catch (e) {
        console.error('Failed to load costs:', e);
    }
    document.getElementById('cost-loading').classList.add('hidden');
}

function renderCostView(data, bars) {
    const byModel = calcModelCosts(data.by_model || []);
    const totalCost = byModel.reduce((s, m) => s + m.cost, 0);
    const totalIn = byModel.reduce((s, m) => s + m.input_tokens, 0);
    const totalOut = byModel.reduce((s, m) => s + m.output_tokens, 0);
    const totalReqs = byModel.reduce((s, m) => s + m.request_count, 0);
    const totalCache = (data.by_model || []).reduce((s, m) => s + (m.cache_creation_tokens || 0), 0);

    document.getElementById('cost-total').textContent = formatCostValue(totalCost);
    document.getElementById('cost-input-tokens').textContent = formatTokensFixed(totalIn);
    document.getElementById('cost-output-tokens').textContent = formatTokensFixed(totalOut);
    document.getElementById('cost-request-count').textContent = formatReqCount(totalReqs);
    document.getElementById('cost-cache-tokens').textContent = formatTokens(totalCache);

    // Update chart title
    const titleEl = document.getElementById('cost-chart-title');
    if (titleEl) titleEl.textContent = chartTitle(_costMode, bars);

    const barMap = aggregateByPeriod(data.by_day || [], bars);
    const colorMap = buildModelColorMap(data.by_day || []);
    renderCostChart(barMap, colorMap, bars);

    renderCostBySession(data.by_session || [], 'cost-by-session');
    renderCostByModel(byModel, 'cost-by-model');
    renderCostByProvider(data.by_provider || [], 'cost-by-provider');
}

function chartTitle(mode, bars) {
    if (mode === 'month') return t('cost.chart_title_month');
    if (mode === 'week')  return t('cost.chart_title_week');
    // day: show which month
    const picker = document.getElementById('cost-month-picker');
    return picker && picker.value ? picker.value : t('cost.chart_title_day');
}

// ── Chart ──

const MODEL_PALETTE = [
    '#6aabcc','#e8906a','#7ec8a0','#c47bb0','#e8c46a',
    '#7ab0e8','#e87a7a','#85c8b8','#b09ae8','#c8b07a',
    '#5a9bb8','#d4795a','#6ab890','#b46aa0','#d4b45a',
];

export function buildModelColorMap(byDay) {
    const seen = [];
    for (const row of byDay) {
        if (row.model && !seen.includes(row.model)) seen.push(row.model);
    }
    const map = {};
    seen.forEach((model, idx) => {
        const provider = findProviderForModel(model);
        map[model] = {
            color: MODEL_PALETTE[idx % MODEL_PALETTE.length],
            providerName: provider ? provider.name : t('cost.unknown_provider'),
        };
    });
    return map;
}

// Store current chart state for redraw on click
let _chartState = null;

function renderCostChart(barMap, colorMap, bars) {
    const canvas = document.getElementById('cost-daily-chart');
    const emptyEl = document.getElementById('cost-chart-empty');
    const tooltip = document.getElementById('cost-chart-tooltip');

    const hasData = Array.from(barMap.values()).some(segs => segs.some(s => s.cost > 0));
    if (!hasData) {
        canvas.classList.add('hidden');
        emptyEl.classList.remove('hidden');
        updateChartLegend({});
        _chartState = null;
        return;
    }
    canvas.classList.remove('hidden');
    emptyEl.classList.add('hidden');
    updateChartLegend(colorMap);

    _chartState = { barMap, colorMap, bars };
    _drawChart(canvas, barMap, colorMap, bars, _selectedBarIdx, tooltip);

    canvas.onclick = (e) => {
        const bnd = canvas.getBoundingClientRect();
        const mx = e.clientX - bnd.left;
        const PAD_L = 54, PAD_R = 16;
        const W = bnd.width;
        const n = bars.length;
        const barW = (W - PAD_L - PAD_R) / n;
        const idx = Math.floor((mx - PAD_L) / barW);
        if (idx < 0 || idx >= n) return;

        if (_selectedBarIdx === idx) {
            // Toggle off
            _selectedBarIdx = -1;
            closeDrilldown();
        } else {
            _selectedBarIdx = idx;
            loadDrilldown(bars[idx]);
        }
        _drawChart(canvas, barMap, colorMap, bars, _selectedBarIdx, tooltip);
    };
}

function _drawChart(canvas, barMap, colorMap, bars, selectedIdx, tooltip) {
    const PAD_L = 54, PAD_R = 16, PAD_T = 12, PAD_B = 36;
    const BAR_GAP = 0.28;

    const dpr = window.devicePixelRatio || 1;
    const rect = canvas.getBoundingClientRect();
    const W = rect.width || canvas.parentElement.clientWidth || 600;
    const H = rect.height || 180;
    canvas.width  = Math.round(W * dpr);
    canvas.height = Math.round(H * dpr);
    const ctx = canvas.getContext('2d');
    ctx.scale(dpr, dpr);

    const cs = getComputedStyle(document.documentElement);
    const COL_GRID = cs.getPropertyValue('--chart-grid').trim() || '#e8eaf0';
    const COL_TEXT = cs.getPropertyValue('--text-muted').trim() || '#8a8a9a';
    const COL_SEL  = 'rgba(74,144,164,0.18)';

    const chartW = W - PAD_L - PAD_R;
    const chartH = H - PAD_T - PAD_B;
    const n = bars.length;
    const barW = chartW / n;
    const innerBarW = barW * (1 - BAR_GAP);

    const maxCost = Math.max(
        ...Array.from(barMap.values()).map(segs => segs.reduce((s, seg) => s + seg.cost, 0)),
        0.000001
    );

    // Grid lines
    ctx.strokeStyle = COL_GRID;
    ctx.lineWidth = 1;
    const GRID_LINES = 4;
    for (let i = 0; i <= GRID_LINES; i++) {
        const y = PAD_T + chartH * (1 - i / GRID_LINES);
        ctx.beginPath(); ctx.moveTo(PAD_L, y); ctx.lineTo(W - PAD_R, y); ctx.stroke();
        ctx.fillStyle = COL_TEXT;
        ctx.font = '10px system-ui,sans-serif';
        ctx.textAlign = 'right';
        ctx.textBaseline = 'middle';
        ctx.fillText(formatCostShort(maxCost * i / GRID_LINES), PAD_L - 6, y);
    }

    const scale = chartH / maxCost;
    bars.forEach((bar, i) => {
        const segs = barMap.get(bar.label) || [];
        const x = PAD_L + i * barW + (barW - innerBarW) / 2;
        const totalCostBar = segs.reduce((s, seg) => s + seg.cost, 0);

        if (totalCostBar <= 0) {
            ctx.fillStyle = COL_GRID;
            ctx.fillRect(x, PAD_T + chartH - 1, innerBarW, 1);
        } else {
            let yBase = PAD_T + chartH;
            for (const seg of segs) {
                if (seg.cost <= 0) continue;
                const h = Math.max(seg.cost * scale, 1);
                ctx.fillStyle = colorMap[seg.model]?.color || '#999';
                ctx.fillRect(x, yBase - h, innerBarW, h);
                yBase -= h;
            }
        }

        // Selected bar highlight overlay
        if (i === selectedIdx) {
            ctx.fillStyle = COL_SEL;
            ctx.fillRect(x - 2, PAD_T, innerBarW + 4, chartH);
            ctx.strokeStyle = 'var(--accent2, #4a90a4)';
            ctx.lineWidth = 1.5;
            ctx.strokeRect(x - 2, PAD_T, innerBarW + 4, chartH);
        }

        // X-axis label
        const showLabel = n <= 14 || i % Math.ceil(n / 14) === 0 || i === n - 1;
        if (showLabel) {
            ctx.fillStyle = i === selectedIdx ? '#4a90a4' : COL_TEXT;
            ctx.font = i === selectedIdx ? 'bold 10px system-ui,sans-serif' : '10px system-ui,sans-serif';
            ctx.textAlign = 'center';
            ctx.textBaseline = 'top';
            ctx.fillText(formatBarLabel(bar.label, _costMode), x + innerBarW / 2, PAD_T + chartH + 6);
        }
    });

    // Hover tooltip
    canvas.onmousemove = (e) => {
        const bnd = canvas.getBoundingClientRect();
        const mx = e.clientX - bnd.left;
        const my = e.clientY - bnd.top;
        const idx = Math.floor((mx - PAD_L) / barW);
        if (idx < 0 || idx >= n || mx < PAD_L || mx > W - PAD_R) {
            tooltip.classList.add('hidden'); return;
        }
        const bar = bars[idx];
        const segs = barMap.get(bar.label) || [];
        const totalCost = segs.reduce((s, seg) => s + seg.cost, 0);
        const totalReqs = segs.reduce((s, seg) => s + seg.request_count, 0);

        const byProv = {};
        for (const seg of segs) {
            const pName = colorMap[seg.model]?.providerName || '?';
            if (!byProv[pName]) byProv[pName] = [];
            byProv[pName].push(seg);
        }
        let html = `<div class="tt-date">${bar.label}${_costMode !== 'day' ? ` (${bar.from} ~ ${bar.to})` : ''}</div>`;
        for (const [pName, pSegs] of Object.entries(byProv)) {
            const pCost = pSegs.reduce((s, seg) => s + seg.cost, 0);
            const pIn   = pSegs.reduce((s, seg) => s + seg.input_tokens, 0);
            const pOut  = pSegs.reduce((s, seg) => s + seg.output_tokens, 0);
            html += `<div class="tt-provider">${esc(pName)} &nbsp;<span class="tt-prov-cost">¥${pCost.toFixed(4)}</span><span class="tt-prov-tokens"> ${formatTokens(pIn)}↑ ${formatTokens(pOut)}↓</span></div>`;
            for (const seg of pSegs) {
                const col = colorMap[seg.model]?.color || '#999';
                html += `<div class="tt-row tt-model-row"><span class="tt-dot" style="background:${col}"></span><span class="tt-model-id">${esc(seg.model)}</span><span class="tt-model-tokens">${formatTokens(seg.input_tokens)}↑ ${formatTokens(seg.output_tokens)}↓</span><span class="tt-model-cost">¥${seg.cost.toFixed(4)}</span></div>`;
            }
        }
        html += `<div class="tt-total">Total &nbsp;¥${totalCost.toFixed(4)} &nbsp;· ${totalReqs} reqs</div>`;
        if (totalCost > 0) html += `<div class="tt-hint">${t('cost.click_drilldown')}</div>`;
        tooltip.innerHTML = html;
        tooltip.classList.remove('hidden');

        const tw = tooltip.offsetWidth;
        let tx = mx + 12;
        if (tx + tw > W - 8) tx = mx - tw - 12;
        tooltip.style.left = `${tx}px`;
        tooltip.style.top  = `${Math.max(4, my - 20)}px`;
    };
    canvas.onmouseleave = () => tooltip.classList.add('hidden');
}

function formatBarLabel(label, mode) {
    if (mode === 'day') {
        // label = YYYY-MM-DD
        return label.slice(5).replace('-', '/'); // MM/DD
    }
    if (mode === 'week') return label; // W27
    // month: YYYY-MM → MM月 (or just MM)
    return label.slice(5) + '月';
}

// ── Drill-down ──

async function loadDrilldown(bar) {
    const titleEl = document.getElementById('cost-drilldown-title');
    const drillEl = document.getElementById('cost-drilldown');
    if (titleEl) titleEl.textContent = drilldownTitle(bar);
    drillEl.classList.remove('hidden');

    // Show loading state
    ['drill-by-session', 'drill-by-model', 'drill-by-provider'].forEach(id => {
        const el = document.getElementById(id);
        if (el) el.innerHTML = `<tr><td colspan="5" style="text-align:center;color:var(--text-muted)">…</td></tr>`;
    });

    try {
        const resp = await fetch(`/api/costs?from=${encodeURIComponent(bar.from)}&to=${encodeURIComponent(bar.to)}`);
        const data = await resp.json();
        const byModel = calcModelCosts(data.by_model || []);
        renderCostBySession(data.by_session || [], 'drill-by-session');
        renderCostByModel(byModel, 'drill-by-model');
        renderCostByProvider(data.by_provider || [], 'drill-by-provider');
    } catch (e) {
        console.error('Failed to load drilldown:', e);
    }

    drillEl.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
}

function drilldownTitle(bar) {
    if (_costMode === 'day') return bar.label;
    if (_costMode === 'week') return `${bar.label}  (${bar.from} ~ ${bar.to})`;
    return bar.label; // YYYY-MM
}

function closeDrilldown() {
    _selectedBarIdx = -1;
    document.getElementById('cost-drilldown').classList.add('hidden');
    // Redraw chart to clear highlight
    if (_chartState) {
        const { barMap, colorMap, bars } = _chartState;
        const tooltip = document.getElementById('cost-chart-tooltip');
        _drawChart(document.getElementById('cost-daily-chart'), barMap, colorMap, bars, -1, tooltip);
    }
}

// ── Legend ──

export function updateChartLegend(colorMap) {
    const container = document.getElementById('cost-chart-legend');
    if (!container) return;
    const entries = Object.entries(colorMap);
    if (entries.length === 0) { container.innerHTML = ''; return; }
    const byProv = {};
    for (const [model, info] of entries) {
        if (!byProv[info.providerName]) byProv[info.providerName] = [];
        byProv[info.providerName].push({ model, color: info.color });
    }
    container.innerHTML = Object.entries(byProv).map(([pName, models]) =>
        `<span class="cost-legend-provider">${esc(pName)}:</span>` +
        models.map(({ model, color }) =>
            `<span class="cost-legend-item"><span class="cost-legend-dot" style="background:${color}"></span><span class="cost-legend-label">${esc(model)}</span></span>`
        ).join('')
    ).join('');
}

// ── Tables (shared by main view and drill-down) ──

export function renderCostBySession(bySess, tbodyId) {
    const tbody = document.getElementById(tbodyId);
    if (!tbody) return;
    if (bySess.length === 0) {
        tbody.innerHTML = `<tr><td colspan="5" style="text-align:center;color:var(--text-muted)">${t('cost.no_data')}</td></tr>`;
        return;
    }
    tbody.innerHTML = bySess.map(s => {
        const price = s.models.length > 0 ? lookupProviderPrice(s.models[0]) : null;
        const sessionCost = s.input_tokens          * (price ? price.in         : 5)    / 1e6
                          + s.output_tokens         * (price ? price.out        : 25)   / 1e6
                          + s.cache_creation_tokens * (price ? price.cacheWrite : 3.75) / 1e6
                          + s.cache_read_tokens     * (price ? price.cacheRead  : 0.3)  / 1e6;
        // Show last 12 chars of session_id as short label
        const shortId = s.session_id ? s.session_id.slice(-12) : '—';
        return `<tr>
            <td title="${esc(s.session_id)}"><a href="#" class="cost-session-link" data-session-id="${esc(s.session_id)}" style="font-family:monospace;font-size:0.78rem">${esc(shortId)}</a></td>
            <td style="font-size:0.72rem;color:var(--text-muted)">${esc(s.models.join(', '))}</td>
            <td>${s.request_count}</td>
            <td>${formatTokens(s.input_tokens)} / ${formatTokens(s.output_tokens)}</td>
            <td>${formatCostValue(sessionCost)}</td>
        </tr>`;
    }).join('');
}

export function renderCostByModel(byModel, tbodyId) {
    const tbody = document.getElementById(tbodyId);
    if (!tbody) return;
    if (byModel.length === 0) {
        tbody.innerHTML = `<tr><td colspan="4" style="text-align:center;color:var(--text-muted)">${t('cost.no_data')}</td></tr>`;
        return;
    }
    tbody.innerHTML = byModel.map(m => `<tr>
        <td>${esc(m.model)}</td>
        <td>${m.request_count}</td>
        <td>${formatTokens(m.input_tokens)} / ${formatTokens(m.output_tokens)}</td>
        <td>${formatCostValue(m.cost)}</td>
    </tr>`).join('');
}

export function renderCostByProvider(byModel, tbodyId) {
    const tbody = document.getElementById(tbodyId);
    if (!tbody) return;
    const providers = groupByProvider(byModel);
    if (providers.length === 0) {
        tbody.innerHTML = `<tr><td colspan="4" style="text-align:center;color:var(--text-muted)">${t('cost.no_data')}</td></tr>`;
        return;
    }
    tbody.innerHTML = providers.map(p => `<tr>
        <td>${esc(p.provider)}</td>
        <td>${formatTokens(p.input_tokens)}</td>
        <td>${formatTokens(p.output_tokens)}</td>
        <td>${formatCostValue(p.cost)}</td>
    </tr>`).join('');
}

// ── Event listeners ──

// Initialize month picker to current month
const _now = new Date();
document.getElementById('cost-month-picker').value =
    `${_now.getFullYear()}-${String(_now.getMonth() + 1).padStart(2, '0')}`;

document.querySelectorAll('.btn-cost-mode').forEach(btn => {
    btn.addEventListener('click', () => {
        document.querySelectorAll('.btn-cost-mode').forEach(b => b.classList.remove('active'));
        btn.classList.add('active');
        _costMode = btn.dataset.mode;

        // Show/hide month picker
        const picker = document.getElementById('cost-month-picker');
        if (picker) picker.classList.toggle('hidden', _costMode !== 'day');

        _selectedBarIdx = -1;
        loadCosts();
    });
});

document.getElementById('btn-cost-refresh').addEventListener('click', loadCosts);

// Navigate to Inspector and select session when clicking session links in cost tables
document.addEventListener('click', e => {
    const link = e.target.closest('.cost-session-link');
    if (!link) return;
    e.preventDefault();
    const sid = link.dataset.sessionId;
    if (!sid) return;
    // Switch to inspector tab
    const navLink = document.querySelector('nav a[data-view="inspector"]');
    if (navLink) navLink.click();
    // Select the session (deferred to let inspector render first)
    setTimeout(() => {
        if (window.selectSession) window.selectSession(sid);
    }, 50);
});

document.getElementById('btn-drilldown-close').addEventListener('click', closeDrilldown);

// Month picker change (day mode)
document.getElementById('cost-month-picker').addEventListener('change', () => {
    if (_costMode === 'day') loadCosts();
});

// Redraw chart when container resized
if (typeof ResizeObserver !== 'undefined') {
    new ResizeObserver(() => {
        const viewCost = document.getElementById('view-cost');
        if (viewCost && viewCost.classList.contains('active') && _chartState) {
            const { barMap, colorMap, bars } = _chartState;
            const tooltip = document.getElementById('cost-chart-tooltip');
            _drawChart(document.getElementById('cost-daily-chart'), barMap, colorMap, bars, _selectedBarIdx, tooltip);
        }
    }).observe(document.querySelector('.cost-chart-container'));
}
