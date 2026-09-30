// ── planx Accounts ──
//
// Two families (gpt / claude) × two modes (api_key / plan).
// Secrets are never sent back by the server (only `has_*` booleans), so the edit
// form leaves credential fields empty and treats blank as "keep current value".

import { t } from './i18n.js';
import { esc } from './utils.js';

let _accounts = [];
let _editing = null;   // account name being edited, or null when adding

const FAMILIES = ['gpt', 'claude'];
const MODES = ['api_key', 'plan'];

export function applyAccounts(list) {
    _accounts = Array.isArray(list) ? list : [];
    renderAccounts();
}

export function accountNames() {
    return _accounts.map(a => a.name);
}

/// Name + family, for forms that must respect the family ↔ protocol rule.
export function accountList() {
    return _accounts.map(a => ({ name: a.name, family: a.family }));
}

async function fetchAccounts(probe = false) {
    const url = probe ? '/api/accounts?probe=1' : '/api/accounts';
    try {
        const resp = await fetch(url);
        if (!resp.ok) return;
        applyAccounts(await resp.json());
    } catch (e) {
        console.warn('[accounts] load failed:', e);
    }
}

export async function loadAccounts(probe = false) {
    await fetchAccounts(probe);
}

// ── Rendering ──

function quotaCell(a) {
    const q = a.quota;
    if (!q) return '<span class="acct-muted">—</span>';
    if (q.error) {
        return `<span class="acct-err" title="${esc(q.error)}">${esc(t('accounts.quota_error'))}</span>`;
    }
    if (!q.windows || q.windows.length === 0) {
        return '<span class="acct-muted">—</span>';
    }
    const bars = q.windows.map(w => {
        const pct = Math.max(0, Math.min(100, Number(w.utilization) || 0));
        const level = pct >= 90 ? 'high' : (pct >= 70 ? 'mid' : 'low');
        const title = w.resets_at
            ? t('accounts.resets_at', { time: new Date(w.resets_at * 1000).toLocaleString() })
            : '';
        return `<span class="acct-bar ${level}" title="${esc(w.label + (title ? ' · ' + title : ''))}">`
            + `<span class="acct-bar-label">${esc(w.label)}</span>`
            + `<span class="acct-bar-track"><span class="acct-bar-fill" style="width:${pct}%"></span></span>`
            + `<span class="acct-bar-pct">${pct.toFixed(0)}%</span>`
            + `</span>`;
    }).join('');
    const extras = [];
    if (q.limit_reached) extras.push(`<span class="acct-err">${esc(t('accounts.limit_reached'))}</span>`);
    if (q.reset_credits) extras.push(`<span class="acct-muted">${esc(t('accounts.reset_credits', { n: q.reset_credits }))}</span>`);
    if (q.credits_unlimited) extras.push(`<span class="acct-muted">${esc(t('accounts.unlimited'))}</span>`);
    return bars + (extras.length ? `<div class="acct-extras">${extras.join(' ')}</div>` : '');
}

function statusCell(a) {
    if (a.problem) {
        return `<span class="acct-err" title="${esc(a.problem)}">${esc(t('accounts.invalid'))}</span>`;
    }
    if (!a.live) {
        return `<span class="acct-warn">${esc(t('accounts.not_loaded'))}</span>`;
    }
    if (a.mode === 'api_key') return `<span class="acct-ok">${esc(t('accounts.ready'))}</span>`;
    return a.has_refresh_token || a.has_access_token
        ? `<span class="acct-ok">${esc(t('accounts.ready'))}</span>`
        : `<span class="acct-warn">${esc(t('accounts.no_credential'))}</span>`;
}

