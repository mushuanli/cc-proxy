import { state } from './state.js';
import { t } from './i18n.js';
import { esc } from './utils.js';

// ── Shared state update from server ──

export function applyUpstreamState(active, codexActive, proxyActive, upstreams, providers, effort, pricing, httpProxy) {
    state.activeUpstream = active;
    state.activeCodexUpstream = codexActive || '';
    state.activeProxyUpstream = proxyActive || active;
    state.upstreamList = upstreams || [];
    state.providerList = providers || [];
    state.modelPricingList = pricing || [];
    state.globalProxy = httpProxy || null;
    if (effort !== undefined) { state.activeEffort = effort; }
    populateUpstreamSelect(upstreams, active);
    populateCodexUpstreamSelect(upstreams, state.activeCodexUpstream);

    populateEffortSelect(state.activeEffort);
    renderModelMatrix();
    renderUpstreamTable();
    refreshProviderSelects();
    renderGlobalProxy();
    // updateInspectorCostStats is hoisted to window by main.js
    if (typeof window._updateInspectorCostStats === 'function') window._updateInspectorCostStats();
}

function renderGlobalProxy() {
    const input = document.getElementById('global-proxy-input');
    if (input) {
        input.value = state.globalProxy || '';
    }
}

// ── Upstream / Effort selects (Inspector toolbar) ──

export function populateUpstreamSelect(upstreams, active) {
    const select = document.getElementById('upstream-select');
    select.innerHTML = '';
    if (!upstreams || upstreams.length === 0) {
        select.innerHTML = '<option value="">— no upstreams —</option>';
        return;
    }
    [...upstreams].sort((a, b) => a.name.localeCompare(b.name)).forEach(u => {
        const opt = document.createElement('option');
        opt.value = u.name;
        opt.textContent = u.name + (u.active ? ' ✓' : '');
        if (u.name === active || u.active) opt.selected = true;
        select.appendChild(opt);
    });
}

export function populateCodexUpstreamSelect(upstreams, active) {
    const select = document.getElementById('codex-upstream-select');
    if (!select) return;
    select.innerHTML = '';
    if (!upstreams || upstreams.length === 0) {
        select.innerHTML = '<option value="">—</option>';
        return;
    }
    [...upstreams].sort((a, b) => a.name.localeCompare(b.name)).forEach(u => {
        const opt = document.createElement('option');
        opt.value = u.name;
        opt.textContent = u.name + (u.codex_active ? ' ✓' : '');
        if (u.name === active || u.codex_active) opt.selected = true;
        select.appendChild(opt);
    });
}

export function populateEffortSelect(active) {
    const select = document.getElementById('effort-select');
    select.innerHTML = '';
    state.EFFORT_LEVELS.forEach(level => {
        const opt = document.createElement('option');
        opt.value = level;
        opt.textContent = level === 'auto' ? 'pass' : level;
        if (level === active) opt.selected = true;
        select.appendChild(opt);
    });
}

export function updateCaptureButton() {
    const btn = document.getElementById('btn-toggle-capture');
    const status = document.getElementById('capture-status');
    if (state.captureEnabled) {
        btn.textContent = t('inspector.recording');
        btn.classList.add('recording');
        status.innerHTML = '<span class="rec-dot"></span> ' + t('inspector.rec_dot');
    } else {
        btn.textContent = t('inspector.record');
        btn.classList.remove('recording');
        status.innerHTML = '';
    }
}

// ── Model Matrix ──

export function renderModelMatrix() {
    const head = document.getElementById('model-matrix-head');
    const body = document.getElementById('model-matrix-body');
    if (!head || !body) return;

    const providerCols = state.providerList.map(p => p.name);

    head.innerHTML = `<tr>
        <th class="mx-th mx-col-id">Model ID</th>
        <th class="mx-th mx-col-price" title="Input / Million tokens">In</th>
        <th class="mx-th mx-col-price" title="Output / Million tokens">Out</th>
        <th class="mx-th mx-col-price" title="Cache Write / Million tokens">CW</th>
        <th class="mx-th mx-col-price" title="Cache Read / Million tokens">CR</th>
        ${providerCols.map(p => `<th class="mx-th mx-col-provider mx-th-provider" data-prov="${esc(p)}" title="Edit ${esc(p)}">${esc(p)}</th>`).join('')}
        <th class="mx-th mx-col-del"></th>
    </tr>`;

    if (state.modelPricingList.length === 0) {
        body.innerHTML = `<tr><td colspan="${5 + providerCols.length + 1}" class="mx-empty">${t('settings.no_model_pricing')}</td></tr>`;
        return;
    }

    body.innerHTML = state.modelPricingList.map(mp => {
        const price = mp.price || [];
        const pIn  = price[0] ?? '';
        const pOut = price[1] ?? '';
        const pCw  = price[2] ?? '';
        const pCr  = price[3] ?? '';

        const providerCells = providerCols.map(prov => {
            const names = mp.providers?.[prov];
            if (!names) return `<td class="mx-td mx-cell-provider mx-cell-none" data-mid="${esc(mp.id)}" data-prov="${esc(prov)}"><span class="mx-none">—</span></td>`;
            const label = names.length === 0 ? `<span class="mx-default">= id</span>` : `<span class="mx-names">${esc(names.join(', '))}</span>`;
            return `<td class="mx-td mx-cell-provider" data-mid="${esc(mp.id)}" data-prov="${esc(prov)}">${label}</td>`;
        }).join('');

        return `<tr class="mx-row" data-mid="${esc(mp.id)}">
            <td class="mx-td mx-col-id"><span class="mx-model-id">${esc(mp.id)}</span></td>
            <td class="mx-td mx-cell-price" data-mid="${esc(mp.id)}" data-idx="0"><span>${pIn}</span></td>
            <td class="mx-td mx-cell-price" data-mid="${esc(mp.id)}" data-idx="1"><span>${pOut}</span></td>
            <td class="mx-td mx-cell-price" data-mid="${esc(mp.id)}" data-idx="2"><span>${pCw !== '' ? pCw : '<em class="mx-auto">auto</em>'}</span></td>
            <td class="mx-td mx-cell-price" data-mid="${esc(mp.id)}" data-idx="3"><span>${pCr !== '' ? pCr : '<em class="mx-auto">auto</em>'}</span></td>
            ${providerCells}
            <td class="mx-td mx-col-del"><button class="mx-del-btn" data-mid="${esc(mp.id)}" title="Delete">×</button></td>
        </tr>`;
    }).join('');

    // Add-row at the bottom
    body.innerHTML += `<tr class="mx-add-row">
        <td class="mx-td" colspan="${5 + providerCols.length + 1}">
            <button id="btn-matrix-add-row" class="mx-add-row-btn">+ ${t('settings.add_model')}</button>
        </td>
    </tr>`;

    bindMatrixEvents();
}

