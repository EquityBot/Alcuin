// Initial entry only. Recovery never toggles privacy or reopens a saved chat.
(cfg, privateChat) => {
  if (window !== window.top || location.origin !== 'https://grok.com') throw Error('Initial Grok origin changed');
  if (!['inspect','activate'].includes(cfg.action)) throw Error('Unsupported Grok entry action');
  if (sessionStorage.getItem('oracle:target-claim') !== null || window.__oracleGrokWitness)
    throw Error('Initial Grok page is already claimed');
  const visible = el => el instanceof HTMLElement && el.isConnected && el.getClientRects().length > 0 &&
    el.getAttribute('aria-hidden') !== 'true' && getComputedStyle(el).visibility !== 'hidden';
  const outsideMessages = el => !el.closest('[id^="response-"], [data-message-author-role], [data-testid="user-message"], [data-testid="assistant-message"], [data-role="user"], [data-role="assistant"]');
  const all = selector => Array.from(document.querySelectorAll(selector)).filter(visible).filter(outsideMessages);
  const editors = all('textarea, [contenteditable="true"][role="textbox"], [contenteditable="true"][role="combobox"]');
  if (editors.length > 1) throw Error('Initial Grok composer is ambiguous');
  if (!editors.length) return {state:'pending'};
  const editor = editors[0], composer = editor.closest('.query-bar, form');
  if (!composer || editor.hasAttribute('disabled') || editor.hasAttribute('readonly') || editor.getAttribute('aria-disabled') === 'true')
    throw Error('Initial Grok composer is unavailable');
  if ((editor instanceof HTMLTextAreaElement ? editor.value : editor.innerText) !== '' ||
      Array.from(document.querySelectorAll('[id^="response-"], [data-testid="user-message"], [data-testid="assistant-message"], [data-message-author-role], [data-role="user"], [data-role="assistant"]')).some(visible) ||
      composer.querySelector('button[aria-label="Open attachment"], [data-testid="attachment"], [role="progressbar"], [aria-busy="true"]'))
    throw Error('Initial Grok page contains an existing request or attachment');
  if (privateChat()) return {state:'private'};
  const switches = all('a[aria-label="Switch to Private Chat"]');
  if (switches.length !== 1 || all('a[aria-label="Switch to Default Chat"], .query-bar.bg-surface-incognito').length ||
      switches[0].getAttribute('aria-disabled') === 'true' || switches[0].hasAttribute('disabled'))
    throw Error('Initial Grok Private control is unavailable or ambiguous');
  if (cfg.action === 'activate') {
    switches[0].click();
    return {state:'activation-issued'};
  }
  return {state:'ordinary'};
}
