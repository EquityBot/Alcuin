// Derived from src/browser/actions/projectSources.ts (MIT).
(() => {
    const normalize = (value) => String(value || '').replace(/\s+/g, ' ').trim();
    const lower = (value) => normalize(value).toLowerCase();
    const visible = (node) => {
      if (!(node instanceof HTMLElement)) return false;
      const rect = node.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return false;
      const style = window.getComputedStyle(node);
      return style.display !== 'none' && style.visibility !== 'hidden' && style.opacity !== '0';
    };
    const isChromeLabel = (label) => {
      const text = lower(label);
      return (
        !text ||
        text === 'sources' ||
        text === 'źródła' ||
        text === 'zrodla' ||
        text === 'chats' ||
        text === 'czaty' ||
        text === 'all' ||
        text === 'wszystkie' ||
        text === 'add' ||
        text === 'add sources' ||
        text === 'dodaj' ||
        text === 'dodaj źródła' ||
        text === 'dodaj zrodla' ||
        text === 'source actions' ||
        text === 'actions' ||
        text === 'share' ||
        text === 'udostępnij' ||
        text === 'udostepnij' ||
        text === 'add files and more' ||
        text === 'chat with chatgpt' ||
        text === 'ask anything' ||
        text === 'pro' ||
        text === 'voice' ||
        text === 'start voice' ||
        text === 'start dictation'
      );
    };
    const hasLikelyFileName = (label) => /\.[a-z0-9]{1,12}(?:\s|$)/iu.test(label);
    const cleanSourceName = (label) => {
      const text = normalize(label);
      return text
        .replace(/(?:file|document|spreadsheet|presentation|pdf|plik|dokument)\s*·.*$/iu, '')
        .trim();
    };
    const panel =
      document.querySelector('[role="tabpanel"][id*="source" i]') ||
      document.querySelector('[data-testid*="source" i]') ||
      document.querySelector('main') ||
      document.body;
    if (!(panel instanceof HTMLElement)) {
      return { ok: false, error: 'Project Sources panel not found.' };
    }
    const candidates = Array.from(panel.querySelectorAll('[aria-label], [title], a, button, [role="listitem"], [data-testid*="source" i], [class*="file" i], [class*="source" i]'))
      .filter((node) => node instanceof HTMLElement && visible(node))
      .map((node) => {
        const raw = normalize(node.getAttribute('aria-label') || node.getAttribute('title') || node.textContent || '');
        const rect = node.getBoundingClientRect();
        return {
          node,
          raw,
          name: cleanSourceName(raw),
          hasMetadata: raw !== cleanSourceName(raw),
          top: Math.round(rect.top),
          left: Math.round(rect.left),
        };
      });
    const plainNames = new Set(candidates.filter((candidate) => !candidate.hasMetadata).map((candidate) => candidate.name));
    const rows = [];
    for (const node of candidates) {
      const raw = node.raw;
      const name = node.name;
      if (node.hasMetadata && plainNames.has(name)) continue;
      if (isChromeLabel(name)) continue;
      if (!node.hasMetadata && !hasLikelyFileName(name)) continue;
      if (name.length < 2 || name.length > 200) continue;
      if (/^(pdf|docx?|txt|md|csv|xlsx?|json)$/iu.test(name)) continue;
      if (/^(copy|edit|remove|delete|pobierz|usuń|usun)$/iu.test(name)) continue;
      if (/^(file|document|spreadsheet|presentation|plik|dokument)$/iu.test(name)) continue;
      rows.push(node);
    }
    const sources = [];
    const seen = new Set();
    for (const row of rows) {
      const key = row.name + '|' + row.top + '|' + row.left;
      if (seen.has(key)) continue;
      seen.add(key);
      sources.push({ name: row.name, index: sources.length, status: 'unknown' });
    }
    return { ok: true, sources };
  })()