export function bindMatrixEvents() {
    const body = document.getElementById('model-matrix-body');
    if (!body) return;

    // Price cell: click → inline input
    body.querySelectorAll('.mx-cell-price').forEach(td => {
        td.addEventListener('click', () => startPriceEdit(td));
    });

    // Provider cell: click → alias popover
    body.querySelectorAll('.mx-cell-provider, .mx-cell-none').forEach(td => {
        td.addEventListener('click', (e) => {
            e.stopPropagation();
            openProviderPopover(td);
        });
    });

    // Provider header: click → URL/Token edit popover
    document.getElementById('model-matrix-head').querySelectorAll('.mx-th-provider').forEach(th => {
        th.addEventListener('click', (e) => {
            e.stopPropagation();
            openProviderHeaderPopover(th);
        });
    });

    // Delete row
    body.querySelectorAll('.mx-del-btn').forEach(btn => {
        btn.addEventListener('click', () => deleteModelPricing(btn.dataset.mid));
    });

    // Add row button
    const addBtn = document.getElementById('btn-matrix-add-row');
    if (addBtn) addBtn.addEventListener('click', openAddModelDialog);
}

// ── Inline price edit ──

export function startPriceEdit(td) {
    if (td.querySelector('input')) return; // already editing
    closeMatrixPopover();
    const mid = td.dataset.mid;
    const idx = parseInt(td.dataset.idx);
    const mp = state.modelPricingList.find(m => m.id === mid);
    if (!mp) return;
    const current = mp.price?.[idx] ?? '';

    const input = document.createElement('input');
    input.type = 'number';
    input.className = 'mx-price-input';
    input.value = current;
    input.min = '0';
    input.step = '0.0001';
    input.placeholder = idx >= 2 ? 'auto' : '0';
    td.innerHTML = '';
    td.appendChild(input);
    input.focus();
    input.select();

    const commit = async () => {
        const val = input.value.trim() === '' ? null : parseFloat(input.value);
        const p = [null, null, null, null];
        (mp.price || []).slice(0, 4).forEach((v, i) => { p[i] = v; });
        p[idx] = val;
        let price;
        if (p[2] == null && p[3] == null) {
            price = [p[0] ?? 0, p[1] ?? 0];
        } else {
            const cw = p[2] ?? ((p[0] ?? 0) * 1.25);
            const cr = p[3] ?? ((p[0] ?? 0) * 0.1);
            price = [p[0] ?? 0, p[1] ?? 0, cw, cr];
        }
        await saveMpField(mid, { price });
    };

    input.addEventListener('blur', commit);
    input.addEventListener('keydown', e => {
        if (e.key === 'Enter') { e.preventDefault(); input.blur(); }
        if (e.key === 'Escape') { renderModelMatrix(); }
    });
}

