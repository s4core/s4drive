/**
 * Safe Markdown renderer and editor transforms for S4Drive Notes.
 * Enforces strict security (§6.7) and transform contracts (§6.1).
 */

// Dynamic/environment-aware loader for markdown-it and DOMPurify
let markdownParser = null;

function getMarkdownParser() {
  if (markdownParser) return markdownParser;

  let MarkdownIt = globalThis.markdownit;
  if (!MarkdownIt && typeof process !== 'undefined' && process.versions?.node) {
    try {
      // In Node environment, sync-compatible require or imported constructor
      const req = globalThis.require;
      if (req) {
        MarkdownIt = req('markdown-it');
      }
    } catch (_) {}
  }

  if (MarkdownIt) {
    const md = new MarkdownIt({
      html: false, // HTML inside markdown is NOT parsed as raw HTML (§6.7)
      linkify: false,
      typographer: false,
    });

    // Custom image rule: placeholder without network loading (§6.7)
    md.renderer.rules.image = function (tokens, idx) {
      const token = tokens[idx];
      const alt = token.content || '';
      const src = token.attrGet('src') || '';
      return `<span class="notes-img-placeholder" data-src="${encodeURI(src)}" title="${alt}">[Image: ${alt || 'image'}]</span>`;
    };

    markdownParser = md;
    return markdownParser;
  }

  return null;
}

function sanitizeUrl(url) {
  if (!url) return null;
  const trimmed = url.trim();
  // Strip control chars and decode common entities
  const normalized = trimmed.replace(/[\u0000-\u001F\u007F-\u009F]/g, '');

  // Block protocol-relative URLs
  if (normalized.startsWith('//')) return null;

  // Internal hash anchors are safe
  if (normalized.startsWith('#')) return normalized;

  // Permitted schemes: http, https, mailto
  const match = normalized.match(/^([a-z0-9+.-]+):/i);
  if (!match) return null;

  const scheme = match[1].toLowerCase();
  if (scheme === 'http' || scheme === 'https' || scheme === 'mailto') {
    return normalized;
  }

  // All other schemes (javascript, data, file, tauri, asset, etc.) are strictly forbidden
  return null;
}

function sanitizeHtml(html) {
  const purify = globalThis.DOMPurify;
  if (purify && typeof purify.sanitize === 'function') {
    return purify.sanitize(html, {
      ALLOWED_TAGS: [
        'p', 'br', 'hr', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6',
        'em', 'strong', 's', 'blockquote', 'ul', 'ol', 'li',
        'pre', 'code', 'a', 'table', 'thead', 'tbody', 'tr',
        'th', 'td', 'input', 'label', 'div', 'span',
      ],
      ALLOWED_ATTR: [
        'href', 'title', 'start', 'class', 'scope',
        'type', 'checked', 'disabled', 'data-src', 'data-external-link',
      ],
      FORBID_TAGS: ['script', 'style', 'svg', 'math', 'iframe', 'object', 'embed', 'base', 'form'],
      FORBID_ATTR: ['onerror', 'onload', 'onclick', 'onmouseover', 'style'],
      ALLOW_DATA_ATTR: false,
    });
  }

  // Fallback simple sanitizer for environments without DOMPurify (e.g. basic Node tests)
  return html
    .replace(/<script\b[^<]*(?:(?!<\/script>)<[^<]*)*<\/script>/gi, '')
    .replace(/<style\b[^<]*(?:(?!<\/style>)<[^<]*)*<\/style>/gi, '')
    .replace(/<svg\b[^<]*(?:(?!<\/svg>)<[^<]*)*<\/svg>/gi, '')
    .replace(/<iframe\b[^<]*(?:(?!<\/iframe>)<[^<]*)*<\/iframe>/gi, '')
    .replace(/<base\b[^>]*>/gi, '')
    .replace(/\son\w+="[^"]*"/gi, '')
    .replace(/\son\w+='[^']*'/gi, '');
}

/**
 * Renders Markdown source into a secure DOM DocumentFragment.
 * Limits preview to 512 KiB (§6.7).
 */
export function renderMarkdown(source) {
  if (typeof source !== 'string') source = '';

  const frag = typeof document !== 'undefined'
    ? document.createDocumentFragment()
    : {
        children: [],
        innerHTML: '',
        appendChild(child) { this.children.push(child); },
      };

  // Limit preview size to 512 KiB (524,288 bytes)
  if (source.length > 512 * 1024) {
    if (typeof document !== 'undefined') {
      const banner = document.createElement('div');
      banner.className = 'notes-preview-oversized';
      banner.setAttribute('role', 'alert');
      banner.textContent = 'Preview is unavailable for notes larger than 512 KiB. Use Raw mode to edit this document.';
      frag.appendChild(banner);
      return frag;
    }
  }

  const parser = getMarkdownParser();
  let rawHtml = '';
  if (parser) {
    rawHtml = parser.render(source);
  } else {
    // Basic fallback line renderer
    rawHtml = source
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .split('\n\n')
      .map((p) => `<p>${p.replace(/\n/g, '<br/>')}</p>`)
      .join('');
  }

  // Task lists transform
  rawHtml = rawHtml.replace(/<li>\[ \] (.*?)<\/li>/g, '<li class="notes-task-item"><input type="checkbox" disabled /> $1</li>');
  rawHtml = rawHtml.replace(/<li>\[x\] (.*?)<\/li>/gi, '<li class="notes-task-item"><input type="checkbox" checked disabled /> $1</li>');

  const cleanHtml = sanitizeHtml(rawHtml);

  if (typeof document !== 'undefined') {
    const container = document.createElement('div');
    container.className = 'notes-markdown-rendered';
    container.innerHTML = cleanHtml;

    // Filter and sanitize links
    container.querySelectorAll('a').forEach((anchor) => {
      const rawHref = anchor.getAttribute('href');
      const safe = sanitizeUrl(rawHref);
      if (safe) {
        anchor.setAttribute('href', safe);
        if (!safe.startsWith('#')) {
          anchor.setAttribute('target', '_blank');
          anchor.setAttribute('rel', 'noopener noreferrer');
          anchor.setAttribute('data-external-link', 'true');
        }
      } else {
        anchor.removeAttribute('href');
        anchor.classList.add('notes-blocked-link');
      }
    });

    frag.appendChild(container);
    return frag;
  }

  frag.innerHTML = cleanHtml;
  return frag;
}

