/**
 * Notes DOM view rendering and UI event bindings.
 */
import { deriveLayout } from './notes-state.js';
import { renderMarkdown, applyMarkdownAction } from './notes-markdown.js';

export class NotesView {
  constructor(rootElement, controller) {
    this.root = rootElement;
    this.controller = controller;
    this.activeMenu = null;
    this.searchDebounceTimer = null;
    this.undoToastTimer = null;
    this.editorMode = 'raw'; // 'raw' | 'preview'
    this.savedSelection = null;
    this.savedScrollTop = 0;
    this.savedPreviewScrollTop = 0;

    this.onKeyDown = this.onKeyDown.bind(this);
    this.onDocumentClick = this.onDocumentClick.bind(this);
    document.addEventListener('keydown', this.onKeyDown);
    document.addEventListener('click', this.onDocumentClick);
  }

  destroy() {
    document.removeEventListener('keydown', this.onKeyDown);
    document.removeEventListener('click', this.onDocumentClick);
    if (this.undoToastTimer) clearTimeout(this.undoToastTimer);
    if (this.searchDebounceTimer) clearTimeout(this.searchDebounceTimer);
  }

  onKeyDown(e) {
    if (e.key === 'Escape') {
      if (this.controller.state.dialog) {
        this.controller.closeDialog();
        e.preventDefault();
      } else if (this.activeMenu) {
        this.closeMenu();
        e.preventDefault();
      } else if (this.controller.state.recentlyDeletedOpen) {
        this.controller.closeRecentlyDeleted();
        e.preventDefault();
      } else if (this.controller.state.drawerOpen) {
        this.controller.setDrawerOpen(false);
        e.preventDefault();
      }
    }
  }

  onDocumentClick(e) {
    if (this.activeMenu && !e.target.closest('.notes-menu-dropdown') && !e.target.closest('.notes-more-btn')) {
      this.closeMenu();
    }
  }

  closeMenu() {
    if (this.activeMenu) {
      this.activeMenu.remove();
      this.activeMenu = null;
    }
  }

  render(state = this.controller.state) {
    if (!state) return;
    const layout = deriveLayout(state);
    this.root.innerHTML = '';

    const container = document.createElement('div');
    container.className = 'notes-container';

    // Header
    container.appendChild(this.renderHeader(state, layout));

    // Error / Unavailable banner if present
    if (state.errorMessage) {
      container.appendChild(this.renderBanner(state));
    }

    // Body with 3 panels
    const body = document.createElement('div');
    body.className = 'notes-body';

    // Tree panel
    body.appendChild(this.renderTreePanel(state, layout));

    // Drawer backdrop on compact layout
    if (layout.drawer) {
      const backdrop = document.createElement('div');
      backdrop.className = 'notes-drawer-backdrop';
      backdrop.onclick = () => this.controller.setDrawerOpen(false);
      body.appendChild(backdrop);
    }

    // List panel
    body.appendChild(this.renderListPanel(state, layout));

    // Editor panel
    body.appendChild(this.renderEditorPanel(state, layout));

    container.appendChild(body);

    // Dialog if active
    if (state.dialog) {
      container.appendChild(this.renderDialog(state.dialog));
    }

    // Recently deleted modal if open
    if (state.recentlyDeletedOpen) {
      container.appendChild(this.renderRecentlyDeletedModal(state));
    }

    // Backup modal if active
    if (state.backupJob) {
      container.appendChild(this.renderBackupModal(state.backupJob));
    }

    // Undo toast if active
    if (state.undoToast) {
      container.appendChild(this.renderUndoToast(state.undoToast));
    }

    this.root.appendChild(container);
  }

  renderHeader(state, layout) {
    const header = document.createElement('header');
    header.className = 'notes-header';

    const left = document.createElement('div');
    left.className = 'notes-header-left';

    // Drawer toggle on compact screens
    if (state.availableWidth < 640) {
      const drawerBtn = document.createElement('button');
      drawerBtn.className = 'btn btn-outline notes-drawer-btn';
      drawerBtn.innerHTML = '📁 Folders';
      drawerBtn.title = 'Toggle folders';
      drawerBtn.onclick = () => this.controller.setDrawerOpen(!state.drawerOpen);
      left.appendChild(drawerBtn);
    }

    const title = document.createElement('span');
    title.className = 'notes-header-title';
    title.textContent = 'Notes';
    left.appendChild(title);

    // File operations need a ready Notes folder; Restore location stays in the banner.
    const ready = Boolean(this.controller.rootId());

    const newNoteBtn = document.createElement('button');
    newNoteBtn.className = 'btn btn-primary notes-new-note-btn';
    newNoteBtn.textContent = 'New note';
    newNoteBtn.onclick = () => this.controller.createNewNote();
    newNoteBtn.disabled = !ready;
    left.appendChild(newNoteBtn);

    const newFolderBtn = document.createElement('button');
    newFolderBtn.className = 'btn btn-outline notes-new-folder-btn';
    newFolderBtn.textContent = 'New folder';
    newFolderBtn.onclick = () => this.controller.openCreateFolderDialog();
    newFolderBtn.disabled = !ready;
    left.appendChild(newFolderBtn);

    const syncBtn = document.createElement('button');
    syncBtn.className = 'btn btn-outline notes-sync-btn';
    syncBtn.textContent = 'Sync';
    syncBtn.onclick = () => this.controller.syncNow();
    syncBtn.disabled = !ready;
    left.appendChild(syncBtn);

    const backupBtn = document.createElement('button');
    backupBtn.className = 'btn btn-outline notes-backup-btn';
    backupBtn.textContent = 'Backup';
    backupBtn.onclick = () => this.controller.backupNow();
    backupBtn.disabled = !ready;
    left.appendChild(backupBtn);

    header.appendChild(left);

    const right = document.createElement('div');
    right.className = 'notes-header-right';

    const searchInput = document.createElement('input');
    searchInput.type = 'search';
    searchInput.className = 'notes-search-input';
    searchInput.placeholder = 'Search note names…';
    searchInput.value = state.searchQuery || '';
    searchInput.disabled = !ready;
    searchInput.oninput = (e) => {
      const val = e.target.value;
      if (this.searchDebounceTimer) clearTimeout(this.searchDebounceTimer);
      this.searchDebounceTimer = setTimeout(() => {
        this.controller.search(val);
      }, 150);
    };
    right.appendChild(searchInput);

    const recentDeletedBtn = document.createElement('button');
    recentDeletedBtn.className = 'btn btn-outline notes-recently-deleted-btn';
    recentDeletedBtn.textContent = 'Recently deleted';
    recentDeletedBtn.title = 'View recently deleted notes';
    recentDeletedBtn.onclick = () => this.controller.openRecentlyDeleted();
    recentDeletedBtn.disabled = !ready;
    right.appendChild(recentDeletedBtn);

    header.appendChild(right);
    return header;
  }

