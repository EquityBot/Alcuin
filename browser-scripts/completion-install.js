// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* industrious-dom:completion-install */
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
    const key = '__oracleCurrentTurnCompletionV1';
    window[key]?.observer?.disconnect?.();
    const turns = () => (() => {
    const root = document;
    const containers = Array.from(root.querySelectorAll("[data-turn-key], [data-testid^=\"conversation-turn\"], [data-content-search-unit-key], [data-chatgpt-search-unit-key]")).filter(
      node => !node.parentElement?.closest("[data-turn-key], [data-testid^=\"conversation-turn\"], [data-content-search-unit-key], [data-chatgpt-search-unit-key]"),
    );
    return containers.length > 0
      ? containers.filter((node) => !containers.some((other) => other !== node && other.contains?.(node)))
      : Array.from(root.querySelectorAll("[data-turn-key], article[data-testid^=\"conversation-turn\"], div[data-testid^=\"conversation-turn\"], section[data-testid^=\"conversation-turn\"], article:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), div:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), section:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), article[data-turn], div[data-turn], section[data-turn]"));
  })();
    const baseline = cfg.baseline ?? null ?? turns().length;
    const completeNode = () => Array.from(
      document.querySelectorAll('[role="status"][aria-live="polite"]'),
    ).find((node) => (node.textContent || '').trim() === 'Response complete') ?? null;
    const isAssistant = (node) => {
      const key = (node.getAttribute?.('data-content-search-unit-key') || node.getAttribute?.('data-chatgpt-search-unit-key') || '').toLowerCase();
      if (key.endsWith(':user')) return false;
      if (key.endsWith(':assistant')) return true;
      const role = (node.getAttribute?.('data-message-author-role') || node.getAttribute?.('data-turn') || '').toLowerCase();
      if (role === 'user') return false;
      if (role === 'assistant') return true;
      return Boolean(node.querySelector?.('[data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"], [data-message-author-role="assistant"], [data-turn="assistant"]'));
    };
    const tracker = {
      baseline,
      priorCompleteNode: completeNode(),
      sawIncomplete: false,
      completedTurnIndex: null,
      observer: null,
    };
    tracker.sawIncomplete = !tracker.priorCompleteNode;
    const observe = () => {
      const currentCompleteNode = completeNode();
      if (!currentCompleteNode) {
        tracker.sawIncomplete = true;
        tracker.priorCompleteNode = null;
        return;
      }
      if (!tracker.sawIncomplete && currentCompleteNode === tracker.priorCompleteNode) return;
      const currentTurns = turns();
      for (let index = currentTurns.length - 1; index >= baseline; index -= 1) {
        if (!isAssistant(currentTurns[index])) continue;
        tracker.completedTurnIndex = index;
        tracker.sawIncomplete = false;
        tracker.priorCompleteNode = currentCompleteNode;
        tracker.observer?.disconnect();
        return;
      }
    };
    tracker.observer = new MutationObserver(observe);
    tracker.observer.observe(document.body, { childList: true, subtree: true, characterData: true });
    window[key] = tracker;
    return { baseline };
  })();
}