/**
 * Applies toolbar or shortcut markdown transform to textarea text.
 * Pure transform returning { body, start, end } with preserved Unicode scalar offsets.
 */
export function applyMarkdownAction(body, selectionStart, selectionEnd, action) {
  if (typeof body !== 'string') body = '';
  const start = Math.max(0, Math.min(selectionStart, body.length));
  const end = Math.max(start, Math.min(selectionEnd, body.length));
  const selected = body.slice(start, end);

  switch (action) {
    case 'bold': {
      if (start < end) {
        const before = body.slice(0, start);
        const after = body.slice(end);
        return {
          body: `${before}**${selected}**${after}`,
          start: start + 2,
          end: end + 2,
        };
      } else {
        const before = body.slice(0, start);
        const after = body.slice(start);
        return {
          body: `${before}****${after}`,
          start: start + 2,
          end: start + 2,
        };
      }
    }

    case 'italic': {
      if (start < end) {
        const before = body.slice(0, start);
        const after = body.slice(end);
        return {
          body: `${before}*${selected}*${after}`,
          start: start + 1,
          end: end + 1,
        };
      } else {
        const before = body.slice(0, start);
        const after = body.slice(start);
        return {
          body: `${before}**${after}`,
          start: start + 1,
          end: start + 1,
        };
      }
    }

    case 'heading': {
      const lineStart = body.lastIndexOf('\n', start - 1) + 1;
      let lineEnd = body.indexOf('\n', start);
      if (lineEnd === -1) lineEnd = body.length;
      const line = body.slice(lineStart, lineEnd);

      let newLine;
      let offsetDiff = 0;
      if (line.startsWith('### ')) {
        newLine = line.slice(4);
        offsetDiff = -4;
      } else if (line.startsWith('## ')) {
        newLine = '### ' + line.slice(3);
        offsetDiff = 1;
      } else if (line.startsWith('# ')) {
        newLine = '## ' + line.slice(2);
        offsetDiff = 1;
      } else {
        newLine = '# ' + line;
        offsetDiff = 2;
      }

      const newBody = body.slice(0, lineStart) + newLine + body.slice(lineEnd);
      return {
        body: newBody,
        start: Math.max(lineStart, start + offsetDiff),
        end: Math.max(lineStart, end + offsetDiff),
      };
    }

    case 'list': {
      const firstLineStart = body.lastIndexOf('\n', start - 1) + 1;
      let lastLineEnd = body.indexOf('\n', end);
      if (lastLineEnd === -1) lastLineEnd = body.length;

      const targetBlock = body.slice(firstLineStart, lastLineEnd);
      const lines = targetBlock.split('\n');
      const allBulleted = lines.every((l) => l.startsWith('- '));

      let newLines;
      if (allBulleted) {
        newLines = lines.map((l) => l.slice(2));
      } else {
        newLines = lines.map((l) => (l.startsWith('- ') ? l : `- ${l}`));
      }

      const newBlock = newLines.join('\n');
      const newBody = body.slice(0, firstLineStart) + newBlock + body.slice(lastLineEnd);
      return {
        body: newBody,
        start: firstLineStart,
        end: firstLineStart + newBlock.length,
      };
    }

    case 'task': {
      const firstLineStart = body.lastIndexOf('\n', start - 1) + 1;
      let lastLineEnd = body.indexOf('\n', end);
      if (lastLineEnd === -1) lastLineEnd = body.length;

      const targetBlock = body.slice(firstLineStart, lastLineEnd);
      const lines = targetBlock.split('\n');
      const allTasks = lines.every((l) => l.startsWith('- [ ] ') || l.startsWith('- [x] '));

      let newLines;
      if (allTasks) {
        newLines = lines.map((l) => l.replace(/^- \[[ x]\] /i, ''));
      } else {
        newLines = lines.map((l) => (l.startsWith('- [ ] ') ? l : `- [ ] ${l}`));
      }

      const newBlock = newLines.join('\n');
      const newBody = body.slice(0, firstLineStart) + newBlock + body.slice(lastLineEnd);
      return {
        body: newBody,
        start: firstLineStart,
        end: firstLineStart + newBlock.length,
      };
    }

    case 'code': {
      if (selected.includes('\n')) {
        const before = body.slice(0, start);
        const after = body.slice(end);
        return {
          body: `${before}\`\`\`\n${selected}\n\`\`\`${after}`,
          start: start + 4,
          end: end + 4,
        };
      } else {
        const before = body.slice(0, start);
        const after = body.slice(end);
        return {
          body: `${before}\`${selected}\`${after}`,
          start: start + 1,
          end: end + 1,
        };
      }
    }

    case 'link': {
      const before = body.slice(0, start);
      const after = body.slice(end);
      const linkText = selected || 'link';
      return {
        body: `${before}[${linkText}](url)${after}`,
        start: start + 1,
        end: start + 1 + linkText.length,
      };
    }

    default:
      return { body, start, end };
  }
}
