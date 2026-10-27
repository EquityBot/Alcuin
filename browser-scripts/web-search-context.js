// Retain the staged composer and the one tools menu opened from its control.
const searchVisible = node => {
  if (!(node instanceof HTMLElement) || !node.isConnected) return false;
  const rect = node.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return false;
  for (let current = node; current; current = current.parentElement) {
    const style = getComputedStyle(current);
    if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0' || current.getAttribute('aria-hidden') === 'true') return false;
  }
  return true;
};
const searchContext = () => {
  const context = window.__oracleAttachmentPromptGuards?.[cfg.guardId]?.context?.();
  if (!context || !searchVisible(context.editor) || !context.form?.isConnected || context.editor.closest('form') !== context.form) return null;
  return context;
};
const searchMenuRoots = () => Array.from(document.querySelectorAll('main .popover, [data-radix-popper-content-wrapper], [data-floating-ui-portal], [role="menu"], [role="listbox"], [class*="ComposerTopMenuShell"]')).filter(searchVisible);
const searchBinding = () => {
  const binding = window.__oracleWebSearchContexts?.[cfg.plusGuardId];
  const context = searchContext();
  return binding && context && binding.guardId === cfg.guardId && binding.editor === context.editor && binding.form === context.form &&
    searchVisible(binding.plus) && binding.plus.closest('form') === context.form ? binding : null;
};
const searchMenu = binding => {
  const roots = searchMenuRoots();
  if (binding.menu) return roots.includes(binding.menu) ? binding.menu : null;
  const ids = (binding.plus.getAttribute('aria-controls') || '').trim().split(/\s+/).filter(Boolean);
  const controlled = ids.map(id => document.getElementById(id)).filter(node => roots.includes(node));
  const fresh = roots.filter(node => !binding.before.includes(node));
  const candidates = ids.length ? controlled : fresh.filter(node => !fresh.some(other => other !== node && other.contains(node)));
  if (candidates.length !== 1) return null;
  binding.menu = candidates[0];
  return binding.menu;
};
