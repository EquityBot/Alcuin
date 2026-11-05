// Rust supplies `observed`, this run's owned read, and readTurns. This script finds the page's own
// control that retries the failed turn and never clicks it: Rust clicks the point it returns.
const failed = observed.uiError === 'temporary_unavailable' || observed.unloaded === true ||
  /^this response couldn['’]t load[.!]?(?:\s+retry)?$/i.test(String(observed.text || '').trim());
// With cfg.before, the message the failed turn held: the turn has started over only on positive
// evidence, since a page that merely dropped its Retry can still show the old partial text.
if (!failed) return { ok:false, failed:false, started:observed.pending === true || observed.stopExists === true ||
  observed.strongThinkingActive === true || (observed.lastAssistantMessageId ?? null) !== (cfg.before ?? null) };
if (observed.stopExists !== false || observed.strongThinkingActive !== false) return { ok:false, failed:true };
const visible = node => {
  const rect = node.getBoundingClientRect(), style = window.getComputedStyle(node);
  return rect.width > 0 && rect.height > 0 && style.display !== 'none' && style.visibility !== 'hidden';
};
const label = node => (node.innerText || node.getAttribute('aria-label') || '').trim();
// The last turn's action row can sit under the composer, so the point must hit the control itself.
const point = (node, fields) => {
  node.scrollIntoView({ block:'center' });
  const rect = node.getBoundingClientRect();
  const x = rect.x + rect.width / 2, y = rect.y + rect.height / 2;
  return node.contains(document.elementFromPoint(x, y)) ? { ok:true, failed:true, x, y, ...fields } : { ok:false, failed:true };
};
if (cfg.menu === true) {
  const item = Array.from(document.querySelectorAll('[role="menu"] [role="menuitem"]'))
    .find(node => visible(node) && /^try again\b/i.test(label(node)));
  return item ? point(item, { label:label(item) }) : { ok:false, failed:true };
}
// A turn can hold the user's message too, so only controls from the assistant's message on qualify.
const turn = readTurns().at(-1);
const assistant = turn && lastAssistant(turn);
const controls = Array.from(assistant ? turn.querySelectorAll('button, [role="button"]') : [])
  .filter(node => (assistant.compareDocumentPosition(node) & 4) && visible(node));
const fields = { baseline:observed.lastUserTurnIndex, messageId:observed.lastAssistantMessageId };
const retry = controls.find(node => label(node).toLowerCase() === 'retry');
if (retry) return point(retry, { ...fields, label:label(retry) });
const regenerate = controls.filter(node => node.getAttribute('aria-label') === 'Regenerate response' &&
  node.getAttribute('aria-haspopup') === 'menu').at(-1);
return regenerate ? point(regenerate, { ...fields, menu:true }) : { ok:false, failed:true };
