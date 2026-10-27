// Native adaptation. Qualified with public synthetic requests on 21 October 2026.
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
  // Images use the provider's qualified multimodal_text envelope. The existing
  // upload guard binds composer attachments; this collector identifies the created
  // user message and never retains asset pointers or other request content.
  const contentMatches = content => {
    if (!content || !Array.isArray(content.parts)) return false;
    if (content.content_type === 'text')
      return expectedImages === 0 && content.parts.length === 1 && typeof content.parts[0] === 'string' && canonical(content.parts[0]) === expected;
    if (content.content_type !== 'multimodal_text') return false;
    let texts = 0, images = 0;
    for (const part of content.parts) {
      if (typeof part === 'string') {
        if (++texts !== 1 || canonical(part) !== expected) return false;
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
            if (payload?.action !== 'next' || message?.author?.role !== 'user' || !contentMatches(message?.content) ||
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
}