  renderBanner(state) {
    const banner = document.createElement('div');
    banner.className = 'notes-state-banner';
    banner.setAttribute('role', 'alert');

    const msg = document.createElement('span');
    msg.textContent = state.errorMessage;
    banner.appendChild(msg);

    if (state.status === 'unavailable' && state.unavailable === 'missing') {
      const actions = document.createElement('div');
      actions.style.display = 'flex';
      actions.style.gap = '8px';

      const restoreBtn = document.createElement('button');
      restoreBtn.className = 'btn btn-outline';
      restoreBtn.textContent = 'Restore folder location';
      restoreBtn.onclick = () => this.controller.restoreRootLocation();
      actions.appendChild(restoreBtn);

      banner.appendChild(actions);
    } else if (state.errorRetryable || state.status === 'unavailable') {
      const retryBtn = document.createElement('button');
      retryBtn.className = 'btn btn-outline';
      retryBtn.textContent = state.status === 'error' ? 'Retry' : 'Reload';
      retryBtn.onclick = () => this.controller.reload();
      banner.appendChild(retryBtn);
    }

    return banner;
  }

  renderTreePanel(state, layout) {
    const panel = document.createElement('div');
    panel.className = 'notes-panel notes-panel-tree';
    panel.setAttribute('data-panel', 'tree');
    panel.setAttribute('role', 'tree');
    panel.setAttribute('aria-label', 'Folders and notes tree');

    if (!layout.showTree && !layout.drawer) {
      panel.classList.add('hidden');
      panel.setAttribute('aria-hidden', 'true');
      panel.inert = true;
    }

    if (layout.drawer) {
      panel.classList.add('drawer-open');
    }

    const content = document.createElement('div');
    content.className = 'notes-tree-content';

    const rootId = this.controller.rootId();
    if (!rootId) {
      // Missing or blocked: there is no folder to show or act on.
      panel.appendChild(content);
      return panel;
    }
    const isRootSelected = state.context.kind === 'folder' && state.context.id === rootId;

    // Root folder item
    const rootRow = document.createElement('div');
    rootRow.className = `notes-tree-item notes-tree-root ${isRootSelected ? 'selected' : ''}`;
    rootRow.setAttribute('role', 'treeitem');
    rootRow.setAttribute('aria-selected', isRootSelected ? 'true' : 'false');
    rootRow.setAttribute('tabindex', '0');

    const rootChevron = document.createElement('button');
    rootChevron.className = 'notes-tree-chevron';
    rootChevron.innerHTML = state.expandedIds.has(rootId) ? '▾' : '▸';
    rootChevron.setAttribute('aria-label', 'Toggle Notes root');
    rootChevron.onclick = (e) => {
      e.stopPropagation();
      this.controller.toggleFolderExpanded(rootId);
    };
    rootRow.appendChild(rootChevron);

    const rootIcon = document.createElement('span');
    rootIcon.className = 'notes-tree-icon';
    rootIcon.textContent = '📁';
    rootRow.appendChild(rootIcon);

    const rootLabel = document.createElement('span');
    rootLabel.className = 'notes-tree-label';
    rootLabel.textContent = 'Notes';
    rootRow.appendChild(rootLabel);

    const rootMoreBtn = document.createElement('button');
    rootMoreBtn.className = 'notes-more-btn';
    rootMoreBtn.textContent = '⋯';
    rootMoreBtn.setAttribute('aria-label', 'More options for Notes');
    rootMoreBtn.onclick = (e) => {
      e.stopPropagation();
      this.openItemMenu(e, { id: rootId, is_root: true, kind: 'folder', name: 'Notes' });
    };
    rootRow.appendChild(rootMoreBtn);

    rootRow.onclick = () => {
      this.controller.selectFolder(rootId);
    };
    content.appendChild(rootRow);

    // Recursive or nested rendering of tree entries
    if (state.expandedIds.has(rootId)) {
      this.renderTreeChildren(content, state, rootId, 1);
    }

    panel.appendChild(content);
    return panel;
  }

