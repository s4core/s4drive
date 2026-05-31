/* ═══════════════════════════════════════════════════════════════
   S4Drive Application — Main Logic
   ═══════════════════════════════════════════════════════════════ */

(function() {
  'use strict';

  // ─── IPC Helper ──────────────────────────────────────────────
  const tauri = window.__TAURI__;
  const invoke = tauri?.core?.invoke;
  const listen = tauri?.event?.listen;

  // ─── State ───────────────────────────────────────────────────
  const state = {
    settings: {
      endpoint: '',
      bucket: '',
      access_key_id: '',
      region: 'us-east-1',
      sync_folder: '~/S4Drive',
      bucket_prefix: '/',
      polling_interval_sec: 30,
      bandwidth_limit_kbps: null,
      max_concurrent_uploads: 4,
      max_concurrent_downloads: 4,
      excludes: [],
      proxy: null,
      dark_mode: true,
      autostart: false,
      use_tls: true,
    },
    sync: { running: false, paused: false, state: 'idle', conflicts: 0, lastSync: null },
    transfers: [],
    conflicts: [],
    activities: [],
  };

  // ─── File Icons by Type ──────────────────────────────────────
  const FILE_ICONS = {
    // Code
    'js': '🟨', 'ts': '🟦', 'jsx': '⚛️', 'tsx': '⚛️', 'py': '🐍', 'rs': '🦀',
    'go': '🔵', 'java': '☕', 'rb': '💎', 'php': '🐘', 'swift': '🕊️', 'kt': '🟣',
    'c': '⚙️', 'cpp': '⚙️', 'h': '📐', 'hpp': '📐', 'sh': '🐚', 'bash': '🐚',
    'css': '🎨', 'scss': '🎨', 'html': '🌐',
    // Data
    'json': '📋', 'yaml': '📋', 'yml': '📋', 'toml': '📋', 'xml': '📋',
    'csv': '📊', 'tsv': '📊', 'sql': '🗄️',
    // Docs
    'md': '📝', 'txt': '📄', 'pdf': '📕', 'doc': '📘', 'docx': '📘',
    'xls': '📗', 'xlsx': '📗', 'ppt': '📙', 'pptx': '📙',
    // Media
    'png': '🖼️', 'jpg': '🖼️', 'jpeg': '🖼️', 'gif': '🎞️', 'svg': '🎨',
    'webp': '🖼️', 'ico': '🖼️', 'bmp': '🖼️',
    'mp3': '🎵', 'wav': '🎵', 'flac': '🎵', 'ogg': '🎵', 'm4a': '🎵',
    'mp4': '🎬', 'avi': '🎬', 'mkv': '🎬', 'mov': '🎬', 'webm': '🎬',
    // Archives
    'zip': '📦', 'tar': '📦', 'gz': '📦', 'bz2': '📦', 'xz': '📦',
    'rar': '📦', '7z': '📦', 'tgz': '📦',
    // Config
    'env': '🔐', 'ini': '⚙️', 'cfg': '⚙️', 'conf': '⚙️', 'lock': '🔒',
    'gitignore': '🙈',
    // Default
    'folder': '📁', 'file': '📄', 'folder-open': '📂',
  };

  function getFileIcon(name) {
    if (!name) return FILE_ICONS.file;
    const ext = name.includes('.') ? name.split('.').pop().toLowerCase() : '';
    return FILE_ICONS[ext] || FILE_ICONS.file;
  }

  // ─── Theme ───────────────────────────────────────────────────
  function setTheme(dark) {
    document.documentElement.classList.toggle('theme-light', !dark);
    document.getElementById('themeToggle').textContent = dark ? '🌙' : '☀️';
    document.getElementById('inputDarkMode').checked = dark;
    state.settings.dark_mode = dark;
  }

  function toggleTheme() {
    setTheme(!state.settings.dark_mode);
    showToast(state.settings.dark_mode ? 'Dark mode enabled' : 'Light mode enabled', 'info');
  }

  // ─── Navigation / Routing ────────────────────────────────────
  function navigate(route) {
    document.querySelectorAll('.nav-btn').forEach(b => {
      b.classList.toggle('active', b.dataset.route === route);
      b.setAttribute('aria-selected', b.dataset.route === route ? 'true' : 'false');
    });
    document.querySelectorAll('.screen').forEach(s => {
      s.classList.toggle('active', s.id === 'screen-' + route);
    });
  }

  // ─── Toast ───────────────────────────────────────────────────
  function showToast(msg, type = 'info', duration = 3500) {
    const container = document.getElementById('toastContainer');
    const toast = document.createElement('div');
    toast.className = 'toast ' + type;
    const icons = { success: '✅', error: '❌', info: 'ℹ️', warning: '⚠️' };
    const icon = document.createElement('span');
    icon.textContent = icons[type] || 'ℹ️';
    const message = document.createElement('span');
    message.textContent = msg;
    toast.append(icon, message);
    container.appendChild(toast);
    setTimeout(() => {
      toast.style.animation = 'toastOut 300ms ease forwards';
      setTimeout(() => toast.remove(), 300);
    }, duration);
  }

  // ─── Status Badge ────────────────────────────────────────────
  function updateStatusBadge(status) {
    const badge = document.getElementById('statusBadge');
    const cls = ['syncing', 'paused', 'error', 'idle'].find(c => badge.classList.contains(c));
    if (cls) badge.classList.remove(cls);
    const next = status.state || 'idle';
    badge.classList.add(next);
    badge.textContent = next.charAt(0).toUpperCase() + next.slice(1);
  }

  // ─── Dashboard ───────────────────────────────────────────────
  async function refreshDashboard() {
    try {
      const sync = await invoke('get_sync_status');
      state.sync = sync;
      document.getElementById('syncState').textContent = sync.state || '—';
      document.getElementById('conflictCount').textContent = sync.conflicts ?? 0;
      document.getElementById('totalFiles').textContent = '—';
      document.getElementById('transferCount').textContent = String(state.transfers.length || 0);
      updateStatusBadge(sync);
    } catch(e) {
      document.getElementById('syncState').textContent = 'Offline';
      console.warn('Dashboard refresh:', e);
    }
  }

  // ─── Activity ────────────────────────────────────────────────
  async function refreshActivity() {
    try {
      const items = await invoke('get_activity');
      state.activities = items || [];
      renderActivity();
    } catch(e) { /* noop */ }
  }

  function renderActivity() {
    renderActivityList(document.getElementById('activityList'));
    renderActivityList(document.getElementById('activityScreenList'));
  }

  function renderActivityList(list) {
    if (!list) return;
    if (!state.activities || state.activities.length === 0) {
      list.innerHTML = `
        <div class="empty-state">
          <div class="empty-icon">📋</div>
          <div class="empty-title">No Activity Yet</div>
          <div class="empty-desc">Sync activity will appear here once you connect a bucket.</div>
        </div>`;
      return;
    }
    list.innerHTML = state.activities.map(a => `
      <div class="activity-item">
        <div class="activity-icon">${getActivityIcon(a.action)}</div>
        <div class="activity-content">
          <div class="activity-path">${escapeHtml(a.path || a.file_id)}</div>
          <div class="activity-action">${escapeHtml(a.action)} — ${escapeHtml(a.status)}</div>
        </div>
        <div class="activity-time">${formatTime(a.timestamp)}</div>
      </div>
    `).join('');
  }

  function getActivityIcon(action) {
    const icons = {
      'upload': '⬆️', 'upload_complete': '✅', 'upload_conflict': '⚠️',
      'download': '⬇️', 'download_complete': '✅',
      'delete': '🗑️', 'rename': '✏️', 'new_file': '🆕', 'initial_upload': '🆕',
      'remote_change': '🌐', 'remote_delete': '🗑️',
      'ready': 'ℹ️',
    };
    return icons[action] || '📄';
  }

  // ─── Transfers ───────────────────────────────────────────────
  async function refreshTransfers() {
    try {
      state.transfers = await invoke('get_transfers') || [];
      renderTransfers();
    } catch(e) { /* noop */ }
  }

  function renderTransfers() {
    const active = document.getElementById('activeTransfers');
    const completed = document.getElementById('completedTransfers');
    const activeItems = state.transfers.filter(t => !['complete', 'completed'].includes(t.status));
    const completedItems = state.transfers.filter(t => ['complete', 'completed'].includes(t.status));
    active.innerHTML = renderTransferList(activeItems, 'No Active Transfers');
    completed.innerHTML = renderTransferList(completedItems, 'No Completed Transfers');
    document.getElementById('transferCount').textContent = String(activeItems.length);
  }

  function renderTransferList(items, emptyTitle) {
    if (!items.length) {
      return `
        <div class="empty-state">
          <div class="empty-icon">⬆️</div>
          <div class="empty-title">${emptyTitle}</div>
        </div>`;
    }

    return items.map(t => {
      const total = Number(t.bytes_total || 0);
      const done = Number(t.bytes_done || 0);
      const progress = total > 0 ? Math.min(100, Math.round((done / total) * 100)) : 0;
      return `
        <div class="transfer-item">
          <div class="transfer-header">
            <div class="transfer-name">${escapeHtml(t.path || t.id)}</div>
            <div class="transfer-status">${escapeHtml(t.status || '')}</div>
          </div>
          <div class="progress-bar"><div class="progress-fill" style="width:${progress}%"></div></div>
          <div class="transfer-details">
            <span>${escapeHtml(t.direction || '')}</span>
            <span>${progress}%</span>
          </div>
        </div>`;
    }).join('');
  }

  // ─── Conflicts ───────────────────────────────────────────────
  async function refreshConflicts() {
    try {
      const items = await invoke('get_conflicts');
      state.conflicts = items || [];
      renderConflicts();
    } catch(e) { /* noop */ }
  }

  function renderConflicts() {
    const list = document.getElementById('conflictList');
    const badge = document.getElementById('conflictBadge');
    const count = state.conflicts?.length || 0;
    badge.textContent = count;

    if (count === 0) {
      list.innerHTML = `
        <div class="empty-state">
          <div class="empty-icon">✅</div>
          <div class="empty-title">No Conflicts</div>
          <div class="empty-desc">All files are in sync.</div>
        </div>`;
      return;
    }

    list.innerHTML = state.conflicts.map(c => `
      <div class="conflict-item">
        <div class="conflict-header">
          <span class="conflict-name">${escapeHtml(c.file_id || 'Unknown')}</span>
          <span class="conflict-type">${escapeHtml(c.conflict_type || '')}</span>
        </div>
        <div class="conflict-reason">${escapeHtml(c.human_reason || '')}</div>
        <div class="conflict-actions">
          <button class="btn btn-primary btn-sm" data-conflict-action data-conflict-id="${escapeAttr(c.conflict_id)}" data-resolution="local">Keep Local</button>
          <button class="btn btn-outline btn-sm" data-conflict-action data-conflict-id="${escapeAttr(c.conflict_id)}" data-resolution="remote">Keep Remote</button>
          <button class="btn btn-outline btn-sm" data-conflict-action data-conflict-id="${escapeAttr(c.conflict_id)}" data-resolution="both">Keep Both</button>
        </div>
      </div>
    `).join('');
  }

  // ─── Settings ────────────────────────────────────────────────
  async function loadSettings() {
    try {
      state.settings = normalizeSettings(await invoke('get_settings'));
    } catch(e) {
      console.warn('Settings load:', e);
    }
    const s = state.settings;
    document.getElementById('inputEndpoint').value = s.endpoint || '';
    document.getElementById('inputBucket').value = s.bucket || '';
    document.getElementById('inputAccessKey').value = s.access_key_id || '';
    document.getElementById('inputRegion').value = s.region || 'us-east-1';
    document.getElementById('inputSyncFolder').value = s.sync_folder || '~/S4Drive';
    document.getElementById('inputPolling').value = s.polling_interval_sec || 30;
    document.getElementById('inputBandwidth').value = s.bandwidth_limit_kbps ?? '';
    document.getElementById('inputMaxUploads').value = s.max_concurrent_uploads || 4;
    document.getElementById('inputMaxDownloads').value = s.max_concurrent_downloads || 4;
    document.getElementById('inputExcludes').value = (s.excludes || []).join('\n');
    document.getElementById('inputProxy').value = s.proxy || '';
    document.getElementById('inputAutostart').checked = Boolean(s.autostart);
    document.getElementById('inputDarkMode').checked = Boolean(s.dark_mode);
    document.getElementById('inputUseTls').checked = Boolean(s.use_tls);
    setTheme(Boolean(s.dark_mode));
  }

  async function refreshSettings() {
    try {
      const info = await invoke('get_app_info');
      document.getElementById('aboutVersion').textContent = info.version || '0.1.0';
      document.getElementById('aboutDeviceId').textContent = info.device_id || '—';
      document.getElementById('aboutScreenVersion').textContent = info.version || '0.1.0';
      document.getElementById('aboutCoreVersion').textContent = info.core_version || '0.1.0';
      document.getElementById('aboutScreenDeviceId').textContent = info.device_id || '—';
    } catch(e) { /* noop */ }
  }

  async function saveSettings() {
    const settings = collectSettings();
    try {
      state.settings = normalizeSettings(await invoke('save_settings', {
        settings,
        secretKey: document.getElementById('inputSecretKey').value || null,
      }));
      document.getElementById('inputSecretKey').value = '';
      setTheme(state.settings.dark_mode);
      await refreshSettings();
      showToast('Settings saved', 'success');
    } catch(e) {
      showToast(`Save failed: ${e}`, 'error');
    }
  }

  async function testConnection() {
    try {
      const result = await invoke('test_connection', {
        settings: collectSettings(),
        secretKey: document.getElementById('inputSecretKey').value || null,
      });
      showToast(result.message || (result.ok ? 'Connection OK' : 'Connection failed'), result.ok ? 'success' : 'error');
    } catch(e) {
      showToast(`Connection test failed: ${e}`, 'error');
    }
  }

  function collectSettings() {
    const bandwidth = parseInt(document.getElementById('inputBandwidth').value, 10);
    const proxy = document.getElementById('inputProxy').value.trim();
    return {
      endpoint: document.getElementById('inputEndpoint').value.trim(),
      bucket: document.getElementById('inputBucket').value.trim(),
      access_key_id: document.getElementById('inputAccessKey').value.trim(),
      region: document.getElementById('inputRegion').value.trim() || 'us-east-1',
      sync_folder: document.getElementById('inputSyncFolder').value.trim() || '~/S4Drive',
      bucket_prefix: state.settings.bucket_prefix || '/',
      polling_interval_sec: parseInt(document.getElementById('inputPolling').value, 10) || 30,
      bandwidth_limit_kbps: Number.isFinite(bandwidth) && bandwidth > 0 ? bandwidth : null,
      max_concurrent_uploads: parseInt(document.getElementById('inputMaxUploads').value, 10) || 4,
      max_concurrent_downloads: parseInt(document.getElementById('inputMaxDownloads').value, 10) || 4,
      excludes: document.getElementById('inputExcludes').value.split(/\r?\n/).map(s => s.trim()).filter(Boolean),
      proxy: proxy || null,
      autostart: document.getElementById('inputAutostart').checked,
      dark_mode: document.getElementById('inputDarkMode').checked,
      use_tls: document.getElementById('inputUseTls').checked,
    };
  }

  function normalizeSettings(settings) {
    return Object.assign({}, state.settings, settings || {});
  }

  async function runDiagnostics() {
    try {
      const items = await invoke('run_diagnostics');
      renderDiagnostics(items || []);
    } catch(e) {
      renderDiagnostics([{ name: 'Diagnostics', status: 'error', detail: String(e) }]);
    }
  }

  function renderDiagnostics(items) {
    const list = document.getElementById('diagnosticsList');
    if (!items.length) {
      list.innerHTML = `<div class="empty-state"><div class="empty-icon">🩺</div><div class="empty-title">No Diagnostics Yet</div></div>`;
      return;
    }
    list.innerHTML = items.map(item => `
      <div class="diagnostic-item ${escapeAttr(item.status || '')}">
        <div class="diagnostic-status">${escapeHtml(item.status || '')}</div>
        <div class="diagnostic-content">
          <div class="diagnostic-name">${escapeHtml(item.name || '')}</div>
          <div class="diagnostic-detail">${escapeHtml(item.detail || '')}</div>
        </div>
      </div>
    `).join('');
  }

  async function checkUpdates() {
    try {
      const info = await invoke('check_for_updates');
      document.getElementById('updateStatus').textContent = info.message || 'No update available';
      showToast(info.update_available ? 'Update available' : 'No update available', 'info');
    } catch(e) {
      showToast(`Update check failed: ${e}`, 'error');
    }
  }

  // ─── Helpers ─────────────────────────────────────────────────
  function escapeHtml(str) {
    return String(str ?? '').replace(/[&<>"']/g, ch => ({
      '&': '&amp;',
      '<': '&lt;',
      '>': '&gt;',
      '"': '&quot;',
      "'": '&#39;',
    }[ch]));
  }

  function escapeAttr(str) {
    return escapeHtml(str);
  }

  function formatTime(ts) {
    if (!ts) return '';
    try {
      return new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    } catch(e) { return ''; }
  }

  // ─── Init ────────────────────────────────────────────────────
  async function init() {
    if (!invoke || !listen) {
      document.body.classList.add('ready');
      return;
    }

    // Load saved theme
    setTheme(true);

    // Wire navigation
    document.querySelectorAll('.nav-btn').forEach(btn => {
      btn.addEventListener('click', () => navigate(btn.dataset.route));
    });

    // Wire settings buttons
    document.getElementById('btnSaveAccount').addEventListener('click', saveSettings);
    document.getElementById('btnTestConnection').addEventListener('click', testConnection);
    document.getElementById('btnRunDiagnostics').addEventListener('click', () => {
      navigate('diagnostics');
      runDiagnostics();
    });
    document.getElementById('btnCheckUpdates').addEventListener('click', checkUpdates);
    document.getElementById('themeToggle').addEventListener('click', toggleTheme);
    document.getElementById('inputDarkMode').addEventListener('change', function() {
      setTheme(this.checked);
    });

    // Wire view toggle
    document.querySelectorAll('[data-view]').forEach(btn => {
      btn.addEventListener('click', function() {
        document.querySelectorAll('[data-view]').forEach(b => b.classList.remove('active'));
        this.classList.add('active');
        document.getElementById('fileList').className = 'card ' + (this.dataset.view === 'grid' ? 'file-grid' : 'file-list');
      });
    });

    document.getElementById('conflictList').addEventListener('click', (event) => {
      const button = event.target.closest('[data-conflict-action]');
      if (!button) return;
      window.__resolveConflict(button.dataset.conflictId, button.dataset.resolution);
    });

    // Wire keyboard navigation
    document.addEventListener('keydown', (e) => {
      if (e.altKey) {
        const routes = ['overview', 'files', 'transfers', 'conflicts', 'activity', 'settings', 'diagnostics', 'about'];
        const idx = '12345678'.indexOf(e.key);
        if (idx >= 0) navigate(routes[idx]);
      }
      if (e.key === 'Escape') {
        document.querySelectorAll('.toast').forEach(t => t.remove());
      }
    });

    // Listen for Tauri events
    await listen('navigate', (event) => {
      navigate(event.payload);
      if (event.payload === 'diagnostics') runDiagnostics();
    });

    await listen('sync-triggered', () => {
      refreshDashboard();
      showToast('Sync triggered', 'info');
    });

    await listen('sync-status-changed', (event) => {
      state.sync = event.payload;
      updateStatusBadge(state.sync);
    });

    await listen('app-exiting', () => {
      showToast('Exiting S4Drive', 'info', 1000);
    });

    // Initial data load
    await loadSettings();
    await Promise.all([
      refreshDashboard(),
      refreshActivity(),
      refreshTransfers(),
      refreshConflicts(),
      refreshSettings(),
    ]);

    const pendingRoute = await invoke('take_pending_route');
    if (pendingRoute) {
      navigate(pendingRoute);
      if (pendingRoute === 'diagnostics') runDiagnostics();
    }

    // Auto-refresh
    setInterval(refreshDashboard, 5000);
    setInterval(refreshActivity, 10000);
    setInterval(refreshTransfers, 10000);
    setInterval(refreshConflicts, 30000);

    // Show app
    document.body.classList.add('ready');
  }

  // Expose for inline onclick
  window.__resolveConflict = async (conflictId, resolution) => {
    try {
      await invoke('resolve_conflict', { conflictId, resolution });
      showToast(`Conflict resolved: ${resolution}`, 'success');
      refreshConflicts();
      refreshDashboard();
    } catch(e) {
      showToast(`Failed to resolve: ${e}`, 'error');
    }
  };

  // Go!
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
