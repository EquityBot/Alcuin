// Synchronous Private composer and exact completed-turn observations. No timers,
// page-global payload cache or provider history lookup are used here.
(cfg, privateChat) => {
  const retirement = cfg.action === 'observe-retirement';
  const visible = el => el instanceof HTMLElement && el.isConnected && el.getClientRects().length > 0 &&
    el.getAttribute('aria-hidden') !== 'true' && getComputedStyle(el).visibility !== 'hidden';
  const outsideMessages = el => !el.closest('[id^="response-"], [data-message-author-role], [data-role]');
  const all = (selector, root = document) => Array.from(root.querySelectorAll(selector)).filter(visible);
  const enabled = el => !el.hasAttribute('disabled') && !el.hasAttribute('data-disabled') && el.getAttribute('aria-disabled') !== 'true';
  const text = el => el instanceof HTMLElement ? el.innerText : el?.textContent ?? '';
  const editors = all('textarea, [contenteditable="true"][role="textbox"], [contenteditable="true"][role="combobox"]').filter(outsideMessages);
  const current = () => {
    if (window !== window.top || !privateChat()) throw Error('Grok Private Chat changed');
    const claim = JSON.parse(sessionStorage.getItem('oracle:target-claim') ?? 'null');
    if (!claim || claim.id !== cfg.owner.claimId || claim.inspection || (retirement ?
        !claim.retiring || claim.retirementId !== cfg.retirementId ||
          typeof cfg.retirementId !== 'string' || !/^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(cfg.retirementId) ||
          claim.submission && (claim.submission.id !== cfg.owner.reservationId ||
            !['quarantined','releasing'].includes(claim.submission.state) || claim.submission.dispatch !== 'acknowledged') :
        claim.retiring || claim.submission?.id !== cfg.owner.reservationId || claim.submission.state !== 'active' ||
          !['not-started','started','acknowledged'].includes(claim.submission.dispatch)))
      throw Error('Grok original composer owner changed');
    return claim.submission;
  };
  const mode = () => {
    const controls = all('button[aria-haspopup="menu"], button[aria-haspopup="listbox"]').filter(outsideMessages)
      .filter(el => /^(Auto|Fast|Expert|Heavy)(?:\s|$)/.test(text(el).trim()));
    if (controls.length !== 1) throw Error('Grok mode control is unavailable or ambiguous');
    return {control:controls[0],name:text(controls[0]).trim().match(/^(Auto|Fast|Expert|Heavy)(?:\s|$)/)[1]};
  };
  const submission = current();
  if (cfg.action === 'inspect') return {mode:mode().name};
  if (['open-mode','choose-mode','type'].includes(cfg.action)) {
    if (submission.dispatch !== 'not-started') throw Error('Grok preparation cannot follow dispatch');
    if (cfg.action === 'open-mode') {
      const selected = mode();
      if (!enabled(selected.control)) throw Error('Grok mode control is disabled');
      current();
      if (selected.control.getAttribute('aria-expanded') !== 'true') selected.control.click();
      current();
      return true;
    }
    if (cfg.action === 'choose-mode') {
      const choices = all('[role="menuitemradio"], [role="menuitem"], [role="option"]')
        .filter(outsideMessages).filter(el => el.closest('[role="menu"], [role="listbox"]'));
      if (!choices.length) return {status:'pending'};
      const available = name => choices.filter(el => new RegExp('^' + name + '(?:\\s|$)').test(text(el).trim()) && enabled(el) &&
        !/Requires\s+(SuperGrok|Premium|sign[- ]in)|Upgrade/i.test(text(el)));
      const auto = available('Auto'), fast = available('Fast');
      if (auto.length > 1 || fast.length > 1) throw Error('Grok mode choices are ambiguous');
      const choice = auto[0] ?? fast[0];
      if (!choice) throw Error('Grok has no available Auto or Fast mode');
      current();
      choice.click();
      current();
      return {status:'selected',mode:auto[0] ? 'Auto' : 'Fast'};
    }
    if (typeof cfg.prompt !== 'string' || !cfg.prompt || cfg.prompt.length > 524288 || !['Auto','Fast'].includes(cfg.mode) ||
        mode().name !== cfg.mode || editors.length !== 1 || !enabled(editors[0])) throw Error('Grok prompt entry is not qualified');
    const editor = editors[0], composer = editor.closest('.query-bar, form');
    if (!composer || composer.querySelector('button[aria-label="Open attachment"], [data-testid="attachment"], [role="progressbar"], [aria-busy="true"]'))
      throw Error('Grok text prompt cannot accept unqualified attachments');
    editor.focus();
    current();
    if (mode().name !== cfg.mode || !editor.isConnected) throw Error('Grok composer changed during focus');
    if (editor instanceof HTMLTextAreaElement) {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(editor, cfg.prompt);
      current();
      editor.dispatchEvent(new InputEvent('input', {bubbles:true,inputType:'insertText',data:cfg.prompt}));
    } else {
      const paragraph = document.createElement('p');
      cfg.prompt.split('\n').forEach((line,index) => {
        if (index) paragraph.append(document.createElement('br'));
        paragraph.append(document.createTextNode(line));
      });
      const range = document.createRange();
      range.selectNodeContents(editor);
      const selection = getSelection();
      selection?.removeAllRanges();
      selection?.addRange(range);
      current();
      if (!document.execCommand('insertHTML', false, paragraph.outerHTML)) throw Error('Grok text insertion was refused');
    }
    current();
    const actual = editor instanceof HTMLTextAreaElement ? editor.value : editor.innerText;
    if (!editor.isConnected || mode().name !== cfg.mode || actual !== cfg.prompt) throw Error('Complete Grok prompt did not survive entry');
    return true;
  }
  if (cfg.action !== 'observe' && !retirement) throw Error('Unsupported Grok DOM action');
  if ((!retirement && submission.dispatch !== 'acknowledged') || mode().name !== cfg.mode) throw Error('Grok answer needs its acknowledged selected mode');
  const witness = window.__oracleGrokWitness;
  if (!witness || witness.owner.claimId !== cfg.owner.claimId || witness.owner.reservationId !== cfg.owner.reservationId ||
      witness.dispatchId !== cfg.receipt?.dispatchId) throw Error('Original live Grok answer witness is unavailable');
  // Retirement uses the original sealed completion after the marker has stopped
  // listeners and released payload references. It never re-arms acknowledgement.
  const wire = retirement ? witness.read() : witness.acknowledge();
  if (wire?.state !== 'completed' || JSON.stringify(wire) !== JSON.stringify(cfg.receipt)) throw Error('Grok completion witness changed');
  const units = role => {
    const selector = '[data-testid="' + role + '-message"], [data-message-author-role="' + role + '"], [data-role="' + role + '"]';
    const roots = new Map();
    for (const unit of all(selector)) {
      const root = unit.closest('[id^="response-"]');
      if (!root) continue;
      const prior = roots.get(root);
      if (!prior || unit.contains(prior)) roots.set(root, unit);
      else if (!prior.contains(unit)) throw Error('Grok response has ambiguous role containers');
    }
    return [...roots].map(([root,unit]) => ({root,unit}));
  };
  const users = units('user'), assistants = units('assistant');
  if (!users.length || !assistants.length) return {state:'pending'};
  const user = users.at(-1), assistant = assistants.at(-1);
  if (user.root.id !== wire.userMessageId || assistant.root.id !== wire.assistantMessageId ||
      document.querySelectorAll('[id="' + wire.userMessageId + '"]').length !== 1 ||
      document.querySelectorAll('[id="' + wire.assistantMessageId + '"]').length !== 1 ||
      !(user.root.compareDocumentPosition(assistant.root) & Node.DOCUMENT_POSITION_FOLLOWING))
    throw Error('Grok DOM reply is not the original wire-paired turn');
  const content = entry => {
    const selector = '[data-testid="message-content"], .message-content, .markdown, .prose';
    const candidates = [...entry.unit.querySelectorAll(selector)];
    if (entry.unit.matches(selector)) candidates.unshift(entry.unit);
    const outer = candidates.filter(el => !candidates.some(other => other !== el && other.contains(el)));
    if (outer.length !== 1 || !visible(outer[0])) throw Error('Grok complete message container is unavailable or ambiguous');
    return outer[0];
  };
  const userContent = content(user), assistantContent = content(assistant);
  const source = text(userContent);
  // Grok can show the user's own turn as rendered Markdown: list markers become <li>
  // elements and blank lines change. Only when the turn's list items account for every
  // marker line, compare the non-blank lines with those markers removed; all other text
  // stays exact, so inline Markdown (links, code, emphasis) still fails closed. Dropped
  // markers can carry meaning (numbering, nesting, diff signs); that is acceptable only
  // because the wire witness has already matched the exact prompt sent. The observation
  // names the match only for this tolerance. browser_grok::listed_prompt and
  // browser_grok::turn_lines mirror `marker` and `lines`.
  const marker = /^(?:[-*+]|\d{1,9}[.)])\s+/;
  const lines = value => value.replace(/\r\n/g, '\n').split('\n').map(line => line.trim()).filter(Boolean);
  const sent = lines(cfg.prompt), markers = sent.filter(line => marker.test(line)).length;
  const promptMatch = source === cfg.prompt ? 'exact' :
    markers > 0 && markers === userContent.querySelectorAll('li').length &&
      sent.map(line => line.replace(marker, '')).join('\n') === lines(source).join('\n') ? 'rendered-list' : null;
  if (!promptMatch) throw Error('Complete Grok user source differs from the original dispatch');
  if (user.root.querySelector('button[aria-label="Open attachment"], [data-testid="attachment"]'))
    throw Error('Grok text answer has unqualified user attachments');
  const pending = '[aria-busy="true"], [aria-invalid="true"], [data-state="uploading"], [data-state="pending"], [data-state="streaming"], [data-state="loading"], [data-state="failed"], [data-state="error"]';
  const stopping = all('button[aria-label*="Stop" i], [role="progressbar"]').length > 0;
  const copy = all('button[aria-label*="Copy" i]', assistant.root).some(enabled);
  const regenerate = all('button[aria-label="Regenerate"]', assistant.root).some(enabled);
  if (stopping || !copy || !regenerate || assistant.root.matches(pending) || assistant.root.querySelector(pending)) return {state:'pending'};
  const answer = text(assistantContent);
  if (!answer.trim()) return {state:'pending'};
  const walker = document.createTreeWalker(assistant.root, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (node.textContent.trim() && !assistantContent.contains(node) &&
        !node.parentElement?.closest('button, [role="button"]'))
      throw Error('Grok response has text outside its complete message container');
  }
  // Refuse hidden, pending and out-of-content assets too. Until the separate
  // asset adapter is qualified, unknown buttons cannot stand in for text.
  const resource = assistant.root.querySelector('img, picture, video, audio, canvas, iframe, object, embed, a[download], [data-testid*="download" i], [data-testid*="attachment" i], [data-testid*="file" i], [data-testid*="image" i]');
  const unknownControl = [...assistant.root.querySelectorAll('button, [role="button"]')].some(el =>
    !/^(Copy|Copy text|Copy response|Regenerate|Good response|Bad response|Like|Dislike|Share|Read aloud)$/i.test((el.getAttribute('aria-label') || text(el)).trim()));
  if (resource || unknownControl) throw Error('Grok answer includes an unqualified asset or control');
  current();
  return {state:'completed',provider:'grok',kind:'grok-private-answer',privateChat:true,mode:cfg.mode,
    completionConfirmed:true,stopExists:false,strongThinkingActive:false,wireReceipt:wire,
    userMessageId:user.root.id,assistantMessageId:assistant.root.id,userText:source,...(promptMatch === 'exact' ? {} : {promptMatch}),text:answer,providerText:answer,images:[],files:[]};
}