  renderTreeChildren(container, state, parentId, depth) {
    const children = (state.treeEntries || []).filter((e) => e.parent_id === parentId);
    // Sort folders first, natural sort by display_name, tie-break by id
    children.sort((a, b) => {
      if (a.kind !== b.kind) return a.kind === 'folder' ? -1 : 1;
      const cmp = a.display_name.localeCompare(b.display_name, undefined, { numeric: true, sensitivity: 'base' });
      return cmp !== 0 ? cmp : a.id.localeCompare(b.id);
    });

    for (const item of children) {
      const isFolder = item.kind === 'folder';
      const isSelected = isFolder
        ? state.context.kind === 'folder' && state.context.id === item.id
        : state.context.kind === 'note' && state.activeNoteId === item.id;

      const row = document.createElement('div');
      row.className = `notes-tree-item ${isSelected ? 'selected' : ''}`;
      row.style.paddingLeft = `${depth * 16 + 8}px`;
      row.setAttribute('role', 'treeitem');
      row.setAttribute('aria-selected', isSelected ? 'true' : 'false');
      row.setAttribute('tabindex', '0');

      if (isFolder) {
        const chevron = document.createElement('button');
        chevron.className = 'notes-tree-chevron';
        chevron.innerHTML = state.expandedIds.has(item.id) ? '▾' : '▸';
        chevron.setAttribute('aria-label', `Toggle ${item.display_name}`);
        chevron.onclick = (e) => {
          e.stopPropagation();
          this.controller.toggleFolderExpanded(item.id);
        };
        row.appendChild(chevron);
      } else {
        const spacer = document.createElement('span');
        spacer.className = 'notes-tree-chevron';
        row.appendChild(spacer);
      }

      const icon = document.createElement('span');
      icon.className = 'notes-tree-icon';
      icon.textContent = isFolder ? '📁' : '📄';
      row.appendChild(icon);

      const label = document.createElement('span');
      label.className = 'notes-tree-label';
      label.textContent = item.display_name;
      row.appendChild(label);

      const moreBtn = document.createElement('button');
      moreBtn.className = 'notes-more-btn';
      moreBtn.textContent = '⋯';
      moreBtn.setAttribute('aria-label', `More options for ${item.display_name}`);
      moreBtn.onclick = (e) => {
        e.stopPropagation();
        this.openItemMenu(e, item);
      };
      row.appendChild(moreBtn);

      row.onclick = () => {
        if (isFolder) {
          this.controller.selectFolder(item.id);
        } else {
          this.controller.selectTreeNote(item.id);
        }
      };

      container.appendChild(row);

      if (isFolder && state.expandedIds.has(item.id)) {
        this.renderTreeChildren(container, state, item.id, depth + 1);
      }
    }
  }

  renderListPanel(state, layout) {
    const panel = document.createElement('div');
    panel.className = 'notes-panel notes-panel-list';
    panel.setAttribute('data-panel', 'list');
    panel.setAttribute('role', 'region');
    panel.setAttribute('aria-label', 'Folder contents');

    if (!layout.showList) {
      panel.classList.add('hidden');
      panel.setAttribute('aria-hidden', 'true');
      panel.inert = true;
    }

    const content = document.createElement('div');
    content.className = 'notes-list-content';

    const items = state.isSearching ? (state.searchResults || []) : (state.listEntries || []);

    if (items.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'notes-empty-prompt';
      empty.innerHTML = state.isSearching
        ? `<div class="notes-empty-icon">🔍</div><div>No results found</div>`
        : `<div class="notes-empty-icon">📂</div><div>No notes in this folder</div>`;
      content.appendChild(empty);
    } else {
      // Sort folders first, natural sort, tie-break by id
      const sorted = [...items].sort((a, b) => {
        if (a.kind !== b.kind) return a.kind === 'folder' ? -1 : 1;
        const cmp = a.display_name.localeCompare(b.display_name, undefined, { numeric: true, sensitivity: 'base' });
        return cmp !== 0 ? cmp : a.id.localeCompare(b.id);
      });

      for (const item of sorted) {
        const isFolder = item.kind === 'folder';
        const isSelected = !isFolder && state.activeNoteId === item.id;

        const row = document.createElement('div');
        row.className = `notes-list-item ${isSelected ? 'selected' : ''}`;
        row.setAttribute('tabindex', '0');

        const header = document.createElement('div');
        header.className = 'notes-list-header';

        const titleWrap = document.createElement('div');
        titleWrap.className = 'notes-list-title-wrap';

        const icon = document.createElement('span');
        icon.className = 'notes-tree-icon';
        icon.textContent = isFolder ? '📁' : '📄';
        titleWrap.appendChild(icon);

        const title = document.createElement('span');
        title.className = 'notes-list-title';
        title.textContent = item.display_name;
        titleWrap.appendChild(title);

        header.appendChild(titleWrap);

        const moreBtn = document.createElement('button');
        moreBtn.className = 'notes-more-btn';
        moreBtn.textContent = '⋯';
        moreBtn.setAttribute('aria-label', `More options for ${item.display_name}`);
        moreBtn.onclick = (e) => {
          e.stopPropagation();
          this.openItemMenu(e, item);
        };
        header.appendChild(moreBtn);
        row.appendChild(header);

        if (!isFolder && item.excerpt) {
          const excerpt = document.createElement('div');
          excerpt.className = 'notes-list-excerpt';
          excerpt.textContent = item.excerpt.slice(0, 100);
          row.appendChild(excerpt);
        }

        const meta = document.createElement('div');
        meta.className = 'notes-list-meta';

        const time = document.createElement('span');
        const d = new Date(item.updated_at);
        time.textContent = isNaN(d) ? '' : d.toLocaleDateString();
        time.title = isNaN(d) ? '' : d.toLocaleString();
        meta.appendChild(time);

        if (!isFolder && item.sync_state) {
          const badge = document.createElement('span');
          badge.className = `notes-status-badge ${item.sync_state}`;
          badge.textContent = item.sync_state === 'synced' ? 'Synced' : item.sync_state === 'syncing' ? 'Syncing' : item.sync_state;
          meta.appendChild(badge);
        }

        row.appendChild(meta);

        row.onclick = () => {
          if (isFolder) {
            this.controller.selectFolder(item.id);
          } else {
            this.controller.selectListNote(item.id, item.parent_id);
          }
        };

        content.appendChild(row);
      }
    }

    panel.appendChild(content);
    return panel;
  }