function accountRow(a) {
    const creds = [];
    if (a.has_api_key) creds.push('api_key');
    if (a.has_refresh_token) creds.push('refresh_token');
    if (a.has_access_token) creds.push('access_token');
    if (a.auth_json) creds.push('auth.json');
    const meta = creds.length ? creds.join(' · ') : t('accounts.no_credential');
    const wantsImpersonation = Boolean(a.impersonate) && a.impersonate !== 'off';
    const impersonate = a.impersonate ? ` · ${esc(a.impersonate)}` : '';
    // A profile configured on a build without the `impersonate` feature does
    // nothing at all, so say so instead of showing it as if it were active.
    const inactiveImpersonation = wantsImpersonation && a.impersonate_supported === false
        ? `<div class="acct-err">${esc(t('accounts.impersonate_not_compiled', { profile: a.impersonate }))}</div>`
        : '';
    return `
        <tr>
            <td>
                <div class="acct-name">${esc(a.name)}</div>
                <div class="acct-meta">${esc(meta)}${impersonate}</div>
                ${inactiveImpersonation}
            </td>
            <td><span class="acct-tag">${esc(a.family)}</span></td>
            <td><span class="acct-tag">${esc(a.mode)}</span></td>
            <td>${statusCell(a)}</td>
            <td class="acct-quota">${quotaCell(a)}</td>
            <td class="acct-actions">
                <button class="btn-sm" data-acct-probe="${esc(a.name)}">${esc(t('accounts.probe'))}</button>
                <button class="btn-sm" data-acct-edit="${esc(a.name)}">${esc(t('accounts.edit'))}</button>
                <button class="btn-sm" data-acct-delete="${esc(a.name)}">${esc(t('accounts.delete'))}</button>
            </td>
        </tr>`;
}

function renderAccounts() {
    const body = document.getElementById('accounts-body');
    if (!body) return;
    if (_accounts.length === 0) {
        body.innerHTML = `<tr><td colspan="6" class="acct-empty">${esc(t('accounts.empty'))}</td></tr>`;
        return;
    }
    body.innerHTML = _accounts.map(accountRow).join('');
}

// ── Add / edit dialog ──

function field(labelKey, id, opts = {}) {
    const type = opts.type || 'text';
    const placeholder = opts.placeholder || '';
    const hint = opts.hint ? `<span class="hint">${esc(opts.hint)}</span>` : '';
    return `
        <div class="acct-field">
            <label for="${id}">${esc(t(labelKey))}</label>
            <input id="${id}" type="${type}" placeholder="${esc(placeholder)}" class="mx-pop-input">
            ${hint}
        </div>`;
}

/// Select with explicit `{value, label}` options, so a "default" choice can show
/// what it will actually resolve to instead of an empty box.
function optionsField(labelKey, id, options, selected, hint) {
    const html = options
        .map(o => `<option value="${esc(o.value)}"${o.value === selected ? ' selected' : ''}>${esc(o.label)}</option>`)
        .join('');
    return `
        <div class="acct-field">
            <label for="${id}">${esc(t(labelKey))}</label>
            <select id="${id}" class="mx-pop-input">${html}</select>
            ${hint ? `<span class="hint">${esc(hint)}</span>` : ''}
        </div>`;
}

/// Identity choices for a family. The empty value means "family default", but the
/// label names it, so leaving it alone is an informed choice.
function identityOptions(family) {
    const fallback = family === 'claude' ? 'claude_code' : 'codex_tui';
    const options = [{ value: '', label: `${t('accounts.identity_default')}（${fallback}）` }];
    if (family === 'claude') {
        options.push({ value: 'claude_code', label: 'claude_code' });
    } else {
        options.push({ value: 'codex_tui', label: 'codex_tui' });
        options.push({ value: 'codex_cli_rs', label: 'codex_cli_rs' });
    }
    options.push({ value: 'passthrough', label: 'passthrough' });
    return options;
}

function impersonationOptions() {
    return [
        { value: '', label: `${t('accounts.identity_default')}（off）` },
        { value: 'off', label: 'off' },
        { value: 'chrome', label: 'chrome' },
        { value: 'chrome142', label: 'chrome142' },
        { value: 'edge', label: 'edge' },
        { value: 'firefox', label: 'firefox' },
        { value: 'safari', label: 'safari' },
    ];
}

