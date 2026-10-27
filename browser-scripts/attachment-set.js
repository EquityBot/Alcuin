// Native receipt inventory. Names describe uploads; only original objects authorise Send.
(cfg) => {
  const uploading = typeof cfg.uploadReceiptId === 'string';
  const pending = uploading ? window.__oracleFileInputGuards?.[cfg.uploadReceiptId] : null;
  const context = uploading ? null : window.__oracleAttachmentPromptGuards?.[cfg.guardId]?.context?.();
  const composer = uploading ? pending?.input.closest('form') : context?.form;
  const editor = uploading ? composer?.querySelector('#prompt-textarea,[contenteditable="true"][role="textbox"]') : context?.editor;
  if (!composer?.isConnected) return null;
  if (!editor?.isConnected || editor.closest('form') !== composer) return null;
  const visible = node => node instanceof HTMLElement && node.getBoundingClientRect().width > 0 && node.getBoundingClientRect().height > 0;
  const controls = Array.from(composer.querySelectorAll('[aria-label*="Remove" i]')).filter(node => {
    if (!visible(node) || node.closest('textarea,[contenteditable="true"]')) return false;
    const label = (node.getAttribute('aria-label') ?? '').toLowerCase();
    return /^remove (?:file|attachment|image|photo)(?: |$)/.test(label) || node.matches('button') && /^remove \S/.test(label) || Boolean(node.closest('[data-testid*="chip"],[data-testid*="attachment"],[data-testid*="upload"]'));
  });
  const receipts = window.__oracleAttachmentEvidence;
  if (receipts !== undefined && !(receipts instanceof Map)) return null;
  if (uploading && (!pending || pending.owner.claimId !== cfg.owner.claimId ||
      pending.owner.reservationId !== cfg.owner.reservationId || !pending.guard.validate())) return null;
  const ids = uploading
    ? Array.from(receipts?.entries() ?? []).filter(([, receipt]) => receipt.composer === composer).map(([id]) => id)
    : cfg.expectations.map(item => item.rendererReceiptId);
  if (new Set(ids).size !== ids.length || ids.some(id => typeof id !== 'string' || !id)) return null;
  if (uploading && !ids.includes(cfg.uploadReceiptId)) return null;
  const selected = ids.map(id => receipts?.get(id));
  const used = new Set();
  for (const receipt of selected) {
    if (!receipt || receipt.document !== document || receipt.owner.claimId !== cfg.owner.claimId ||
        receipt.owner.reservationId !== cfg.owner.reservationId || receipt.composer !== composer ||
        !receipt.control?.isConnected || !controls.includes(receipt.control) || used.has(receipt.control) ||
        receipt.control.getAttribute('aria-label') !== receipt.label) return null;
    used.add(receipt.control);
    if (!uploading || receipt.input) {
      if (!(receipt.input instanceof HTMLInputElement) || !receipt.input.isConnected ||
          !composer.contains(receipt.input) || !Array.isArray(receipt.inputFiles)) return null;
      const files = Array.from(receipt.input.files);
      if (!(uploading && receipt.input === pending.input) &&
          (files.length !== receipt.inputFiles.length || files.some((file, i) => file !== receipt.inputFiles[i]))) return null;
    }
  }
  if (controls.length !== used.size || controls.some(control => !used.has(control))) return null;
  // A ready tile without its original removal control is also unowned.
  const tiles = composer.querySelectorAll('[data-testid*="chip"],[data-testid*="attachment"],[data-testid*="upload"],[role="group"][aria-label]');
  for (const tile of tiles) {
    if (tile.matches('input,button') || tile.contains(editor) || tile.closest('textarea,[contenteditable="true"]') || !visible(tile)) continue;
    if (!Array.from(used).some(control => tile.contains(control))) return null;
  }
  for (const input of composer.querySelectorAll('input[type="file"]')) {
    if (input.files?.length && !(uploading && pending.input === input) && !selected.some(receipt => receipt.input === input)) return null;
  }
  return composer;
}