  renderEditorPanel(state, layout) {
    const panel = document.createElement('div');
    panel.className = 'notes-panel notes-panel-editor';
    panel.setAttribute('data-panel', 'editor');
    panel.setAttribute('role', 'region');
    panel.setAttribute('aria-label', 'Note editor');

    if (!layout.showEditor) {
      panel.classList.add('hidden');
      panel.setAttribute('aria-hidden', 'true');
      panel.inert = true;
    }

    if (!state.activeNoteId || !state.activeDocument) {
      const prompt = document.createElement('div');
      prompt.className = 'notes-empty-prompt';
      prompt.innerHTML = `
        <div class="notes-empty-icon">📝</div>
        <div class="notes-empty-title">Select a note or create a new one</div>
      `;
      const btn = document.createElement('button');
      btn.className = 'btn btn-primary notes-new-note-prompt-btn';
      btn.textContent = 'New note';
      btn.onclick = () => this.controller.createNewNote();
      prompt.appendChild(btn);
      panel.appendChild(prompt);
      return panel;
    }

    const doc = state.activeDocument;
    const entry = doc.entry;

    // Editor Header
    const header = document.createElement('div');
    header.className = 'notes-editor-header';

    const headerLeft = document.createElement('div');
    headerLeft.className = 'notes-editor-header-left';

    // Back button on compact / medium screens
    if (state.availableWidth < 960) {
      const backBtn = document.createElement('button');
      backBtn.className = 'btn btn-outline notes-back-btn';
      backBtn.innerHTML = '← Back';
      backBtn.setAttribute('aria-label', 'Back');
      backBtn.onclick = () => this.controller.backToFolderList();
      headerLeft.appendChild(backBtn);
    }

    const breadcrumb = document.createElement('div');
    breadcrumb.className = 'notes-breadcrumb';
    breadcrumb.textContent = `Notes / ${entry.relative_path || entry.display_name}`;
    headerLeft.appendChild(breadcrumb);
    header.appendChild(headerLeft);

    const headerRight = document.createElement('div');
    headerRight.style.display = 'flex';
    headerRight.style.alignItems = 'center';
    headerRight.style.gap = '8px';

    const modeToggle = document.createElement('div');
    modeToggle.className = 'notes-mode-toggle';

    const rawBtn = document.createElement('button');
    rawBtn.type = 'button';
    rawBtn.className = `btn btn-outline notes-raw-btn ${this.editorMode === 'raw' ? 'active' : ''}`;
    rawBtn.textContent = 'Raw';
    rawBtn.setAttribute('role', 'button');
    rawBtn.onclick = () => this.switchEditorMode('raw');
    modeToggle.appendChild(rawBtn);

    const previewBtn = document.createElement('button');
    previewBtn.type = 'button';
    previewBtn.className = `btn btn-outline notes-preview-btn ${this.editorMode === 'preview' ? 'active' : ''}`;
    previewBtn.textContent = 'Preview';
    previewBtn.setAttribute('role', 'button');
    previewBtn.onclick = () => this.switchEditorMode('preview');
    modeToggle.appendChild(previewBtn);

    headerRight.appendChild(modeToggle);

    const statusInfo = document.createElement('span');
    statusInfo.className = 'notes-doc-status notes-status-badge';
    const status = this.controller.getActiveSessionStatus();
    statusInfo.textContent = this.getStatusText(status);
    statusInfo.setAttribute('data-status', status);
    headerRight.appendChild(statusInfo);

    const moreBtn = document.createElement('button');
    moreBtn.className = 'notes-more-btn';
    moreBtn.style.opacity = '1';
    moreBtn.textContent = '⋯';
    moreBtn.setAttribute('aria-label', `More options for ${entry.display_name}`);
    moreBtn.onclick = (e) => {
      e.stopPropagation();
      this.openItemMenu(e, entry);
    };
    headerRight.appendChild(moreBtn);

    header.appendChild(headerRight);
    panel.appendChild(header);

    const problemSlot = document.createElement('div');
    problemSlot.className = 'notes-save-problem-slot';
    this.renderSaveProblem(problemSlot, status);
    panel.appendChild(problemSlot);

    // Title input
    const titleWrap = document.createElement('div');
    titleWrap.style.padding = '12px 16px 0 16px';
    const titleInput = document.createElement('input');
    titleInput.type = 'text';
    titleInput.className = 'notes-title-input';
    titleInput.value = entry.display_name;
    titleInput.setAttribute('aria-label', 'Note title');

    let oldTitle = entry.display_name;
    titleInput.onkeydown = (e) => {
      if (e.key === 'Enter') {
        titleInput.blur();
      } else if (e.key === 'Escape') {
        titleInput.value = oldTitle;
        titleInput.blur();
      }
    };
    titleInput.onblur = () => {
      const val = titleInput.value.trim();
      if (val && val !== oldTitle) {
        this.controller.renameEntry(entry.id, val);
        oldTitle = val;
      } else {
        titleInput.value = oldTitle;
      }
    };
    titleWrap.appendChild(titleInput);
    panel.appendChild(titleWrap);

    // Toolbar
    const toolbar = document.createElement('div');
    toolbar.className = 'notes-toolbar';
    toolbar.setAttribute('role', 'toolbar');

    const tools = [
      { label: 'B', action: 'bold', title: 'Bold' },
      { label: 'I', action: 'italic', title: 'Italic' },
      { label: 'H', action: 'heading', title: 'Heading' },
      { label: 'List', action: 'list', title: 'List' },
      { label: 'Task', action: 'task', title: 'Task' },
      { label: 'Link', action: 'link', title: 'Link' },
      { label: 'Code', action: 'code', title: 'Code' },
    ];
    for (const tool of tools) {
      const toolBtn = document.createElement('button');
      toolBtn.type = 'button';
      toolBtn.className = 'notes-tool-btn';
      toolBtn.textContent = tool.label;
      toolBtn.setAttribute('aria-label', tool.label);
      toolBtn.disabled = this.editorMode === 'preview';
      toolBtn.onmousedown = (e) => {
        e.preventDefault();
      };
      toolBtn.onclick = (e) => {
        e.preventDefault();
        this.applyToolbarAction(tool.action);
      };
      toolbar.appendChild(toolBtn);
    }
    panel.appendChild(toolbar);

    // Body Area
    const bodyArea = document.createElement('div');
    bodyArea.className = 'notes-body-area';

    const bodyContent = (this.controller.activeSession ? this.controller.activeSession.body : doc.body) || '';

    const textarea = document.createElement('textarea');
    textarea.className = `notes-textarea ${this.editorMode === 'preview' ? 'hidden' : ''}`;
    textarea.value = bodyContent;
    textarea.textContent = bodyContent;
    textarea.setAttribute('aria-label', 'Note body');
    textarea.oninput = (e) => {
      this.controller.onEditorInput(e.target.value);
    };
    textarea.addEventListener('compositionstart', () => {
      this.controller.onCompositionStart();
    });
    textarea.addEventListener('compositionend', () => {
      this.controller.onCompositionEnd();
    });
    textarea.addEventListener('keydown', (e) => {
      if ((e.ctrlKey || e.metaKey) && (e.key === 's' || e.key === 'S')) {
        e.preventDefault();
        this.controller.retrySave();
      } else if ((e.ctrlKey || e.metaKey) && (e.key === 'b' || e.key === 'B')) {
        e.preventDefault();
        this.applyToolbarAction('bold');
      } else if ((e.ctrlKey || e.metaKey) && (e.key === 'i' || e.key === 'I')) {
        e.preventDefault();
        this.applyToolbarAction('italic');
      }
    });
    bodyArea.appendChild(textarea);

    const previewArea = document.createElement('div');
    previewArea.className = `notes-preview ${this.editorMode === 'preview' ? '' : 'hidden'}`;
    previewArea.setAttribute('aria-label', 'Note preview');
    if (this.editorMode === 'preview') {
      previewArea.appendChild(renderMarkdown(bodyContent));
    }
    bodyArea.appendChild(previewArea);

    panel.appendChild(bodyArea);
    return panel;
  }