function selectField(labelKey, id, values, selected) {
    const options = values.map(v =>
        `<option value="${esc(v)}"${v === selected ? ' selected' : ''}>${esc(v)}</option>`
    ).join('');
    return `
        <div class="acct-field">
            <label for="${id}">${esc(t(labelKey))}</label>
            <select id="${id}" class="mx-pop-input">${options}</select>
        </div>`;
}

/// Show only the credential fields relevant to the chosen family + mode.
/// Whether a field's wrapper is currently visible.
function fieldVisible(id) {
    const el = document.getElementById(id);
    if (!el) return false;
    return !el.closest('.acct-field')?.classList.contains('hidden');
}

/// Value of a visible field; `''` when it is hidden (so it is not submitted).
function fieldValue(id) {
    const el = document.getElementById(id);
    if (!el || !fieldVisible(id)) return '';
    return (el.value || '').trim();
}

/// Show/hide one field by its wrapper. Hidden fields are also *not submitted*
/// (see `fieldValue`), so switching family never sends the previous family's
/// credential and trips validation.
function showAccountField(id, visible) {
    const el = document.getElementById(id);
    if (el) el.closest('.acct-field')?.classList.toggle('hidden', !visible);
}

/// Rebuild the identity list for the current family, keeping the selection when
/// the stored profile is still valid for it.
function syncIdentityOptions() {
    const select = document.getElementById('acct-identity');
    if (!select) return;
    const family = document.getElementById('acct-family')?.value || 'gpt';
    const current = select.value;
    select.innerHTML = identityOptions(family)
        .map(o => `<option value="${esc(o.value)}"${o.value === current ? ' selected' : ''}>${esc(o.label)}</option>`)
        .join('');
}

function syncCredentialFields() {
    const family = document.getElementById('acct-family')?.value;
    const mode = document.getElementById('acct-mode')?.value;
    const isPlan = mode === 'plan';
    const isGpt = family === 'gpt';

    // Outside the fold: only what this (family, mode) *requires*.
    //   api_key  → the key
    //   gpt+plan → nothing: the prefilled auth.json default is already enough
    //   claude+plan → a refresh token (auth_json is rejected for Claude, so this
    //                 is the field that makes the account usable at all)
    showAccountField('acct-api-key', !isPlan);
    showAccountField('acct-refresh-token', isPlan && !isGpt);
    // Inside the fold:
    showAccountField('acct-auth-json', isPlan && isGpt);
    // A second refresh-token box rather than sharing one id, so it can live in
    // the fold for gpt (only needed when there is no Codex CLI on this host).
    showAccountField('acct-refresh-token-adv', isPlan && isGpt);
    showAccountField('acct-access-token', isPlan);
    // `account_id` only becomes the `chatgpt-account-id` header for GPT, and
    // identity/impersonate are only used in plan mode, so they are inert
    // otherwise — do not offer dead knobs.
    showAccountField('acct-account-id', isPlan && isGpt);
    showAccountField('acct-identity', isPlan);
    showAccountField('acct-impersonate', isPlan);
    showAccountField('acct-persist', isPlan && isGpt);
    syncIdentityOptions();
    const advanced = document.getElementById('acct-advanced');
    if (advanced) {
        // Nothing inside is relevant for a static key.
        advanced.closest('.acct-field')?.classList.toggle('hidden', !isPlan);
        if (!isPlan) advanced.open = false;
    }

    const note = document.getElementById('acct-form-note');
    if (note) {
        note.textContent = !isPlan
            ? t('accounts.rec_api_key')
            : (isGpt ? t('accounts.rec_gpt_plan') : t('accounts.rec_claude_plan'));
    }
}

