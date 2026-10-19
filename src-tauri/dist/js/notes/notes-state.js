/**
 * Pure state and layout derivation for S4Drive Notes.
 * No DOM or Tauri dependencies.
 */

export function createInitialState(options = {}) {
  return {
    workspace: options.workspace || null,
    context: options.context || { kind: 'folder', id: null },
    activeNoteId: options.activeNoteId || null,
    activeDocument: options.activeDocument || null,
    treeEntries: options.treeEntries || [],
    listEntries: options.listEntries || [],
    deletedEntries: options.deletedEntries || [],
    expandedIds: new Set(options.expandedIds || []),
    availableWidth: typeof options.availableWidth === 'number' ? options.availableWidth : 1024,
    compactPage: options.compactPage || 'folder',
    drawerOpen: Boolean(options.drawerOpen),
    status: options.status || 'idle',
    errorMessage: options.errorMessage || null,
    errorRetryable: Boolean(options.errorRetryable),
    unavailable: options.unavailable || null, // 'missing' | 'blocked' while the root cannot be used
    requestGeneration: options.requestGeneration || 0,
    searchQuery: options.searchQuery || '',
    searchResults: options.searchResults || null,
    isSearching: Boolean(options.isSearching),
    undoToast: options.undoToast || null,
    historyOpen: Boolean(options.historyOpen),
    historyEntries: options.historyEntries || [],
    recentlyDeletedOpen: Boolean(options.recentlyDeletedOpen),
    dialog: options.dialog || null,
    jobStatus: options.jobStatus || null,
    backupJob: options.backupJob || null,
    editorMode: options.editorMode || 'raw',
  };
}

/**
 * Derives visual panels layout based on available width and navigation context.
 * Returns: { showTree: boolean, showList: boolean, showEditor: boolean, drawer: boolean }
 *
 * Rules (§4.3, §4.4):
 * - >= 960 px (Wide): Tree 200-240, folder list 220-280, editor min 440.
 *   If context.kind === 'note', list is hidden (width 0, excluded from tab order), editor takes remaining width.
 * - 640-959 px (Medium): Tree 180-220 + main panel.
 *   Main panel shows EITHER folder list OR editor (with Back button); no 3rd narrow column.
 * - < 640 px (Compact): Exactly one panel shown.
 *   'Folders' button opens tree drawer; selection closes drawer. Folder -> editor via push/back.
 */
export function deriveLayout(state) {
  const width = typeof state?.availableWidth === 'number' ? state.availableWidth : 1024;
  const isNoteContext = state?.context?.kind === 'note';
  const isCompactPageEditor = state?.compactPage === 'editor' && Boolean(state?.activeNoteId);

  if (width >= 960) {
    return {
      showTree: true,
      showList: !isNoteContext,
      showEditor: true,
      drawer: false,
    };
  } else if (width >= 640) {
    if (isNoteContext || isCompactPageEditor) {
      return {
        showTree: true,
        showList: false,
        showEditor: true,
        drawer: false,
      };
    } else {
      return {
        showTree: true,
        showList: true,
        showEditor: false,
        drawer: false,
      };
    }
  } else {
    const drawer = Boolean(state?.drawerOpen);
    if (isNoteContext || isCompactPageEditor) {
      return {
        showTree: false,
        showList: false,
        showEditor: true,
        drawer,
      };
    } else {
      return {
        showTree: false,
        showList: true,
        showEditor: false,
        drawer,
      };
    }
  }
}

export function selectTreeNote(state, noteId) {
  return {
    ...state,
    context: { kind: 'note', id: noteId },
    activeNoteId: noteId,
    compactPage: 'editor',
    drawerOpen: false,
    requestGeneration: (state.requestGeneration || 0) + 1,
  };
}

export function selectFolder(state, folderId) {
  return {
    ...state,
    context: { kind: 'folder', id: folderId },
    compactPage: 'folder',
    drawerOpen: false,
    requestGeneration: (state.requestGeneration || 0) + 1,
  };
}

export function selectListNote(state, noteId, parentFolderId) {
  return {
    ...state,
    context: { kind: 'folder', id: parentFolderId },
    activeNoteId: noteId,
    compactPage: 'editor',
    drawerOpen: false,
    requestGeneration: (state.requestGeneration || 0) + 1,
  };
}

export function toggleFolderExpanded(state, folderId) {
  const next = new Set(state.expandedIds || []);
  if (next.has(folderId)) {
    next.delete(folderId);
  } else {
    next.add(folderId);
  }
  return {
    ...state,
    expandedIds: next,
  };
}

export function setAvailableWidth(state, width) {
  return {
    ...state,
    availableWidth: width,
  };
}

export function setDrawerOpen(state, drawerOpen) {
  return {
    ...state,
    drawerOpen: Boolean(drawerOpen),
  };
}

export function setCompactPage(state, compactPage) {
  return {
    ...state,
    compactPage,
  };
}

export function setSearchQuery(state, query) {
  return {
    ...state,
    searchQuery: query,
    isSearching: Boolean(query && query.trim().length > 0),
  };
}

export function setDialog(state, dialog) {
  return {
    ...state,
    dialog,
  };
}

export function setUndoToast(state, undoToast) {
  return {
    ...state,
    undoToast,
  };
}

export function setRecentlyDeletedOpen(state, open) {
  return {
    ...state,
    recentlyDeletedOpen: Boolean(open),
  };
}

export function setEditorMode(state, mode) {
  return {
    ...state,
    editorMode: mode === 'preview' ? 'preview' : 'raw',
  };
}

export function setBackupJob(state, backupJob) {
  return {
    ...state,
    backupJob,
  };
}
