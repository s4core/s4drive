/* ═══════════════════════════════════════════════════════════════
   S4Drive Application — Main Logic
   ═══════════════════════════════════════════════════════════════ */

(function() {
  'use strict';

  // ─── IPC Helper ──────────────────────────────────────────────
  const invoke = window.__TAURI_INTERNALS__.invoke;
  const listen = window.__TAURI_INTERNALS__.listen;

  // ─── State ───────────────────────────────────────────────────
  const state = {
    settings: {
      endpoint: '',
      bucket: '',
      accessKey: '',
      syncFolder: '',
      polling: 30,
      darkMode: true,
      autostart: false,
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
    state.settings.darkMode = dark;
  }

  function toggleTheme() {
    setTheme(!state.settings.darkMode);
    showToast(state.settings.darkMode ? 'Dark mode enabled' : 'Light mode enabled', 'info');
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
    toast.innerHTML = `<span>${icons[type] || 'ℹ️'}</span><span>${msg}</span>`;
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
    badge.classList.add(status.state || 'idle');
    badge.textContent = status.state ? status.state.charAt(0).toUpperCase() + status.state.slice(1) : 'Idle';
  }

  // ─── Dashboard ───────────────────────────────────────────────
  async function refreshDashboard() {
    try {
      const sync = await invoke('get_sync_status');
      state.sync = sync;
      document.getElementById('syncState').textContent = sync.state || '—';
      document.getElementById('conflictCount').textContent = sync.conflicts ?? 0;
      document.getElementById('totalFiles').textContent = '—';
      document.getElementById('transferCount').textContent = '—';
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
    const list = document.getElementById('activityList');
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
    // Placeholder — will be wired to TransferQueue
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
          <button class="btn btn-primary btn-sm" onclick="window.__resolveConflict('${c.conflict_id}','local')">Keep Local</button>
          <button class="btn btn-outline btn-sm" onclick="window.__resolveConflict('${c.conflict_id}','remote')">Keep Remote</button>
          <button class="btn btn-outline btn-sm" onclick="window.__resolveConflict('${c.conflict_id}','both')">Keep Both</button>
        </div>
      </div>
    `).join('');
  }

  // ─── Settings ────────────────────────────────────────────────
  function loadSettings() {
    const s = state.settings;
    document.getElementById('inputEndpoint').value = s.endpoint;
    document.getElementById('inputBucket').value = s.bucket;
    document.getElementById('inputSyncFolder').value = s.syncFolder;
    document.getElementById('inputPolling').value = s.polling;
    document.getElementById('inputAutostart').checked = s.autostart;
    document.getElementById('inputDarkMode').checked = s.darkMode;
  }

  async function refreshSettings() {
    try {
      const info = await invoke('get_app_info');
      document.getElementById('aboutVersion').textContent = info.version || '0.1.0';
      document.getElementById('aboutDeviceId').textContent = info.device_id || '—';
    } catch(e) { /* noop */ }
  }

  function saveSettings() {
    state.settings.endpoint = document.getElementById('inputEndpoint').value;
    state.settings.bucket = document.getElementById('inputBucket').value;
    state.settings.syncFolder = document.getElementById('inputSyncFolder').value;
    state.settings.polling = parseInt(document.getElementById('inputPolling').value) || 30;
    state.settings.autostart = document.getElementById('inputAutostart').checked;
    const dark = document.getElementById('inputDarkMode').checked;
    setTheme(dark);
    showToast('Settings saved', 'success');
  }

  // ─── Helpers ─────────────────────────────────────────────────
  function escapeHtml(str) {
    if (!str) return '';
    const div = document.createElement('div');
    div.textContent = str;
    return div.innerHTML;
  }

  function formatTime(ts) {
    if (!ts) return '';
    try {
      return new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    } catch(e) { return ''; }
  }

  // ─── Init ────────────────────────────────────────────────────
  async function init() {
    // Load saved theme
    setTheme(true);

    // Wire navigation
    document.querySelectorAll('.nav-btn').forEach(btn => {
      btn.addEventListener('click', () => navigate(btn.dataset.route));
    });

    // Wire settings buttons
    document.getElementById('btnSaveAccount').addEventListener('click', saveSettings);
    document.getElementById('btnTestConnection').addEventListener('click', () => {
      showToast('Testing connection... (not yet implemented)', 'info');
    });
    document.getElementById('btnRunDiagnostics').addEventListener('click', () => {
      showToast('Running diagnostics... (not yet implemented)', 'info');
    });
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

    // Wire keyboard navigation
    document.addEventListener('keydown', (e) => {
      if (e.altKey) {
        const routes = ['overview', 'files', 'transfers', 'conflicts', 'settings'];
        const idx = '12345'.indexOf(e.key);
        if (idx >= 0) navigate(routes[idx]);
      }
      if (e.key === 'Escape') {
        document.querySelectorAll('.toast').forEach(t => t.remove());
      }
    });

    // Listen for Tauri events
    listen('navigate', (event) => {
      navigate(event.payload);
    });

    listen('sync-triggered', () => {
      refreshDashboard();
      showToast('Sync triggered', 'info');
    });

    // Initial data load
    await Promise.all([
      refreshDashboard(),
      refreshActivity(),
      refreshSettings(),
    ]);

    loadSettings();

    // Auto-refresh
    setInterval(refreshDashboard, 5000);
    setInterval(refreshActivity, 10000);
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
