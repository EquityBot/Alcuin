// Derived from maintained Web Search and composer controls (MIT).
(() => {
    const visible = node => node instanceof HTMLElement && node.getBoundingClientRect().width > 0 && node.getBoundingClientRect().height > 0;
    const context = searchContext();
    if (!context) return {selected:false,promptMatches:false};
    const editor = context.editor;
    // Older layout: an inline pill inside the editor. September 2026: a removable
    // "Web search" chip beside the editor, inside the same composer form. October 2026:
    // the inline pill again, without its former data-id attribute.
    const inlinePill = editor.querySelector('[data-inline-selection-pill][data-system-hint-type="search"]');
    const composer = context.form;
    const chip = inlinePill || Array.from(composer.querySelectorAll('form button[aria-label="Remove Web search"]')).find(visible) || null;
    const copy = editor.cloneNode(true);
    copy.querySelectorAll('[data-inline-selection-pill], [data-inline-selection-pill-cursor-target]').forEach(node => node.remove());
    const readText = node => {
      if (node.nodeType === 3) return node.textContent ?? '';
      const text = Array.from(node.childNodes).map(readText).join('');
      return text + (['P', 'DIV', 'BR', 'LI', 'PRE'].includes(node.nodeName) ? '\n' : '');
    };
    const normalize = text => String(text ?? '').replace(/[\u200b\ufeff]/g, '').replace(/\s+/g, ' ').trim();
    return { selected: Boolean(chip && searchVisible(chip)), promptMatches: normalize(editor instanceof HTMLTextAreaElement ? editor.value : readText(copy)) === normalize(cfg.prompt) };
  })()
