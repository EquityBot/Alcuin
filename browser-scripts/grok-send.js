// Called once, after the native worker durably reserves dispatch. Revalidate
// synchronously at the physical click, including an unexpired live observer.
(cfg, privateChat) => {
  const visible = el => el instanceof HTMLElement && el.isConnected && el.getClientRects().length > 0 &&
    el.getAttribute('aria-hidden') !== 'true' && getComputedStyle(el).visibility !== 'hidden';
  const editors = Array.from(document.querySelectorAll('textarea, [contenteditable="true"][role="textbox"], [contenteditable="true"][role="combobox"]')).filter(visible);
  if (editors.length !== 1) throw Error('Grok composer is unavailable or ambiguous');
  const editor = editors[0], composer = editor.closest('.query-bar, form');
  if (!composer || editor.hasAttribute('disabled') || editor.getAttribute('aria-disabled') === 'true' ||
      (editor instanceof HTMLTextAreaElement ? editor.value : editor.innerText) !== cfg.prompt)
    throw Error('Complete Grok prompt is not verified');
  // This text witness has no qualified attachment envelope. The native adapter
  // must use its separately qualified attachment witness before extending this.
  if (composer.querySelector('button[aria-label="Open attachment"], [data-testid="attachment"], [role="progressbar"], [aria-busy="true"]'))
    throw Error('Grok text dispatch cannot accept unqualified attachments');
  const buttons = Array.from(composer.querySelectorAll('button[aria-label="Send message"], button[aria-label="Submit"], button[type="submit"]'))
    .filter(el => visible(el) && !el.hasAttribute('disabled') && !el.hasAttribute('data-disabled') && el.getAttribute('aria-disabled') !== 'true');
  if (buttons.length !== 1) throw Error('Grok send control is unavailable or ambiguous');
  if (!privateChat()) throw Error('Grok Private Chat changed before Send');
  const witness = window.__oracleGrokWitness;
  if (!witness || witness.owner.claimId !== cfg.owner.claimId || witness.owner.reservationId !== cfg.owner.reservationId ||
      witness.dispatchId !== cfg.dispatchId) throw Error('Original live Grok witness is required before Send');
  const intent = witness.authorizeSend();
  if (intent.promptSha256 !== cfg.promptSha256 || intent.mode !== cfg.mode)
    throw Error('Grok send intent differs');
  buttons[0].click();
  return true;
}
