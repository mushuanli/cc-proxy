import { state } from './state.js';
import { t } from './i18n.js';
import { escHtml, fmtBytes, fmtRelTime } from './utils.js';

// ── Archive View ──

export function archiveSid(file) {
    // file is "<sid>.yaml" — extract sid portion (strip .yaml)
    return file.replace(/\.yaml$/, '');
}

export function renderArchiveList(files) {
    const pane = document.getElementById('archive-list-pane');
    const status = document.getElementById('archive-search-status');
    status.textContent = t('archive.file_count', { n: files.length });
    if (!files.length) {
        pane.innerHTML = `<div class="archive-empty">${t('archive.no_files')}</div>`;
        return;
    }
    pane.innerHTML = files.map((f, i) => {
        const sid = archiveSid(f.file);
        const displayName = f.name || '';
        const sidSuffix = sid.slice(-8);
        const titleHtml = displayName
            ? `<span class="archive-card-sid">${escHtml(sidSuffix)}</span><span class="archive-card-name"> - ${escHtml(displayName)}</span>`
            : `<span class="archive-card-sid">${escHtml(sidSuffix)}</span>`;
        const lastActive = f.last_active_at
            ? `${t('archive.last_active')}: ${fmtRelTime(f.last_active_at)}`
            : '';
        return `<div class="archive-card" data-idx="${i}">
            <div class="archive-card-header">${titleHtml}</div>
            <div class="archive-card-meta">${lastActive} · ${fmtBytes(f.size)}</div>
        </div>`;
    }).join('');
    pane.querySelectorAll('.archive-card').forEach(card => {
        const idx = parseInt(card.dataset.idx, 10);
        const f = files[idx];
        card.dataset.file = f.file;
        card.dataset.sid = archiveSid(f.file);
        card.addEventListener('click', () => loadArchiveFile(f.file));
    });
}

