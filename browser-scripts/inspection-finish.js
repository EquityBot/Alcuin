// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* alcuin-dom:inspection-finish */

return (async () => {
                const registry = window.__oraclePickerInspections;

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

                const claim = readOracleTargetClaim();
                const gate = claim?.inspection;
                const record = registry?.get(cfg.token);
                if (!record || gate?.token !== cfg.token || gate.parentReservationId !== cfg.owner.reservationId ||
                    gate.rendererId !== window.__oracleOwnershipRendererId ||
                    ((cfg.rendererId ?? null) && gate.rendererId !== (cfg.rendererId ?? null)))
                  return { token: cfg.token, acknowledged: false };
                record.active = false;
                record.stop?.();
                record.finalising = true;
                const settleGate = state => {
                  const current = readOracleTargetClaim();
                  const owned = current?.inspection;
                  if (owned?.token !== cfg.token || owned.parentReservationId !== cfg.owner.reservationId || owned.rendererId !== gate.rendererId)
                    throw new Error('Picker ownership changed during finalisation.');
                  owned.state = state;
                  if (state === 'quarantined' && current.submission?.id === cfg.owner.reservationId) current.submission.state = 'quarantined';
                  writeOracleTargetClaim(current);
                };
                try {
                  settleGate('finalising');
                  const cleanupComplete = record.cleanup ? await record.cleanup(true) : true;
                  settleGate(cleanupComplete ? 'clean' : 'quarantined');
                  record.cleanupComplete = cleanupComplete;
                  return { token: cfg.token, parentReservationId: cfg.owner.reservationId, rendererId: gate.rendererId, acknowledged: true, cleanupComplete,
                    ...(cleanupComplete ? {} : { cleanupState: String(record.cleanupState || 'unknown').slice(0, 120) }) };
                } catch (error) {
                  settleGate('quarantined');
                  throw error;
                } finally {
                  record.finalising = false;
                }
              })();
}
