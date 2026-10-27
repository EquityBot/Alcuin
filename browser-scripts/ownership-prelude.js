
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