export function renderArchiveSearch(results, q) {
    const pane = document.getElementById('archive-list-pane');
    const status = document.getElementById('archive-search-status');
    if (!results.length) {
        status.textContent = t('archive.no_results');
        pane.innerHTML = `<div class="archive-empty">${t('archive.no_results')}</div>`;
        return;
    }
    const roleFilter = results[0]?.role_filter;
    const keywords = results[0]?.keywords?.length ? results[0].keywords : [q];
    const filterLabel = roleFilter === 'user'
        ? ` <span class="archive-filter-tag">${t('archive.filter_user_only')}</span>`
        : '';
    status.innerHTML = `${results.length} ${results.length === 1 ? 'file' : 'files'}${filterLabel}`;
    const ALLOWED_ROLES = new Set(['user', 'assistant', 'system']);
    pane.innerHTML = results.map((r, i) => {
        const snippetsHtml = r.snippets.map(s => {
            let hi = escHtml(s.text);
            keywords.forEach(kw => {
                const re = new RegExp(kw.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi');
                hi = hi.replace(re, m => `<mark>${m}</mark>`);
            });
            const roleAllowed = ALLOWED_ROLES.has(s.role);
            const roleClass = roleAllowed ? `snippet-role--${s.role}` : '';
            const roleLabel = s.role
                ? (roleAllowed ? escHtml(t(`archive.role_${s.role}`)) : escHtml(s.role))
                : '';
            const roleTag = s.role
                ? `<span class="snippet-role ${roleClass}">${roleLabel}</span>`
                : '';
            return `<div class="archive-snippet">${roleTag}${hi}</div>`;
        }).join('');
        const sid = archiveSid(r.file);
        const displayName = r.name || '';
        const sidSuffix = sid.slice(-8);
        const titleHtml = displayName
            ? `<span class="archive-card-sid">${escHtml(sidSuffix)}</span><span class="archive-card-name"> - ${escHtml(displayName)}</span>`
            : `<span class="archive-card-sid">${escHtml(sidSuffix)}</span>`;
        const lastActive = r.last_active_at
            ? `<span class="archive-card-meta">${t('archive.last_active')}: ${fmtRelTime(r.last_active_at)}</span>`
            : '';
        return `<div class="archive-card" data-idx="${i}">
            <div class="archive-card-header">${titleHtml}</div>
            ${lastActive}
            <div class="archive-card-matches">${t('archive.matches', { n: r.match_count })}</div>
            ${snippetsHtml}
        </div>`;
    }).join('');
    pane.querySelectorAll('.archive-card').forEach(card => {
        const idx = parseInt(card.dataset.idx, 10);
        const r = results[idx];
        card.dataset.file = r.file;
        card.dataset.sid = archiveSid(r.file);
        card.addEventListener('click', () => loadArchiveFile(r.file));
    });
}

export async function loadArchiveList() {
    const status = document.getElementById('archive-search-status');
    status.textContent = t('archive.loading');
    try {
        const resp = await fetch('/api/summaries/list');
        state.archiveFiles = await resp.json();
        renderArchiveList(state.archiveFiles);
    } catch (e) {
        document.getElementById('archive-list-pane').innerHTML =
            `<div class="archive-empty">Load failed: ${e.message}</div>`;
    }
}

// Keys whose child block is collapsed by default
const YAML_AUTO_COLLAPSE = new Set([
    'pricing', 'daily_usage', 'assistant_actions', 'touched_files', 'stats',
]);

/**
 * Render a YAML string as a foldable DOM tree inside `container`.
 */
export function renderYamlFoldable(yamlText, container) {
    container.innerHTML = '';
    const lines = yamlText.split('\n');
    const n = lines.length;

    function indent(line) {
        if (!line.trim()) return Infinity;
        let i = 0;
        while (i < line.length && line[i] === ' ') i++;
        return i;
    }

    function keyOf(line) {
        const m = line.match(/^\s*(?:-\s+)?([a-zA-Z_][a-zA-Z0-9_]*):/);
        return m ? m[1] : null;
    }

    const nodes = lines.map((line, i) => ({
        line, depth: indent(line),
        isBlockHead: false, collapsed: false,
        childStart: -1, childEnd: -1,
    }));

    for (let i = 0; i < n - 1; i++) {
        const cur = nodes[i], nxt = nodes[i + 1];
        if (nxt.depth > cur.depth) {
            cur.isBlockHead = true;
            let j = i + 1;
            while (j < n && nodes[j].depth > cur.depth) j++;
            cur.childStart = i + 1;
            cur.childEnd = j;
            const key = keyOf(cur.line);
            if (key && YAML_AUTO_COLLAPSE.has(key)) cur.collapsed = true;
        }
    }

    function renderRange(parent, from, to) {
        let idx = from;
        while (idx < to) {
            const node = nodes[idx];
            if (node.isBlockHead) {
                const row = document.createElement('div');
                row.className = 'yf-row yf-head';
                const toggle = document.createElement('span');
                toggle.className = 'yf-toggle';
                toggle.textContent = node.collapsed ? '▶' : '▼';
                const text = document.createElement('span');
                text.className = 'yf-text';
                text.textContent = node.line;
                row.appendChild(toggle);
                row.appendChild(text);
                parent.appendChild(row);

                const childWrap = document.createElement('div');
                childWrap.className = 'yf-children';
                if (node.collapsed) childWrap.classList.add('yf-hidden');
                parent.appendChild(childWrap);

                renderRange(childWrap, node.childStart, node.childEnd);

                toggle.addEventListener('click', (e) => {
                    e.stopPropagation();
                    node.collapsed = !node.collapsed;
                    toggle.textContent = node.collapsed ? '▶' : '▼';
                    childWrap.classList.toggle('yf-hidden', node.collapsed);
                });
                row.addEventListener('click', () => toggle.click());

                idx = node.childEnd;
            } else {
                const row = document.createElement('div');
                row.className = 'yf-row';
                const gap = document.createElement('span');
                gap.className = 'yf-toggle yf-toggle-gap';
                const text = document.createElement('span');
                text.className = 'yf-text';
                text.textContent = node.line;
                row.appendChild(gap);
                row.appendChild(text);
                parent.appendChild(row);
                idx++;
            }
        }
    }

    renderRange(container, 0, n);
}

export async function loadArchiveFile(filename) {
    document.querySelectorAll('.archive-card').forEach(c => c.classList.remove('active'));
    document.querySelector(`.archive-card[data-file="${CSS.escape(filename)}"]`)?.classList.add('active');

    const pane = document.getElementById('archive-content-pane');
    const body = document.getElementById('archive-content-body');
    const nameEl = document.getElementById('archive-content-name');
    pane.classList.remove('hidden');
    body.textContent = t('archive.loading');
    state.archiveCurrentFile = filename;

    const meta = state.archiveFiles.find(f => f.file === filename) || {};
    nameEl.textContent = meta.name || archiveSid(filename).slice(-8);

    try {
        const resp = await fetch(`/api/summaries/file/${encodeURIComponent(filename)}`);
        const text = await resp.text();
        renderYamlFoldable(text, body);
    } catch (e) {
        body.textContent = `Error: ${e.message}`;
    }
}

export async function runArchiveSearch(q) {
    if (!q.trim()) { renderArchiveList(state.archiveFiles); return; }
    const status = document.getElementById('archive-search-status');
    status.textContent = t('archive.loading');
    try {
        const resp = await fetch(`/api/summaries/search?q=${encodeURIComponent(q)}`);
        const results = await resp.json();
        renderArchiveSearch(results, q);
    } catch (e) {
        document.getElementById('archive-list-pane').innerHTML =
            `<div class="archive-empty">Search failed: ${e.message}</div>`;
    }
}

export function startArchiveRename() {
    if (!state.archiveCurrentFile) return;
    const sid = archiveSid(state.archiveCurrentFile);
    const meta = state.archiveFiles.find(f => f.file === state.archiveCurrentFile) || {};
    const nameEl = document.getElementById('archive-content-name');
    const renameBtn = document.getElementById('btn-archive-rename');

    const input = document.createElement('input');
    input.className = 'archive-rename-input';
    input.value = meta.name || '';
    input.maxLength = 64;
    nameEl.replaceWith(input);
    renameBtn.textContent = '✓';
    input.focus();

    async function commit() {
        const newName = input.value.trim();
        try {
            await fetch(`/api/summaries/name/${encodeURIComponent(sid)}`, {
                method: 'PUT',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ name: newName }),
            });
            if (meta) meta.name = newName;
        } catch (e) { /* silent */ }
        const newNameEl = document.createElement('span');
        newNameEl.id = 'archive-content-name';
        newNameEl.className = 'archive-content-title';
        newNameEl.textContent = newName || sid.slice(0, 8);
        input.replaceWith(newNameEl);
        renameBtn.textContent = '✎';
        renameBtn.onclick = startArchiveRename;
        loadArchiveList();
    }

    renameBtn.onclick = commit;
    input.addEventListener('keydown', e => {
        if (e.key === 'Enter') commit();
        if (e.key === 'Escape') {
            const el = document.createElement('span');
            el.id = 'archive-content-name';
            el.className = 'archive-content-title';
            el.textContent = meta.name || sid.slice(0, 8);
            input.replaceWith(el);
            renameBtn.textContent = '✎';
            renameBtn.onclick = startArchiveRename;
        }
    });
}

// ── Event listeners ──

document.getElementById('btn-archive-refresh').addEventListener('click', loadArchiveList);

document.getElementById('btn-archive-close-pane').addEventListener('click', () => {
    document.getElementById('archive-content-pane').classList.add('hidden');
    document.querySelectorAll('.archive-card').forEach(c => c.classList.remove('active'));
    state.archiveCurrentFile = null;
});

document.getElementById('btn-archive-rename').addEventListener('click', startArchiveRename);

document.getElementById('archive-search-input').addEventListener('input', e => {
    clearTimeout(state.archiveSearchTimer);
    const q = e.target.value;
    state.archiveSearchTimer = setTimeout(() => runArchiveSearch(q), 300);
});
