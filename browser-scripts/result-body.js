// Rust supplies the retained original-source, turn, completion and thinking predicates.
if (window !== window.top) throw Error('Top renderer required');
const claim = readOracleTargetClaim();
if (cfg.disposition === true) {
  if (!claim || claim.id !== cfg.owner.claimId || claim.submission || claim.inspection ||
      (cfg.retirementId ? claim.retiring !== true || claim.retirementId !== cfg.retirementId : claim.retiring))
    throw Error('Committed target ownership changed');
} else if (cfg.retirementId) {
  if (!claim || claim.id !== cfg.owner.claimId || claim.retiring !== true ||
      claim.retirementId !== cfg.retirementId ||
      (claim.submission && (claim.submission.id !== cfg.owner.reservationId ||
        !['quarantined', 'releasing'].includes(claim.submission.state))) ||
      (claim.inspection && (claim.inspection.parentReservationId !== cfg.owner.reservationId ||
        !['quarantined', 'clean'].includes(claim.inspection.state))))
    throw Error('Retirement ownership changed');
} else if (cfg.cleanup === true && claim?.id === cfg.owner.claimId && !claim.retiring &&
           !claim.submission && !claim.inspection) {
  // A completed session can retain its exact target after submission ownership was released.
} else {
if (!matchesOracleSubmission(claim, cfg.owner)) throw Error('Result ownership changed');
if (cfg.recovery === true) {
  if (!['active', 'releasing', 'quarantined'].includes(claim.submission.state) ||
      !['started', 'acknowledged', 'ambiguous'].includes(claim.submission.dispatch)) throw Error('Recovery submission identity changed');
  if (claim.submission.commitReceipt &&
      (claim.submission.commitReceipt.messageFingerprint !== cfg.receipt.messageFingerprint ||
       claim.submission.commitReceipt.messageId !== cfg.receipt.messageId)) throw Error('Recovery commitment changed');
} else if (claim.submission.state !== 'active' || claim.submission.dispatch !== 'acknowledged' ||
           claim.submission.commitReceipt?.messageFingerprint !== cfg.receipt.messageFingerprint ||
           claim.submission.commitReceipt?.messageId !== cfg.receipt.messageId) throw Error('Result commitment changed');
}
const url = new URL(location.href);
const conversationId = url.pathname.match(/(?:^|\/)c\/([a-zA-Z0-9-]+)(?=[/]|$)/)?.[1];
if (url.origin !== cfg.receipt.conversation.origin || conversationId !== cfg.receipt.conversation.conversationId)
  throw Error('Result conversation changed');
const turns = readTurns();
const legacyDisposition = cfg.disposition === true && cfg.legacyDisposition === true;
const userSelector = ':is([data-message-author-role="user"], [data-content-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"])';
const users = [];
for (const [index, turn] of turns.entries()) {
  for (const user of [...(turn.matches(userSelector) ? [turn] : []), ...turn.querySelectorAll(userSelector)].filter(node => !node.querySelector(userSelector)))
    users.push({ user, turn, index, id:userMessageId(user, turn) });
}
const user = users.at(-1);
if (!user || !user.id || (!legacyDisposition && user.id !== cfg.receipt.messageId) || users.filter(item => item.id === user.id).length !== 1)
  throw Error('Latest user message differs from commitment');
const original = readUserOriginalSource(user.user, user.turn, user.id);
if (cfg.native !== true && (!legacyDisposition || cfg.originalSourceRequired === true) && original.status !== 'present') throw Error('Complete original source is unavailable');
const visible = node => { const rect = node.getBoundingClientRect(), style = window.getComputedStyle(node); return rect.width > 0 && rect.height > 0 && style.display !== 'none' && style.visibility !== 'hidden' && style.opacity !== '0'; };
const stopExists = Array.from(document.querySelectorAll('[data-testid="stop-button"], [data-testid="composer-stop-button"], form button[aria-label*="stop" i]:not([aria-label*="dictat" i]):not([aria-label*="voice" i]):not([aria-label*="read" i])')).some(visible);
const strongThinkingActive = readThinkingActivity().strong;
const userTurnId = user.turn.getAttribute('data-turn-key') || user.turn.getAttribute('data-testid') ||
  user.turn.getAttribute('data-content-search-unit-key') || user.turn.getAttribute('data-chatgpt-search-unit-key');
const source = { currentUrl:location.href, conversationId, origin:url.origin,
  lastUserText:(user.user.textContent || '').replace(/\s+/g, ' ').trim(),
  lastUserTextRaw:(user.user.querySelector('[data-user-message-bubble="true"]') || user.user).textContent,
  lastUserMessageId:user.id, lastUserTurnIndex:user.index, lastUserOriginalSourceMessageId:user.id,
  lastUserTurnId:userTurnId, lastUserOriginalSourceText:original.status === 'present' ? original.text : undefined,
  stopExists, strongThinkingActive };
if (cfg.sourceOnly === true) return { ...source, researchStatus:readResearchCompletion(user.index), scopedResearchIframeCount:
  turns.slice(user.index).flatMap(turn => Array.from(turn.querySelectorAll('iframe'))).filter(frame => {
    const rect=frame.getBoundingClientRect();
    return rect.width>200 && rect.height>200 && /connector_openai_deep_research|deep-research/i.test(String(frame.getAttribute('src') || '')+' '+String(frame.getAttribute('name') || ''));
  }).length };
