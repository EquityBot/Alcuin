// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* alcuin-dom:send-focus */
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
      if (!context) return {status:'unavailable'};
      const selectors = cfg.sendSelectors;
      const button = selectors.flatMap((selector) => Array.from(context.form.querySelectorAll(selector))).find(node => node.getBoundingClientRect().width > 0 && node.getBoundingClientRect().height > 0);
      if (!(button instanceof HTMLElement)) return { status: 'absent' };
      const rect = button.getBoundingClientRect();
      const style = window.getComputedStyle(button);
      const enabled = !button.hasAttribute('disabled') &&
        button.getAttribute('aria-disabled') !== 'true' &&
        button.getAttribute('data-disabled') !== 'true' &&
        style.pointerEvents !== 'none' &&
        style.display !== 'none' &&
        style.visibility !== 'hidden';
      if (rect.width <= 0 || rect.height <= 0 || !enabled) return { status: 'unavailable' };
      const point = null;
      if (point && !button.contains(document.elementFromPoint(point.x, point.y))) return { status: 'unavailable' };
      button.focus({ preventScroll: true });
      return { status: document.activeElement === button ? 'focused' : 'unavailable' };
    })();
}
