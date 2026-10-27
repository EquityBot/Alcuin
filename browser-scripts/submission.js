// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* industrious-dom:submission */

return (() => {
    if (window !== window.top) return false;

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

    try {
      const owner = cfg.owner;
      if (!oracleOwnershipId(owner.claimId) || !oracleOwnershipId(owner.reservationId)) return false;
      const action = cfg.action;
      if (!['acquire', 'assert', 'cleanup', 'releasing', 'clean', 'quarantine', 'dispatch', 'acknowledge', 'release'].includes(action)) return false;
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

      if (action === 'acquire') {
        if (claim?.retiring || (claim?.inspection && !inspectionAuthority) ||
            (inspectionAuthority && inspectionAuthority.parentReservationId !== null)) return false;
        if (inspectionAuthority && claim?.submission) return false;
        if (claim?.submission) return matchesOracleSubmission(claim, owner) && claim.submission.state === 'active';
        claim = { ...claim, id: owner.claimId, retiring: false,
          submission: { id: owner.reservationId, state: 'active', dispatch: 'not-started' } };
        if (inspectionAuthority) delete claim.inspection;
        writeOracleTargetClaim(claim);
        if (inspectionAuthority) inspectionRegistry.delete(inspectionAuthority.token);
        return true;
      }
      if (!matchesOracleSubmission(claim, owner)) return false;
      if (inspectionAuthority && (inspectionAuthority.parentReservationId !== owner.reservationId ||
          !['assert', 'dispatch', 'clean'].includes(action))) return false;
      const submission = claim.submission;
      if (action === 'assert') {
        if (submission.state !== 'active') return false;
        if (inspectionAuthority) {
          delete claim.inspection;
          writeOracleTargetClaim(claim);
          inspectionRegistry.delete(inspectionAuthority.token);
        }
        return true;
      }
      if (action === 'cleanup') return ['active', 'releasing', 'quarantined'].includes(submission.state);
      if (action === 'release') {
        if (submission.state !== 'clean' || claim.inspection) return false;
        delete claim.submission;
      } else if (action === 'quarantine') {
        submission.state = 'quarantined';
        if (submission.dispatch === 'started') submission.dispatch = 'ambiguous';
      } else if (action === 'releasing') {
        if (submission.state !== 'active' && submission.state !== 'releasing') return false;
        submission.state = 'releasing';
      } else if (action === 'clean') {
        if (submission.state !== 'releasing' || (claim.inspection && !inspectionAuthority) || submission.dispatch === 'started' || submission.dispatch === 'ambiguous') return false;
        submission.state = 'clean';
      } else if (action === 'dispatch') {
        if (submission.state !== 'active' || submission.dispatch !== 'not-started' || (claim.inspection && !inspectionAuthority)) return false;
        submission.dispatch = 'started';
      } else if (action === 'acknowledge') {
        if (submission.state !== 'active' || submission.dispatch !== 'started') return false;
        const receipt = cfg.receipt ?? null;
        const boundedId = value => typeof value === 'string' && value.length > 0 && value.length <= 256;
        const fingerprint = value => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value);
        if (cfg.provider === 'grok') {
          const uuid = value => typeof value === 'string' && /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(value);
          const message = value => typeof value === 'string' && value.startsWith('response-') && uuid(value.slice(9));
          if (location.origin !== 'https://grok.com' || !receipt || receipt.version !== 1 ||
              receipt.provider !== 'grok' || receipt.kind !== 'grok-websocket-v1' ||
              !['committed','completed'].includes(receipt.state) || receipt.privateChat !== true ||
              !['Auto','Fast'].includes(receipt.mode) || receipt.claimId !== owner.claimId ||
              receipt.reservationId !== owner.reservationId || !uuid(receipt.dispatchId) ||
              !uuid(receipt.clientMessageId) || !message(receipt.userMessageId) ||
              !fingerprint(receipt.promptSha256) || !fingerprint(receipt.digest) ||
              (receipt.state === 'completed' && (!uuid(receipt.responseId) || !message(receipt.assistantMessageId) ||
                receipt.assistantMessageId === receipt.userMessageId))) return false;
          // The renderer must independently match its own retained observer,
          // rather than trusting a caller-supplied provider label or hash.
          const witness = window.__oracleGrokWitness;
          const observed = witness?.acknowledge?.();
          // A live stream can advance between the caller's witness read and this check, so
          // accept only the same commitment, extended by later response identity or completion.
          const later = ['state', 'responseId', 'assistantMessageId'];
          const same = key => JSON.stringify(observed[key]) === JSON.stringify(receipt[key]);
          if (!observed || !['committed','completed'].includes(observed.state) ||
              Object.keys(receipt).some(key => key !== 'digest' && !Object.hasOwn(observed,key)) ||
              Object.keys(observed).some(key => !Object.hasOwn(receipt,key) || !later.includes(key) && !same(key)) ||
              receipt.state === 'completed' && !later.every(same) ||
              ['responseId','assistantMessageId'].some(key => receipt[key] !== null && !same(key))) return false;
          submission.dispatch = 'acknowledged';
          submission.commitReceipt = receipt;
          writeOracleTargetClaim(claim);
          return true;
        }
        if (!receipt || receipt.version !== 1 || receipt.kind !== 'new-user-message' ||
            receipt.claimId !== owner.claimId || receipt.reservationId !== owner.reservationId ||
            !boundedId(receipt.messageId) || !boundedId(receipt.turnId) ||
            !fingerprint(receipt.messageFingerprint) || !fingerprint(receipt.promptTextFingerprint) || !fingerprint(receipt.digest) ||
            (receipt.messageFingerprintFormat !== undefined && receipt.messageFingerprintFormat !== 'original-source-v1') ||
            !Number.isSafeInteger(receipt.turnsCount) || receipt.turnsCount < 1 ||
            receipt.conversation?.origin !== 'https://chatgpt.com' ||
            typeof receipt.conversation.conversationId !== 'string' || !/^[a-zA-Z0-9-]{1,256}$/.test(receipt.conversation.conversationId)) return false;
        const navigation = new URL(location.href);
        const conversationId = navigation.pathname.match(/\/c\/([a-zA-Z0-9-]+)(?=[/]|$)/)?.[1];
        if (navigation.origin !== receipt.conversation.origin || conversationId !== receipt.conversation.conversationId) return false;
        submission.dispatch = 'acknowledged';
        submission.commitReceipt = receipt;
      }
      if (inspectionAuthority) delete claim.inspection;
      writeOracleTargetClaim(claim);
      if (inspectionAuthority) inspectionRegistry.delete(inspectionAuthority.token);
      // Reference targetClaim buildDeliveryGuardDisposal: the persisted claim keeps a
      // quarantined target excluded, so keep a guard listening only while it still protects
      // the page (an unrolled file selection, or a delivered press whose release may follow).
      // Unlike the reference, entries stay registered: each owner cleanup must still find
      // its own guard to acknowledge positively, and guard cleanup is idempotent.
      if (action === 'quarantine') {
        const settle = (name, guardOf, retain) => {
          const guards = window[name];
          if (!guards) return;
          for (const id of Object.keys(guards)) {
            try {
              const guard = guardOf(guards[id]);
              if (!retain(guard)) guard.cleanup();
            } catch {}
          }
        };
        settle('__oracleFileInputGuards', record => record.guard, guard => { guard.validate(true); return !guard.rollbackComplete; });
        settle('__oracleAttachmentPromptGuards', guard => guard, () => false);
        settle('__oracleAttachmentPlusGuards', guard => guard, guard => guard.sawKeyDown);
        const nodes = window.__oracleAttachmentPlusNodes;
        if (nodes) for (const id of Object.keys(nodes)) if (!window.__oracleAttachmentPlusGuards?.[id]) delete nodes[id];
        const dispatch = window.__oracleAttachmentDispatchGuard;
        try { if (dispatch && !dispatch.sawKeyDown && !dispatch.sawPress) dispatch.cleanup(); } catch {}
      }
      return true;
    } catch { return false; }
  })();
}