export async function saveMpField(id, fields) {
    const mp = state.modelPricingList.find(m => m.id === id);
    if (!mp) return;
    const body = { id, price: mp.price || [], providers: mp.providers || {}, ...fields };
    const resp = await fetch(`/api/model-pricing/${encodeURIComponent(id)}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    });
    if (!resp.ok) {
        const err = await resp.json().catch(() => ({}));
        alert(err.error || `Failed to update model pricing (${resp.status})`);
    }
}

// ── Provider cell popover ──

export function openProviderPopover(td) {
    closeMatrixPopover();
    const mid = td.dataset.mid;
    const provName = td.dataset.prov;
    const mp = state.modelPricingList.find(m => m.id === mid);
    if (!mp) return;

    const current = mp.providers?.[provName] ?? null;
    const currentStr = current === null ? '' : current.join(', ');

    const pop = document.createElement('div');
    pop.className = 'mx-popover';
    pop.innerHTML = `
        <div class="mx-pop-title">${esc(provName)} → <em>${esc(mid)}</em></div>
        <div class="mx-pop-hint">${t('settings.mx_alias_hint')}</div>
        <input class="mx-pop-input" type="text" value="${esc(currentStr)}" placeholder="${esc(mid)}">
        <div class="mx-pop-actions">
            ${current !== null ? `<button class="mx-pop-remove btn-danger">${t('settings.mx_alias_remove')}</button>` : ''}
            <button class="mx-pop-save btn-primary">${t('settings.save')}</button>
            <button class="mx-pop-cancel">${t('settings.cancel')}</button>
        </div>`;

    const rect = td.getBoundingClientRect();
    const tableRect = document.getElementById('model-matrix-table').getBoundingClientRect();
    pop.style.top = (rect.bottom - tableRect.top + 4) + 'px';
    pop.style.left = Math.max(0, rect.left - tableRect.left) + 'px';

    document.getElementById('model-matrix-table').style.position = 'relative';
    document.getElementById('model-matrix-table').appendChild(pop);
    state._matrixPopover = { mid, provName, el: pop };

    pop.querySelector('.mx-pop-input').focus();

    pop.querySelector('.mx-pop-save').addEventListener('click', async () => {
        const raw = pop.querySelector('.mx-pop-input').value.trim();
        const names = raw ? raw.split(',').map(s => s.trim()).filter(Boolean) : [];
        const mp2 = state.modelPricingList.find(m => m.id === mid);
        if (!mp2) return;
        const providers = { ...(mp2.providers || {}) };
        providers[provName] = names;  // empty [] means "use model id"
        await saveMpField(mid, { providers });
        closeMatrixPopover();
    });

    const removeBtn = pop.querySelector('.mx-pop-remove');
    if (removeBtn) {
        removeBtn.addEventListener('click', async () => {
            const mp2 = state.modelPricingList.find(m => m.id === mid);
            if (!mp2) return;
            const providers = { ...(mp2.providers || {}) };
            delete providers[provName];
            await saveMpField(mid, { providers });
            closeMatrixPopover();
        });
    }

    pop.querySelector('.mx-pop-cancel').addEventListener('click', closeMatrixPopover);
}

export function closeMatrixPopover() {
    if (state._matrixPopover) {
        state._matrixPopover.el.remove();
        state._matrixPopover = null;
    }
}

// ── Provider header popover (edit URL/Token) ──

function providerPopoverHtml(title, p) {
    const hasToken = p?.has_token ? ` · 🔑` : '';
    return `
        <div class="mx-pop-title">${esc(title)}${hasToken}</div>
        <label class="mx-pop-field-label">URL</label>
        <input class="mx-pop-input mx-pop-url" type="text" value="${p ? esc(p.url) : ''}" placeholder="https://api.example.com">
        <label class="mx-pop-field-label">Codex URL</label>
        <input class="mx-pop-input mx-pop-codex-url" type="text" value="${p?.codex_url ? esc(p.codex_url) : ''}" placeholder="https://api.example.com/v1">
        <label class="mx-pop-field-label">Protocols</label>
        <div class="pe-protocols mx-pop-protocols">
            <label><input type="checkbox" class="mx-pop-proto-a" ${(p?.protocols || []).includes('anthropic') ? 'checked' : ''}> ${t('settings.proto_anthropic')}</label>
            <label><input type="checkbox" class="mx-pop-proto-c" ${(p?.protocols || []).includes('codex') ? 'checked' : ''}> ${t('settings.proto_codex')}</label>
        </div>
        <label class="mx-pop-field-label">Outbound network proxy</label>
        <input class="mx-pop-input mx-pop-proxy" type="text" value="${p?.proxy ? esc(p.proxy) : ''}" placeholder="仅 Relay 生效 / http://proxy:8080">
        <label class="mx-pop-field-label">${t('settings.token')} <span style="font-weight:normal;color:var(--text-muted)">(${t('settings.keep_current_token')})</span></label>
        <input class="mx-pop-input mx-pop-token" type="password" placeholder="sk-...">`;
}

export function openProviderHeaderPopover(th) {
    closeMatrixPopover();
    const provName = th.dataset.prov;
    const p = state.providerList.find(p => p.name === provName);
    if (!p) return;

    const pop = document.createElement('div');
    pop.className = 'mx-popover';
    pop.innerHTML = `
        ${providerPopoverHtml(provName, p)}
        <div class="mx-pop-actions" style="margin-top:4px">
            <button class="mx-pop-delete btn-danger">${t('settings.confirm_delete_provider').replace(' «{name}»?','')}</button>
            <button class="mx-pop-save btn-primary">${t('settings.save')}</button>
            <button class="mx-pop-cancel">${t('settings.cancel')}</button>
        </div>`;

    const rect = th.getBoundingClientRect();
    const tableRect = document.getElementById('model-matrix-table').getBoundingClientRect();
    pop.style.top = (rect.bottom - tableRect.top + 4) + 'px';
    pop.style.left = Math.max(0, rect.left - tableRect.left) + 'px';
    document.getElementById('model-matrix-table').style.position = 'relative';
    document.getElementById('model-matrix-table').appendChild(pop);
    state._matrixPopover = { el: pop };
    pop.querySelector('.mx-pop-url').focus();

    pop.querySelector('.mx-pop-save').addEventListener('click', async () => {
        const url = pop.querySelector('.mx-pop-url').value.trim();
        const codexUrl = pop.querySelector('.mx-pop-codex-url')?.value.trim() || null;
        const token = pop.querySelector('.mx-pop-token').value.trim();
        const proxy = pop.querySelector('.mx-pop-proxy').value.trim();
        if (!url) { alert(t('settings.name_url_required')); return; }
        const body = { name: provName, url, proxy: proxy || null };
        if (token) body.token = token;
        if (codexUrl) body.codex_url = codexUrl;
        const protocols = [];
        if (pop.querySelector('.mx-pop-proto-a')?.checked) protocols.push('anthropic');
        if (pop.querySelector('.mx-pop-proto-c')?.checked) protocols.push('codex');
        body.protocols = protocols;
        const resp = await fetch(`/api/providers/${encodeURIComponent(provName)}`, {
            method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
        });
        if (resp.ok) closeMatrixPopover();
        else alert(t('settings.failed_save_provider'));
    });
    pop.querySelector('.mx-pop-delete').addEventListener('click', async () => {
        if (!confirm(t('settings.confirm_delete_provider', { name: provName }))) return;
        await fetch(`/api/providers/${encodeURIComponent(provName)}`, { method: 'DELETE' });
        closeMatrixPopover();
    });
    pop.querySelector('.mx-pop-cancel').addEventListener('click', closeMatrixPopover);
}

export function openAddProviderPopover() {
    closeMatrixPopover();
    const btn = document.getElementById('btn-matrix-add-provider');

    const pop = document.createElement('div');
    pop.className = 'mx-popover';
    pop.innerHTML = `
        <div class="mx-pop-title">${t('settings.new_provider')}</div>
        <label class="mx-pop-field-label">${t('settings.name')}</label>
        <input class="mx-pop-input mx-pop-name" type="text" placeholder="deepseek">
        <label class="mx-pop-field-label">URL</label>
        <input class="mx-pop-input mx-pop-url" type="text" placeholder="https://api.deepseek.com">
        <label class="mx-pop-field-label">Codex URL (optional)</label>
        <input class="mx-pop-input mx-pop-codex-url" type="text" placeholder="https://api.deepseek.com/v1">
        <label class="mx-pop-field-label">Outbound network proxy</label>
        <input class="mx-pop-input mx-pop-proxy" type="text" placeholder="仅 Relay 生效 / http://proxy:8080">
        <label class="mx-pop-field-label">${t('settings.token')}</label>
        <input class="mx-pop-input mx-pop-token" type="password" placeholder="sk-...">
        <label class="mx-pop-field-label">Protocols</label>
        <div class="pe-protocols mx-pop-protocols">
            <label><input type="checkbox" class="mx-pop-proto-a"> ${t('settings.proto_anthropic')}</label>
            <label><input type="checkbox" class="mx-pop-proto-c"> ${t('settings.proto_codex')}</label>
        </div>
        <div class="mx-pop-actions" style="margin-top:4px">
            <button class="mx-pop-save btn-primary">${t('settings.save')}</button>
            <button class="mx-pop-cancel">${t('settings.cancel')}</button>
        </div>`;

    const tableEl = document.getElementById('model-matrix-table');
    const tableRect = tableEl.getBoundingClientRect();
    const btnRect = btn.getBoundingClientRect();
    pop.style.top = (btnRect.bottom - tableRect.top + 4) + 'px';
    pop.style.right = '0px';
    tableEl.style.position = 'relative';
    tableEl.appendChild(pop);
    state._matrixPopover = { el: pop };
    pop.querySelector('.mx-pop-name').focus();

    pop.querySelector('.mx-pop-save').addEventListener('click', async () => {
        const name = pop.querySelector('.mx-pop-name').value.trim();
        const url = pop.querySelector('.mx-pop-url').value.trim();
        const token = pop.querySelector('.mx-pop-token').value.trim();
        const proxy = pop.querySelector('.mx-pop-proxy').value.trim();
        if (!name || !url) { alert(t('settings.name_url_required')); return; }
        if (state.providerList.some(p => p.name === name)) { alert(`Provider '${name}' already exists`); return; }
        const body = { name, url, proxy: proxy || null };
        if (token) body.token = token;
        const codexUrl = pop.querySelector('.mx-pop-codex-url')?.value.trim();
        if (codexUrl) body.codex_url = codexUrl;
        const protocols = [];
        if (pop.querySelector('.mx-pop-proto-a')?.checked) protocols.push('anthropic');
        if (pop.querySelector('.mx-pop-proto-c')?.checked) protocols.push('codex');
        if (protocols.length > 0) body.protocols = protocols;
        const resp = await fetch('/api/providers', {
            method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
        });
        if (resp.ok) closeMatrixPopover();
        else alert(t('settings.failed_save_provider'));
    });
    pop.querySelector('.mx-pop-cancel').addEventListener('click', closeMatrixPopover);
}

// Close popover on outside click (ignore clicks inside the popover itself)
document.addEventListener('click', (e) => {
    if (!state._matrixPopover) return;
    if (e.target.closest('.mx-popover')) return;
    closeMatrixPopover();
});

// ── Add model dialog ──

export function openAddModelDialog() {
    closeMatrixPopover();
    const id = prompt('New model ID (e.g. claude-opus):');
    if (!id || !id.trim()) return;
    const tid = id.trim();
    if (state.modelPricingList.some(m => m.id === tid)) {
        alert(`Model '${tid}' already exists`);
        return;
    }
    fetch('/api/model-pricing', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id: tid, price: [], providers: {} }),
    });
}

export async function deleteModelPricing(id) {
    if (!confirm(`Delete model pricing '${id}'?`)) return;
    await fetch(`/api/model-pricing/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

// ── Settings: Providers ──

export function renderProviderList() {
    const container = document.getElementById('provider-list');
    if (state.providerList.length === 0) {
        container.innerHTML = '<div class="item-empty">' + t('settings.no_providers') + '</div>';
        return;
    }
    // Count how many models each provider has in the matrix
    const modelCounts = {};
    state.providerList.forEach(p => {
        modelCounts[p.name] = state.modelPricingList.filter(mp => mp.providers && p.name in mp.providers).length;
    });
    container.innerHTML = state.providerList.map(p => `
        <div class="item-row-wrap" id="provider-wrap-${esc(p.name)}">
            <div class="item-row">
                <div class="item-row-info">
                    <div class="item-row-name" style="cursor:pointer" onclick="openProviderEdit('${esc(p.name)}')">${esc(p.name)}</div>
                    <div class="item-row-meta">${esc(p.url)}${p.codex_url ? ` · codex: ${esc(p.codex_url)}` : ''}${p.proxy ? ` · proxy ${esc(p.proxy)}` : ''}${p.has_token ? ' · 🔑' : ''} · ${modelCounts[p.name] || 0} models</div>
                </div>
                <div class="item-row-actions">
                    <button class="btn-sm" onclick="openProviderEdit('${esc(p.name)}')">Edit</button>
                    <button class="btn-sm btn-danger" onclick="deleteProvider('${esc(p.name)}')">×</button>
                </div>
            </div>
        </div>`).join('');
}

export function openProviderEdit(name) {
    const p = name ? state.providerList.find(p => p.name === name) : null;
    state.providerEditMode = p ? 'edit' : 'add';

    if (p) {
        closeProviderEditAccordion();
        document.getElementById('provider-edit').classList.add('hidden');
        state.providerAccordionName = name;

        const wrap = document.getElementById(`provider-wrap-${name}`);
        if (!wrap) return;
        wrap.classList.add('item-expanded');

        const accordion = document.createElement('div');
        accordion.className = 'item-accordion';
        accordion.id = 'provider-accordion';
        accordion.innerHTML = `
            <div class="form-group">
                <label>URL</label>
                <input type="text" id="pe-url" value="${esc(p.url)}" placeholder="https://api.example.com">
            </div>
            <div class="form-group" id="pe-codex-url-group" style="display:${(p.protocols || []).includes('codex') || (p.protocols || []).length === 0 ? '' : 'none'}">
                <label>Codex URL</label>
                <input type="text" id="pe-codex-url" value="${esc(p.codex_url || '')}" placeholder="https://api.example.com/v1">
            </div>
            <div class="form-group">
                <label>Token</label>
                <input type="password" id="pe-token" placeholder="${t('settings.keep_current_token')}">
            </div>
            <div class="form-group">
                <label>Outbound network proxy</label>
                <input type="text" id="pe-proxy" value="${p.proxy ? esc(p.proxy) : ''}" placeholder="仅 Relay 生效 / http://proxy:8080">
            </div>
            <div class="form-group">
                <label>Protocols</label>
                <div class="pe-protocols">
                    <label><input type="checkbox" id="pe-proto-anthropic" ${(p.protocols || []).includes('anthropic') ? 'checked' : ''}> ${t('settings.proto_anthropic')}</label>
                    <label><input type="checkbox" id="pe-proto-codex" ${(p.protocols || []).includes('codex') ? 'checked' : ''}> ${t('settings.proto_codex')}</label>
                    <div class="pe-protocols-hint">${t('settings.proto_hint')}</div>
                </div>
            </div>
            <div class="form-actions">
                <button id="btn-provider-save" class="btn-primary">${t('settings.save')}</button>
                <button id="btn-provider-cancel">${t('settings.cancel')}</button>
            </div>`;
        wrap.appendChild(accordion);
        accordion.querySelector('#btn-provider-save').addEventListener('click', saveProvider);
        accordion.querySelector('#btn-provider-cancel').addEventListener('click', closeProviderEdit);
        // Toggle codex URL visibility based on protocol checkboxes.
        const cbCodex = accordion.querySelector('#pe-proto-codex');
        const cbAnthropic = accordion.querySelector('#pe-proto-anthropic');
        const codexUrlGroup = accordion.querySelector('#pe-codex-url-group');
        const syncCodexUrlVisibility = () => {
            if (codexUrlGroup && cbCodex) {
                // Show codex URL when codex is checked OR neither is checked (all protocols)
                const allOther = cbAnthropic && !cbAnthropic.checked;
                codexUrlGroup.style.display = (cbCodex.checked || allOther) ? '' : 'none';
            }
        };
        if (cbCodex) cbCodex.addEventListener('change', syncCodexUrlVisibility);
        if (cbAnthropic) cbAnthropic.addEventListener('change', syncCodexUrlVisibility);
        accordion.querySelector('#pe-url').focus();
    } else {
        closeProviderEditAccordion();
        document.getElementById('pe-name').value = '';
        document.getElementById('pe-name').disabled = false;
        document.getElementById('pe-url').value = '';
        document.getElementById('pe-token').value = '';
        document.getElementById('pe-proxy').value = '';
        document.getElementById('provider-edit').classList.remove('hidden');
        document.getElementById('pe-url').focus();
    }
}

export function closeProviderEditAccordion() {
    const acc = document.getElementById('provider-accordion');
    if (acc) acc.remove();
    if (state.providerAccordionName) {
        const wrap = document.getElementById(`provider-wrap-${state.providerAccordionName}`);
        if (wrap) wrap.classList.remove('item-expanded');
        state.providerAccordionName = null;
    }
}

export async function saveProvider() {
    const nameEl = document.getElementById('pe-name');
    const name = state.providerAccordionName || nameEl.value.trim();
    const url = document.getElementById('pe-url').value.trim();
    const token = document.getElementById('pe-token').value.trim();
    const proxy = document.getElementById('pe-proxy').value.trim();
    if (!name || !url) { alert(t('settings.name_url_required')); return; }

    const body = { name, url, proxy: proxy || null };
    if (token) body.token = token;
    const codexUrl = document.getElementById('pe-codex-url')?.value.trim();
    if (codexUrl) body.codex_url = codexUrl;

    // Collect selected protocols (omit when none selected = serve all).
    const protoEls = ['pe-proto-anthropic', 'pe-proto-codex'];
    const protocols = protoEls
        .filter(id => document.getElementById(id)?.checked)
        .map(id => id === 'pe-proto-anthropic' ? 'anthropic' : 'codex');
    if (protocols.length > 0) body.protocols = protocols;

    let resp;
    if (state.providerEditMode === 'add') {
        resp = await fetch('/api/providers', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    } else {
        resp = await fetch(`/api/providers/${encodeURIComponent(name)}`, { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    }
    if (resp.ok) {
        closeProviderEdit();
    } else {
        const err = await resp.json();
        alert(err.error || t('settings.failed_save_provider'));
    }
}

export function closeProviderEdit() {
    document.getElementById('provider-edit').classList.add('hidden');
    closeProviderEditAccordion();
    state.providerEditMode = null;
}

export async function deleteProvider(name) {
    if (!confirm(t('settings.confirm_delete_provider', { name }))) return;
    await fetch(`/api/providers/${encodeURIComponent(name)}`, { method: 'DELETE' });
}

// ── Settings: Upstreams (table mode) ──

export function renderUpstreamTable() {
    const head = document.getElementById('upstream-table-head');
    const body = document.getElementById('upstream-table-body');
    if (!head || !body) return;

    const proxyActiveName = state.activeProxyUpstream || '__forbid__';
    const isAuto = proxyActiveName === '__auto__';
    const isForbid = proxyActiveName === '__forbid__';
    head.innerHTML = `<tr>
        <th class="ut-th ut-col-name">${t('settings.name')}</th>
        <th class="ut-th ut-col-active ut-proxy-th${isAuto ? ' ut-proxy-th-auto' : ''}${isForbid ? ' ut-proxy-th-forbid' : ''}" data-name="${isForbid ? '__auto__' : '__forbid__'}" title="${isForbid ? 'Click to switch to Auto (auto-detect)' : 'Click to forbid transparent proxy'}">
            Transparent Proxy<span class="ut-proxy-th-active">${isForbid ? '⊘ Forbid' : (isAuto ? '◉ Auto' : esc(proxyActiveName))}</span>
        </th>
        <th class="ut-th ut-col-active">Relay</th>
        <th class="ut-th ut-col-tier">Opus</th>
        <th class="ut-th ut-col-tier">Sonnet</th>
        <th class="ut-th ut-col-tier">Haiku</th>
        <th class="ut-th ut-col-tier">${t('settings.tier_default')}</th>
        <th class="ut-th ut-col-effort">${t('settings.effort')}</th>
        <th class="ut-th ut-col-actions"></th>
    </tr>`;

    if (state.upstreamList.length === 0) {
        body.innerHTML = `<tr><td colspan="9" class="mx-empty">${t('settings.no_upstreams')}</td></tr>`;
        return;
    }
    const sorted = [...state.upstreamList].sort((a, b) => a.name.localeCompare(b.name));
    body.innerHTML = sorted.map(u => upstreamRowHtml(u)).join('');
    bindUpstreamTableEvents();
}

function tierCellHtml(rule, defRule) {
    if (!rule) return '<span class="mx-none">—</span>';
    if (defRule && rule.provider === defRule.provider && rule.model === defRule.model) {
        return '<span class="mx-none">—</span>';
    }
    const sameProvider = defRule && rule.provider === defRule.provider;
    const val = sameProvider
        ? (rule.model || rule.provider)
        : [rule.provider, rule.model].filter(Boolean).join('/');
    return `<span class="ut-tier-val has-val">${esc(val)}</span>`;
}

function upstreamRowHtml(u) {
    const activeCell = u.active
        ? `<span class="ut-active-check" title="${t('settings.active_badge')}">✓</span>`
        : `<button class="btn-sm ut-activate-btn" data-name="${esc(u.name)}">${t('settings.activate')}</button>`;
    const proxyCell = u.proxy_active
        ? `<span class="ut-proxy-on" title="Transparent proxy active (click to change)">◉</span>`
        : `<span class="ut-proxy-off" title="Click to use as transparent proxy">◯</span>`;
    const defRule = u.default || null;
    return `<tr class="ut-row${u.active ? ' ut-row-active' : ''}" id="ut-row-${esc(u.name)}">
        <td class="ut-td ut-col-name"><span class="ut-name">${esc(u.name)}</span>${u.active ? `<span class="active-badge">${t('settings.active_badge')}</span>` : ''}</td>
        <td class="ut-td ut-col-active ut-proxy-cell" data-name="${esc(u.name)}">${proxyCell}</td>
        <td class="ut-td ut-col-active">${activeCell}</td>
        <td class="ut-td ut-col-tier">${tierCellHtml(u.high, defRule)}</td>
        <td class="ut-td ut-col-tier">${tierCellHtml(u.mid, defRule)}</td>
        <td class="ut-td ut-col-tier">${tierCellHtml(u.low, defRule)}</td>
        <td class="ut-td ut-col-tier">${tierCellHtml(u.default)}</td>
        <td class="ut-td ut-col-effort">${esc(u.effort || 'auto')}</td>
        <td class="ut-td ut-col-actions">
            <button class="btn-sm ut-edit-btn" data-name="${esc(u.name)}">${t('settings.edit')}</button>
            <button class="btn-sm btn-danger ut-del-btn" data-name="${esc(u.name)}">×</button>
        </td>
    </tr>`;
}

function upstreamEditRowHtml(name, u, simpleTransparent = false) {
    const effortOpts = state.EFFORT_LEVELS.map(level =>
        `<option value="${level}"${(u?.effort || 'auto') === level ? ' selected' : ''}>${level === 'auto' ? 'pass' : level}</option>`
    ).join('');
    const tierRows = ['high', 'mid', 'low'].map(tier => {
        const badge = `tier-${tier}`;
        const rule = u?.[tier];
        return `<div class="ut-edit-tier-row">
            <span class="tier-badge ${badge}">${t(`settings.tier_${tier}`)}</span>
            <label>${t('settings.provider')}</label>
            <select class="ut-provider-select ue-provider" data-tier="${tier}"><option value="">— none —</option></select>
            <label>Relay Model</label>
            <input type="text" class="ue-model" data-tier="${tier}" value="${rule ? esc(rule.model) : ''}" list="ut-dl-${tier}" placeholder="optional for transparent proxy">
            <datalist id="ut-dl-${tier}"></datalist>
        </div>`;
    }).join('');
    const defRule = u?.default;
    const defaultRow = `<div class="ut-edit-tier-row">
        <span class="tier-badge tier-default">${t('settings.tier_default')}</span>
        <label>${t('settings.provider')}</label>
        <select class="ut-provider-select ue-provider" data-tier="default"><option value="">— none —</option></select>
        <label>Relay Model</label>
        <input type="text" class="ue-model" data-tier="default" value="${defRule ? esc(defRule.model) : ''}" list="ut-dl-default" placeholder="optional for transparent proxy">
        <datalist id="ut-dl-default"></datalist>
    </div>`;
    const nameRow = !name ? `<div class="ut-edit-tier-row">
        <label>${t('settings.name')}</label>
        <input type="text" id="ue-name" placeholder="production">
    </div>` : '';
    const transparentDefaultRow = `<div class="ut-edit-tier-row">
        <span class="tier-badge tier-default">Default</span>
        <label>${t('settings.provider')}</label>
        <select class="ut-provider-select ue-provider" data-tier="default"><option value="">— choose provider —</option></select>
        <span class="field-hint">请求 model/header/body 保持原样；费率可稍后配置</span>
    </div>`;
    return `<tr class="ut-edit-row"><td colspan="9" class="ut-edit-td">
        <div class="ut-edit-form">
            ${nameRow}
            ${simpleTransparent ? transparentDefaultRow : tierRows + defaultRow}
            <div class="ut-edit-bottom">
                ${simpleTransparent ? '<span class="field-hint">保存后会自动设为当前 Transparent Proxy upstream</span>' : `<label>${t('settings.effort')}</label><select id="ue-effort">${effortOpts}</select><span class="field-hint">${t('settings.effort_hint')}</span>`}
                <div class="ut-edit-actions">
                    <button class="btn-primary ut-save-btn">${t('settings.save')}</button>
                    <button class="ut-cancel-btn">${t('settings.cancel')}</button>
                </div>
            </div>
        </div>
    </td></tr>`;
}

function bindUpstreamTableEvents() {
    // The Auto control lives in <thead>, outside the delegated <tbody> handler.
    // Assigning onclick avoids accumulating listeners when the table re-renders.
    document.getElementById('upstream-table-head').onclick = e => {
        const proxyTh = e.target.closest('.ut-proxy-th');
        if (proxyTh) activateProxyUpstream(proxyTh.dataset.name);
    };
    document.getElementById('upstream-table-body').onclick = e => {
        const editBtn = e.target.closest('.ut-edit-btn');
        if (editBtn) { openUpstreamTableEdit(editBtn.dataset.name); return; }
        const delBtn = e.target.closest('.ut-del-btn');
        if (delBtn) { deleteUpstream(delBtn.dataset.name); return; }
        const actBtn = e.target.closest('.ut-activate-btn');
        if (actBtn) { activateUpstream(actBtn.dataset.name); return; }
        const proxyCell = e.target.closest('.ut-proxy-cell');
        if (proxyCell) { activateProxyUpstream(proxyCell.dataset.name); return; }
        const saveBtn = e.target.closest('.ut-save-btn');
        if (saveBtn) { saveUpstream(); return; }
        const cancelBtn = e.target.closest('.ut-cancel-btn');
        if (cancelBtn) { closeUpstreamTableEdit(); return; }
    };
}

export function openUpstreamTableEdit(name, simpleTransparent = false) {
    closeUpstreamTableEdit();
    const u = name ? state.upstreamList.find(u => u.name === name) : null;
    state.upstreamEditMode = u ? 'edit' : 'add';
    state.upstreamCreateKind = simpleTransparent ? 'transparent' : null;
    state.upstreamAccordionName = name || null;

    const editHtml = upstreamEditRowHtml(name, u, simpleTransparent);
    if (name) {
        const row = document.getElementById(`ut-row-${name}`);
        if (!row) return;
        row.insertAdjacentHTML('afterend', editHtml);
    } else {
        document.getElementById('upstream-table-body').insertAdjacentHTML('beforeend', editHtml);
    }

    // Populate + bind provider selects
    const form = document.querySelector('.ut-edit-row');
    if (!form) return;
    const provOpts = '<option value="">— none —</option>' +
        state.providerList.map(p => `<option value="${esc(p.name)}">${esc(p.name)}</option>`).join('');
    form.querySelectorAll('.ut-provider-select').forEach(sel => {
        const tier = sel.dataset.tier;
        sel.innerHTML = provOpts;
        sel.value = (u?.[tier]?.provider) || '';
        sel.addEventListener('change', () => updateModelDatalist(`ut-dl-${tier}`, sel));
        updateModelDatalist(`ut-dl-${tier}`, sel);
    });
    const nameInput = form.querySelector('#ue-name');
    if (nameInput) nameInput.focus();
}

export function closeUpstreamTableEdit() {
    document.querySelector('.ut-edit-row')?.remove();
    state.upstreamAccordionName = null;
    state.upstreamEditMode = null;
    state.upstreamCreateKind = null;
}

export function getTierPayload(tier) {
    const form = document.querySelector('.ut-edit-row');
    if (!form) return null;
    const provider = form.querySelector(`.ue-provider[data-tier="${tier}"]`)?.value ?? '';
    const model = form.querySelector(`.ue-model[data-tier="${tier}"]`)?.value.trim() ?? '';
    if (!provider && !model) return null;
    return {
        keywords: [],
        provider,
        model,
    };
}

export async function saveUpstream() {
    const nameEl = document.querySelector('.ut-edit-row #ue-name');
    const name = state.upstreamAccordionName || (nameEl ? nameEl.value.trim() : '');
    if (!name) { alert(t('settings.name_required')); return; }

    const body = {
        name,
        high:    getTierPayload('high'),
        mid:     getTierPayload('mid'),
        low:     getTierPayload('low'),
        default: getTierPayload('default'),
        effort:  document.getElementById('ue-effort')?.value || 'auto',
    };
    if (state.upstreamCreateKind === 'transparent' && !body.default?.provider) {
        alert('Transparent upstream requires a default provider');
        return;
    }

    let resp;
    if (state.upstreamEditMode === 'add') {
        resp = await fetch('/api/upstreams', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    } else {
        resp = await fetch(`/api/upstreams/${encodeURIComponent(name)}`, { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    }
    if (resp.ok) {
        if (state.upstreamCreateKind === 'transparent') {
            await fetch(`/api/upstreams/${encodeURIComponent(name)}/activate-proxy`, { method: 'POST' });
        }
        closeUpstreamTableEdit();
    } else {
        const err = await resp.json();
        alert(err.error || t('settings.failed_save_upstream'));
    }
}

export async function activateUpstream(name) {
    await fetch(`/api/upstreams/${encodeURIComponent(name)}/activate`, { method: 'POST' });
}

export async function activateProxyUpstream(name) {
    await fetch(`/api/upstreams/${encodeURIComponent(name)}/activate-proxy`, { method: 'POST' });
}

export async function deleteUpstream(name) {
    if (!confirm(t('settings.confirm_delete_upstream', { name }))) return;
    await fetch(`/api/upstreams/${encodeURIComponent(name)}`, { method: 'DELETE' });
}

// ── Provider selects & model datalists ──

export function refreshProviderSelects() {
    const opts = '<option value="">— none —</option>' +
        state.providerList.map(p => `<option value="${esc(p.name)}">${esc(p.name)}</option>`).join('');
    document.querySelectorAll('.ut-provider-select').forEach(sel => {
        const current = sel.value;
        sel.innerHTML = opts;
        sel.value = current;
    });
}

export function updateModelDatalist(datalistId, providerSelectOrEl) {
    const dl = document.getElementById(datalistId);
    if (!dl) return;
    const providerName = typeof providerSelectOrEl === 'string'
        ? (document.getElementById(providerSelectOrEl)?.value || '')
        : (providerSelectOrEl?.value || '');
    const options = state.modelPricingList
        .filter(mp => !providerName || (mp.providers && providerName in mp.providers))
        .map(mp => `<option value="${esc(mp.id)}">`)
        .join('');
    dl.innerHTML = options;
}

// ── Event listeners ──

document.getElementById('btn-upstream-add').addEventListener('click', () => {
    openUpstreamTableEdit(null);
});

document.getElementById('btn-upstream-add-transparent').addEventListener('click', () => {
    openUpstreamTableEdit(null, true);
});

document.getElementById('btn-matrix-add-provider').addEventListener('click', (e) => {
    e.stopPropagation();
    openAddProviderPopover();
});

document.getElementById('btn-matrix-add-model').addEventListener('click', openAddModelDialog);

// Upstream select in Inspector toolbar
document.getElementById('upstream-select').addEventListener('change', async () => {
    const name = document.getElementById('upstream-select').value;
    if (!name) return;
    await fetch(`/api/upstreams/${encodeURIComponent(name)}/activate`, { method: 'POST' });
});

// Codex upstream select — activates the codex-specific upstream
document.getElementById('codex-upstream-select').addEventListener('change', async () => {
    const name = document.getElementById('codex-upstream-select').value;
    if (!name) return;
    await fetch(`/api/upstreams/${encodeURIComponent(name)}/activate?target=codex`, { method: 'POST' });
});

// Effort select in Inspector toolbar — saves to active upstream's effort field
document.getElementById('effort-select').addEventListener('change', async () => {
    const effort = document.getElementById('effort-select').value;
    if (!effort) return;
    const prev = state.activeEffort;
    state.activeEffort = effort;
    try {
        const resp = await fetch('/api/effort', {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ effort }),
        });
        if (!resp.ok) throw new Error('save failed');
    } catch (e) {
        state.activeEffort = prev;
        populateEffortSelect(prev);
    }
});

// Global proxy save
document.getElementById('btn-global-proxy-save').addEventListener('click', async () => {
    const val = document.getElementById('global-proxy-input').value.trim();
    const statusEl = document.getElementById('global-proxy-status');
    statusEl.textContent = t('settings.saving') || 'Saving...';
    try {
        const resp = await fetch('/api/proxy', {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ http_proxy: val || null }),
        });
        if (resp.ok) {
            statusEl.textContent = t('settings.proxy_saved') || 'Saved';
            setTimeout(() => { statusEl.textContent = ''; }, 3000);
        } else {
            const err = await resp.json();
            statusEl.textContent = (err.error || t('settings.proxy_save_failed'));
        }
    } catch (e) {
        statusEl.textContent = t('settings.proxy_save_failed') || 'Save failed';
    }
});