export function openAccountDialog(name) {
    const existing = name ? _accounts.find(a => a.name === name) : null;
    _editing = existing ? existing.name : null;

    const overlay = document.createElement('div');
    overlay.className = 'mx-overlay';
    overlay.id = 'acct-overlay';
    overlay.innerHTML = `
        <div class="mx-modal acct-modal">
            <div class="mx-pop-title">${esc(existing ? t('accounts.edit_title', { name: existing.name }) : t('accounts.new_title'))}</div>
            <div class="acct-form">
                ${field('accounts.name', 'acct-name', { placeholder: 'gpt-sub', hint: existing ? t('accounts.name_locked') : '' })}
                ${selectField('accounts.family', 'acct-family', FAMILIES, existing?.family || 'gpt')}
                ${selectField('accounts.mode', 'acct-mode', MODES, existing?.mode || 'plan')}
                ${field('accounts.api_key', 'acct-api-key', { type: 'password', hint: t('accounts.keep_secret_hint') })}
                ${field('accounts.refresh_token', 'acct-refresh-token', { type: 'password', hint: t('accounts.refresh_token_required') })}
                <div class="acct-field">
                    <details id="acct-advanced" class="acct-adv">
                        <summary>${esc(t('accounts.advanced'))}</summary>
                        ${field('accounts.auth_json', 'acct-auth-json', { placeholder: '~/.codex/auth.json', hint: t('accounts.auth_json_hint') })}
                        ${field('accounts.refresh_token', 'acct-refresh-token-adv', { type: 'password', hint: t('accounts.refresh_token_adv_hint') })}
                        ${field('accounts.access_token', 'acct-access-token', { type: 'password', hint: t('accounts.access_token_hint') })}
                        ${field('accounts.account_id', 'acct-account-id', { placeholder: '(optional)', hint: t('accounts.account_id_hint') })}
                        ${optionsField('accounts.identity', 'acct-identity', identityOptions(existing?.family || 'gpt'), existing?.identity || '', t('accounts.identity_hint'))}
                        ${optionsField('accounts.impersonate', 'acct-impersonate', impersonationOptions(), existing?.impersonate || '', t('accounts.impersonate_hint'))}
                        <div class="acct-field acct-inline">
                            <label><input type="checkbox" id="acct-persist"> ${esc(t('accounts.persist'))}</label>
                            <span class="hint">${esc(t('accounts.persist_hint'))}</span>
                        </div>
                    </details>
                </div>
                <div id="acct-form-error" class="acct-err hidden"></div>
                <div class="mx-pop-actions">
                    <button class="btn-sm" id="acct-cancel">${esc(t('accounts.cancel'))}</button>
                    <button class="btn-sm btn-primary" id="acct-save">${esc(t('accounts.save'))}</button>
                </div>
            </div>
        </div>`;

    document.body.appendChild(overlay);

    const nameInput = overlay.querySelector('#acct-name');
    nameInput.value = existing?.name || '';
    nameInput.disabled = Boolean(existing);
    const mode = existing?.mode || 'plan';
    const family = existing?.family || 'gpt';
    overlay.querySelector('#acct-account-id').value = existing?.account_id || '';
    // Selects: keep whatever the account already stored; '' means "family default".
    overlay.querySelector('#acct-identity').value = existing?.identity || '';
    overlay.querySelector('#acct-impersonate').value = existing?.impersonate || '';
    // Recommended defaults for a new account: the Codex CLI file everyone
    // already has, and write-back so the rotated refresh token survives.
    const usesCodexFile = mode === 'plan' && family === 'gpt';
    overlay.querySelector('#acct-auth-json').value =
        existing?.auth_json || (usesCodexFile ? '~/.codex/auth.json' : '');
    overlay.querySelector('#acct-persist').checked =
        existing ? Boolean(existing.persist) : usesCodexFile;

    overlay.querySelector('#acct-family').addEventListener('change', syncCredentialFields);
    overlay.querySelector('#acct-mode').addEventListener('change', syncCredentialFields);
    syncCredentialFields();

    overlay.querySelector('#acct-cancel').addEventListener('click', closeAccountDialog);
    overlay.addEventListener('click', event => {
        if (event.target === overlay) closeAccountDialog();
    });
    overlay.querySelector('#acct-save').addEventListener('click', saveAccount);
    nameInput.focus();
}

