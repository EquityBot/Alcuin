// Derived from maintained buildCopyExpression; Rust acknowledges exact-token cancellation.
(cfg) => { /* industrious-dom:historical-copy-markdown */
if (window !== window.top || typeof cfg.captureToken !== 'string' || cfg.captureToken.length < 16) throw Error('Invalid historical copy token');
const key='__calvinoHistoricalCopiesV1';
if (window[key] === undefined) Object.defineProperty(window,key,{value:new Map(),configurable:false,writable:false});
const registry=window[key];if (!(registry instanceof Map)) throw Error('Historical copy registry changed');
for(const [token,record] of registry)if(record.deadline<Date.now()){record.cancelled=true;record.cancel?.();if(!record.started || record.settled)registry.delete(token);}
let state=registry.get(cfg.captureToken);
if(cfg.action==='cancel') {
  if(!state){state={deadline:Date.now()+10000,cancelled:true,settled:true};registry.set(cfg.captureToken,state);}
  state.cancelled=true;state.cancel?.();if(!state.started)state.settled=true;return {cancelled:true,settled:state.settled===true};
}
if(cfg.action==='prepare') {
  if(state || registry.size>=16 || Array.from(registry.values()).some(record=>!record.cancelled && !record.settled))return {prepared:false};
  state={deadline:Date.now()+Math.max(1,Math.min(10000,cfg.timeoutMs)),cancelled:false,settled:false,started:false};
  registry.set(cfg.captureToken,state);return {prepared:true};
}
if(cfg.action!=='run' || !state || state.started || state.cancelled || Date.now()>=state.deadline)throw Error('Historical copy preparation changed');
state.started=true;
const oracleSubmissionIsActive=()=>registry.get(cfg.captureToken)===state && !state.cancelled && Date.now()<state.deadline;
// Ownership alone; expiry settles as 'timeout' or 'missing-button', never as a conversation change.
const captureIsOwned=()=>registry.get(cfg.captureToken)===state && !state.cancelled;
 return Promise.resolve((() => {
    const expectedConversationId = cfg.receipt.conversation.conversationId;
    const expectedOrigin = cfg.receipt.conversation.origin;
    const conversationChanged = () => !captureIsOwned() || (expectedOrigin && new URL(location.href).origin !== expectedOrigin) || (expectedConversationId && new URL(location.href).pathname.match(/(?:^|\/)c\/([a-zA-Z0-9-]+)/)?.[1] !== expectedConversationId);
    if (conversationChanged()) return {success: false, status: 'conversation-changed'};
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
    // ChatGPT's October 2026 renderer drops the test ID; its answer Copy sits in the turn's action
    // bar beside the user's 'Copy message' and in-answer 'Copy code' controls, and reads 'Copied' for about
    // 2 s after a click (observed 27 October 2026).
    const BUTTON_SELECTOR = 'button[data-testid="copy-turn-action-button"], .turn-action-controls button[aria-label="Copy"], .turn-action-controls button[aria-label="Copied"]';


    let copySelectionAmbiguous = false;
    const locateButton = () => {
      copySelectionAmbiguous = false;
      const hint = cfg.meta;
      const messageSelector = '[data-message-id], [data-content-search-unit-key], [data-chatgpt-search-message-ids]';
      const assistantSelector = '[data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"], [data-turn="assistant"]';
      const identity = node => node.getAttribute('data-message-id') || node.getAttribute('data-chatgpt-search-message-ids')?.split(' ')[0] || node.getAttribute('data-content-search-unit-key');
      const ambiguous = () => { copySelectionAmbiguous = true; return null; };
      if (!hint?.messageId || !hint?.turnId) return ambiguous();
      const messages = Array.from(document.querySelectorAll(messageSelector)).filter(node => identity(node) === hint.messageId);
      if (!messages.length) return null;
      if (messages.length !== 1) return ambiguous();
      const message = messages[0];
      const turn = message.closest('[data-turn-key], [data-testid^="conversation-turn"]');
      if (!turn || (turn.getAttribute('data-turn-key') || turn.getAttribute('data-testid')) !== hint.turnId ||
          !message.closest(assistantSelector) || message.closest('[data-message-author-role="user"], pre, code')) return ambiguous();
      const safe = button => !button.closest('[data-user-message-bubble], [data-message-author-role="user"], [data-content-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"], pre, code');
      const buttons = Array.from(turn.querySelectorAll(BUTTON_SELECTOR)).filter(safe);
      const inside = buttons.filter(button => message.contains(button) && button.closest(messageSelector) === message);
      if (inside.length > 1) return ambiguous();
      if (inside.length === 1) return inside[0];
      // A sibling action bar is attributable only when both message and action are unique.
      const peers = Array.from(turn.querySelectorAll(messageSelector)).filter(node => node.closest(assistantSelector));
      if (buttons.length > 1 || peers.length !== 1 || peers[0] !== message) return ambiguous();
      return buttons[0] ?? null;
    };

    const interceptClipboard = () => {
      const clipboard = navigator.clipboard;
      const state = { text: '', updatedAt: 0 };
      if (!clipboard) {
        return { state, restore: () => {} };
      }
      const originalWriteText = clipboard.writeText;
      const originalWrite = clipboard.write;
      clipboard.writeText = (value) => {
        state.text = typeof value === 'string' ? value : '';
        state.updatedAt = Date.now();
        return Promise.resolve();
      };
      clipboard.write = async (items) => {
        try {
          const list = Array.isArray(items) ? items : items ? [items] : [];
          for (const item of list) {
            if (!item) continue;
            const types = Array.isArray(item.types) ? item.types : [];
            if (types.includes('text/plain') && typeof item.getType === 'function') {
              const blob = await item.getType('text/plain');
              const text = await blob.text();
              state.text = text ?? '';
              state.updatedAt = Date.now();
              break;
            }
          }
        } catch {
          state.text = '';
          state.updatedAt = Date.now();
        }
        return Promise.resolve();
      };
      return {
        state,
        restore: () => {
          clipboard.writeText = originalWriteText;
          clipboard.write = originalWrite;
        },
      };
    };

    return new Promise((resolveOuter) => { let waitTimer=null; const resolve=payload=>{if(waitTimer)clearTimeout(waitTimer); state.settled=true;resolveOuter(payload);}; state.cancel=()=>{state.cancelled=true;resolve({success:false,status:'cancelled'});};
      const deadline = state.deadline;
      const waitForButton = () => {
        if (conversationChanged()) {resolve({success: false, status: 'conversation-changed'});return;}
        const button = locateButton();
        if (copySelectionAmbiguous) {resolve({success:false,status:'ambiguous-copy-control'});return;}
        // A Copy still reading 'Copied' from an earlier capture may ignore this click; wait for it to reset.
        if (button && button.getAttribute('aria-label') !== 'Copied') {
          const interception = interceptClipboard();
          let settled = false;
          let pollId = null;
          let timeoutId = null;
          const finish = (payload) => {
            if (settled) {
              return;
            }
            settled = true;
            if (pollId) {
              clearInterval(pollId);
            }
            if (timeoutId) {
              clearTimeout(timeoutId);
            }
            button.removeEventListener('copy', handleCopy, true);
            interception.restore?.();
            resolve(payload);
          };

          state.cancel=()=>{state.cancelled=true;finish({success:false,status:'cancelled'});}; const readIntercepted = () => {
            const markdown = interception.state.text ?? '';
            const updatedAt = interception.state.updatedAt ?? 0;
            return { success: Boolean(markdown.trim()), markdown, updatedAt,
              fileCard: Boolean(((() => { const root = button.closest("[data-turn-key], article[data-testid^=\"conversation-turn\"], div[data-testid^=\"conversation-turn\"], section[data-testid^=\"conversation-turn\"], article:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), div:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), section:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), article[data-turn], div[data-turn], section[data-turn]"); const selector = ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]'; const messages = Array.from(root.querySelectorAll(selector)).filter(node => { const owner = node.parentElement?.closest(selector); return !owner || owner === root; }); return messages.at(-1) ?? root.querySelector(selector) ?? (root.matches?.(selector) ? root : null); })())?.querySelector('button[aria-label="Download file"]')) };
          };

          let lastText = '';
          let stableTicks = 0;
          const requiredStableTicks = 3;
          const requiredStableMs = 250;
          const maybeFinish = () => {
            if (conversationChanged() || locateButton() !== button) {finish({success: false, status: 'conversation-changed'});return;}
            const payload = readIntercepted();
            if (!payload.success) return;
            if (payload.markdown !== lastText) {
              lastText = payload.markdown;
              stableTicks = 0;
              return;
            }
            stableTicks += 1;
            const ageMs = Date.now() - (payload.updatedAt || 0);
            if (stableTicks >= requiredStableTicks && ageMs >= requiredStableMs) {
              finish(payload);
            }
          };

          const handleCopy = () => {
            maybeFinish();
          };

          button.addEventListener('copy', handleCopy, true);
          button.scrollIntoView({ block: 'center', behavior: 'instant' });
          dispatchClickSequence(button, () => !conversationChanged() && locateButton() === button);
          pollId = setInterval(maybeFinish, 120);
          timeoutId = setTimeout(() => {
            button.removeEventListener('copy', handleCopy, true);
            finish({ success: false, status: 'timeout' });
          }, Math.max(1, deadline-Date.now()));
          return;
        }
        if (Date.now() > deadline) {
          resolve({ success: false, status: 'missing-button' });
          return;
        }
        waitTimer=setTimeout(waitForButton, Math.min(120,Math.max(1,deadline-Date.now())));
      };

      waitForButton();
    });
})()).finally(()=>{state.cancelled=true;state.settled=true;});
}
