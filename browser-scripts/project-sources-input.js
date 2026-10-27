// Derived from src/browser/actions/projectSources.ts (MIT).
(() => {
    const normalize = (value) => String(value || '').toLowerCase().replace(/\s+/g, ' ').trim();
    const dialog = Array.from(document.querySelectorAll('[role="dialog"],dialog')).find((node) => {
      if (!(node instanceof HTMLElement)) return false;
      const text = normalize(node.innerText || node.textContent || '');
      return text.includes('add source') || text.includes('add sources') || text.includes('dodaj źródła') || text.includes('dodaj zrodla') || text.includes('przeciągnij źródła') || text.includes('przeciagnij zrodla');
    });
    if (!dialog) return { ok: false, reason: 'Project Sources Add dialog missing' };
    const roots = [dialog];
    const input = roots
      .flatMap((root) => Array.from(root.querySelectorAll('input[type="file"]')))
      .find((node) => node instanceof HTMLInputElement);
    if (!(input instanceof HTMLInputElement)) return { ok: false, reason: 'file input missing' };
    Array.from(document.querySelectorAll('input[data-oracle-project-sources-input]')).forEach((node) => node.removeAttribute('data-oracle-project-sources-input'));
    input.setAttribute('data-oracle-project-sources-input', '1');
    return { ok: true };
  })()