export function closeAccountDialog() {
    document.getElementById('acct-overlay')?.remove();
    _editing = null;
}

function showFormError(message) {
    const box = document.getElementById('acct-form-error');
    if (!box) return;
    box.textContent = message;
    box.classList.toggle('hidden', !message);
}

async function saveAccount() {
    const value = id => document.getElementById(id)?.value.trim() || '';
    const name = value('acct-name') || _editing;
    if (!name) {
        showFormError(t('accounts.name_required'));
        return;
    }
    // A field hidden by the current family/mode is not submitted at all, so
    // switching family cannot smuggle the previous family's credential in.
    const body = {
        name,
        family: value('acct-family') || 'gpt',
        mode: value('acct-mode') || 'plan',
        account_id: fieldValue('acct-account-id') || null,
        identity: fieldValue('acct-identity') || null,
        impersonate: fieldValue('acct-impersonate') || null,
        persist: fieldVisible('acct-persist')
            && (document.getElementById('acct-persist')?.checked || false),
    };
    // Blank secrets mean "leave unchanged"; the server never sends them back.
    const apiKey = fieldValue('acct-api-key');
    const refreshToken =
        fieldValue('acct-refresh-token') || fieldValue('acct-refresh-token-adv');
    const accessToken = fieldValue('acct-access-token');
    const authJson = fieldValue('acct-auth-json');
    if (apiKey) body.api_key = apiKey;
    if (refreshToken) body.refresh_token = refreshToken;
    if (accessToken) body.access_token = accessToken;
    if (authJson) body.auth_json = authJson;

    // Blank secrets are omitted, not sent as empty: PUT /api/accounts/:name is a
    // patch, so the stored credential survives an edit that does not touch it.
    // A change that genuinely leaves the account without a usable credential is
    // rejected by server-side validation, whose message is shown below.

    const url = _editing ? `/api/accounts/${encodeURIComponent(_editing)}` : '/api/accounts';
    const resp = await fetch(url, {
        method: _editing ? 'PUT' : 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    });
    if (!resp.ok) {
        const detail = await resp.json().catch(() => ({}));
        showFormError(detail.error || t('accounts.save_failed'));
        return;
    }
    closeAccountDialog();
    await loadAccounts();
}

// ── Row actions ──

async function deleteAccount(name) {
    if (!confirm(t('accounts.confirm_delete', { name }))) return;
    const resp = await fetch(`/api/accounts/${encodeURIComponent(name)}`, { method: 'DELETE' });
    if (!resp.ok) {
        const detail = await resp.json().catch(() => ({}));
        alert(detail.error || t('accounts.delete_failed'));
        return;
    }
    await loadAccounts();
}

async function probeAccount(name) {
    const resp = await fetch(`/api/accounts/${encodeURIComponent(name)}/probe`, { method: 'POST' });
    if (!resp.ok) {
        const detail = await resp.json().catch(() => ({}));
        alert(detail.error || t('accounts.probe_failed'));
        return;
    }
    await loadAccounts();
}

export function bindAccountEvents() {
    document.getElementById('btn-account-add')?.addEventListener('click', () => openAccountDialog(null));
    document.getElementById('btn-account-probe-all')?.addEventListener('click', async event => {
        const button = event.currentTarget;
        const original = button.textContent;
        button.disabled = true;
        button.textContent = t('accounts.probing');
        await loadAccounts(true);
        button.disabled = false;
        button.textContent = original;
    });

    document.getElementById('accounts-body')?.addEventListener('click', event => {
        const button = event.target.closest('button');
        if (!button) return;
        if (button.dataset.acctEdit) openAccountDialog(button.dataset.acctEdit);
        else if (button.dataset.acctDelete) deleteAccount(button.dataset.acctDelete);
        else if (button.dataset.acctProbe) probeAccount(button.dataset.acctProbe);
    });
}
