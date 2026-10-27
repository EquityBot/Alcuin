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
    const labels = new Set(['upload anyway', 'upload', 'add', 'continue', 'prześlij', 'przeslij', 'dodaj', 'kontynuuj']);
    const upload=window.__oracleProjectSourcesUpload;
                if(upload?.token!==cfg.token || !upload.input.isConnected || !upload.dialog.isConnected
                    || upload.input.closest('[role="dialog"],dialog')!==upload.dialog
                    || JSON.stringify(Array.from(upload.input.files,file=>file.name))!==JSON.stringify(cfg.names))return {ok:false};
                const label=normalize(upload.dialog.innerText||upload.dialog.textContent||'');
                if(!['add source','add sources','dodaj źródła','dodaj zrodla','przeciągnij źródła','przeciagnij zrodla'].some(text=>label.includes(text)))return {ok:false};
                const roots=[upload.dialog];
    const buttons = roots.flatMap((root) => Array.from(root.querySelectorAll('button,[role="button"]')));
    const button = buttons.find((node) => {
      if (!visible(node)) return false;
      const label = normalize(node.innerText || node.textContent || node.getAttribute('aria-label') || node.getAttribute('title'));
      return labels.has(label);
    });
    if (!(button instanceof HTMLElement)) return { ok: false };
    button.scrollIntoView({ block: 'center', inline: 'center' });
    if(cfg.click){button.click();return {ok:true,clicked:true};} const rect = button.getBoundingClientRect();
    return { ok: true, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
  })()
