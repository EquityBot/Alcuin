// Grok's own visible Private controls are evidence; prompt text and URLs are not.
(() => {
  if (window !== window.top || location.origin !== 'https://grok.com') return false;
  const visible = el => el instanceof HTMLElement && el.isConnected &&
    el.getClientRects().length > 0 && el.getAttribute('aria-hidden') !== 'true' &&
    getComputedStyle(el).visibility !== 'hidden' &&
    !el.closest('[id^="response-"], [data-message-author-role], [data-testid="user-message"], [data-testid="assistant-message"], [data-role="user"], [data-role="assistant"]');
  const all = selector => Array.from(document.querySelectorAll(selector)).filter(visible);
  const editors = all('textarea, [contenteditable="true"][role="textbox"], [contenteditable="true"][role="combobox"]');
  if (editors.length !== 1 || all('a[aria-label="Switch to Private Chat"]').length !== 0) return false;
  return all('a[aria-label="Switch to Default Chat"]').length === 1 ||
    all('.query-bar.bg-surface-incognito').length === 1;
})()
