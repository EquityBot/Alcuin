// Maintained Deep Research labels and delivery guard, bound to the sealed composer (MIT).
(() => {
  const binding = searchBinding();
  const menu = binding && searchMenu(binding);
  if (!menu) return {status:'missing',available:[]};
  const label = node => String(node.textContent || node.getAttribute('aria-label') || '').replace(/\s+/g,' ').trim().toLowerCase();
  const matchesLabel = text => text.length <= 180 && (
    ['deep research','深度研究','get a detailed report','获取详细报告'].some(value => text === value || text.startsWith(value + ' ')) ||
    ['deepresearch','深度研究'].some(value => text.replace(/\s+/g,'').startsWith(value)) &&
    ['getadetailedreport','获取详细报告'].some(value => text.replace(/\s+/g,'').includes(value)));
  const selector = '[data-radix-collection-item], [role="menuitem"], [role="menuitemradio"], [role="option"], [cmdk-item], .__menu-item, [class*="menu-item"], button';
  const matches = () => Array.from(new Set(Array.from(menu.querySelectorAll(selector)).filter(node => searchVisible(node) && matchesLabel(label(node)))
    .map(node => node.closest('[data-radix-collection-item], [role="menuitem"], [role="menuitemradio"], [role="option"], [cmdk-item], button') || node)))
    .filter(node => !node.hasAttribute('disabled') && node.getAttribute('aria-disabled') !== 'true' && node.getAttribute('data-disabled') !== 'true');
  // Reference collectAvailableItems: visible menu labels, for the missing-option message.
  const available = () => {
    const seen = new Set();
    return Array.from(menu.querySelectorAll(selector)).filter(searchVisible)
      .map(node => String(node.textContent || '').replace(/\s+/g,' ').trim())
      .filter(text => text && text.length <= 180 && !seen.has(text.toLowerCase()) && seen.add(text.toLowerCase()));
  };
  const found = matches();
  if (found.length !== 1) return found.length ? {status:'ambiguous'} : {status:'missing',available:available()};
  const match = found[0], startUrl = location.href, selectedText = label(match);
  match.scrollIntoView({block:'center',inline:'center'});
  const rect = match.getBoundingClientRect(), x = rect.left + rect.width / 2, y = rect.top + rect.height / 2;
  const guards = window.__alcuinResearchSelections ??= new Map();
  if (guards.has(cfg.researchGuardId)) throw Error('Research selection guard already exists');
  const guard = {blocked:false,sawPress:false,sawClick:false};
  const valid = () => {
    try {
      assertOracleSubmission();
      const navigation = researchNavigation(), current = searchBinding(), found = matches();
      return navigation.contextMatches && !navigation.workSelected && !navigation.modeUnverified &&
        location.href === startUrl && current === binding && searchMenu(current) === menu &&
        match.isConnected && menu.contains(match) && searchVisible(match) && label(match) === selectedText &&
        found.length === 1 && found[0] === match && getComputedStyle(match).pointerEvents !== 'none' &&
        match.contains(document.elementFromPoint(x,y));
    } catch { return false; }
  };
  if (!valid()) return {status:'context-changed'};
  const block = event => {guard.blocked=true;event.preventDefault();event.stopImmediatePropagation();};
  const pointer = event => {
    if (!event.isTrusted || event.button !== 0) return;
    if (event.type === 'pointerdown' || event.type === 'mousedown') guard.sawPress=true;
    if (guard.blocked || !valid() || !(event.target instanceof Node) || !match.contains(event.target)) block(event);
  };
  const click = event => {
    if (!event.isTrusted || !guard.sawPress) return;
    if (guard.blocked || !valid() || !(event.target instanceof Node) || !match.contains(event.target)) block(event);
    else guard.sawClick=true;
  };
  const pointerEvents=['pointerdown','mousedown','pointerup','mouseup'];
  pointerEvents.forEach(type => window.addEventListener(type,pointer,true));
  window.addEventListener('click',click,true);
  guard.cleanup = () => {pointerEvents.forEach(type => window.removeEventListener(type,pointer,true));window.removeEventListener('click',click,true);guards.delete(cfg.researchGuardId);};
  guards.set(cfg.researchGuardId,guard);
  return {status:'ready',x,y};
})()
