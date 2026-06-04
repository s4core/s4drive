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
      excludes: ['node_modules', '.DS_Store'],
      proxy: null,
      dark_mode: true,
      use_system_theme: true,
      autostart: false,
      use_tls: true,
      large_sync_confirmed: false,
    },
    sync: { running: false, paused: false, state: 'idle', conflicts: 0, total_files: null, lastSync: null, detail: 'Idle' },
    files: [],
    fileQuery: '',
    fileView: 'list',
    fileScrollTop: 0,
    fileScrollFrame: null,
    selectedFileId: null,
    versions: [],
    devices: [],
    transfers: [],
    conflicts: [],
    activities: [],
    largeSyncPromptOpen: false,
    largeSyncPromptPending: false,
    syncRequestInFlight: false,
    initialized: false,
  };

  const FILE_ROW_HEIGHT = 42;
  const FILE_OVERSCAN_ROWS = 8;
  const FILE_VIRTUAL_THRESHOLD = 200;

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
  function setTheme(dark, persist = true) {
    document.documentElement.classList.toggle('theme-light', !dark);
    document.getElementById('themeToggle').textContent = dark ? '🌙' : '☀️';
    document.getElementById('inputDarkMode').checked = dark;
    if (persist) state.settings.dark_mode = dark;
  }

  function applyThemeSettings() {
    const systemDark = window.matchMedia?.('(prefers-color-scheme: dark)').matches ?? true;
    setTheme(state.settings.use_system_theme ? systemDark : state.settings.dark_mode, false);
    document.getElementById('inputUseSystemTheme').checked = Boolean(state.settings.use_system_theme);
  }

  function toggleTheme() {
    state.settings.use_system_theme = false;
    document.getElementById('inputUseSystemTheme').checked = false;
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
    if (route === 'files') refreshFiles();
    if (route === 'versions') refreshVersions();
    if (route === 'account') refreshDevices();
    if (route === 'diagnostics') runDiagnostics();
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
    const cls = ['syncing', 'paused', 'attention', 'error', 'idle'].find(c => badge.classList.contains(c));
    if (cls) badge.classList.remove(cls);
    const next = status.state || 'idle';
    badge.classList.add(next);
    badge.textContent = syncStateLabel(next);
    badge.title = status.detail || next;
  }

  function syncStateLabel(value) {
    if (value === 'attention') return 'Action required';
    return value.charAt(0).toUpperCase() + value.slice(1);
  }

  function updateTotalFiles() {
    const total = Number.isFinite(state.sync?.total_files)
      ? state.sync.total_files
      : (state.files.length || 0);
    document.getElementById('totalFiles').textContent = String(total);
  }

  // ─── Dashboard ───────────────────────────────────────────────
  async function refreshDashboard() {
    try {
      const sync = await invoke('get_sync_status');
      state.sync = sync;
      const syncState = document.getElementById('syncState');
      syncState.textContent = sync.state ? syncStateLabel(sync.state) : '—';
      syncState.title = sync.detail || sync.state || '';
      document.getElementById('conflictCount').textContent = sync.conflicts ?? 0;
      updateTotalFiles();
      document.getElementById('transferCount').textContent = String(state.transfers.length || 0);
      document.getElementById('btnPauseSync').textContent = sync.paused ? 'Resume Sync' : 'Pause Sync';
      updateStatusBadge(sync);
    } catch(e) {
      document.getElementById('syncState').textContent = 'Offline';
      console.warn('Dashboard refresh:', e);
    }
  }

  // ─── Files ───────────────────────────────────────────────────
  async function refreshFiles() {
    try {
      state.files = await invoke('get_files') || [];
      if (state.selectedFileId && !state.files.some(file => file.file_id === state.selectedFileId)) {
        state.selectedFileId = null;
      }
      renderFiles();
      renderFileDetails();
      updateTotalFiles();
    } catch(e) {
      renderFileError(humanizeError(e));
    }
  }

  function renderFiles() {
    const list = document.getElementById('fileList');
    const files = filteredFiles();
    const isGrid = state.fileView === 'grid';
    list.className = isGrid ? 'file-grid' : 'file-list';

    if (!files.length) {
      const isSearch = state.fileQuery.trim().length > 0;
      list.innerHTML = `
        <div class="empty-state">
          <div class="empty-icon">📁</div>
          <div class="empty-title">${isSearch ? 'No Matching Files' : 'No Files Synced'}</div>
          <div class="empty-desc">${isSearch ? 'Try a different search term.' : 'Connect storage and choose a sync folder to populate this explorer.'}</div>
        </div>`;
      return;
    }

    if (!isGrid && files.length > FILE_VIRTUAL_THRESHOLD) {
      renderVirtualFileList(list, files);
      return;
    }

    list.innerHTML = files.map(renderFileItem).join('');
  }

  function renderVirtualFileList(list, files) {
    list.className = 'file-list is-virtualized';

    const viewportHeight = list.clientHeight || 420;
    const totalHeight = files.length * FILE_ROW_HEIGHT;
    const maxScrollTop = Math.max(0, totalHeight - viewportHeight);
    const scrollTop = Math.max(0, Math.min(state.fileScrollTop, maxScrollTop));
    const firstVisible = Math.floor(scrollTop / FILE_ROW_HEIGHT);
    const start = Math.max(0, firstVisible - FILE_OVERSCAN_ROWS);
    const visibleRows = Math.ceil(viewportHeight / FILE_ROW_HEIGHT) + FILE_OVERSCAN_ROWS * 2;
    const end = Math.min(files.length, start + visibleRows);
    const topSpacer = start * FILE_ROW_HEIGHT;
    const bottomSpacer = Math.max(0, totalHeight - topSpacer - (end - start) * FILE_ROW_HEIGHT);

    list.innerHTML = `
      <div class="file-virtual-spacer" style="height:${topSpacer}px"></div>
      ${files.slice(start, end).map(renderFileItem).join('')}
      <div class="file-virtual-spacer" style="height:${bottomSpacer}px"></div>`;

    if (Math.abs(list.scrollTop - scrollTop) > 1) {
      list.scrollTop = scrollTop;
    }
  }

  function renderFileItem(file) {
    return `
      <button class="file-item ${file.file_id === state.selectedFileId ? 'selected' : ''}" data-file-id="${escapeAttr(file.file_id)}">
        <span class="file-icon">${getFileIcon(file.kind === 'folder' ? 'folder' : file.name)}</span>
        <span class="file-name">${escapeHtml(file.name || file.path)}</span>
        <span class="file-size">${formatBytes(file.size_bytes)}</span>
        <span class="file-date">${formatDate(file.modified_at)}</span>
        <span class="file-status"><span class="sync-badge ${escapeAttr(file.sync_state || 'synced')}">${formatStatus(file.sync_state)}</span></span>
      </button>`;
  }

  function filteredFiles() {
    const query = state.fileQuery.trim().toLowerCase();
    if (!query) return state.files;
    return state.files.filter(file => [file.name, file.path, file.kind, file.sync_state]
      .filter(Boolean)
      .some(value => String(value).toLowerCase().includes(query)));
  }

  function renderFileDetails() {
    const panel = document.getElementById('fileDetailsPanel');
    const file = state.files.find(item => item.file_id === state.selectedFileId);

    if (!file) {
      panel.innerHTML = `
        <div class="details-empty">
          <div class="empty-icon">⌁</div>
          <div class="empty-title">Select a File</div>
          <div class="empty-desc">Size, dates, sync state, versions, and tags appear here.</div>
        </div>`;
      return;
    }

    panel.innerHTML = `
      <div class="details-header">
        <div class="details-icon">${getFileIcon(file.kind === 'folder' ? 'folder' : file.name)}</div>
        <div>
          <div class="details-title">${escapeHtml(file.name || file.path)}</div>
          <div class="details-subtitle">${escapeHtml(file.path || '')}</div>
        </div>
      </div>
      <div class="details-list">
        <div><span>Size</span><strong>${formatBytes(file.size_bytes)}</strong></div>
        <div><span>Modified</span><strong>${formatDate(file.modified_at) || 'Unknown'}</strong></div>
        <div><span>Status</span><strong>${formatStatus(file.sync_state)}</strong></div>
        <div><span>File ID</span><strong class="mono">${escapeHtml(file.file_id)}</strong></div>
      </div>
      <div class="details-actions">
        <button class="btn btn-outline btn-sm" data-route-target="versions">Versions</button>
        <button class="btn btn-outline btn-sm" disabled>Restore</button>
      </div>`;
  }

  function renderFileError(message) {
    document.getElementById('fileList').innerHTML = `
      <div class="error-state">
        <div class="error-icon">!</div>
        <div class="error-title">Cannot Load Files</div>
        <div class="error-desc">${escapeHtml(message)}</div>
      </div>`;
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

  // ─── Versions / Devices ──────────────────────────────────────
  async function refreshVersions() {
    try {
      state.versions = await invoke('get_versions') || [];
      renderVersions();
    } catch(e) {
      renderTimelineError(humanizeError(e));
    }
  }

  function renderVersions() {
    const timeline = document.getElementById('versionTimeline');
    if (!state.versions.length) {
      timeline.innerHTML = `
        <div class="empty-state">
          <div class="empty-icon">↺</div>
          <div class="empty-title">No Versions Yet</div>
          <div class="empty-desc">Synced file revisions will appear in this timeline.</div>
        </div>`;
      return;
    }

    timeline.innerHTML = state.versions.map(version => `
      <div class="timeline-item">
        <div class="timeline-dot"></div>
        <div class="timeline-content">
          <div class="timeline-title">${escapeHtml(version.label || version.revision_id)}</div>
          <div class="timeline-meta">${escapeHtml(version.author || 'Unknown device')} · ${formatDate(version.created_at)}</div>
          <div class="timeline-detail">${formatBytes(version.size_bytes)} · ${escapeHtml(version.status || 'saved')}</div>
        </div>
      </div>
    `).join('');
  }

  function renderTimelineError(message) {
    document.getElementById('versionTimeline').innerHTML = `
      <div class="error-state">
        <div class="error-icon">!</div>
        <div class="error-title">Cannot Load Versions</div>
        <div class="error-desc">${escapeHtml(message)}</div>
      </div>`;
  }

  async function refreshDevices() {
    try {
      state.devices = await invoke('get_devices') || [];
      renderDevices();
    } catch(e) {
      document.getElementById('deviceList').innerHTML = `
        <div class="error-state">
          <div class="error-icon">!</div>
          <div class="error-title">Cannot Load Devices</div>
          <div class="error-desc">${escapeHtml(humanizeError(e))}</div>
        </div>`;
    }
  }

  function renderDevices() {
    const list = document.getElementById('deviceList');
    if (!state.devices.length) {
      list.innerHTML = `
        <div class="empty-state">
          <div class="empty-icon">●</div>
          <div class="empty-title">No Devices Loaded</div>
        </div>`;
      return;
    }

    list.innerHTML = state.devices.map(device => `
      <div class="device-item">
        <div class="device-avatar">${device.trusted ? '✓' : '?'}</div>
        <div class="device-content">
          <div class="device-name">${escapeHtml(device.name || 'Device')}</div>
          <div class="device-meta">${escapeHtml(device.role || '')} · ${formatDate(device.last_seen) || 'Never seen'}</div>
          <div class="device-id mono">${escapeHtml(device.device_id || '')}</div>
        </div>
        <span class="sync-badge ${device.trusted ? 'synced' : 'pending'}">${device.trusted ? 'Trusted' : 'Review'}</span>
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
    document.getElementById('inputAccountSyncFolder').value = s.sync_folder || '~/S4Drive';
    document.getElementById('inputSyncFolder').value = s.sync_folder || '~/S4Drive';
    document.getElementById('inputPolling').value = s.polling_interval_sec || 30;
    document.getElementById('inputBandwidth').value = s.bandwidth_limit_kbps ?? '';
    document.getElementById('inputMaxUploads').value = s.max_concurrent_uploads || 4;
    document.getElementById('inputMaxDownloads').value = s.max_concurrent_downloads || 4;
    document.getElementById('inputExcludes').value = (s.excludes || []).join('\n');
    document.getElementById('inputProxy').value = s.proxy || '';
    document.getElementById('inputAutostart').checked = Boolean(s.autostart);
    document.getElementById('inputDarkMode').checked = Boolean(s.dark_mode);
    document.getElementById('inputUseSystemTheme').checked = Boolean(s.use_system_theme);
    document.getElementById('inputUseTls').checked = Boolean(s.use_tls);
    applyThemeSettings();
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
      await refreshFiles();
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
    const accountSyncFolder = document.getElementById('inputAccountSyncFolder').value.trim();
    const settingsSyncFolder = document.getElementById('inputSyncFolder').value.trim();
    const syncFolder = accountSyncFolder || settingsSyncFolder || '~/S4Drive';
    return {
      endpoint: document.getElementById('inputEndpoint').value.trim(),
      bucket: document.getElementById('inputBucket').value.trim(),
      access_key_id: document.getElementById('inputAccessKey').value.trim(),
      region: document.getElementById('inputRegion').value.trim() || 'us-east-1',
      sync_folder: syncFolder,
      bucket_prefix: state.settings.bucket_prefix || '/',
      polling_interval_sec: parseInt(document.getElementById('inputPolling').value, 10) || 30,
      bandwidth_limit_kbps: Number.isFinite(bandwidth) && bandwidth > 0 ? bandwidth : null,
      max_concurrent_uploads: parseInt(document.getElementById('inputMaxUploads').value, 10) || 4,
      max_concurrent_downloads: parseInt(document.getElementById('inputMaxDownloads').value, 10) || 4,
      excludes: document.getElementById('inputExcludes').value.split(/\r?\n/).map(s => s.trim()).filter(Boolean),
      proxy: proxy || null,
      autostart: document.getElementById('inputAutostart').checked,
      dark_mode: document.getElementById('inputDarkMode').checked,
      use_system_theme: document.getElementById('inputUseSystemTheme').checked,
      use_tls: document.getElementById('inputUseTls').checked,
      large_sync_confirmed: Boolean(state.settings.large_sync_confirmed) && syncFolder === state.settings.sync_folder,
    };
  }

  function normalizeSettings(settings) {
    return Object.assign({}, state.settings, settings || {});
  }

  async function confirmLargeSyncIfNeeded() {
    if (state.largeSyncPromptOpen) return false;
    state.largeSyncPromptOpen = true;
    try {
      const inspection = await invoke('inspect_sync_folder');
      if (!inspection?.requires_confirmation || state.settings.large_sync_confirmed) {
        return true;
      }

      const accepted = window.confirm(
        `${inspection.message}\n\nS4Drive will process this folder in staged batches so the desktop app stays responsive.`
      );
      if (!accepted) {
        showToast('Large sync still requires confirmation. Click the warning tray icon to review it again.', 'warning', 7000);
        return false;
      }

      state.settings.large_sync_confirmed = true;
      state.settings = normalizeSettings(await invoke('save_settings', {
        settings: collectSettings(),
        secretKey: null,
      }));
      showToast('Large sync confirmed', 'success');
      return true;
    } finally {
      state.largeSyncPromptOpen = false;
    }
  }

  async function startSyncNow() {
    if (state.syncRequestInFlight) return;
    state.syncRequestInFlight = true;
    const button = document.getElementById('btnSyncNow');
    button.disabled = true;
    try {
      const canSync = await confirmLargeSyncIfNeeded();
      if (!canSync) return;
      const result = await invoke('sync_now');
      showToast(result?.message || 'Sync complete', 'success', 7000);
      await Promise.all([refreshDashboard(), refreshFiles(), refreshActivity(), refreshTransfers()]);
    } catch(e) {
      if (String(e).toLowerCase().includes('already running')) {
        await refreshDashboard();
      } else {
        showToast(`Sync failed: ${humanizeError(e)}`, 'error');
      }
    } finally {
      button.disabled = false;
      state.syncRequestInFlight = false;
    }
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

  function formatBytes(value) {
    const bytes = Number(value || 0);
    if (!bytes) return '—';
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    let size = bytes;
    let unit = 0;
    while (size >= 1024 && unit < units.length - 1) {
      size /= 1024;
      unit += 1;
    }
    return `${size.toFixed(size >= 10 || unit === 0 ? 0 : 1)} ${units[unit]}`;
  }

  function formatDate(ts) {
    if (!ts) return '';
    try {
      return new Date(ts).toLocaleDateString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
    } catch(e) { return ''; }
  }

  function formatStatus(status) {
    const value = String(status || 'synced').replace(/_/g, ' ');
    return value.charAt(0).toUpperCase() + value.slice(1);
  }

  function formatTime(ts) {
    if (!ts) return '';
    try {
      return new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    } catch(e) { return ''; }
  }

  function humanizeError(error) {
    const message = String(error || '');
    const lower = message.toLowerCase();
    if (lower.includes('timeout') || lower.includes('network') || lower.includes('could not resolve')) {
      return 'Cannot reach the storage server. Check your network and storage URL.';
    }
    if (lower.includes('auth') || lower.includes('access') || lower.includes('credential')) {
      return 'The credentials were rejected. Check the access key, secret key, and region.';
    }
    if (lower.includes('bucket') || lower.includes('not found')) {
      return 'The bucket was not found or is not accessible.';
    }
    return message || 'The operation failed.';
  }

  function moveFileSelection(delta) {
    const files = filteredFiles();
    if (!files.length) return;
    const currentIndex = files.findIndex(file => file.file_id === state.selectedFileId);
    const nextIndex = currentIndex < 0 ? 0 : Math.max(0, Math.min(files.length - 1, currentIndex + delta));
    state.selectedFileId = files[nextIndex].file_id;
    keepFileIndexVisible(nextIndex);
    renderFiles();
    renderFileDetails();
  }

  function keepFileIndexVisible(index) {
    if (state.fileView !== 'list') return;
    const list = document.getElementById('fileList');
    const viewportHeight = list?.clientHeight || 420;
    const rowTop = index * FILE_ROW_HEIGHT;
    const rowBottom = rowTop + FILE_ROW_HEIGHT;
    if (rowTop < state.fileScrollTop) {
      state.fileScrollTop = rowTop;
    } else if (rowBottom > state.fileScrollTop + viewportHeight) {
      state.fileScrollTop = rowBottom - viewportHeight;
    }
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
    document.body.addEventListener('click', (event) => {
      const target = event.target.closest('[data-route-target]');
      if (target) navigate(target.dataset.routeTarget);
    });

    // Wire dashboard actions
    document.getElementById('btnSyncNow').addEventListener('click', startSyncNow);
    document.getElementById('btnPauseSync').addEventListener('click', async () => {
      try {
        const paused = await invoke('toggle_pause');
        showToast(paused ? 'Sync paused' : 'Sync resumed', 'info');
        refreshDashboard();
      } catch(e) {
        showToast(`Pause failed: ${humanizeError(e)}`, 'error');
      }
    });
    document.getElementById('btnOpenAccount').addEventListener('click', () => navigate('account'));

    // Wire settings buttons
    document.getElementById('btnSaveAccount').addEventListener('click', saveSettings);
    document.getElementById('btnTestConnection').addEventListener('click', testConnection);
    document.getElementById('btnRunDiagnostics').addEventListener('click', () => {
      navigate('diagnostics');
      runDiagnostics();
    });
    document.getElementById('btnCheckUpdates').addEventListener('click', checkUpdates);
    document.getElementById('themeToggle').addEventListener('click', toggleTheme);
    ['inputAccountSyncFolder', 'inputSyncFolder'].forEach((id) => {
      document.getElementById(id).addEventListener('input', (event) => {
        const otherId = id === 'inputAccountSyncFolder' ? 'inputSyncFolder' : 'inputAccountSyncFolder';
        document.getElementById(otherId).value = event.target.value;
      });
    });
    document.getElementById('inputUseSystemTheme').addEventListener('change', function() {
      state.settings.use_system_theme = this.checked;
      applyThemeSettings();
    });
    document.getElementById('inputDarkMode').addEventListener('change', function() {
      state.settings.use_system_theme = false;
      document.getElementById('inputUseSystemTheme').checked = false;
      setTheme(this.checked);
    });
    window.matchMedia?.('(prefers-color-scheme: dark)').addEventListener?.('change', () => {
      if (state.settings.use_system_theme) applyThemeSettings();
    });

    // Wire view toggle
    document.querySelectorAll('[data-view]').forEach(btn => {
      btn.addEventListener('click', function() {
        document.querySelectorAll('[data-view]').forEach(b => b.classList.remove('active'));
        this.classList.add('active');
        state.fileView = this.dataset.view === 'grid' ? 'grid' : 'list';
        state.fileScrollTop = 0;
        renderFiles();
      });
    });

    document.getElementById('fileSearch').addEventListener('input', (event) => {
      state.fileQuery = event.target.value;
      state.fileScrollTop = 0;
      renderFiles();
    });
    const fileList = document.getElementById('fileList');
    fileList.addEventListener('scroll', () => {
      if (state.fileView !== 'list') return;
      state.fileScrollTop = fileList.scrollTop;
      if (state.fileScrollFrame) return;
      state.fileScrollFrame = requestAnimationFrame(() => {
        state.fileScrollFrame = null;
        renderFiles();
      });
    });
    fileList.addEventListener('click', (event) => {
      const item = event.target.closest('[data-file-id]');
      if (!item) return;
      state.selectedFileId = item.dataset.fileId;
      renderFiles();
      renderFileDetails();
    });

    document.getElementById('conflictList').addEventListener('click', (event) => {
      const button = event.target.closest('[data-conflict-action]');
      if (!button) return;
      window.__resolveConflict(button.dataset.conflictId, button.dataset.resolution);
    });

    // Wire keyboard navigation
    document.addEventListener('keydown', (e) => {
      if (e.altKey) {
        const routes = ['overview', 'files', 'transfers', 'conflicts', 'versions', 'activity', 'account', 'settings', 'diagnostics', 'about'];
        const idx = '1234567890'.indexOf(e.key);
        if (idx >= 0) navigate(routes[idx]);
      }
      if (e.key === 'Escape') {
        document.querySelectorAll('.toast').forEach(t => t.remove());
      }
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        moveFileSelection(e.key === 'ArrowDown' ? 1 : -1);
      }
    });

    // Listen for Tauri events
    await listen('navigate', (event) => {
      navigate(event.payload);
      if (event.payload === 'diagnostics') runDiagnostics();
    });

    await listen('sync-triggered', () => {
      refreshDashboard();
      refreshFiles();
    });

    await listen('sync-completed', () => {
      refreshDashboard();
      refreshFiles();
      refreshActivity();
      refreshTransfers();
    });

    await listen('files-changed', () => {
      refreshFiles();
      refreshActivity();
      refreshTransfers();
    });

    await listen('sync-status-changed', (event) => {
      const previousTotal = state.sync?.total_files;
      state.sync = event.payload || {};
      if (!Number.isFinite(state.sync.total_files)) {
        state.sync.total_files = previousTotal;
      }
      updateStatusBadge(state.sync);
      const syncState = document.getElementById('syncState');
      if (syncState) {
        syncState.textContent = state.sync.state ? syncStateLabel(state.sync.state) : '—';
        syncState.title = state.sync.detail || state.sync.state || '';
      }
    });

    await listen('large-sync-confirmation-required', () => {
      if (state.initialized) {
        startSyncNow();
      } else {
        state.largeSyncPromptPending = true;
      }
    });

    await listen('app-exiting', () => {
      showToast('Exiting S4Drive', 'info', 1000);
    });

    // Initial data load
    await loadSettings();
    await Promise.all([
      refreshFiles(),
      refreshDashboard(),
      refreshActivity(),
      refreshTransfers(),
      refreshConflicts(),
      refreshVersions(),
      refreshDevices(),
      refreshSettings(),
    ]);

    const pendingRoute = await invoke('take_pending_route');
    if (pendingRoute) {
      navigate(pendingRoute);
      if (pendingRoute === 'diagnostics') runDiagnostics();
      if (pendingRoute === 'account') {
        showToast('Connect storage and choose a sync folder before syncing', 'warning', 6000);
      }
    }
    state.initialized = true;
    if (state.largeSyncPromptPending || state.sync?.state === 'attention') {
      state.largeSyncPromptPending = false;
      startSyncNow();
    }

    // Auto-refresh
    setInterval(refreshDashboard, 5000);
    setInterval(refreshFiles, 15000);
    setInterval(refreshActivity, 10000);
    setInterval(refreshTransfers, 10000);
    setInterval(refreshConflicts, 30000);

    // Show app
    document.body.classList.add('ready');
  }

  // Conflict actions are delegated from the conflict list.
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