  switchEditorMode(mode) {
    if (this.editorMode === mode) return;
    this.editorMode = mode;

    const textarea = this.root.querySelector('textarea.notes-textarea');
    const previewArea = this.root.querySelector('.notes-preview');
    const rawBtn = this.root.querySelector('.notes-raw-btn');
    const previewBtn = this.root.querySelector('.notes-preview-btn');
    const toolbar = this.root.querySelector('.notes-toolbar');

    if (mode === 'preview') {
      if (textarea) {
        this.savedSelection = {
          start: textarea.selectionStart,
          end: textarea.selectionEnd,
        };
        this.savedScrollTop = textarea.scrollTop;
        this.controller.onEditorInput(textarea.value);
      }

      if (previewArea) {
        previewArea.innerHTML = '';
        const body = (this.controller.activeSession ? this.controller.activeSession.body : textarea?.value) || '';
        previewArea.appendChild(renderMarkdown(body));
        previewArea.classList.remove('hidden');
        previewArea.scrollTop = this.savedPreviewScrollTop || 0;
      }
      if (textarea) {
        textarea.classList.add('hidden');
      }
      if (rawBtn) rawBtn.classList.remove('active');
      if (previewBtn) previewBtn.classList.add('active');
      if (toolbar) {
        toolbar.querySelectorAll('button').forEach((b) => { b.disabled = true; });
      }
    } else {
      if (previewArea) {
        this.savedPreviewScrollTop = previewArea.scrollTop;
        previewArea.classList.add('hidden');
      }
      if (textarea) {
        textarea.classList.remove('hidden');
        const body = (this.controller.activeSession ? this.controller.activeSession.body : '') || '';
        if (body) {
          textarea.value = body;
          textarea.textContent = body;
        }
        if (this.savedSelection) {
          textarea.setSelectionRange(this.savedSelection.start, this.savedSelection.end);
        }
        if (typeof this.savedScrollTop === 'number') {
          textarea.scrollTop = this.savedScrollTop;
        }
        textarea.focus();
      }
      if (rawBtn) rawBtn.classList.add('active');
      if (previewBtn) previewBtn.classList.remove('active');
      if (toolbar) {
        toolbar.querySelectorAll('button').forEach((b) => { b.disabled = false; });
      }
    }
  }

  applyToolbarAction(action) {
    const textarea = this.root.querySelector('textarea.notes-textarea');
    if (!textarea) return;
    const val = textarea.value;
    const start = textarea.selectionStart;
    const end = textarea.selectionEnd;
    const res = applyMarkdownAction(val, start, end, action);

    textarea.focus();
    textarea.value = res.body;
    textarea.textContent = res.body;
    textarea.setSelectionRange(res.start, res.end);
    this.controller.onEditorInput(res.body);
  }

  updateSaveStatus(status, session) {
    const badge = this.root.querySelector('.notes-doc-status');
    if (badge) {
      badge.textContent = this.getStatusText(status);
      badge.setAttribute('data-status', status);
    }
    const slot = this.root.querySelector('.notes-save-problem-slot');
    if (slot) this.renderSaveProblem(slot, status);
  }

