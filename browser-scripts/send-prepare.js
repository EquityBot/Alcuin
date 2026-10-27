// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* alcuin-dom:send-prepare */
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
        const context = window.__oracleAttachmentPromptGuards?.[cfg.guardId]?.context?.();
        if (!context) return {focused:false};
        const selectors = cfg.sendSelectors;
        const readOracleEditorText = node => {
    if (!node) return '';
    if (node instanceof HTMLTextAreaElement) return (node.value ?? '').replace(/\r\n?/g, '\n');
    const read = current => {
      if (current instanceof Element && current.matches('[data-inline-selection-pill], [data-inline-selection-pill-cursor-target]')) return '';
      // October 2026: selecting a pill also inserts one space after it.
      if (current.nodeType === 3) return current.previousSibling instanceof Element && current.previousSibling.matches('[data-inline-selection-pill]') ?
        (current.nodeValue || '').replace(/^[ \u00a0]/, '') : current.nodeValue || '';
      if (current.nodeName === 'BR') return current.classList.contains('ProseMirror-trailingBreak') ? '' : '\n';
      const children = Array.from(current.childNodes || []);
      if (!children.length) return current.innerText ?? current.textContent ?? '';
      const block = value => /^(P|DIV|PRE|LI)$/.test(value?.nodeName || '');
      return children.map((child, index) =>
        (index > 0 && (block(child) || block(children[index - 1])) ? '\n' : '') + read(child)
      ).join('');
    };
    return read(node).replace(/\r\n?/g, '\n');
  };
        const resolveButton = () => selectors.flatMap((selector) => Array.from(context.form.querySelectorAll(selector))).find(node => node.getBoundingClientRect().width > 0 && node.getBoundingClientRect().height > 0);
        const button = resolveButton();
        const inputSelectors = ['form[data-chatgpt-composer] [contenteditable="true"][role="textbox"]','textarea[data-id="prompt-textarea"]','textarea[placeholder*="Send a message"]','textarea[aria-label="Chat with ChatGPT"]','textarea[aria-label="Message ChatGPT"]','textarea:not([disabled]):not(#pending-home-input)','textarea[name="prompt-textarea"]','#prompt-textarea','.ProseMirror','[contenteditable="true"][role="textbox"]','[contenteditable="true"][data-virtualkeyboard="true"]'];
        const resolveEditor = () => inputSelectors.map(selector => document.querySelector(selector)).find(node => node?.getBoundingClientRect().width > 0 && node.getBoundingClientRect().height > 0);
        const editor = context.editor;
        const expectedPrompt = cfg.prompt.replace(/\r\n?/g, '\n');
        const startUrl = location.href;
        const point = null;
        const check = () => {
          assertOracleSubmission();
          const navigation = (() => {
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
          const rect = button?.getBoundingClientRect();
          const style = button ? window.getComputedStyle(button) : null;
          const preparation = (() => {
    const guard = window.__oracleAttachmentPromptGuards?.[cfg.guardId];
    if (!guard || typeof guard.verify !== 'function' || typeof guard.admit !== 'function') return { ready: false };
    return guard.verify();
  })();
          return {
            ...navigation,
            focused: context.form.isConnected && editor.closest('form') === context.form && button instanceof HTMLElement && document.activeElement === button &&
              resolveButton() === button && (!point || button.contains(document.elementFromPoint(point.x, point.y))) &&
              !button.hasAttribute('disabled') && button.getAttribute('aria-disabled') !== 'true' &&
              button.getAttribute('data-disabled') !== 'true' && rect.width > 0 && rect.height > 0 &&
              style.display !== 'none' && style.visibility !== 'hidden' && style.pointerEvents !== 'none',
            attachmentsReady: (() => { const composer = (// Native receipt inventory. Names describe uploads; only original objects authorise Send.
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
  })()); })(),
            promptReady: Boolean(expectedPrompt.trim()) && editor === resolveEditor() && readOracleEditorText(editor) === expectedPrompt,
            preparationReady: preparation.ready === true && preparation.complete === true && preparation.convertedToFile === false && !preparation.blocked,
          };
        };
        const safeCheck = () => {
          try { return check(); } catch { return { currentUrl: location.href, contextMatches: false, focused: false, attachmentsReady: false, promptReady: false, preparationReady: false }; }
        };
        const snapshot = safeCheck();
        if (!snapshot.preparationReady || !snapshot.contextMatches || !snapshot.focused || !snapshot.attachmentsReady || !snapshot.promptReady) return snapshot;
        const existing = window.__oracleAttachmentDispatchGuard;
        if (existing && !existing.finished) return { ...snapshot, focused: false };
        existing?.cleanup?.();
        const guard = { id: cfg.dispatchGuardId, sawKeyDown: false, sawPress: false, sawClick: false, blocked: null, finished: false, admitted: false };
        const installWitness = // Native adaptation. Qualified with public synthetic requests on 21 October 2026.
