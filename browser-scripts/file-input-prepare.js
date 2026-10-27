(cfg) => {
/* industrious-dom:file-input-prepare */
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
const input = document.querySelector(cfg.selector); const editor = document.querySelector('#prompt-textarea, form[data-chatgpt-composer] [contenteditable="true"][role="textbox"]');
if (!input || input.type !== 'file' || !editor || input.closest('form') !== editor.closest('form') || !input.closest('form')) return { ready:false };
const guard = (() => {
    const guardedInput = input;
    const initialFiles = guardedInput?.files;
    const initialSelection = Array.from(initialFiles || []);
    let transferredFiles = null;
    const selectionMatches = () => {
      const current = Array.from(guardedInput?.files || []);
      return current.length === initialSelection.length && current.every((file, index) => file === initialSelection[index]);
    };
    let blocked = null;
    let listening = false;
    let rollbackComplete = true;
    const cleanup = () => {
      if (!listening) return;
      window.removeEventListener('input', onEvent, true);
      window.removeEventListener('change', onEvent, true);
      listening = false;
    };
    const validate = (rollback = false) => {
      try {
        if (typeof assertOracleSubmission === 'function') assertOracleSubmission();
        if (transferredFiles !== null) {
          const files = Array.from(guardedInput.files);
          if (files.length && (files.length !== transferredFiles.length || files.some((file,index) => file !== transferredFiles[index]))) blocked = {currentUrl:location.href,modeUnverified:true};
        }
        const snapshot = (() => {
    const snapshot = (() => {
      const normalize = value => String(value || '').replace(/\s+/g, ' ').trim().toLowerCase();
      const isWorkLabel = value => ['work', '工作'].includes(normalize(value));
      const controlLabel = node => normalize(node.textContent || node.getAttribute('aria-label'));
      const modeValue = node => normalize(node.getAttribute('data-mode') || node.getAttribute('data-value') || node.getAttribute('value'));
      const visible = node => {
        if (node.closest?.(':is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), [data-testid^="conversation-turn-"], [data-turn-key], article[data-turn]')) return false;
        const rect = node.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return false;
        for (let current = node; current && typeof current.getBoundingClientRect === 'function'; current = current.parentElement) {
          const style = window.getComputedStyle(current);
          if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0' || current.getAttribute?.('aria-hidden') === 'true') return false;
        }
        return true;
      };
      const selected = node =>
        node?.getAttribute?.('aria-checked') === 'true' ||
        node?.getAttribute?.('aria-selected') === 'true' ||
        node?.getAttribute?.('aria-pressed') === 'true' ||
        node?.getAttribute?.('data-state') === 'on' ||
        node?.getAttribute?.('data-state') === 'active';
      const prompt = document.querySelector('#prompt-textarea, form[data-chatgpt-composer] [contenteditable="true"][role="textbox"]');
      const fallbackPrompt = document.querySelector('textarea[name="prompt-textarea"]');
      const composer = (prompt ?? fallbackPrompt)?.closest('form, [data-testid="composer"]');
      const selectedControls = Array.from(
        document.querySelectorAll('[aria-checked="true"],[aria-selected="true"],[aria-pressed="true"],[data-state="on"],[data-state="active"]'),
      ).filter(visible).filter(selected);
      const workToggleSelected = selectedControls.some(node => modeValue(node) === 'work' ||
        composer?.contains(node) && node.matches('button[role="radio"],button[role="tab"],button[aria-pressed]') && isWorkLabel(controlLabel(node)));
      // Labels outside the composer may name unrelated sidebar items or tabs.
      const placeholder = normalize(
        prompt?.getAttribute?.('data-placeholder') ||
        prompt?.getAttribute?.('placeholder') ||
        fallbackPrompt?.getAttribute?.('placeholder'),
      );
      return {
        currentUrl: location.href,
        workSelected: /(?:^|\/)c\/WEB(?::|%3a)/i.test(location.pathname) || workToggleSelected || placeholder === 'work on anything',
        modeUnverified: !(prompt ?? fallbackPrompt)?.isConnected || !composer?.isConnected || !visible(prompt ?? fallbackPrompt) || (prompt ?? fallbackPrompt).disabled === true || (prompt ?? fallbackPrompt).readOnly === true,
      };
    })();
    const expected = cfg.navigation;
    let contextMatches = false;
    try {
      const current = new URL(snapshot.currentUrl);
      const id = current.pathname.match(/(?:^|\/)c\/([^/]+)/)?.[1] ?? null;
      contextMatches = Boolean(expected) && oracleTemporaryChat() && !snapshot.workSelected && !snapshot.modeUnverified &&
        current.origin.toLowerCase() === expected.origin &&
        (expected.conversationId !== null ? id === expected.conversationId :
          id === null && (current.pathname.replace(/\/+$/, '') || '/') === expected.landingPath);
    } catch {}
    return { ...snapshot, contextMatches };
  })();
        if (!snapshot.contextMatches || !guardedInput?.isConnected || guardedInput.ownerDocument !== document || guardedInput.type !== 'file') blocked = { ...snapshot, modeUnverified: !guardedInput?.isConnected || guardedInput.ownerDocument !== document || guardedInput.type !== 'file' };
      } catch { blocked = { currentUrl: location.href, modeUnverified: true }; }
      if (blocked && rollback && guardedInput && !selectionMatches()) {
        try { guardedInput.files = initialFiles; rollbackComplete = selectionMatches(); }
        catch { rollbackComplete = false; }
      }
      return !blocked;
    };
    const onEvent = event => {
      if (event.target !== guardedInput) return;
      try {
        if (transferredFiles === null) {
          const files = Array.from(guardedInput.files);
          if (files.length !== 1 || files[0].name !== cfg.names[0] || files[0].size !== cfg.fileBytes) blocked = {currentUrl:location.href,modeUnverified:true};
          else transferredFiles = Object.freeze(files);
        }
      } catch { blocked = {currentUrl:location.href,modeUnverified:true}; }
      if (!validate(true)) { event.preventDefault(); event.stopImmediatePropagation(); }
    };
    if (validate()) {
      window.addEventListener('input', onEvent, true);
      window.addEventListener('change', onEvent, true);
      listening = true;
    }
    return { get blocked() { return blocked; }, get rollbackComplete() { return rollbackComplete; }, validate, cleanup, sealFiles: () => { if (!transferredFiles || !validate()) throw Error('Original transferred files changed'); return Object.freeze(Array.from(guardedInput.files)); } };
  })(); const ready = guard.validate();
(window.__oracleFileInputGuards ??= Object.create(null))[cfg.id] = { guard, input, id:cfg.id, owner:cfg.owner, navigation:cfg.navigation };
return { ready, blocked:guard.blocked };
}