let assistant = null, assistantIndex = -1, assistantTurn = null;
for (let index = turns.length - 1; index >= 0; index--) {
  const node = lastAssistant(turns[index]);
  if (node) { assistant = node; assistantIndex = index; assistantTurn = turns[index]; break; }
}
if (!assistant || assistantIndex < user.index || !(user.user.compareDocumentPosition(assistant) & 4)) {
  // ChatGPT can announce a finished turn that holds only activity summaries and no answer unit.
  // Only an announcement the completion tracker saw arrive after this submission counts, and
  // Rust decides only once it has persisted.
  const answerless = !stopExists && !strongThinkingActive &&
    window.__oracleCurrentTurnCompletionV1?.completeWithoutAnswer === true &&
    Array.from(document.querySelectorAll('[role="status"][aria-live="polite"]'))
      .some(node => (node.textContent || '').trim() === 'Response complete');
  return { pending:true, stopExists, strongThinkingActive, answerless };
}
const ids = [assistant.getAttribute('data-message-id'), assistant.getAttribute('data-chatgpt-selection-message-id'),
  ...(assistant.getAttribute('data-chatgpt-search-message-ids')?.trim().split(/\s+/) || [])].filter(Boolean);
if (new Set(ids).size > 1) throw Error('Assistant message identity is ambiguous');
const messageId = ids[0] || assistant.getAttribute('data-content-search-unit-key') || assistant.getAttribute('data-chatgpt-search-unit-key');
const turnId = assistantTurn.getAttribute('data-turn-key') || assistantTurn.getAttribute('data-testid') ||
  assistantTurn.getAttribute('data-content-search-unit-key') || assistantTurn.getAttribute('data-chatgpt-search-unit-key');
if (!messageId && !turnId) throw Error('Assistant identity is unavailable');
const content = Array.from(assistant.querySelectorAll('[data-markdown-text-style="assistant-message"]'))
  .filter(node => node.getAttribute('data-markdown-text-tone') !== 'tertiary').at(-1) ||
  assistant.querySelector('.markdown, [data-message-content], .prose, [class*="MarkdownRoot"]') || assistant;
const rawText = content.innerText || content.textContent || '';
const text = legacyDisposition ? rawText.trim() : rawText;
// A turn can hold the user's message too; a control counts only from the assistant's message on.
const retryVisible = Array.from(assistantTurn.querySelectorAll('button, [role="button"]')).some(node => (assistant.compareDocumentPosition(node) & 4) && visible(node) && (node.innerText || node.textContent || node.getAttribute('aria-label') || '').trim().toLowerCase() === 'retry');
// Reference isAssistantTurnUiErrorText: this terminal state can render without Retry. Only known
// UI chrome may follow it, so an answer that quotes the phrase stays an answer.
const thinkingFailed = /^thinking failed[.!]?(?:\s+retry)?(?:\s+is this conversation helpful so far\??(?:\s+yes\s+no)?)?$/.test(text.toLowerCase().replace(/\s+/g, ' ').trim());
const roots = Array.from(assistant.querySelectorAll('[data-markdown-text-style="assistant-message"], .markdown, [data-message-content], .prose, [class*="MarkdownRoot"], [class*="markdown"]'));
const notice = Array.from(assistant.querySelectorAll('p, [role="alert"]'))
  .filter(node => !roots.some(root => root.contains(node)) && visible(node))
  .map(node => String(node.innerText || node.textContent || '').trim()).filter(Boolean).join('\n');
const isLoadNotice = value => /^this response couldn['’]t load[.!]?(?:\s+retry)?$/.test(value.toLowerCase().replace(/\s+/g, ' ').trim());
// The load notice can sit beside a Retry of its own, and activity summaries can share its turn.
// It stays a load failure, whose reply may still be read, and that Retry says the turn has ended.
const loadNotice = [text, notice, assistant.innerText || assistant.textContent || ''].some(isLoadNotice);
const tracker = window.__oracleCurrentTurnCompletionV1;
const completionVisible = retryVisible && loadNotice && !strongThinkingActive ||
  hasTurnCompletion({ messageId, turnId }, assistantIndex, false) ||
  tracker?.completedTurnIndex === assistantIndex && assistantIndex >= tracker.baseline;
if ((retryVisible || thinkingFailed) && !strongThinkingActive && !loadNotice) {
  if (cfg.uiErrors === true) return { ...source, uiError:'temporary_unavailable', lastAssistantMessageId:messageId };
  throw Error('Assistant displays a generation failure; result refused');
}
const images = serializeImages(assistant);
const files = serializeFiles(assistant, assistantIndex);
// Reference withheld candidate: empty answer roots beside a visible notice in the last turn.
// Only the terminal gate decides it, and it is never answer text.
if (!legacyDisposition && assistantIndex === turns.length - 1 && content !== assistant) {
  const answerEmpty = roots.filter(node => node.getAttribute('data-markdown-text-tone') !== 'tertiary')
    .every(node => !String(node.textContent || '').trim());
  const unloaded = isLoadNotice(notice);
  if (answerEmpty && notice && (images.length === 0 || unloaded))
    return { ...source, sameTurnExchange:assistantTurn === user.turn, lastAssistantMessageId:messageId,
      lastAssistantTurnId:turnId, lastAssistantTurnIndex:assistantIndex, assistantFollowsLatestUser:true,
      stopExists, strongThinkingActive, completionVisible, text:notice, html:assistant.innerHTML, images, files,
      withheld:true, unloaded };
}
return { ...source, sameTurnExchange:assistantTurn === user.turn, lastAssistantMessageId:messageId,
  lastAssistantTurnId:turnId, lastAssistantTurnIndex:assistantIndex, assistantFollowsLatestUser:true,
  stopExists, strongThinkingActive, completionVisible, text, html:content.innerHTML, images, files };