// This observes only the admitted composer's outbound message identity. Request bodies
// and headers are never retained. DOM timing and navigation are not creation evidence.
(cfg, dispatch, assertCurrent, temporary) => {
  const previous = window.__oracleSubmissionWitness;
  if (previous) throw Error('An earlier submission witness requires exact cleanup');
  if (typeof window.fetch !== 'function') throw Error('Submission transport is unavailable');
  const original = window.fetch;
  const originalDocument = document;
  const originalPath = location.pathname;
  let conversationId = originalPath.match(/^\/c\/([a-zA-Z0-9-]+)$/)?.[1] ?? null;
  const owner = Object.freeze({claimId:cfg.owner.claimId,reservationId:cfg.owner.reservationId});
  const canonical = value => value.normalize('NFC').replace(/\r\n?/g, '\n');
  let expected = canonical(cfg.prompt);
  const expectedImages = cfg.expectations.filter(item => item.image === true).length;
  // With the inline Web search pill selected, ChatGPT sends the pill as an '@Web search'
  // token at its composer position plus a 'search' system hint (observed 27 October 2026):
  // '<prompt>@Web search' when selected after the staged prompt, '@Web search <prompt>' when
  // the prompt follows an existing pill.
  const webSearch = cfg.webSearch === true;
  const textMatches = part => {
    const text = canonical(part);
    return webSearch ? text === expected + '@Web search' || text === '@Web search ' + expected : text === expected;
  };
  const hinted = hints => Array.isArray(hints) && hints.length === 1 && hints[0] === 'search';
  const hintsMatch = (payload, message) => !webSearch || (hinted(payload?.system_hints) && hinted(message?.metadata?.system_hints));
  // Images use the provider's qualified multimodal_text envelope. The existing
  // upload guard binds composer attachments; this collector identifies the created
  // user message and never retains asset pointers or other request content.
  const contentMatches = content => {
    if (!content || !Array.isArray(content.parts)) return false;
    if (content.content_type === 'text')
      return expectedImages === 0 && content.parts.length === 1 && typeof content.parts[0] === 'string' && textMatches(content.parts[0]);
    if (content.content_type !== 'multimodal_text') return false;
    let texts = 0, images = 0;
    for (const part of content.parts) {
      if (typeof part === 'string') {
        if (++texts !== 1 || !textMatches(part)) return false;
        continue;
      }
      if (!part || typeof part !== 'object' || Array.isArray(part) ||
          part.content_type !== 'image_asset_pointer' ||
          Object.keys(part).length !== 5 ||
          !Object.keys(part).every(key => ['content_type','asset_pointer','width','height','size_bytes'].includes(key)) ||
          typeof part.asset_pointer !== 'string' || part.asset_pointer.length === 0 ||
          !['width','height','size_bytes'].every(key => Number.isSafeInteger(part[key]) && part[key] > 0)) return false;
      images++;
    }
    return texts === 1 && images > 0 && images === expectedImages;
  };
  const expires = Date.now() + 120000;
  let witness = null, failure = null, diagnostic = null, privacyFailure = null, routeFailure = null, count = 0, closed = false, sealedResult = null;
  const pathShape = path => ({kind:path === '/' ? 'landing' : /^\/c\/[a-zA-Z0-9-]+$/.test(path) ? 'conversation' :
    /^\/c\/[a-zA-Z0-9-]+\/$/.test(path) ? 'conversation-trailing-slash' : /^\/c\/?$/.test(path) ? 'empty-conversation' :
    path.startsWith('/c/') ? 'unqualified-conversation' : 'other',segments:path.split('/').filter(Boolean).length});
  // First failure only, from a fixed vocabulary. Never retain a URL, body or
  // header to diagnose why this exact request must remain uncertain.
  const conflicts = [];
  const invalidate = (reason, detail, phase) => {
    failure ??= reason;
    const next = {reason:detail,phase};
    if (!diagnostic) diagnostic = next;
    else if (conflicts.length < 8 && JSON.stringify(diagnostic) !== JSON.stringify(next) &&
        !conflicts.some(value => value.reason === detail && value.phase === phase)) conflicts.push(next);
  };
  const refusePrivacy = reason => { privacyFailure = reason; return false; };
  const privacyState = () => {
    privacyFailure = null;
    if (document !== originalDocument) return refusePrivacy('document-changed');
    if (window !== window.top) return refusePrivacy('frame-changed');
    if (location.origin !== 'https://chatgpt.com') return refusePrivacy('origin-changed');
    if (!conversationId && /^\/c\/local-chatgpt%3A[a-f\d]{8}(?:-[a-f\d]{4}){3}-[a-f\d]{12}$/i.test(location.pathname))
      return temporary() === false ? refusePrivacy('temporary-refused') : null;
    const current = location.pathname.match(/^\/c\/([a-zA-Z0-9-]+)$/)?.[1] ?? null;
    if (current) {
      if (conversationId && current !== conversationId) return refusePrivacy('conversation-changed');
      conversationId ??= current;
    } else if (conversationId || location.pathname !== originalPath) {
      routeFailure ??= {initial:pathShape(originalPath),current:pathShape(location.pathname),latched:conversationId !== null};
      return refusePrivacy('path-changed');
    }
    const state = temporary();
    return state === false ? refusePrivacy('temporary-refused') : state;
  };
  const restore = () => {
    if (window.fetch === observedFetch) window.fetch = original;
    closed = true;
  };
  const release = () => {
    // A foreign fetch wrapper may still delegate to observedFetch. Clear the
    // payload and guard references in that closure too, not just the global record.
    cfg = dispatch = assertCurrent = temporary = expected = null;
  };
  const read = phase => {
    if (closed && sealedResult) return sealedResult;
    try {
      const privacy = privacyState();
      if (privacy === false) invalidate('submission-privacy-changed', privacyFailure, phase);
      if (failure) return {state:'invalid',reason:failure,failureDetail:diagnostic.reason,failurePhase:diagnostic.phase,
        ...(routeFailure ? {routeShape:routeFailure} : {}),
        ...(conflicts.length ? {conflicts:conflicts.map(value => ({...value}))} : {})};
      if (privacy === null) return {state:'pending'};
      if (witness) return {state:'captured',witness};
      return {state:closed || Date.now() >= expires ? 'expired' : 'pending'};
    } catch {
      invalidate('submission-witness-unavailable', 'privacy-probe-unavailable', phase);
      return {state:'invalid',reason:failure,failureDetail:diagnostic.reason,failurePhase:diagnostic.phase};
    }
  };
  function observedFetch(input, init) {
    if (!closed && Date.now() >= expires) { try { record.stop(); } catch {} }
    if (!closed && dispatch.admitted) {
      try {
        // Do not consume Request streams or inspect other bodies or request headers.
        const raw = typeof input === 'string' ? input : input instanceof URL ? input.href : input instanceof Request ? input.url : null;
        const url = raw === null ? null : new URL(raw, location.href);
        if (url?.origin === 'https://chatgpt.com' && url.pathname === '/backend-api/f/conversation' &&
            String(init?.method ?? '').toUpperCase() === 'POST') {
          if (url.username || url.password || url.search || url.hash) {
            invalidate('unqualified-submission-endpoint', 'unqualified-endpoint', 'matched-fetch');
          } else if (typeof init?.body !== 'string' || init.body.length > 8 * 1024 * 1024) {
            invalidate('unqualified-submission-body', 'unqualified-body', 'matched-fetch');
          } else {
            const payload = JSON.parse(init.body);
            const messages = payload?.messages;
            const message = Array.isArray(messages) && messages.length === 1 ? messages[0] : null;
            if (payload?.action !== 'next' || message?.author?.role !== 'user' || !contentMatches(message?.content) || !hintsMatch(payload, message) ||
                typeof message.id !== 'string' || !/^[a-f\d]{8}(?:-[a-f\d]{4}){3}-[a-f\d]{12}$/i.test(message.id)) {
              invalidate('unqualified-submission-identity', 'unqualified-identity', 'matched-fetch');
            } else {
              // This records only a candidate during route-before-mount. It is
              // never exposed as capture evidence until the exact view is verified.
              // New collection needs active admission; read-only recovery may
              // retain an existing witness after its exact owner is quarantined.
              assertCurrent();
              const privacy = privacyState();
              if (privacy === false) invalidate('submission-privacy-changed', privacyFailure, 'matched-fetch');
              if (payload.history_and_training_disabled !== true)
                invalidate('submission-privacy-changed', 'history-disabled-not-true', 'matched-fetch');
              if (failure) {
                // Invalid evidence never becomes a candidate after a later mode change.
              } else if (++count !== 1) {
                invalidate('multiple-submission-requests', 'multiple-requests', 'matched-fetch');
              } else {
                witness = Object.freeze({version:1,owner,witnessId:cfg.witnessId,dispatchGuardId:cfg.dispatchGuardId,
                  messageId:message.id,promptSha256:cfg.promptSha256,source:'chatgpt-fetch-v1',
                  requestPath:url.pathname,historyDisabled:true});
              }
            }
          }
        }
      } catch {
        invalidate('submission-witness-unavailable', 'collector-probe-unavailable', 'matched-fetch');
      }
    }
    // Preserve the caller, input object and argument list, including synchronous errors.
    return Reflect.apply(original, this, arguments);
  }
  const record = Object.freeze({
    witnessId:cfg.witnessId,
    dispatchGuardId:cfg.dispatchGuardId,
    owner,
    stop: () => {
      let settled = false;
      try {
        restore();
        clearTimeout(timer);
        if (window.__oracleSubmissionWitness !== record) return false;
        // Only settled evidence and document identity survive read-only recovery.
        sealedResult = read('stop');
        window.__oracleSubmissionWitness = Object.freeze({witnessId:record.witnessId,
          dispatchGuardId:record.dispatchGuardId,owner,document:originalDocument,
          conversationId,result:sealedResult,candidate:!failure && sealedResult.state==='pending' ? witness : null});
        settled = true;
        return true;
      } finally {
        closed = true;
        clearTimeout(timer);
        if (!settled) sealedResult = {state:'invalid',reason:'submission-witness-unavailable'};
        release();
      }
    },
    read: () => read('receipt-read'),
    cleanup: () => {
      try {
        restore();
        if (window.__oracleSubmissionWitness !== record) return false;
        delete window.__oracleSubmissionWitness;
        return true;
      } finally {
        closed = true;
        clearTimeout(timer);
        sealedResult = {state:'invalid',reason:'submission-witness-unavailable'};
        release();
      }
    }
  });
  // This minimal proof survives successful collector cleanup, but not a reload.
  // A saved witness must still belong to the exact original Document.
  window.__oracleSubmissionDocument = Object.freeze({witnessId:cfg.witnessId,
    dispatchGuardId:cfg.dispatchGuardId,owner,document:originalDocument});
  window.fetch = observedFetch;
  window.__oracleSubmissionWitness = record;
  const timer = setTimeout(() => record.stop(), 120000);
  return true;
};
        installWitness(cfg, guard, assertOracleSubmission, oracleTemporaryChat);
        const admit = () => {
          const preparation = (() => {
    const guard = window.__oracleAttachmentPromptGuards?.[cfg.guardId];
    if (!guard || typeof guard.verify !== 'function' || typeof guard.admit !== 'function') return { ready: false };
    return guard.admit();
  })();
          if (preparation.ready !== true || preparation.complete !== true || preparation.convertedToFile !== false || preparation.blocked) return false;
          guard.admitted = true; return true;
        };
        const detach = () => {
          window.removeEventListener('keydown', onKeyDown, true);
          window.removeEventListener('keyup', onKeyUp, true);
          window.removeEventListener('click', onClick, true);
          for (const event of ['pointerdown', 'mousedown', 'pointerup', 'mouseup']) window.removeEventListener(event, onPointer, true);
          guard.finished = true;
        };
        const cancel = (event, state) => {
          event.preventDefault(); event.stopImmediatePropagation(); guard.blocked = state;
        };
        const onKeyDown = event => {
          if (event.key !== 'Enter' || !event.isTrusted) return;
          guard.sawKeyDown = true;
          const state = safeCheck();
          if (guard.admitted || point || !state.preparationReady || !state.contextMatches || !state.focused || !state.attachmentsReady || !state.promptReady) cancel(event, state);
          else if (false && !admit()) cancel(event, { ...state, preparationReady: false });
        };
        const onKeyUp = event => {
          if (event.key !== 'Enter' || !event.isTrusted) return;
          if (guard.blocked || !oracleSubmissionIsActive()) cancel(event, guard.blocked ?? { focused: false });
        };
        const onClick = event => {
          if (!guard.sawKeyDown && !guard.sawPress) return;
          if (guard.admitted) { cancel(event, { preparationReady: false }); return; }
          const state = safeCheck();
          if (guard.blocked || !state.preparationReady || !state.contextMatches || !state.focused || !state.attachmentsReady || !state.promptReady ||
              !(event.target instanceof Node) || !button.contains(event.target)) cancel(event, guard.blocked ?? state);
          else if (!admit()) cancel(event, { ...state, preparationReady: false });
          else guard.sawClick = true;
        };
        const onPointer = event => {
          if (!point || !event.isTrusted || event.button !== 0) return;
          if (guard.admitted) {
            if (event.type === 'mousedown' || event.type === 'pointerdown') cancel(event, { preparationReady: false });
            return;
          }
          if (event.type === 'mousedown' || event.type === 'pointerdown') guard.sawPress = true;
          const state = safeCheck();
          if (guard.blocked || !state.preparationReady || !state.contextMatches || !state.focused || !state.attachmentsReady || !state.promptReady ||
              !(event.target instanceof Node) || !button.contains(event.target)) cancel(event, guard.blocked ?? state);
        };
        guard.click = () => {
          const state = safeCheck();
          if (!state.preparationReady || !state.contextMatches || !state.focused || !state.attachmentsReady || !state.promptReady) { guard.blocked = state; return false; }
          guard.sawPress = true; button.click(); return true;
        };
        guard.cleanup = () => { detach(); if (window.__oracleAttachmentDispatchGuard === guard) delete window.__oracleAttachmentDispatchGuard; };
        window.__oracleAttachmentDispatchGuard = guard;
        window.addEventListener('keydown', onKeyDown, true);
        window.addEventListener('keyup', onKeyUp, true);
        window.addEventListener('click', onClick, true);
        for (const event of ['pointerdown', 'mousedown', 'pointerup', 'mouseup']) window.addEventListener(event, onPointer, true);
        return { ...snapshot, guardInstalled: true };
      })();
}
