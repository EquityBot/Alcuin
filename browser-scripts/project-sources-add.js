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
    if ((() => {     const normalize = (value) => String(value || '').toLowerCase().replace(/\s+/g, ' ').trim();     return Array.from(document.querySelectorAll('[role="dialog"],dialog')).some((dialog) => {       if (!(dialog instanceof HTMLElement)) return false;       const text = normalize(dialog.innerText || dialog.textContent || '');       return text.includes('add source') || text.includes('add sources') || text.includes('dodaj źródła') || text.includes('dodaj zrodla') || text.includes('przeciągnij źródła') || text.includes('przeciagnij zrodla');     });   })()) return { ok: true, alreadyOpen: true };
    const roots = [
      document.querySelector('[role="tabpanel"][id*="source" i]'),
      document.querySelector('[data-testid*="source" i]'),
      document.querySelector('main'),
      document.body,
    ].filter(Boolean);
    const controls = roots.flatMap((root) => Array.from(root.querySelectorAll('button,[role="button"],a,label')));
    const add = controls.find((node) => {
      if (!visible(node)) return false;
      const label = normalize(node.innerText || node.textContent || node.getAttribute('aria-label') || node.getAttribute('title'));
      return label === 'add source' || label === 'add sources' || label === 'dodaj źródła' || label === 'dodaj zrodla';
    });
    if (!(add instanceof HTMLElement)) return { ok: false, reason: 'Project Sources add control not found.' };
    add.scrollIntoView({ block: 'center', inline: 'center' });
    if(cfg.click){add.click();return {ok:true,clicked:true};} const rect = add.getBoundingClientRect();
    return { ok: true, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
  })()
