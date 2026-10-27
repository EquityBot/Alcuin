// Derived from src/browser/actions/projectSources.ts (MIT).
(() => {
    const normalize = (value) => String(value || '').toLowerCase().replace(/\s+/g, ' ').trim();
    return Array.from(document.querySelectorAll('[role="dialog"],dialog')).some((dialog) => {
      if (!(dialog instanceof HTMLElement)) return false;
      const text = normalize(dialog.innerText || dialog.textContent || '');
      return text.includes('add source') || text.includes('add sources') || text.includes('dodaj źródła') || text.includes('dodaj zrodla') || text.includes('przeciągnij źródła') || text.includes('przeciagnij zrodla');
    });
  })()
