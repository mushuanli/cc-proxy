// ── I18n ──
let _i18n = {};
let _lang = 'en';

export async function loadI18n() {
    // Auto-detect: load zh.json only for Chinese browsers, otherwise use English defaults
    _lang = (navigator.language || '').startsWith('zh') ? 'zh' : 'en';
    if (_lang !== 'zh') return;
    try {
        const resp = await fetch('/assets/zh.json');
        if (resp.ok) _i18n = await resp.json();
    } catch (e) { console.warn('[i18n] load failed:', e); }
}

export function t(key, params) {
    let val = _i18n;
    for (const k of key.split('.')) { val = val?.[k]; }
    val = val ?? key;
    if (params) {
        Object.entries(params).forEach(([k, v]) => { val = val.replace(`{${k}}`, v); });
    }
    return val;
}

export function applyI18n() {
    document.querySelectorAll('[data-i18n]').forEach(el => {
        const text = t(el.dataset.i18n);
        if (text) el.textContent = text;
    });
    document.querySelectorAll('[data-i18n-title]').forEach(el => {
        el.title = t(el.dataset.i18nTitle);
    });
    document.querySelectorAll('[data-i18n-placeholder]').forEach(el => {
        el.placeholder = t(el.dataset.i18nPlaceholder);
    });
}
