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
    const controls = Array.from(document.querySelectorAll('[role="tab"],button,a,[aria-selected]'));
    const sourceTab = controls.find((node) => {
      if (!visible(node)) return false;
      const label = normalize(node.textContent || node.getAttribute('aria-label') || node.getAttribute('title'));
      return label === 'sources' || label === 'źródła' || label === 'zrodla';
    });
    if (!(sourceTab instanceof HTMLElement)) {
      return { ok: false, reason: 'Project Sources tab control not found.' };
    }
    const selected =
      sourceTab.getAttribute('aria-selected') === 'true' ||
      sourceTab.getAttribute('data-state') === 'active' ||
      /\bactive\b/i.test(sourceTab.getAttribute('class') || '');
    if (selected) return { ok: true, alreadyOpen: true };
    sourceTab.scrollIntoView({ block: 'center', inline: 'center' });
    if(cfg.click){sourceTab.click();return {ok:true,clicked:true};} const rect = sourceTab.getBoundingClientRect();
    return { ok: true, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
  })()