  /**
   * An unsaved note blocks leaving it. Explain why and offer the two ways out:
   * save again, or take the text elsewhere. Neither discards the draft.
   */
  renderSaveProblem(slot, status) {
    slot.replaceChildren();
    if (status !== 'error' && status !== 'stale') return;
    const alert = document.createElement('div');
    alert.className = 'notes-save-problem';
    alert.setAttribute('role', 'alert');

    const message = document.createElement('span');
    message.textContent = status === 'stale'
      ? 'This note changed somewhere else, so your text was not saved. It is still here: copy it before reopening the note.'
      : 'Your text is not saved yet, so this note stays open. Nothing is lost: retry, or copy the text.';
    alert.appendChild(message);

    const retryBtn = document.createElement('button');
    retryBtn.type = 'button';
    retryBtn.className = 'btn btn-outline notes-save-retry-btn';
    retryBtn.textContent = 'Retry';
    retryBtn.onclick = () => this.controller.retrySave();
    alert.appendChild(retryBtn);

    const copyBtn = document.createElement('button');
    copyBtn.type = 'button';
    copyBtn.className = 'btn btn-outline notes-save-copy-btn';
    copyBtn.textContent = 'Copy text';
    copyBtn.onclick = async () => {
      const copied = await this.controller.copyDraft();
      copyBtn.textContent = copied ? 'Copied' : 'Copy failed';
    };
    alert.appendChild(copyBtn);
    slot.appendChild(alert);
  }

  getStatusText(status) {
    switch (status) {
      case 'dirty':
        return 'Waiting to save';
      case 'saving':
        return 'Saving…';
      case 'error':
        return 'Save error';
      case 'stale':
        return 'Conflict';
      case 'clean':
      default:
        return 'Saved locally';
    }
  }

  updateDocumentExternally(doc, bodyChanged) {
    const textarea = this.root.querySelector('textarea.notes-textarea');
    const titleInput = this.root.querySelector('.notes-title-input');
    const breadcrumb = this.root.querySelector('.notes-breadcrumb');

    if (titleInput && doc.entry?.display_name) {
      titleInput.value = doc.entry.display_name;
    }
    if (breadcrumb && doc.entry) {
      breadcrumb.textContent = `Notes / ${doc.entry.relative_path || doc.entry.display_name}`;
    }
    if (bodyChanged && textarea) {
      const start = textarea.selectionStart;
      const end = textarea.selectionEnd;
      textarea.value = doc.body || '';
      textarea.textContent = doc.body || '';
      textarea.setSelectionRange(Math.min(start, textarea.value.length), Math.min(end, textarea.value.length));
      if (this.editorMode === 'preview') {
        const previewArea = this.root.querySelector('.notes-preview');
        if (previewArea) {
          previewArea.innerHTML = '';
          previewArea.appendChild(renderMarkdown(doc.body || ''));
        }
      }
    }
  }

  updateTitleOnly(newTitle) {
    const titleInput = this.root.querySelector('.notes-title-input');
    if (titleInput) {
      titleInput.value = newTitle;
    }
  }

  openItemMenu(event, item) {
    this.closeMenu();
    const menu = document.createElement('div');
    menu.className = 'notes-menu-dropdown';

    const isRoot = Boolean(item.is_root);
    const isFolder = item.kind === 'folder';

    if (isRoot) {
      const openFolderBtn = document.createElement('button');
      openFolderBtn.className = 'notes-menu-item';
      openFolderBtn.textContent = 'Open folder';
      openFolderBtn.onclick = () => {
        this.closeMenu();
        this.controller.openInFileManager(item.id);
      };
      menu.appendChild(openFolderBtn);

      const backupBtn = document.createElement('button');
      backupBtn.className = 'notes-menu-item';
      backupBtn.textContent = 'Backup';
      backupBtn.onclick = () => {
        this.closeMenu();
        this.controller.backupNow();
      };
      menu.appendChild(backupBtn);
    } else {
      const renameBtn = document.createElement('button');
      renameBtn.className = 'notes-menu-item';
      renameBtn.textContent = 'Rename';
      renameBtn.onclick = () => {
        this.closeMenu();
        this.controller.openRenameDialog(item);
      };
      menu.appendChild(renameBtn);

      const moveBtn = document.createElement('button');
      moveBtn.className = 'notes-menu-item';
      moveBtn.textContent = 'Move to…';
      moveBtn.onclick = () => {
        this.closeMenu();
        this.controller.openMoveDialog(item);
      };
      menu.appendChild(moveBtn);

      const deleteBtn = document.createElement('button');
      deleteBtn.className = 'notes-menu-item danger';
      deleteBtn.textContent = 'Delete';
      deleteBtn.onclick = () => {
        this.closeMenu();
        this.controller.openDeleteDialog(item);
      };
      menu.appendChild(deleteBtn);
    }

    const rect = event.target.getBoundingClientRect();
    menu.style.top = `${rect.bottom + window.scrollY + 4}px`;
    menu.style.left = `${Math.min(rect.left + window.scrollX, window.innerWidth - 180)}px`;

    document.body.appendChild(menu);
    this.activeMenu = menu;
  }

