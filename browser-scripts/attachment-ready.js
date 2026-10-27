// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* industrious-dom:attachment-ready */
const assertOracleSubmission = () => { if (window !== window.top) throw Error('Top renderer required');
    const oracleOwnershipId = value => typeof value === 'string' && value.length > 0 && value.length <= 256;
    const readOracleTargetClaim = () => {
      const raw = sessionStorage.getItem('oracle:target-claim');
      if (raw === null) return null;
      const claim = JSON.parse(raw);
      const id = oracleOwnershipId;
      const object = value => value && typeof value === 'object' && !Array.isArray(value);
      if (!object(claim) || !id(claim.id) || typeof claim.retiring !== 'boolean') throw new Error('Invalid target ownership record.');
      if (claim.submission !== undefined && (!object(claim.submission) || !id(claim.submission.id) ||
          !['active', 'releasing', 'clean', 'quarantined'].includes(claim.submission.state) ||
          !['not-started', 'started', 'acknowledged', 'ambiguous'].includes(claim.submission.dispatch))) throw new Error('Invalid submission ownership record.');
      if (claim.inspection !== undefined && (!object(claim.inspection) || !id(claim.inspection.token) ||
          !id(claim.inspection.rendererId) ||
          !(claim.inspection.parentReservationId === null || id(claim.inspection.parentReservationId)) ||
          !['active', 'finalising', 'clean', 'quarantined'].includes(claim.inspection.state))) throw new Error('Invalid inspection ownership record.');
      return claim;
    };
    const writeOracleTargetClaim = claim => sessionStorage.setItem('oracle:target-claim', JSON.stringify(claim));
    const matchesOracleSubmission = (claim, owner) => claim && !claim.retiring &&
      claim.id === owner.claimId && claim.submission?.id === owner.reservationId;
   const claim = readOracleTargetClaim(); if (!matchesOracleSubmission(claim, cfg.owner) || claim.submission.state !== 'active') throw Error('Inactive submission'); }; const oracleSubmissionIsActive = () => { try { assertOracleSubmission(); return true; } catch { return false; } };
return (() => { const composer = (// Native receipt inventory. Names describe uploads; only original objects authorise Send.
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
})(cfg); return Boolean(composer) && !((() => {
    const root = composer;
    if (!root || root === document || root === document.body) return false;
    // Text can name a file or describe a completed operation; only explicit state blocks sending.
    const selectors = ['[data-state="loading"]', '[data-state="uploading"]', '[data-state="pending"]', '[aria-busy="true"]', '[role~="progressbar"]', 'progress'];
    const candidates = new Set(selectors.flatMap(selector => Array.from(root.querySelectorAll(selector))));
    if (selectors.some(selector => root.matches?.(selector))) candidates.add(root);
    return Array.from(candidates).some(node => {
      if (typeof node.getBoundingClientRect !== 'function') return false;
      // Non-editable widgets inside rich-text editors own their upload controls.
      const boundary = node.closest('textarea,[contenteditable=""],[contenteditable="true" i],[contenteditable="plaintext-only" i],[contenteditable="false" i]');
      const editor = boundary?.matches('[contenteditable="false" i]') ? null : boundary;
      if (editor && editor !== node) return false;
      const state = node.getAttribute('data-state');
      const pending = node.getAttribute('aria-busy') === 'true' || ['loading', 'uploading', 'pending'].includes(state);
      // File inputs and backing editors may be hidden behind the visible composer.
      if (pending && (node.matches('input[type="file"]') || editor === node)) return true;
      const rect = node.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return false;
      for (let ancestor = node; ancestor && typeof ancestor.getBoundingClientRect === 'function'; ancestor = ancestor.parentElement) {
        const style = typeof window === 'undefined' ? {} : window.getComputedStyle(ancestor);
        if (style.display === 'none' || style.visibility === 'hidden' || Number.parseFloat(style.opacity) === 0) return false;
        const clip = style.clip?.match(/^rect\((.*)\)$/)?.[1].split(/[,\s]+/).filter(Boolean).map(Number.parseFloat);
        if (clip?.length === 4 && clip.every(Number.isFinite) && (clip[2] <= clip[0] || clip[1] <= clip[3])) return false;
        if (style.clipPath === 'inset(50%)') return false;
      }
      if (pending) return true;
      const nativeProgress = node.matches('progress');
      if (nativeProgress || node.matches('[role~="progressbar"]')) {
        const current = nativeProgress
          ? (node.hasAttribute('value') ? node.value : NaN)
          : Number.parseFloat(node.getAttribute('aria-valuenow'));
        const maximum = nativeProgress ? node.max : Number.parseFloat(node.getAttribute('aria-valuemax') ?? '100');
        return !Number.isFinite(current) || !Number.isFinite(maximum) || current < maximum;
      }
      return false;
    });
  })()); })();
}
