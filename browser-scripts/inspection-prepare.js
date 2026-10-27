// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* alcuin-dom:inspection-prepare */

return (() => {
        const registry = window.__oraclePickerInspections ||= new Map();

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

        let writeIssued = false;
        try {
        let claim = readOracleTargetClaim();

    const inspectionAuthority = cfg.inspection ?? null;
    const inspectionRegistry = window.__oraclePickerInspections;
    const predecessor = claim?.inspection;
    const predecessorRecord = inspectionAuthority ? inspectionRegistry?.get(inspectionAuthority.token) : null;
    if (inspectionAuthority && (!predecessor || predecessor.token !== inspectionAuthority.token ||
        predecessor.parentReservationId !== inspectionAuthority.parentReservationId ||
        predecessor.rendererId !== inspectionAuthority.rendererId ||
        predecessor.rendererId !== window.__oracleOwnershipRendererId || predecessor.state !== 'clean' ||
        inspectionRegistry?.size !== 1 || !predecessorRecord || predecessorRecord.active !== false ||
        predecessorRecord.finalising || predecessorRecord.cleanupComplete !== true)) return false;

        if (claim?.retiring || (claim?.inspection && !inspectionAuthority) ||
            (registry.size !== 0 && !inspectionAuthority)) return false;
        const parent = cfg.owner;
        if (parent ? !matchesOracleSubmission(claim, parent) || claim.submission.state !== 'active' : claim?.submission) return false;
        if (inspectionAuthority && inspectionAuthority.parentReservationId !== cfg.owner.reservationId) return false;
        const rendererId = window.__oracleOwnershipRendererId ||= cfg.token;
        claim ??= { id: cfg.token, retiring: false };
        claim.inspection = { token: cfg.token, parentReservationId: cfg.owner.reservationId, rendererId, state: 'active' };
        writeIssued = true;
        writeOracleTargetClaim(claim);
        registry.set(cfg.token, { active: true, parent });
        if (inspectionAuthority) registry.delete(inspectionAuthority.token);
        return { token: cfg.token, parentReservationId: cfg.owner.reservationId, rendererId, acknowledged: true };
        } catch (error) { if (writeIssued) throw error; return false; }
      })();
}
