// Only the verified native journal calls this on its exact original target.
// A stopped completion survives here; reload/history cannot recreate it.
(cfg, privateChat) => {
  if (window !== window.top || !privateChat()) return false;
  const uuid = value => typeof value === 'string' && /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(value);
  if (!uuid(cfg.retirementId) || !uuid(cfg.owner?.claimId) || !uuid(cfg.owner?.reservationId)) return false;
  const claim = JSON.parse(sessionStorage.getItem('oracle:target-claim') ?? 'null');
  if (!claim || claim.id !== cfg.owner.claimId || claim.inspection ||
      typeof claim.retiring !== 'boolean' || claim.retiring && claim.retirementId !== cfg.retirementId) return false;
  const submission = claim.submission;
  if (submission && (submission.id !== cfg.owner.reservationId ||
      !['active','quarantined','releasing'].includes(submission.state) || submission.dispatch !== 'acknowledged' ||
      claim.retiring && submission.state === 'active')) return false;
  if (submission) {
    const receipt = submission.commitReceipt;
    if (!receipt || !/^[a-f0-9]{64}$/.test(receipt.digest) ||
        !Object.keys(cfg.committedReceipt).every(key => JSON.stringify(cfg.committedReceipt[key]) === JSON.stringify(receipt[key])) ||
        Object.keys(receipt).some(key => key !== 'digest' && !Object.hasOwn(cfg.committedReceipt,key))) return false;
  }
  const witness = window.__oracleGrokWitness;
  if (!witness || witness.owner.claimId !== cfg.owner.claimId || witness.owner.reservationId !== cfg.owner.reservationId ||
      witness.dispatchId !== cfg.receipt?.dispatchId) return false;
  // Stop before marking, whilst the live observer can still verify its active
  // owner. A repeated mark reads the same sealed receipt without retaining work.
  const observed = claim.retiring ? witness.read() : witness.stop();
  if (observed?.state !== 'completed' || JSON.stringify(observed) !== JSON.stringify(cfg.receipt)) return false;
  if (submission) submission.state = 'quarantined';
  claim.retiring = true;
  claim.retirementId = cfg.retirementId;
  sessionStorage.setItem('oracle:target-claim', JSON.stringify(claim));
  return true;
}