  renderDialog(dialog) {
    const backdrop = document.createElement('div');
    backdrop.className = 'notes-dialog-backdrop';

    const box = document.createElement('div');
    box.className = 'notes-dialog';
    box.setAttribute('role', 'dialog');
    box.setAttribute('aria-modal', 'true');

    const title = document.createElement('h3');
    title.className = 'notes-dialog-title';

    const body = document.createElement('div');
    body.className = 'notes-dialog-body';

    const actions = document.createElement('div');
    actions.className = 'notes-dialog-actions';

    const cancelBtn = document.createElement('button');
    cancelBtn.className = 'btn btn-outline';
    cancelBtn.textContent = 'Cancel';
    cancelBtn.onclick = () => this.controller.closeDialog();

    if (dialog.type === 'create_folder') {
      title.textContent = 'New Folder';
      const input = document.createElement('input');
      input.type = 'text';
      input.className = 'notes-dialog-input';
      input.value = dialog.name ?? 'New folder';
      setTimeout(() => { input.focus(); input.select(); }, 50);

      const createBtn = document.createElement('button');
      createBtn.className = 'btn btn-primary';
      createBtn.dataset.testid = 'notes-dialog-confirm';
      createBtn.textContent = 'Create';
      createBtn.onclick = () => {
        const val = input.value.trim();
        if (val) this.controller.createFolder(dialog.parentId, val);
      };
      input.onkeydown = (e) => {
        if (e.key === 'Enter') createBtn.click();
      };

      body.appendChild(input);
      if (dialog.errorMessage) {
        const error = document.createElement('p');
        error.setAttribute('role', 'alert');
        error.textContent = dialog.errorMessage;
        body.appendChild(error);
      }
      actions.appendChild(cancelBtn);
      actions.appendChild(createBtn);
    } else if (dialog.type === 'rename') {
      title.textContent = `Rename ${dialog.item.display_name}`;
      const input = document.createElement('input');
      input.type = 'text';
      input.className = 'notes-dialog-input';
      input.value = dialog.item.display_name;
      setTimeout(() => { input.focus(); input.select(); }, 50);

      const saveBtn = document.createElement('button');
      saveBtn.className = 'btn btn-primary';
      saveBtn.dataset.testid = 'notes-dialog-confirm';
      saveBtn.textContent = 'Rename';
      saveBtn.onclick = () => {
        const val = input.value.trim();
        if (val) this.controller.renameEntry(dialog.item.id, val);
      };
      input.onkeydown = (e) => {
        if (e.key === 'Enter') saveBtn.click();
      };

      body.appendChild(input);
      actions.appendChild(cancelBtn);
      actions.appendChild(saveBtn);
    } else if (dialog.type === 'delete') {
      title.textContent = `Delete ${dialog.item.display_name}`;
      body.textContent = dialog.item.kind === 'folder'
        ? `Are you sure you want to delete the folder "${dialog.item.display_name}" and its contents? This will sync to other devices.`
        : `Are you sure you want to delete "${dialog.item.display_name}"? This will sync to other devices.`;

      const deleteBtn = document.createElement('button');
      deleteBtn.className = 'btn btn-primary';
      deleteBtn.dataset.testid = 'notes-dialog-confirm';
      deleteBtn.style.backgroundColor = 'var(--error)';
      deleteBtn.style.borderColor = 'var(--error)';
      deleteBtn.textContent = 'Delete';
      deleteBtn.onclick = () => {
        this.controller.deleteEntry(dialog.item.id);
      };

      actions.appendChild(cancelBtn);
      actions.appendChild(deleteBtn);
    } else if (dialog.type === 'move') {
      title.textContent = `Move ${dialog.item.display_name}`;
      body.textContent = 'Select destination folder:';

      const select = document.createElement('select');
      select.className = 'notes-dialog-input';
      const rootId = this.controller.rootId();

      const rootOpt = document.createElement('option');
      rootOpt.value = rootId;
      rootOpt.textContent = 'Notes (root)';
      select.appendChild(rootOpt);

      for (const entry of this.controller.state.treeEntries) {
        if (entry.kind === 'folder' && entry.id !== dialog.item.id && entry.parent_id !== dialog.item.id) {
          const opt = document.createElement('option');
          opt.value = entry.id;
          opt.textContent = entry.display_name;
          select.appendChild(opt);
        }
      }

      select.value = dialog.destinationParentId || rootId;
      select.onchange = () => { dialog.destinationParentId = select.value; };

      body.appendChild(select);

      const moveBtn = document.createElement('button');
      moveBtn.className = 'btn btn-primary';
      moveBtn.dataset.testid = 'notes-dialog-confirm';
      moveBtn.textContent = 'Move';
      moveBtn.onclick = () => {
        this.controller.moveEntry(dialog.item.id, select.value);
      };

      actions.appendChild(cancelBtn);
      actions.appendChild(moveBtn);
    }

    box.appendChild(title);
    box.appendChild(body);
    box.appendChild(actions);
    backdrop.appendChild(box);
    return backdrop;
  }

  renderUndoToast(toast) {
    const el = document.createElement('div');
    el.className = 'notes-undo-toast';
    el.setAttribute('role', 'status');

    const msg = document.createElement('span');
    msg.textContent = toast.message;
    el.appendChild(msg);

    const undoBtn = document.createElement('button');
    undoBtn.className = 'btn btn-primary';
    undoBtn.style.padding = '4px 10px';
    undoBtn.style.fontSize = '12px';
    undoBtn.textContent = 'Undo';
    undoBtn.onclick = () => {
      this.controller.restoreUndo(toast.undoId);
    };
    el.appendChild(undoBtn);

    return el;
  }

