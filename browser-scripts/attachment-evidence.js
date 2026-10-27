// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* alcuin-dom:attachment-evidence */
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
return (() => {
    const names = cfg.names.map(name => String(name).toLowerCase().replace(/\s+/g, ' ').trim());
    const action = cfg.action;
    const id = cfg.id ?? '';
    const key = '__oracleAttachmentEvidence';
    if (action === 'clear') { delete globalThis[key]; return true; }
    const empty = names.map(() => false);
    if (action === 'read' && !globalThis[key]) return empty;
    const receipts = globalThis[key] ??= new Map();
    const visible = node => node instanceof HTMLElement && node.getBoundingClientRect().width > 0 && node.getBoundingClientRect().height > 0;
    const sendSelectors = ['button[data-testid="send-button"]','button[data-testid*="composer-send"]','form button[type="submit"]','button[type="submit"][data-testid*="send"]','button[aria-label*="Send"]'];
    const prompt = ['form[data-chatgpt-composer] [contenteditable="true"][role="textbox"]','textarea[data-id="prompt-textarea"]','textarea[placeholder*="Send a message"]','textarea[aria-label="Chat with ChatGPT"]','textarea[aria-label="Message ChatGPT"]','textarea:not([disabled]):not(#pending-home-input)','textarea[name="prompt-textarea"]','#prompt-textarea','.ProseMirror','[contenteditable="true"][role="textbox"]','[contenteditable="true"][data-virtualkeyboard="true"]'].flatMap(selector => Array.from(document.querySelectorAll(selector))).find(visible);
    const send = sendSelectors.map(selector => document.querySelector(selector)).find(visible);
    // Upload controls can make a smaller ancestor match after the baseline.
    // Keep the original form as the composer when it owns the send control.
    const form = (prompt ?? send)?.closest('form');
    let composer = form && sendSelectors.some(selector => form.querySelector(selector)) ? form : null;
    let fallback = null;
    for (let node = composer ? null : (prompt ?? send)?.parentElement; node && node !== document.body && node !== document.documentElement; node = node.parentElement) {
      if (!sendSelectors.some(selector => node.querySelector(selector))) continue;
      fallback ??= node;
      if (node.querySelector('input[type="file"],[data-testid*="attachment"],[aria-label*="Remove" i]')) {
        composer = node;
        break;
      }
    }
    composer ??= fallback;
    if (!composer) return action === 'read' ? empty : false;
    const controls = () => Array.from(composer.querySelectorAll('[aria-label*="Remove" i]')).filter(node => {
      if (!visible(node) || node.closest('textarea,[contenteditable="true"]')) return false;
      const label = (node.getAttribute('aria-label') ?? '').toLowerCase();
      return /^remove (?:file|attachment|image|photo)(?: |$)/.test(label) || node.matches('button') && /^remove \S/.test(label) || Boolean(node.closest('[data-testid*="chip"],[data-testid*="attachment"],[data-testid*="upload"]'));
    });
    const currentControls = controls();
    const valid = receipt => receipt.composer === composer && receipt.control?.isConnected && currentControls.includes(receipt.control) && receipt.control.getAttribute('aria-label') === receipt.label;
    for (const [receiptId, receipt] of receipts) {
      if (receipt.composer !== composer || (receipt.control && !valid(receipt))) receipts.delete(receiptId);
    }
    if (action === 'begin') {
      if (receipts.has(id)) return false;
      receipts.set(id, { name: names[0], composer, document, owner:cfg.owner, before: new Set(currentControls) });
      return true;
    }
    if (action === 'confirm') {
      const receipt = receipts.get(id);
      if (!receipt || receipt.composer !== composer) return false;
      if (valid(receipt)) return true;
      const claimed = new Set(Array.from(receipts.values()).map(entry => entry.control).filter(Boolean));
      const added = currentControls.filter(control => !receipt.before.has(control) && !claimed.has(control));
      // Ambiguous additions cannot identify which file was accepted.
      if (added.length !== 1) return false;
      const addedLabel = (added[0].getAttribute('aria-label') ?? '').toLowerCase().replace(/\s+/g, ' ').trim();
      const bareFilename = added[0].matches('button') && /^remove \S/.test(addedLabel) && !/^remove (?:file|attachment|image|photo)(?: |$)/.test(addedLabel);
      if (bareFilename && addedLabel !== 'remove ' + receipt.name) return false;
      receipt.control = added[0];
      receipt.label = added[0].getAttribute('aria-label');
      receipt.before.clear();
      return true;
    }
    return names.map(name => Array.from(receipts.values()).some(receipt => receipt.name === name && valid(receipt)));
  })();
}
