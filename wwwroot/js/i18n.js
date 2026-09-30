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

/// Translate, falling back to the English wording supplied by the caller.
///
/// `t()` returns the key when no dictionary is loaded. That is right for
/// `data-i18n` attributes (the HTML already holds English, so `applyI18n` skips
/// missing keys) but wrong for strings assembled in JS, where the key would be
/// rendered literally.
export function tOr(key, english, params) {
    const text = t(key, params);
    return text === key ? english : text;
}

// `t()` returns the key itself when a translation is missing, which is how the
// English build is meant to work: `loadI18n()` only fetches zh.json, so English
// keeps the wording already present in the HTML. Writing the key back would
// replace readable English with "settings.some_key", so missing keys are skipped.
function resolved(key) {
    const text = t(key);
    return text && text !== key ? text : null;
}

export function applyI18n() {
    document.querySelectorAll('[data-i18n]').forEach(el => {
        const text = resolved(el.dataset.i18n);
        if (text) el.textContent = text;
    });
    document.querySelectorAll('[data-i18n-title]').forEach(el => {
        const text = resolved(el.dataset.i18nTitle);
        if (text) el.title = text;
    });
    document.querySelectorAll('[data-i18n-placeholder]').forEach(el => {
        const text = resolved(el.dataset.i18nPlaceholder);
        if (text) el.placeholder = text;
    });
}