  renderRecentlyDeletedModal(state) {
    const backdrop = document.createElement('div');
    backdrop.className = 'notes-dialog-backdrop';

    const box = document.createElement('div');
    box.className = 'notes-dialog notes-deleted-modal';
    box.setAttribute('role', 'dialog');

    const title = document.createElement('h3');
    title.className = 'notes-dialog-title';
    title.textContent = 'Recently Deleted';
    box.appendChild(title);

    const list = document.createElement('div');
    list.className = 'notes-deleted-list';

    if (!state.deletedEntries || state.deletedEntries.length === 0) {
      list.innerHTML = '<div style="color: var(--text-muted); padding: 12px 0;">No recently deleted items.</div>';
    } else {
      for (const item of state.deletedEntries) {
        const row = document.createElement('div');
        row.className = 'notes-deleted-item';

        const info = document.createElement('div');
        info.innerHTML = `<strong>${item.name}</strong> <span style="font-size: 11px; color: var(--text-muted);">(${new Date(item.deleted_at).toLocaleDateString()})</span>`;
        row.appendChild(info);

        const restoreBtn = document.createElement('button');
        restoreBtn.className = 'btn btn-outline';
        restoreBtn.textContent = 'Restore';
        restoreBtn.onclick = () => {
          this.controller.restoreUndo(item.undo_id);
        };
        row.appendChild(restoreBtn);

        list.appendChild(row);
      }
    }
    box.appendChild(list);

    const actions = document.createElement('div');
    actions.className = 'notes-dialog-actions';
    const closeBtn = document.createElement('button');
    closeBtn.className = 'btn btn-outline';
    closeBtn.textContent = 'Close';
    closeBtn.onclick = () => this.controller.closeRecentlyDeleted();
    actions.appendChild(closeBtn);
    box.appendChild(actions);

    backdrop.appendChild(box);
    return backdrop;
  }

  renderBackupModal(job) {
    const backdrop = document.createElement('div');
    backdrop.className = 'notes-dialog-backdrop';

    const box = document.createElement('div');
    box.className = 'notes-dialog notes-backup-modal';
    box.setAttribute('role', 'dialog');
    box.setAttribute('aria-label', 'Notes Backup');

    const title = document.createElement('h3');
    title.className = 'notes-dialog-title';

    const body = document.createElement('div');
    body.className = 'notes-dialog-body';

    const actions = document.createElement('div');
    actions.className = 'notes-dialog-actions';

    if (job.state === 'starting') {
      title.textContent = 'Preparing Backup…';
      body.textContent = 'Taking snapshot and checking notes integrity…';

      const cancelBtn = document.createElement('button');
      cancelBtn.className = 'btn btn-outline notes-backup-cancel-btn';
      cancelBtn.textContent = 'Cancel';
      cancelBtn.onclick = () => this.controller.cancelBackup();
      actions.appendChild(cancelBtn);
    } else if (job.state === 'running') {
      title.textContent = 'Creating Backup…';

      const statusText = document.createElement('p');
      const total = job.total || '?';
      statusText.textContent = `Compressing and verifying entries (${job.done} / ${total})…`;
      body.appendChild(statusText);

      const progressContainer = document.createElement('div');
      progressContainer.className = 'notes-progress-bar';
      const progressFill = document.createElement('div');
      progressFill.className = 'notes-progress-fill';
      const pct = job.total ? Math.min(100, Math.round((job.done / job.total) * 100)) : 20;
      progressFill.style.width = `${pct}%`;
      progressContainer.appendChild(progressFill);
      body.appendChild(progressContainer);

      const cancelBtn = document.createElement('button');
      cancelBtn.className = 'btn btn-outline notes-backup-cancel-btn';
      cancelBtn.textContent = 'Cancel';
      cancelBtn.onclick = () => this.controller.cancelBackup();
      actions.appendChild(cancelBtn);
    } else if (job.state === 'succeeded') {
      title.textContent = 'Backup Complete';

      const desc = document.createElement('p');
      const res = job.result;
      const sizeStr = res?.bytes ? `${Math.round(res.bytes / 1024)} KB` : '0 KB';
      desc.textContent = `Backup archive created with ${res?.files || 0} files and ${res?.directories || 0} folders (${sizeStr}).`;
      body.appendChild(desc);

      if (res?.archive_path) {
        const pathEl = document.createElement('div');
        pathEl.className = 'notes-backup-path';
        pathEl.textContent = res.archive_path;
        body.appendChild(pathEl);
      }

      if (res?.archive_path) {
        const openBtn = document.createElement('button');
        openBtn.className = 'btn btn-primary notes-backup-open-btn';
        openBtn.textContent = 'Open folder';
        openBtn.onclick = () => {
          this.controller.revealArchive(res.archive_path);
          this.controller.dismissBackupModal();
        };
        actions.appendChild(openBtn);
      }

      const doneBtn = document.createElement('button');
      doneBtn.className = 'btn btn-outline notes-backup-done-btn';
      doneBtn.textContent = 'Done';
      doneBtn.onclick = () => this.controller.dismissBackupModal();
      actions.appendChild(doneBtn);
    } else if (job.state === 'cancelled') {
      title.textContent = 'Backup Cancelled';
      body.textContent = 'The backup operation was cancelled. No files were written.';

      const closeBtn = document.createElement('button');
      closeBtn.className = 'btn btn-outline notes-backup-close-btn';
      closeBtn.textContent = 'Close';
      closeBtn.onclick = () => this.controller.dismissBackupModal();
      actions.appendChild(closeBtn);
    } else {
      title.textContent = 'Backup Failed';
      body.textContent = job.error?.message || 'The backup could not be completed.';

      const closeBtn = document.createElement('button');
      closeBtn.className = 'btn btn-outline notes-backup-close-btn';
      closeBtn.textContent = 'Close';
      closeBtn.onclick = () => this.controller.dismissBackupModal();
      actions.appendChild(closeBtn);
    }

    box.appendChild(title);
    box.appendChild(body);
    box.appendChild(actions);
    backdrop.appendChild(box);
    return backdrop;
  }
}
