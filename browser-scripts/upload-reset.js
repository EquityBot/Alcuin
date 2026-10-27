// Native upload receipt checks adapt attachment-evidence.js and the maintained clear flow.
(cfg) => {
  /* alcuin-dom:upload-reset */
  const assertOwner = () => {
    const claim = JSON.parse(sessionStorage.getItem('oracle:target-claim'));
    if (window !== window.top || !claim || claim.retiring || claim.id !== cfg.owner.claimId ||
        claim.submission?.id !== cfg.owner.reservationId || claim.submission.state !== 'active' ||
        claim.submission.dispatch !== 'not-started') throw Error('Upload reset ownership changed');
  };
  assertOwner();
  const normalize = value => String(value).toLowerCase().replace(/\s+/g, ' ').trim();
  const visible = node => node instanceof HTMLElement && node.getBoundingClientRect().width > 0 && node.getBoundingClientRect().height > 0;
  const controls = composer => Array.from(composer.querySelectorAll('[aria-label*="Remove" i]')).filter(node => {
    if (!visible(node) || node.closest('textarea,[contenteditable="true"]')) return false;
    const label = (node.getAttribute('aria-label') ?? '').toLowerCase();
    return /^remove (?:file|attachment|image|photo)(?: |$)/.test(label) || node.matches('button') && /^remove \S/.test(label) || Boolean(node.closest('[data-testid*="chip"],[data-testid*="attachment"],[data-testid*="upload"]'));
  });
  const checkSet = (reset, allowRemoved) => {
    assertOwner();
    if (document !== reset.document || location.href !== reset.url || oracleTemporaryChat() !== true)
      throw Error('Upload reset privacy or navigation changed');
    if (!reset.composer.isConnected) throw Error('Original upload composer changed');
    const connected = reset.entries.filter(entry => entry.receipt.control.isConnected);
    if ((!allowRemoved && connected.length !== reset.entries.length) || connected.some(entry => entry.receipt.control.getAttribute('aria-label') !== entry.receipt.label)) throw Error('Original attachment controls changed');
    const expected = new Set(connected.map(entry => entry.receipt.control));
    const current = controls(reset.composer);
    if (current.length !== expected.size || current.some(control => !expected.has(control))) throw Error('Unowned attachment controls prevent reset');
    for (const {input, files} of reset.inputs) {
      if (!input.isConnected || !reset.composer.contains(input)) throw Error('Original file input changed during reset');
      const selected = Array.from(input.files);
      if (!(allowRemoved && selected.length === 0) && (selected.length !== files.length || selected.some((file,index) => file !== files[index]))) throw Error('Original input files changed during reset');
    }
    if (Array.from(reset.composer.querySelectorAll('input[type="file"]')).some(input => !reset.inputs.some(owned => owned.input === input) && input.files?.length)) throw Error('Unowned file input prevents reset');
  };
  const resets = window.__alcuinOwnedUploadResets ??= new Map();
  if (!(resets instanceof Map)) throw Error('Upload reset storage changed');
  let reset = resets.get(cfg.id);
  if (cfg.action === 'begin') {
    if (reset) throw Error('Upload reset cannot repeat removal');
    const receipts = window.__oracleAttachmentEvidence;
    if (!(receipts instanceof Map) || !cfg.uploads.length) throw Error('Upload reset has no original receipts');
    const entries = cfg.uploads.map(file => {
      const receipt = receipts.get(file.rendererReceiptId);
      if (!receipt || receipt.name !== normalize(file.name) || !receipt.control?.isConnected ||
          !(receipt.input instanceof HTMLInputElement) || !receipt.input.isConnected || !receipt.composer?.contains(receipt.input) || !Array.isArray(receipt.inputFiles) ||
          receipt.control.getAttribute('aria-label') !== receipt.label) throw Error('Original upload receipt changed');
      return {id:file.rendererReceiptId, receipt};
    });
    const composer = entries[0].receipt.composer;
    if (!composer?.isConnected || entries.some(entry => entry.receipt.composer !== composer)) throw Error('Original upload composer changed');
    const current = controls(composer);
    const expected = new Set(entries.map(entry => entry.receipt.control));
    if (expected.size !== entries.length || current.length !== expected.size || current.some(control => !expected.has(control))) throw Error('Unowned attachment controls prevent reset');
    const inputs = Array.from(new Map(entries.map(entry => [entry.receipt.input, {input:entry.receipt.input,files:entry.receipt.inputFiles}])).values());
    if (entries.some(entry => {const files=inputs.find(owned=>owned.input===entry.receipt.input).files;return files.length!==entry.receipt.inputFiles.length || files.some((file,index)=>file!==entry.receipt.inputFiles[index]);})) throw Error('Original input receipts disagree');
    const url = new URL(location.href);
    const conversation = url.pathname.match(/(?:^|\/)c\/([^/]+)/)?.[1] ?? null;
    if (url.origin !== cfg.navigation?.origin ||
        (cfg.navigation.conversationId !== null ? conversation !== cfg.navigation.conversationId :
          conversation !== null || (url.pathname.replace(/\/+$/, '') || '/') !== cfg.navigation.landingPath))
      throw Error('Upload reset navigation changed');
    reset = {composer, entries, inputs, owner:cfg.owner, document, url:location.href};
    checkSet(reset, false);
    resets.set(cfg.id, reset);
    for (const entry of entries) {
      checkSet(reset, true);
      if (!composer.isConnected || !entry.receipt.control.isConnected || entry.receipt.control.getAttribute('aria-label') !== entry.receipt.label) throw Error('Attachment changed before removal');
      const control = entry.receipt.control;
      if (!(control instanceof HTMLButtonElement)) throw Error('Owned removal control is not a button');
      control.type = 'button';
      control.click();
      checkSet(reset, true);
    }
    for (const {input} of inputs) {
      checkSet(reset, true);
      if (input.files.length) input.value = '';
      checkSet(reset, true);
    }
  }
  if (!reset || reset.owner.claimId !== cfg.owner.claimId || reset.owner.reservationId !== cfg.owner.reservationId || !reset.composer.isConnected) throw Error('Upload reset guard changed');
  checkSet(reset, true);
  if (controls(reset.composer).length || reset.entries.some(entry => entry.receipt.control.isConnected)) return {complete:false};
  if (reset.inputs.some(({input}) => input.files.length)) throw Error('Original file input did not clear');
  for (const entry of reset.entries) window.__oracleAttachmentEvidence.delete(entry.id);
  resets.delete(cfg.id);
  return {complete:true, removed:reset.entries.length, originalInputsCleared:true};
}
