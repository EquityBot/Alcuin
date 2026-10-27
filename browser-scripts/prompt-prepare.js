// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* industrious-dom:prompt-prepare */
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
        function dispatchClickSequence(target, validate = () => true){
    if(typeof oracleSubmissionIsActive === 'function' && !oracleSubmissionIsActive()) return false;
    if(!target || !(target instanceof EventTarget)) return false;
    const types = ['pointerdown','mousedown','pointerup','mouseup','click'];
    for (const type of types) {
      if(typeof oracleSubmissionIsActive === 'function' && !oracleSubmissionIsActive()) return false;
      if(!validate()) return false;
      const common = { bubbles: true, cancelable: true, view: window };
      let event;
      if (type.startsWith('pointer') && 'PointerEvent' in window) {
        event = new PointerEvent(type, { ...common, pointerId: 1, pointerType: 'mouse' });
      } else {
        event = new MouseEvent(type, common);
      }
      target.dispatchEvent(event);
    }
    return true;
  }
        const readOracleEditorText = node => {
    if (!node) return '';
    if (node instanceof HTMLTextAreaElement) return (node.value ?? '').replace(/\r\n?/g, '\n');
    const read = current => {
      if (current instanceof Element && current.matches('[data-inline-selection-pill], [data-inline-selection-pill-cursor-target]')) return '';
      // October 2026: selecting a pill also inserts one space after it.
      if (current.nodeType === 3) return current.previousSibling instanceof Element && current.previousSibling.matches('[data-inline-selection-pill]') ?
        (current.nodeValue || '').replace(/^[ \u00a0]/, '') : current.nodeValue || '';
      if (current.nodeName === 'BR') return current.classList.contains('ProseMirror-trailingBreak') ? '' : '\n';
      const children = Array.from(current.childNodes || []);
      if (!children.length) return current.innerText ?? current.textContent ?? '';
      const block = value => /^(P|DIV|PRE|LI)$/.test(value?.nodeName || '');
      return children.map((child, index) =>
        (index > 0 && (block(child) || block(children[index - 1])) ? '\n' : '') + read(child)
      ).join('');
    };
    return read(node).replace(/\r\n?/g, '\n');
  };
        const selectors = ['form[data-chatgpt-composer] [contenteditable="true"][role="textbox"]','textarea[data-id="prompt-textarea"]','textarea[placeholder*="Send a message"]','textarea[aria-label="Chat with ChatGPT"]','textarea[aria-label="Message ChatGPT"]','textarea:not([disabled]):not(#pending-home-input)','textarea[name="prompt-textarea"]','#prompt-textarea','.ProseMirror','[contenteditable="true"][role="textbox"]','[contenteditable="true"][data-virtualkeyboard="true"]'];
        const visible = node => { const r = node?.getBoundingClientRect(); return r && r.width > 0 && r.height > 0; };
        const resolveEditor = () => selectors.map(s => document.querySelector(s)).find(visible);
        const editor = resolveEditor();
        const preparedDocument = document;
        const preparedComposer = editor?.closest('form');
        const expectedText = cfg.prompt.replace(/\r\n?/g, '\n');
        const read = () => readOracleEditorText(editor);
        const write = value => { if (editor instanceof HTMLTextAreaElement) editor.value = value; else if (editor) editor.textContent = value; };
        const chipScope = editor?.closest('form') ?? document;
        const chipSelector = 'button[aria-label^="Remove Pasted text"]';
        const chips = () => chipScope.querySelectorAll(chipSelector).length;
        const guard = { blocked: null, beforeValue: '', sawInput: false, sealed: false, admitted: false, chipsBefore: null, convertedToFile: false };
        const conversionObserver = new MutationObserver(records => {
          if (guard.chipsBefore === null) return;
          if (chips() > guard.chipsBefore || records.some(record =>
            record.type === 'attributes' && record.target instanceof Element && record.target.matches(chipSelector) ||
            Array.from(record.addedNodes).some(node => node instanceof Element && (node.matches(chipSelector) || node.querySelector(chipSelector)))))
            guard.convertedToFile = true;
        });
        const validate = (requireFocus = true) => {
          try {
            assertOracleSubmission();
            const state = (() => {
    const snapshot = (() => {
      const normalize = value => String(value || '').replace(/\s+/g, ' ').trim().toLowerCase();
      const isWorkLabel = value => ['work', '工作'].includes(normalize(value));
      const controlLabel = node => normalize(node.textContent || node.getAttribute('aria-label'));
      const modeValue = node => normalize(node.getAttribute('data-mode') || node.getAttribute('data-value') || node.getAttribute('value'));
      const visible = node => {
        if (node.closest?.(':is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), [data-testid^="conversation-turn-"], [data-turn-key], article[data-turn]')) return false;
        const rect = node.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return false;
        for (let current = node; current && typeof current.getBoundingClientRect === 'function'; current = current.parentElement) {
          const style = window.getComputedStyle(current);
          if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0' || current.getAttribute?.('aria-hidden') === 'true') return false;
        }
        return true;
      };
      const selected = node =>
        node?.getAttribute?.('aria-checked') === 'true' ||
        node?.getAttribute?.('aria-selected') === 'true' ||
        node?.getAttribute?.('aria-pressed') === 'true' ||
        node?.getAttribute?.('data-state') === 'on' ||
        node?.getAttribute?.('data-state') === 'active';
      const prompt = document.querySelector('#prompt-textarea, form[data-chatgpt-composer] [contenteditable="true"][role="textbox"]');
      const fallbackPrompt = document.querySelector('textarea[name="prompt-textarea"]');
      const composer = (prompt ?? fallbackPrompt)?.closest('form, [data-testid="composer"]');
      const selectedControls = Array.from(
        document.querySelectorAll('[aria-checked="true"],[aria-selected="true"],[aria-pressed="true"],[data-state="on"],[data-state="active"]'),
      ).filter(visible).filter(selected);
      const workToggleSelected = selectedControls.some(node => modeValue(node) === 'work' ||
        composer?.contains(node) && node.matches('button[role="radio"],button[role="tab"],button[aria-pressed]') && isWorkLabel(controlLabel(node)));
      // Labels outside the composer may name unrelated sidebar items or tabs.
      const placeholder = normalize(
        prompt?.getAttribute?.('data-placeholder') ||
        prompt?.getAttribute?.('placeholder') ||
        fallbackPrompt?.getAttribute?.('placeholder'),
      );
      return {
        currentUrl: location.href,
        workSelected: /(?:^|\/)c\/WEB(?::|%3a)/i.test(location.pathname) || workToggleSelected || placeholder === 'work on anything',
        modeUnverified: !(prompt ?? fallbackPrompt)?.isConnected || !composer?.isConnected || !visible(prompt ?? fallbackPrompt) || (prompt ?? fallbackPrompt).disabled === true || (prompt ?? fallbackPrompt).readOnly === true,
      };
    })();
    const expected = cfg.navigation;
    let contextMatches = false;
    try {
      const current = new URL(snapshot.currentUrl);
      const id = current.pathname.match(/(?:^|\/)c\/([^/]+)/)?.[1] ?? null;
      contextMatches = Boolean(expected) && oracleTemporaryChat() && !snapshot.workSelected && !snapshot.modeUnverified &&
        current.origin.toLowerCase() === expected.origin &&
        (expected.conversationId !== null ? id === expected.conversationId :
          id === null && (current.pathname.replace(/\/+$/, '') || '/') === expected.landingPath);
    } catch {}
    return { ...snapshot, contextMatches };
  })();
            const focused = document === preparedDocument && editor instanceof HTMLElement && editor.ownerDocument === preparedDocument && editor.isConnected && resolveEditor() === editor &&
              (!requireFocus || document.activeElement === editor || editor.contains(document.activeElement));
            if (!state.contextMatches || !focused) guard.blocked = { ...state, focused };
          } catch { guard.blocked = { currentUrl: location.href, modeUnverified: true }; }
          return !guard.blocked;
        };
        const cancel = event => { event.preventDefault(); event.stopImmediatePropagation(); };
        const onBeforeInput = event => {
          if (!validate() || event.target !== editor) { guard.blocked ??= { currentUrl: location.href, focused: false }; cancel(event); return; }
        };
        const onInput = event => {
          if (!validate() || event.target !== editor) {
            guard.blocked ??= { currentUrl: location.href, focused: false };
            cancel(event); return;
          }
          guard.sawInput = true;
        };
        const detachInput = () => { window.removeEventListener('beforeinput', onBeforeInput, true); window.removeEventListener('input', onInput, true); };
        guard.cleanup = () => { detachInput(); conversionObserver.disconnect(); };
        guard.rollback = () => {
          if (guard.sealed || guard.admitted || !guard.sawInput || !validate() || read() !== guard.observedValue) return {restored:false};
          write(guard.beforeValue);
          editor.dispatchEvent(new InputEvent('input', {bubbles:true,inputType:'deleteContentBackward'}));
          return {restored:validate() && read() === guard.beforeValue};
        };
        const observe = (requireSealed, requireFocus) => {
          const valid = validate(requireFocus);
          const text = read().replace(/\r\n?/g, '\n');
          const convertedToFile = guard.convertedToFile || guard.chipsBefore !== null && chips() > guard.chipsBefore;
          if (convertedToFile) guard.convertedToFile = true;
          const complete = text === expectedText;
          if (convertedToFile || requireSealed && (!guard.sealed || guard.admitted || !complete))
            guard.blocked ??= { currentUrl: location.href, convertedToFile, complete, preparationInvalid: true };
          return { ready: valid && !guard.blocked, blocked: guard.blocked, complete, convertedToFile, length: text.length };
        };
        guard.verify = () => observe(true, false);
        guard.context = () => { const state = guard.verify(); return state.ready && state.complete && preparedComposer?.isConnected && editor.closest('form') === preparedComposer ? {editor,form:preparedComposer} : null; };
        guard.admit = () => { const state = guard.verify(); if (state.ready && state.complete) guard.admitted = true; return state; };
        guard.seal = () => {
          const state = observe(false, true);
          if (!state.ready || !state.complete) return { ...state, listenersDetached: false, guardRetained: false };
          detachInput(); guard.sealed = true;
          delete guard.insert; delete guard.fallback; delete guard.paste; delete guard.summary;
          return { ...state, guardId: cfg.guardId, listenersDetached: true, guardRetained: true };
        };
        guard.summary = () => {
          const state = observe(false, true);
          const ready = state.ready;
          guard.observedValue = read();
          const text = read().replace(/\r\n?/g, '\n');
          return { ...state, ready, blocked: guard.blocked, length: text.length, sawInput: guard.sawInput, complete: text === expectedText };
        };
        guard.insert = text => {
          if (!validate()) return;
          guard.beforeValue = read();
          const before = new InputEvent('beforeinput', { bubbles: true, cancelable: true, data: text, inputType: 'insertFromPaste' });
          if (!editor.dispatchEvent(before) || !validate()) return;
          try { document.execCommand('insertText', false, text); } catch {}
        };
        guard.fallback = text => {
          if (!validate() || read().trim()) return;
          const before = new InputEvent('beforeinput', { bubbles: true, cancelable: true, data: text, inputType: 'insertFromPaste' });
          if (!editor.dispatchEvent(before) || !validate()) return;
          write(text);
          editor.dispatchEvent(new InputEvent('input', { bubbles: true, data: text, inputType: 'insertFromPaste' }));
        };
        guard.paste = async text => {
          if (editor instanceof HTMLTextAreaElement) return { used: false };
          if (!editor?.isContentEditable || !validate()) return { used: true, complete: false };
          guard.chipsBefore = chips();
          conversionObserver.observe(chipScope, { childList: true, subtree: true, attributes: true, attributeFilter: ['aria-label'] });
          for (let i = 0; i < text.length;) {
            if (!validate()) return { used: true, complete: false, blocked: guard.blocked };
            let end = Math.min(i + 4000, text.length);
            if (end < text.length && /[\uD800-\uDBFF]/.test(text[end - 1])) end--;
            if (text[end - 1] === '\r' && text[end] === '\n') end--;
            const data = new DataTransfer();
            const chunk = text.slice(i, end).replace(/\r\n?/g, '\n');
            data.setData('text/plain', chunk);
            editor.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
            i = end;
            await new Promise(resolve => setTimeout(resolve, 0));
          }
          const summary = guard.summary();
          return { ...summary, used: true, convertedToFile: chips() > guard.chipsBefore };
        };
        (window.__oracleAttachmentPromptGuards ??= Object.create(null))[cfg.guardId] = guard;
        if (!validate(false)) return { ready: false, blocked: guard.blocked };
        guard.beforeValue = read();
        window.addEventListener('beforeinput', onBeforeInput, true);
        window.addEventListener('input', onInput, true);
        dispatchClickSequence(editor);
        if (!validate(false)) return { ready: false, blocked: guard.blocked };
        editor.focus();
        if (!validate()) return { ready: false, blocked: guard.blocked };
        const selection = document.getSelection();
        if (selection && !(editor instanceof HTMLTextAreaElement)) {
          const range = document.createRange(); range.selectNodeContents(editor); range.collapse(false);
          selection.removeAllRanges(); selection.addRange(range);
        }
        return guard.summary();
      })();
}
