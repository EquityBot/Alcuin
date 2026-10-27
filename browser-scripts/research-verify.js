// Scoped adaptation of maintained Deep Research pills and Web Search prompt verification (MIT).
(() => {
  const navigation = researchNavigation();
  const context = searchContext();
  if (!context || !navigation.contextMatches || navigation.workSelected || navigation.modeUnverified) return {selected:false,promptMatches:false};
  const labels = ['deep research', '深度研究'];
  const menus = searchMenuRoots();
  const candidates = Array.from(context.form.querySelectorAll('[data-inline-selection-pill], .__composer-pill, .__composer-pill-composite, button, [role="button"]'));
  const normalizeLabel = text => String(text || '').replace(/\s+/g,' ').trim().toLowerCase();
  const attachment = node => Boolean(node.closest('[data-testid*="attachment"], [data-testid*="upload"], [data-testid^="file-"], [data-file-id], [data-upload-id]'));
  const selected = candidates.some(node => searchVisible(node) && !menus.some(menu => menu.contains(node)) &&
    !attachment(node) && (
      node.matches('[data-inline-selection-pill], .__composer-pill, .__composer-pill-composite') && labels.includes(normalizeLabel(node.textContent)) ||
      normalizeLabel(node.getAttribute('aria-label')) === 'remove deep research'));
  const copy = context.editor.cloneNode(true);
  copy.querySelectorAll('[data-inline-selection-pill], [data-inline-selection-pill-cursor-target]').forEach(node => node.remove());
  const readText = node => node.nodeType === 3 ? node.textContent ?? '' :
    Array.from(node.childNodes).map(readText).join('') + (['P','DIV','BR','LI','PRE'].includes(node.nodeName) ? '\n' : '');
  const normalize = text => String(text ?? '').replace(/[\u200b\ufeff]/g,'').replace(/\s+/g,' ').trim();
  return {selected,promptMatches:normalize(context.editor instanceof HTMLTextAreaElement ? context.editor.value : readText(copy)) === normalize(cfg.prompt)};
})()
