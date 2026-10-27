// Derived from src/browser/actions/projectSources.ts (MIT).
(() => {
    const normalize = (value) => String(value || '').toLowerCase().replace(/\s+/g, ' ').trim();
    const visible = (node) => {
      if (!(node instanceof HTMLElement)) return false;
      const rect = node.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return false;
      const style = window.getComputedStyle(node);
      return style.display !== 'none' && style.visibility !== 'hidden' && style.opacity !== '0';
    };
    const bodyText = normalize(document.body?.innerText || document.body?.textContent || '');
    const selectedSourceTab = Array.from(document.querySelectorAll('[role="tab"],button,[aria-selected]')).some((node) => {
      const label = normalize(node.textContent || node.getAttribute?.('aria-label') || '');
      const selected =
        node.getAttribute?.('aria-selected') === 'true' ||
        node.getAttribute?.('data-state') === 'active' ||
        /\bactive\b/i.test(node.getAttribute?.('class') || '');
      return visible(node) && selected && (label === 'sources' || label === 'źródła' || label === 'zrodla');
    });
    const addSurface = Array.from(document.querySelectorAll('button,[role="button"]')).some((node) => {
      const label = normalize(node.textContent || node.getAttribute?.('aria-label') || node.getAttribute?.('title') || '');
      return visible(node) && /^(add source|add sources|dodaj źródła|dodaj zrodla)$/u.test(label);
    });
    if (selectedSourceTab || addSurface) return { ready: true };
    if (bodyText.includes('new chat') || bodyText.includes('nowy czat')) return { ready: false, reason: 'project page loaded but sources tab not visible' };
    return { ready: false, reason: 'sources surface not detected' };
  })()
