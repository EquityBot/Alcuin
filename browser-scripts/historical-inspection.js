// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* industrious-dom:historical-inspection */
const userMessageIds = node => [node?.getAttribute('data-message-id'), node?.getAttribute('data-chatgpt-selection-message-id'),
    ...(node?.getAttribute('data-chatgpt-search-message-ids')?.trim().split(/\s+/) || [])].filter(Boolean);
  const userMessageHost = (user, turn) => userMessageIds(user).length ||
    !turn?.contains(user?.parentElement) || !userMessageIds(user?.parentElement).length ? user : user.parentElement;
  const userMessageId = (user, turn) => {
    const host = userMessageHost(user, turn);
    return userMessageIds(host)[0] || user?.closest('[data-turn-key]')?.getAttribute('data-turn-key') ||
      user?.getAttribute('data-content-search-unit-key') || user?.getAttribute('data-chatgpt-search-unit-key');
  };
  const readUserOriginalSource = (user, turn, id) => {
    const unavailable = { status: 'unavailable' };
    const ambiguous = { status: 'ambiguous' };
    const selector = ':is([data-message-author-role="user"], [data-content-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"])';
    if (!user?.isConnected || !turn?.contains(user) || !user.matches(selector) || user.querySelector(selector)) return ambiguous;
    const messageHost = userMessageHost(user, turn);
    const ids = [...userMessageIds(user), ...userMessageIds(messageHost),
      ...(turn.contains(user.parentElement) ? userMessageIds(user.parentElement) : [])];
    if (ids.some(value => value !== id)) return ambiguous;
    const own = (object, key) => {
      if (!object || typeof object !== 'object' || Array.isArray(object)) return undefined;
      const descriptor = Object.getOwnPropertyDescriptor(object, key);
      if (descriptor && !('value' in descriptor)) throw Error('Accessor message props');
      return descriptor?.value;
    };
    const child = (object, index) => {
      const children = own(object, 'children');
      if (children === undefined) return undefined;
      if (!Array.isArray(children)) throw Error('Unexpected children shape');
      const descriptor = Object.getOwnPropertyDescriptor(children, String(index));
      if (descriptor && !('value' in descriptor)) throw Error('Accessor child');
      return descriptor?.value;
    };
    try {
      // A nested search-unit can sit immediately inside the message-ID host.
      const host = messageHost.parentElement;
      if (!host || !turn.contains(host)) return unavailable;
      if (userMessageIds(host).some(value => value !== id)) return ambiguous;
      const keys = Object.getOwnPropertyNames(host).filter(key => key.startsWith('__reactProps$'));
      if (keys.length > 1) return ambiguous;
      if (!keys.length) return unavailable;
      const descriptor = Object.getOwnPropertyDescriptor(host, keys[0]);
      if (!descriptor || !('value' in descriptor)) return ambiguous;
      const item = own(own(child(own(child(descriptor.value, 2), 'props'), 1), 'props'), 'item');
      if (item === undefined) return unavailable;
      const text = own(item, 'message');
      if (own(item, 'type') !== 'user-message' || own(item, 'messageId') !== id || typeof text !== 'string') return ambiguous;
      return { status: 'present', text: text.normalize('NFC').replace(/\r\n?/g, '\n') };
    } catch { return ambiguous; }
  };

const readTurns = () => (() => {
    const root = document;
    const containers = Array.from(root.querySelectorAll("[data-turn-key], [data-testid^=\"conversation-turn\"], [data-content-search-unit-key], [data-chatgpt-search-unit-key]")).filter(
      node => !node.parentElement?.closest("[data-turn-key], [data-testid^=\"conversation-turn\"], [data-content-search-unit-key], [data-chatgpt-search-unit-key]"),
    );
    return containers.length > 0
      ? containers.filter((node) => !containers.some((other) => other !== node && other.contains?.(node)))
      : Array.from(root.querySelectorAll("[data-turn-key], article[data-testid^=\"conversation-turn\"], div[data-testid^=\"conversation-turn\"], section[data-testid^=\"conversation-turn\"], article:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), div:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), section:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), article[data-turn], div[data-turn], section[data-turn]"));
  })();
const lastAssistant = turn => (() => { const root = turn; const selector = ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]'; const messages = Array.from(root.querySelectorAll(selector)).filter(node => { const owner = node.parentElement?.closest(selector); return !owner || owner === root; }); return messages.at(-1) ?? root.querySelector(selector) ?? (root.matches?.(selector) ? root : null); })();
const hasTurnCompletion = (meta, minTurnIndex = -1, allowPageStatus = false) => {
    const EXPECTED_MESSAGE_ID = typeof meta?.messageId === 'string' ? meta.messageId : null;
    const EXPECTED_TURN_ID = typeof meta?.turnId === 'string' ? meta.turnId : null;
    const MIN_TURN_INDEX = Number.isFinite(minTurnIndex) && minTurnIndex >= 0 ? Math.floor(minTurnIndex) : -1;
    // Find the LAST assistant turn to check completion status. Must match the same logic as
    // buildAssistantExtractor, then correlate the controls to the sampled response.
    const ASSISTANT_SELECTOR = ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]';
    const isAssistantTurn = (node) => {
      if (!(node instanceof HTMLElement)) return false;
      const searchKey = (node.getAttribute('data-content-search-unit-key') || node.getAttribute('data-chatgpt-search-unit-key') || '').toLowerCase();
      if (searchKey.endsWith(':user')) return false;
      if (searchKey.endsWith(':assistant')) return true;
      const turnAttr = (node.getAttribute('data-turn') || node.dataset?.turn || '').toLowerCase();
      if (turnAttr === 'assistant') return true;
      const role = (node.getAttribute('data-message-author-role') || (node.getAttribute?.('data-content-search-unit-key') || node.getAttribute?.('data-chatgpt-search-unit-key'))?.split(':').at(-1) || node.dataset?.messageAuthorRole || '').toLowerCase();
      if (role === 'assistant') return true;
      const testId = (node.getAttribute('data-testid') || '').toLowerCase();
      if (testId.includes('assistant')) return true;
      return Boolean(node.querySelector(ASSISTANT_SELECTOR) || node.querySelector('[data-testid*="assistant"]'));
    };
    const findTurnAction = (turn, inTurnSelector, besideSelector) => {
    const assistant = (() => { const root = turn; const selector = ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]'; const messages = Array.from(root.querySelectorAll(selector)).filter(node => { const owner = node.parentElement?.closest(selector); return !owner || owner === root; }); return messages.at(-1) ?? root.querySelector(selector) ?? (root.matches?.(selector) ? root : null); })();
    const safe = control => !control.closest('[data-user-message-bubble], [data-content-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"], [data-message-author-role="user"], pre, code');
    const inside = Array.from((assistant || turn).querySelectorAll(inTurnSelector)).find(safe);
    if (inside) return inside;
    const exchange = assistant?.closest?.('[data-content-search-turn-key], [data-turn-key]') || turn;
    if (!exchange) return null;
    return Array.from(exchange.querySelectorAll(besideSelector)).find(
      (control) =>
        safe(control) && (control.closest('[data-content-search-unit-key], [data-chatgpt-search-unit-key]') === assistant ||
          (!control.closest('[data-content-search-unit-key], [data-chatgpt-search-unit-key]') && assistant && Boolean(assistant.compareDocumentPosition(control) & 4))),
    ) ?? null;
  };

    const turns = (() => {
    const root = document;
    const containers = Array.from(root.querySelectorAll("[data-turn-key], [data-testid^=\"conversation-turn\"], [data-content-search-unit-key], [data-chatgpt-search-unit-key]")).filter(
      node => !node.parentElement?.closest("[data-turn-key], [data-testid^=\"conversation-turn\"], [data-content-search-unit-key], [data-chatgpt-search-unit-key]"),
    );
    return containers.length > 0
      ? containers.filter((node) => !containers.some((other) => other !== node && other.contains?.(node)))
      : Array.from(root.querySelectorAll("[data-turn-key], article[data-testid^=\"conversation-turn\"], div[data-testid^=\"conversation-turn\"], section[data-testid^=\"conversation-turn\"], article:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), div:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), section:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), article[data-turn], div[data-turn], section[data-turn]"));
  })();
    let lastAssistantTurn = null;
    let lastAssistantIndex = -1;
    for (let i = turns.length - 1; i >= 0; i--) {
      if (isAssistantTurn(turns[i])) {
        lastAssistantTurn = turns[i];
        lastAssistantIndex = i;
        break;
      }
    }
    if (!lastAssistantTurn) return false;

    const hasExpectedIdentity = Boolean(EXPECTED_MESSAGE_ID || EXPECTED_TURN_ID);
    if (hasExpectedIdentity) {
      const identityNodes = [
        lastAssistantTurn,
        ...Array.from(lastAssistantTurn.querySelectorAll('[data-message-id], [data-testid], [data-content-search-unit-key], [data-chatgpt-search-unit-key], [data-turn-key]')),
      ];
      const identityMatches = identityNodes.some((node) =>
        (EXPECTED_MESSAGE_ID && (node.getAttribute?.('data-message-id') || node.getAttribute?.('data-chatgpt-selection-message-id') || node.getAttribute?.('data-chatgpt-search-message-ids')?.split(' ')[0] || node.getAttribute?.('data-content-search-unit-key') || node.getAttribute?.('data-chatgpt-search-unit-key')) === EXPECTED_MESSAGE_ID) ||
        (EXPECTED_TURN_ID && (node.getAttribute?.('data-turn-key') || node.getAttribute?.('data-testid')) === EXPECTED_TURN_ID),
      );
      if (!identityMatches) return false;
    } else if (MIN_TURN_INDEX < 0 || lastAssistantIndex < MIN_TURN_INDEX) {
      // Fallback/project snapshots without an identity may use the new-turn baseline, but an
      // uncorrelated persistent action bar from an older turn must never prove completion.
      return false;
    }

    if (allowPageStatus) return true;
    const finished = '.turn-action-controls button[aria-label="Copy"], .turn-action-controls button[aria-label="Copied"], .turn-action-controls button[aria-label="Rate response"], .turn-action-controls button[aria-label="Regenerate response"], button[data-testid="copy-turn-action-button"], button[data-testid="good-response-turn-action-button"], button[data-testid="bad-response-turn-action-button"], button[aria-label="Share"]';
    if (findTurnAction(lastAssistantTurn, finished, finished)) return true;
    const assistantRoot = (() => { const root = lastAssistantTurn; const selector = ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]'; const messages = Array.from(root.querySelectorAll(selector)).filter(node => { const owner = node.parentElement?.closest(selector); return !owner || owner === root; }); return messages.at(-1) ?? root.querySelector(selector) ?? (root.matches?.(selector) ? root : null); })();
    if (assistantRoot?.querySelector('button[aria-label="Download file"]:not(:disabled):not([aria-disabled="true"])') &&
        assistantRoot.querySelector('button[aria-label^="Open preview of "][aria-busy="false"]')) return true;
    const markdowns = lastAssistantTurn.querySelectorAll('.markdown');
    return Array.from(markdowns).some((node) => (node.textContent || '').trim() === 'Done');
  };
const readThinkingActivity = () => {
    const STOP_SELECTOR = '[data-testid="stop-button"], [data-testid="composer-stop-button"], form button[aria-label*="stop" i]:not([aria-label*="dictat" i]):not([aria-label*="voice" i]):not([aria-label*="read" i])';
    const ACTIVE_LABELS = ['thinking','pro thinking','thinking longer for a better answer','reasoning','finalizing answer','finalizing','analyzing','researching','working on it','working','planning','searching the web','searching','reading'];
    const ASSISTANT_SELECTOR = ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]';
    const isVisible = (node) => {
      if (!(node instanceof HTMLElement)) return false;
      const rect = node.getBoundingClientRect();
      if (!rect || rect.width <= 0 || rect.height <= 0) return false;
      const style = window.getComputedStyle(node);
      if (
        style.display === 'none' ||
        style.visibility === 'hidden' ||
        (style.opacity !== '' && Number(style.opacity) === 0)
      ) {
        return false;
      }
      return true;
    };
    const some = (selector, pred) => {
      let nodes;
      try { nodes = document.querySelectorAll(selector); } catch { return false; }
      return Array.from(nodes).some((node) => pred(node));
    };
    // 1) Stop/interrupt control visible -> generation is active (language-independent).
    if (some(STOP_SELECTOR, isVisible)) return {active:true,strong:true};
    // 2-3) Shimmer/aria-busy are checked only inside the current turn or verified thinking
    // panel below. Page-global busy UI (history/sidebar/upload) is not model activity.
    const hasBusyIndicator = (scope) => {
      let nodes;
      try {
        nodes = scope.querySelectorAll(
          'span.loading-shimmer, .loading-shimmer, [class*="loading-shimmer"], [aria-busy="true"]',
        );
      } catch { return false; }
      return Array.from(nodes).some((node) => isVisible(node));
    };
    // 4) Active (present-tense) thinking status label near a status/reasoning node.
    const norm = (value) =>
      String(value || '')
        .normalize('NFD')
        .replace(/[\u0300-\u036f]/g, '')
        .toLowerCase()
        .replace(/\s+/g, ' ')
        .trim();
    // Completed reasoning summary: the whole visible label must be a duration summary. Anchoring
    // prevents an early live trace such as "Thought for 2s: Searching the web" from being
    // mistaken for completion before the trace grows beyond an arbitrary length threshold.
    const isCompletedSummary = (text) =>
      /^(?:(?:reasoning|pro thinking)\s*)?thought for (?:\d+(?:\.\d+)?\s*(?:s|sec|secs|second|seconds|m|min|mins|minute|minutes|h|hr|hrs|hour|hours)(?:\s+\d+(?:\.\d+)?\s*(?:s|sec|secs|second|seconds|m|min|mins|minute|minutes|h|hr|hrs|hour|hours))*|(?:a|an) [a-z]+(?: [a-z]+){0,2})(?: edit)?$/.test(text);
    const isActiveLabel = (raw) => {
      const text = norm(raw);
      if (!text || text.length > 60) return false;
      if (isCompletedSummary(text)) return false;
      return ACTIVE_LABELS.some((label) => text === label || text.startsWith(label + ' '));
    };
    const statusNodes = (() => {
      try {
        return Array.from(
          document.querySelectorAll(
            '[data-testid*="thinking"], [data-testid*="reasoning"]',
          ),
        );
      } catch {
        return [];
      }
    })();
    for (const node of statusNodes) {
      if (!(node instanceof HTMLElement) || !isVisible(node)) continue;
      const testId = norm(node.getAttribute('data-testid'));
      const verifiedThinkingChrome = testId.includes('thinking') || testId.includes('reasoning');
      const matches =
        verifiedThinkingChrome &&
        (isActiveLabel(node.textContent) || isActiveLabel(node.getAttribute('aria-label')));
      if (matches) return {active:true,strong:true};
    }
    // 5) A live progress bar (determinate or indeterminate) is active generation, even when no
    //    label or shimmer is present (some connector/tool phases surface only a progress bar).
    const hasLiveProgress = (scope) => {
      let nodes;
      // Only genuine progress indicators — NOT generic [aria-valuenow] range widgets (sliders,
      // spinbuttons), which are not liveness signals and would falsely veto completion forever.
      try {
        nodes = scope.querySelectorAll('progress, [role="progressbar"]');
      } catch { return false; }
      return Array.from(nodes).some((n) => {
        if (!(n instanceof HTMLElement) || !isVisible(n)) return false;
        if (n instanceof HTMLProgressElement) {
          // Determinate <progress> is active only while it has not reached its max.
          if (Number.isFinite(n.value) && Number.isFinite(n.max) && n.max > 0) return n.value < n.max;
          return true; // indeterminate
        }
        const rawNow = n.getAttribute('aria-valuenow');
        if (rawNow != null) {
          const now = Number(rawNow);
          const rawMax = n.getAttribute('aria-valuemax');
          const max = rawMax != null && Number.isFinite(Number(rawMax)) ? Number(rawMax) : 100;
          return Number.isFinite(now) ? now < max : true;
        }
        return true; // role=progressbar with no value -> indeterminate -> active
      });
    };
    // Scoped to the CURRENT assistant turn, not the whole document: unrelated page UI can keep
    // a visible progress bar mounted indefinitely (review P1), and a document-wide veto would
    // then hold thinkingActive true until the watchdog timeout on a completed response. The
    // sidecar check below covers verified reasoning panels; here only the latest turn counts.
    const turns = (() => {
    const root = document;
    const containers = Array.from(root.querySelectorAll("[data-turn-key], [data-testid^=\"conversation-turn\"], [data-content-search-unit-key], [data-chatgpt-search-unit-key]")).filter(
      node => !node.parentElement?.closest("[data-turn-key], [data-testid^=\"conversation-turn\"], [data-content-search-unit-key], [data-chatgpt-search-unit-key]"),
    );
    return containers.length > 0
      ? containers.filter((node) => !containers.some((other) => other !== node && other.contains?.(node)))
      : Array.from(root.querySelectorAll("[data-turn-key], article[data-testid^=\"conversation-turn\"], div[data-testid^=\"conversation-turn\"], section[data-testid^=\"conversation-turn\"], article:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), div:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), section:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), article[data-turn], div[data-turn], section[data-turn]"));
  })();
    const lastTurn = turns.length ? turns[turns.length - 1] : null;
    if (
      lastTurn instanceof HTMLElement &&
      (hasBusyIndicator(lastTurn) || hasLiveProgress(lastTurn))
    ) return {active:true,strong:true};
    // 6) A visible thinking/reasoning sidecar panel (the connector/reasoning phase is often
    //    exposed ONLY through a right-side panel with no inline label). Match the existing
    //    thinking-monitor heuristic: a right-side panel that looks like thinking, or any such
    //    container that carries a live progress bar. Presence alone is NOT enough (a collapsed
    //    reasoning summary persists post-completion); require the thinking cue or live progress.
    const looksLikeThinking = (node) => {
      // Judge completion on the panel's VISIBLE text only: data-testid values like
      // "reasoning-panel" would otherwise taint the completed-summary check, and a live trace
      // is judged by its full rendered length, not by fragments of it.
      const visible = norm(node.textContent) || norm(node.getAttribute?.('aria-label'));
      if (isCompletedSummary(visible)) return false;
      if (visible.includes('thought for ')) return true;
      const label = norm([
        node.textContent,
        node.getAttribute?.('aria-label'),
        node.getAttribute?.('data-testid'),
      ].filter(Boolean).join(' '));
      return label.includes('thinking') || label.includes('reasoning') || label.includes('pro thinking');
    };
    let panels;
    try {
      panels = document.querySelectorAll(
        'aside, [role="complementary"], [role="dialog"], [data-testid*="thinking"], [data-testid*="reasoning"], [class*="sidecar"], [class*="sidebar"]',
      );
    } catch { panels = []; }
    for (const node of Array.from(panels)) {
      if (!(node instanceof HTMLElement) || !isVisible(node)) continue;
      const rect = node.getBoundingClientRect();
      const rightSide = rect.left >= window.innerWidth * 0.35 && rect.width >= 180 && rect.height >= 120;
      const panelLabel = norm([
        node.getAttribute?.('aria-label'),
        node.getAttribute?.('data-testid'),
        node.className,
      ].filter(Boolean).join(' '));
      const verifiedThinkingPanel =
        panelLabel.includes('thinking') ||
        panelLabel.includes('reasoning') ||
        panelLabel.includes('sidecar');
      if ((hasBusyIndicator(node) || hasLiveProgress(node)) && verifiedThinkingPanel) return {active:true,strong:true};
      // A text-only sidecar match is intentionally weak: completed turns can retain a mounted
      // reasoning panel whose shape/text heuristics still look active. The terminal gate may
      // override only this weak evidence after a stable, debounced finished-action bar.
      if (rightSide && looksLikeThinking(node)) return {active:true,strong:false};
    }
    return {active:false,strong:false};
  };

return (() => {
      const expectedConversationId = cfg.expectedConversationId ?? null;
      const expectedOrigin = cfg.expectedOrigin ?? null;
      const currentUrl = expectedOrigin || expectedConversationId ? new URL(location.href) : null;
      if ((expectedOrigin && currentUrl.origin !== expectedOrigin) || (expectedConversationId && currentUrl.pathname.match(/(?:^|\/)c\/([a-zA-Z0-9-]+)/)?.[1] !== expectedConversationId)) return { identityChanged: true };
      const assistantSnapshot = (() => {
    const MIN_TURN_INDEX = -1;
    const EXPECTED_CONVERSATION_ID = cfg.expectedConversationId ?? null;
    const currentHref = typeof location === 'object' && location.href ? location.href : '';
    const currentConversationId = currentHref.match(/\/c\/([a-zA-Z0-9-]+)/)?.[1] ?? null;
    if (
      EXPECTED_CONVERSATION_ID &&
      currentConversationId &&
      currentConversationId !== EXPECTED_CONVERSATION_ID
    ) {
      return null;
    }
    // Learned: the default turn DOM misses project view; keep a fallback extractor.
    const extractAssistantTurn = () => {
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
    const readRetryThinkingActivity=readThinkingActivity;
    const isRetryAssistantUiError = function isRetryAssistantUiErrorText(value, retryVisible) {
  if (!retryVisible || typeof value !== "string") return false;
  const text = value.toLowerCase().replace(/\s+/g, " ").trim();
  return (
    text.includes("something went wrong") ||
    text.includes("failed to generate") ||
    text.includes("try again later")
  );
};
    const ASSISTANT_SELECTOR = ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]';
    const isAssistantTurn = (node) => {
      if (!(node instanceof HTMLElement)) return false;
      const searchKey = (node.getAttribute('data-content-search-unit-key') || node.getAttribute('data-chatgpt-search-unit-key') || '').toLowerCase();
      if (searchKey.endsWith(':user')) return false;
      if (searchKey.endsWith(':assistant')) return true;
      const turnAttr = (node.getAttribute('data-turn') || node.dataset?.turn || '').toLowerCase();
      if (turnAttr === 'assistant') {
        return true;
      }
      const role = (node.getAttribute('data-message-author-role') || (node.getAttribute?.('data-content-search-unit-key') || node.getAttribute?.('data-chatgpt-search-unit-key'))?.split(':').at(-1) || node.dataset?.messageAuthorRole || '').toLowerCase();
      if (role === 'assistant') {
        return true;
      }
      const testId = (node.getAttribute('data-testid') || '').toLowerCase();
      if (testId.includes('assistant')) {
        return true;
      }
      return Boolean(node.querySelector(ASSISTANT_SELECTOR) || node.querySelector('[data-testid*="assistant"]'));
    };

    const expandCollapsibles = (root) => {
      const buttons = Array.from(root.querySelectorAll('button'));
      for (const button of buttons) {
        const label = (button.textContent || '').toLowerCase();
        const testid = (button.getAttribute('data-testid') || '').toLowerCase();
        if (
          label.includes('more') ||
          label.includes('expand') ||
          label.includes('show') ||
          testid.includes('markdown') ||
          testid.includes('toggle')
        ) {
          dispatchClickSequence(button);
        }
      }
    };

    const turns = readTurns();
    for (let index = turns.length - 1; index >= 0; index -= 1) {
      const turn = turns[index];
      if (!isAssistantTurn(turn)) {
        continue;
      }
      const messageRoot = lastAssistant(turn) ?? turn;
      expandCollapsibles(messageRoot);
      const preferred =
        (messageRoot.matches?.('.markdown') || messageRoot.matches?.('[data-message-content]') ? messageRoot : null) ||
        Array.from(messageRoot.querySelectorAll('[data-markdown-text-style="assistant-message"]'))
          .filter(node => node.getAttribute('data-markdown-text-tone') !== 'tertiary').at(-1) ||
        messageRoot.querySelector('.markdown') ||
        messageRoot.querySelector('[data-message-content]') ||
        messageRoot.querySelector('[data-testid*="message"]') ||
        messageRoot.querySelector('[data-testid*="assistant"]') ||
        messageRoot.querySelector('.prose') ||
        messageRoot.querySelector('[class*="MarkdownRoot"]') ||
        messageRoot.querySelector('[class*="markdown"]');
      const contentRoot = preferred ?? messageRoot;
      if (!contentRoot) {
        continue;
      }
      const innerText = contentRoot?.innerText ?? '';
      const textContent = contentRoot?.textContent ?? '';
      const fileNames = Array.from(messageRoot.querySelectorAll('button[aria-label^="Open preview of "][aria-busy="false"]'))
        .map(node => node.getAttribute('aria-label').slice('Open preview of '.length));
      const text = innerText.trim().length > 0 ? innerText : textContent.trim().length > 0 ? textContent : fileNames.join('\n');
      const html = fileNames.length > 0 ? messageRoot.innerHTML : contentRoot?.innerHTML ?? '';
      const messageId = messageRoot.getAttribute('data-message-id') || messageRoot.getAttribute?.('data-chatgpt-selection-message-id') || messageRoot.getAttribute('data-chatgpt-search-message-ids')?.split(' ')[0] || messageRoot.getAttribute('data-content-search-unit-key') || messageRoot.getAttribute('data-chatgpt-search-unit-key');
      const turnId = turn.getAttribute('data-turn-key') || messageRoot.getAttribute('data-testid');
      const generatedImages = Array.from(messageRoot.querySelectorAll('img')).filter((img) =>
        String(img?.src || '').includes('/backend-api/estuary/content?id=file_')
      );
      const normalizedText = String(text || '').toLowerCase().replace(/\s+/g, ' ').trim();
      const retryVisible = Array.from(turn.querySelectorAll('button, [role="button"]')).some((button) => {
        const label = String(button.innerText || button.textContent || button.getAttribute?.('aria-label') || '')
          .toLowerCase()
          .replace(/\s+/g, ' ')
          .trim();
        if (label !== 'retry' || !(button instanceof HTMLElement)) return false;
        const rect = button.getBoundingClientRect();
        const style = window.getComputedStyle(button);
        return rect.width > 0 && rect.height > 0 && style.display !== 'none' && style.visibility !== 'hidden' &&
          !(style.opacity !== '' && Number(style.opacity) === 0);
      });
      if (isRetryAssistantUiError(text, retryVisible) && !readRetryThinkingActivity().strong) {
        return { text, html, messageId, turnId, turnIndex: index, uiError: 'temporary_unavailable' };
      }
      const imageOnlyChrome =
        !normalizedText ||
        normalizedText === 'edit' ||
        normalizedText === 'stopped thinking' ||
        normalizedText === 'stopped thinking edit' ||
        /^(?:reasoning\s+|pro thinking\s+)?thought for \d+(?:\.\d+)?\s*(?:s|sec|secs|second|seconds|m|min|mins|minute|minutes|h|hr|hrs|hour|hours)\s+edit$/.test(normalizedText);
      if (generatedImages.length > 0 && imageOnlyChrome) {
        const label = generatedImages.length === 1 ? 'Generated image.' : ('Generated ' + generatedImages.length + ' images.');
        return { text: label, html: messageRoot?.innerHTML ?? html, messageId, turnId, turnIndex: index };
      }
      if (text.trim()) {
        return { text, html, messageId, turnId, turnIndex: index };
      }
    }
    return null;
  };
    const extracted = extractAssistantTurn();
    const isPlaceholder=function isAnswerNowPlaceholderText(value) {
  let raw = "";
  if (typeof value === "string") {
    raw = value;
  } else if (value && typeof value === "object" && "text" in value) {
    const candidate = value.text;
    if (typeof candidate === "string") raw = candidate;
  }
  const text = raw.toLowerCase().replace(/\s+/g, " ").trim();
  if (!text) return false;
  if (text === "chatgpt said:" || text === "chatgpt said") return true;
  if (text.length > 60) return false;
  // Whole-string match: consume the text left to right, longest chrome label first.
  // Any residue means real content is present, so this is an answer, not a placeholder.
  const chromeLabels = [
    "chatgpt said:",
    "chatgpt said",
    "file upload request",
    "pro thinking",
    "answer now",
  ];
  let rest = text;
  let sawOwner = false; // "pro thinking" / "chatgpt said"
  let sawGate = false; // "answer now" / "file upload request"
  while (rest.length > 0) {
    let matched = "";
    for (const label of chromeLabels) {
      if (label.length > matched.length && rest.startsWith(label)) matched = label;
    }
    if (!matched) return false;
    if (matched === "answer now" || matched === "file upload request") sawGate = true;
    else sawOwner = true;
    rest = rest.slice(matched.length).replace(/^[\s:.,;|\u00b7\u2022-]+/, "");
  }
  return sawGate && sawOwner;
};
    const isStopControlVisible = () => {
    const isVisible = (node) => {
      if (!(node instanceof HTMLElement)) return false;
      const rect = node.getBoundingClientRect();
      if (!rect || rect.width <= 0 || rect.height <= 0) return false;
      const style = window.getComputedStyle(node);
      return !(
        style.display === 'none' ||
        style.visibility === 'hidden' ||
        (style.opacity !== '' && Number(style.opacity) === 0)
      );
    };
    return Array.from(document.querySelectorAll(([
  '[data-testid="stop-button"]',
  '[data-testid="composer-stop-button"]',
  'form button[aria-label*="stop" i]:not([aria-label*="dictat" i]):not([aria-label*="voice" i]):not([aria-label*="read" i])',
]).join(','))).some((node) => isVisible(node));
  };
  const isActiveThinkingStatus = (snapshot) => {
    const normalized = String(snapshot?.text ?? '').toLowerCase().replace(/\s+/g, ' ').trim();
    if (!normalized) return false;
    const labels =  [
  "thinking",
  "pro thinking",
  "thinking longer for a better answer",
  "reasoning",
  "finalizing answer",
  "finalizing",
  "analyzing",
  "researching",
  "working on it",
  "working",
  "planning",
  "searching the web",
  "searching",
  "reading",
];
    const matches =
      labels.includes(normalized) ||
      (normalized.includes('thought for ') && normalized.length <= 40) ||
      (normalized.startsWith('pro thinking') && normalized.length <= 40);
    return matches && isStopControlVisible();
  };
    if (
      extracted &&
      extracted.text &&
      !isPlaceholder(extracted) &&
      !isActiveThinkingStatus(extracted)
    ) {
      return extracted;
    }
    // Fallback for ChatGPT project view: answers can live outside conversation turns.
    const extractFallback = (() => {
    const __minTurn = null;
    const roots = [
      document.querySelector('section[data-testid="screen-threadFlyOut"]'),
      document.querySelector('[data-testid="chat-thread"]'),
      document.querySelector('main'),
      document.querySelector('[role="main"]'),
    ].filter(Boolean);
    if (roots.length === 0) return null;
    const markdownSelector = '.markdown,[data-message-content],[data-testid*="message"],.prose,[class*="MarkdownRoot"],[class*="markdown"]';
    const isExcluded = (node) =>
      Boolean(
        node?.closest?.(
          'nav, aside, [data-testid*="sidebar"], [data-testid*="chat-history"], [data-testid*="composer"], form',
        ),
      );
    const scoreRoot = (node) => {
      const actions = node.querySelectorAll('.turn-action-controls button[aria-label="Copy"], .turn-action-controls button[aria-label="Copied"], .turn-action-controls button[aria-label="Rate response"], .turn-action-controls button[aria-label="Regenerate response"], button[data-testid="copy-turn-action-button"], button[data-testid="good-response-turn-action-button"], button[data-testid="bad-response-turn-action-button"], button[aria-label="Share"]').length;
      const assistants = node.querySelectorAll(':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]').length;
      const markdowns = node.querySelectorAll(markdownSelector).length;
      return actions * 10 + assistants * 5 + markdowns;
    };
    let root = roots[0];
    let bestScore = scoreRoot(root);
    for (let i = 1; i < roots.length; i += 1) {
      const candidate = roots[i];
      const score = scoreRoot(candidate);
      if (score > bestScore) {
        bestScore = score;
        root = candidate;
      }
    }
    if (!root) return null;
    const turnNodes = readTurns();
    const hasTurns = turnNodes.length > 0;
    const resolveTurnIndex = (node) => {
      // Current ChatGPT DOM can return nested outer/inner turn containers.
      // The last containing node is the specific turn; the first is a wrapper
      // that also contains the user and falsely predates the submit baseline.
      for (let idx = turnNodes.length - 1; idx >= 0; idx -= 1) {
        const turn = turnNodes[idx];
        if (turn === node || turn.contains?.(node)) return idx;
      }
      return null;
    };
    const normalize = (value) => String(value || '').toLowerCase().replace(/\s+/g, ' ').trim();
    const collectLastUser = (scope) => {
      if (!scope?.querySelectorAll) return null;
      const userTurns = Array.from(scope.querySelectorAll(':is([data-message-author-role="user"], [data-content-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"]), [data-turn="user"]'));
      return userTurns[userTurns.length - 1] ?? null;
    };
    const lastUser = collectLastUser(root) || collectLastUser(document);
    const userText = lastUser ? normalize(lastUser.innerText || lastUser.textContent || '') : '';
    const isAfterCurrentUser = (node) => {
      if (!lastUser || typeof lastUser.compareDocumentPosition !== 'function') return false;
      // Node.DOCUMENT_POSITION_FOLLOWING = 4. Use the numeric bit so the injected expression
      // also works in stripped browser test contexts without a global Node constructor.
      return Boolean(lastUser.compareDocumentPosition(node) & 4);
    };
    const isAfterMinTurn = (node) => {
      if (__minTurn === null) return true;
      if (!hasTurns) return isAfterCurrentUser(node);
      const idx = resolveTurnIndex(node);
      return idx !== null && idx >= __minTurn;
    };
    const isUserEcho = (text) => {
      if (!userText) return false;
      const normalized = normalize(text);
      if (!normalized) return false;
      return normalized === userText || normalized.startsWith(userText);
    };
    const markdowns = Array.from(root.querySelectorAll(markdownSelector))
      .filter((node) => !isExcluded(node))
      .filter((node) => {
        const container = node.closest(':is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), [data-turn]');
        if (!container) return true;
        const role =
          (container.getAttribute('data-message-author-role') || (container.getAttribute?.('data-content-search-unit-key') || container.getAttribute?.('data-chatgpt-search-unit-key'))?.split(':').at(-1) || container.getAttribute('data-turn') || '').toLowerCase();
        return role !== 'user';
      });
    if (markdowns.length === 0) return null;
    const actionButtons = Array.from(root.querySelectorAll('.turn-action-controls button[aria-label="Copy"], .turn-action-controls button[aria-label="Copied"], .turn-action-controls button[aria-label="Rate response"], .turn-action-controls button[aria-label="Regenerate response"], button[data-testid="copy-turn-action-button"], button[data-testid="good-response-turn-action-button"], button[data-testid="bad-response-turn-action-button"], button[aria-label="Share"]'));
    // ChatGPT's current Pro layout can omit the per-turn action bar. Its live
    // completion announcement is positive evidence once the sampled answer is
    // after the current user turn; an empty/working announcement is not.
    const actionMarkdowns = [];
    for (const button of actionButtons) {
      const container =
        button.closest("[data-turn-key], " +
  'article[data-testid^="conversation-turn"], div[data-testid^="conversation-turn"], section[data-testid^="conversation-turn"], ' +
  "article:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), div:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), section:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), " +
  "article[data-turn], div[data-turn], section[data-turn]") ||
        button.closest(':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]') ||
        button.closest(':is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), [data-turn]') ||
        button.closest('[data-testid*="assistant"]');
      if (!container || container === root || container === document.body) continue;
      const scoped = Array.from(container.querySelectorAll(markdownSelector))
        .filter((node) => !isExcluded(node))
        .filter((node) => {
          const roleNode = node.closest(':is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), [data-turn]');
          if (!roleNode) return true;
          const role =
            (roleNode.getAttribute('data-message-author-role') || (roleNode.getAttribute?.('data-content-search-unit-key') || roleNode.getAttribute?.('data-chatgpt-search-unit-key'))?.split(':').at(-1) || roleNode.getAttribute('data-turn') || '').toLowerCase();
          return role !== 'user';
        });
      if (scoped.length === 0) continue;
      for (const node of scoped) {
        actionMarkdowns.push(node);
      }
    }
    const assistantMarkdowns = markdowns.filter((node) => {
      const container = node.closest(':is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), [data-turn], [data-testid*="assistant"]');
      if (!container) return false;
      const role =
        (container.getAttribute('data-message-author-role') || (container.getAttribute?.('data-content-search-unit-key') || container.getAttribute?.('data-chatgpt-search-unit-key'))?.split(':').at(-1) || container.getAttribute('data-turn') || '').toLowerCase();
      if (role === 'assistant') return true;
      const testId = (container.getAttribute('data-testid') || '').toLowerCase();
      return testId.includes('assistant');
    });
    const hasAssistantIndicators = Boolean(
      root.querySelector('.turn-action-controls button[aria-label="Copy"], .turn-action-controls button[aria-label="Copied"], .turn-action-controls button[aria-label="Rate response"], .turn-action-controls button[aria-label="Regenerate response"], button[data-testid="copy-turn-action-button"], button[data-testid="good-response-turn-action-button"], button[data-testid="bad-response-turn-action-button"], button[aria-label="Share"]') ||
        root.querySelector(':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"], [data-testid*="assistant"]'),
    );
    const allowMarkdownFallback = hasAssistantIndicators || hasTurns || Boolean(userText);
    const rawCandidates =
      actionMarkdowns.length > 0
        ? actionMarkdowns
        : assistantMarkdowns.length > 0
          ? assistantMarkdowns
          : allowMarkdownFallback
            ? markdowns
            : [];
    // Inline citation/code spans also have "markdown" in their class name.
    // Prefer the complete message root, otherwise the last inline span can be
    // mistaken for the full assistant answer (including during artifact saves).
    const blockCandidates = rawCandidates.filter((node) =>
      node.matches?.('.markdown,.prose,[data-message-content],[class*="MarkdownRoot"]'),
    );
    const candidates = blockCandidates.length > 0 ? blockCandidates : rawCandidates;
    for (let i = candidates.length - 1; i >= 0; i -= 1) {
      const node = candidates[i];
      if (!node) continue;
      if (!isAfterMinTurn(node)) continue;
      const text = (node.innerText || node.textContent || '').trim();
      if (!text) continue;
      if (isUserEcho(text)) continue;
      const html = node.innerHTML ?? '';
      const turnIndex = resolveTurnIndex(node);
      return {
        text,
        html,
        messageId: null,
        turnId: null,
        turnIndex,
        completionVisible: actionMarkdowns.includes(node),
      };
    }
    return null;
  });
    const fallback = extractFallback();
    if (fallback && !isPlaceholder(fallback) && !isActiveThinkingStatus(fallback)) {
      return fallback;
    }
    return null;
  })();
      const hasInspectedTurnCompletion=hasTurnCompletion;
      const readInspectedThinkingActivity=readThinkingActivity;
      // As in live completion (result-prelude.js), finished-turn controls can show before ChatGPT
      // renders the streamed text; a text block still holding a raw text node is not complete.
      const lastAnswer = Array.from(document.querySelectorAll(':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"])')).at(-1);
      const unrenderedAnswer = Boolean(lastAnswer && Array.from(lastAnswer.querySelectorAll('[data-d-text-flow=""]')).some((node) =>
        Array.from(node.childNodes).some((child) => child.nodeType === 3 && child.textContent.trim())));
      const completionVisible = Boolean(assistantSnapshot && !unrenderedAnswer &&
        (assistantSnapshot.completionVisible === true || hasInspectedTurnCompletion(assistantSnapshot, assistantSnapshot.turnIndex)));
      const strongThinkingActive = readInspectedThinkingActivity().strong;
      const INPUT_SELECTORS = [
  'form[data-chatgpt-composer] [contenteditable="true"][role="textbox"]',
  'textarea[data-id="prompt-textarea"]',
  'textarea[placeholder*="Send a message"]',
  'textarea[aria-label="Chat with ChatGPT"]',
  'textarea[aria-label="Message ChatGPT"]',
  "textarea:not([disabled]):not(#pending-home-input)",
  'textarea[name="prompt-textarea"]',
  "#prompt-textarea",
  ".ProseMirror",
  '[contenteditable="true"][role="textbox"]',
  '[contenteditable="true"][data-virtualkeyboard="true"]',
];
      const SEND_SELECTORS = [
  'button[data-testid="send-button"]',
  'button[data-testid*="composer-send"]',
  'form button[type="submit"]',
  'button[type="submit"][data-testid*="send"]',
  'button[aria-label*="Send"]',
];
      const ANSWER_SELECTORS = [
  'article[data-testid^="conversation-turn"]:is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"])',
  'article[data-testid^="conversation-turn"][data-turn="assistant"]',
  'article[data-testid^="conversation-turn"] :is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"])',
  'article[data-testid^="conversation-turn"] [data-turn="assistant"]',
  'article[data-testid^="conversation-turn"] .markdown',
  ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]) .markdown',
  '[data-turn="assistant"] .markdown',
  ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"])',
  '[data-turn="assistant"]',
];
      const ASSISTANT_ROLE_SELECTOR = ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]';
      const MODEL_BUTTON_SELECTOR = 'button[aria-label="Select ChatGPT model"], ' +
  'button[data-codex-intelligence-trigger="true"], ' +
  '[data-testid="model-switcher-dropdown-button"], button.__composer-pill[aria-haspopup="menu"]';
      const STOP_BUTTON_SELECTOR = ([
  '[data-testid="stop-button"]',
  '[data-testid="composer-stop-button"]',
  'form button[aria-label*="stop" i]:not([aria-label*="dictat" i]):not([aria-label*="voice" i]):not([aria-label*="read" i])',
]).join(',');
      const LOGIN_CTA = /\b(log in|login|sign up|sign in|continue with google|continue with microsoft)\b/i;
      const normalize = (value) => String(value ?? '').replace(/\s+/g, ' ').trim();
      const isVisible = (node) => {
        if (!(node instanceof Element)) return false;
        const style = window.getComputedStyle(node);
        if (!style) return false;
        if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0') return false;
        const rect = node.getBoundingClientRect();
        return rect.width > 0 && rect.height > 0;
      };
      const firstVisible = (selectors) => {
        for (const selector of selectors) {
          const node = document.querySelector(selector);
          if (node && isVisible(node)) return node;
        }
        return null;
      };
      const loginButtonExists = Array.from(document.querySelectorAll('button,a,[role="button"]')).some((node) => {
        const label = normalize(node.textContent || node.getAttribute('aria-label') || node.getAttribute('title'));
        return LOGIN_CTA.test(label);
      });
      const stopExists = Array.from(document.querySelectorAll(STOP_BUTTON_SELECTOR)).some(isVisible);
      const sendButton = firstVisible(SEND_SELECTORS);
      const sendExists = Boolean(sendButton);
      const promptNode = firstVisible(INPUT_SELECTORS);
      const promptReady = Boolean(promptNode);
      const turns = readTurns();
      const turnRole = (turn) => {
        const explicit = normalize(turn.getAttribute('data-message-author-role') || turn.getAttribute('data-turn')).toLowerCase();
        if (explicit === 'user' || explicit === 'assistant') return explicit;
        const key = turn.getAttribute('data-content-search-unit-key') || turn.getAttribute('data-chatgpt-search-unit-key') || '';
        return key.match(/:(user|assistant)$/)?.[1] || '';
      };
      const assistantTurns = turns.filter((turn) => {
        const role = turnRole(turn);
        if (role === 'assistant') return true;
        return Boolean(turn.querySelector(ASSISTANT_ROLE_SELECTOR));
      });
      const fallbackUserTurns = Array.from(
        document.querySelectorAll(':is([data-message-author-role="user"], [data-content-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"]), [data-turn="user"]'),
      );
      const userTurns = turns.filter((turn) => {
        const role = turnRole(turn);
        if (role === 'user') return true;
        return Boolean(
          turn.querySelector(':is([data-message-author-role="user"], [data-content-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"]), [data-turn="user"]'),
        );
      });
      const answerNode = ANSWER_SELECTORS
        .map((selector) => document.querySelectorAll(selector))
        .find((matches) => matches && matches.length > 0);
      const currentModelButton = document.querySelector(MODEL_BUTTON_SELECTOR);
      const hasProPill = Boolean(document.querySelector('button.__composer-pill, button[aria-label="Pro, click to remove"]'));
      let currentModelLabel = normalize(currentModelButton?.textContent || currentModelButton?.getAttribute?.('aria-label') || '');
      if (currentModelLabel === 'ChatGPT' && hasProPill) {
        currentModelLabel = 'ChatGPT + Pro';
      }
      const rawAnswerNodes = Array.from(answerNode || []);
      const userCandidates = Array.from(new Set([...userTurns, ...fallbackUserTurns]));
      const lastUserTurn = userCandidates.reduce((latest, candidate) => {
        if (!latest) return candidate;
        return latest.compareDocumentPosition(candidate) & 4 ? candidate : latest;
      }, null);
      const lastUserContainer = lastUserTurn
        ? turns.find((turn) => turn === lastUserTurn || turn.contains?.(lastUserTurn))
        : null;
      const answerNodes = rawAnswerNodes.filter(
        (node) =>
          !lastUserTurn ||
          (node !== lastUserTurn &&
            !lastUserTurn.contains?.(node) &&
            !node.contains?.(lastUserTurn)),
      );
      const answerTexts = answerNodes.map((node) => normalize(node.textContent)).filter(Boolean);
      const assistantCandidates = Array.from(new Set([...assistantTurns, ...answerNodes]));
      const lastAssistantNode = assistantCandidates.reduce((latest, candidate) => {
        if (!latest) return candidate;
        return latest.compareDocumentPosition(candidate) & 4 ? candidate : latest;
      }, null);
      const lastAssistantContainer = lastAssistantNode
        ? turns.find((turn) => turn === lastAssistantNode || turn.contains?.(lastAssistantNode))
        : null;
      const assistantFollowsLatestUser = Boolean(
        lastAssistantNode &&
        lastUserTurn &&
        lastAssistantNode !== lastUserTurn &&
        (lastUserTurn.compareDocumentPosition(lastAssistantNode) & 4),
      );
      const lastAssistantTurnIndex = lastAssistantContainer
        ? turns.indexOf(lastAssistantContainer)
        : -1;
      const lastUserTurnIndex = lastUserContainer ? turns.indexOf(lastUserContainer) : -1;
      const assistantOwners = assistantCandidates.map(
        (node) => turns.find((turn) => turn === node || turn.contains?.(node)) || node,
      );
      const assistantCount = new Set(assistantOwners).size;
      const assistantContent =
        Array.from(lastAssistantNode?.querySelectorAll?.('[data-markdown-text-style="assistant-message"]') || [])
          .filter(node => node.getAttribute('data-markdown-text-tone') !== 'tertiary').at(-1) ||
        lastAssistantNode?.querySelector?.('.markdown') || lastAssistantNode;
      const lastAssistantText = normalize(assistantContent?.textContent);
      const lastUserText = normalize(lastUserTurn?.textContent);
      const lastUserMessage = lastUserTurn?.matches?.(':is([data-message-author-role="user"], [data-content-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"])')
        ? lastUserTurn : lastUserTurn?.querySelector?.(':is([data-message-author-role="user"], [data-content-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"])');
      const userContent = lastUserMessage?.querySelectorAll?.('[data-user-message-bubble="true"], [class~="whitespace-pre-wrap"]');

      const sourceMessageId = userMessageId(lastUserMessage, lastUserContainer);
      const original = readUserOriginalSource(lastUserMessage, lastUserContainer, sourceMessageId);
      const authenticated = !loginButtonExists && (promptReady || sendExists || stopExists || assistantCount > 0);
      return {
        title: normalize(document.title),
        url: location.href,
        currentModelLabel,
        stopExists,
        sendExists,
        promptReady,
        loginButtonExists,
        authenticated,
        assistantCount,
        assistantSnapshot,
        completionVisible,
        strongThinkingActive,
        lastAssistantText,
        assistantFollowsLatestUser,
        lastAssistantTurnIndex,
        lastUserTurnIndex,
        lastUserText,
        lastUserTextRaw: (lastUserMessage?.querySelector?.('[data-user-message-bubble="true"]') || lastUserMessage)?.textContent,
        lastUserContentText: userContent?.length === 1 ? userContent[0].textContent : undefined,
        lastUserOriginalSourceText: original.status === 'present' ? original.text : undefined,
        lastUserOriginalSourceMessageId: original.status === 'present' ? sourceMessageId : undefined,
        lastUserMessageId: lastUserMessage?.getAttribute?.('data-message-id') || lastUserMessage?.getAttribute?.('data-chatgpt-selection-message-id') || lastUserMessage?.getAttribute?.('data-chatgpt-search-message-ids')?.split(' ')[0] || lastUserMessage?.closest?.('[data-turn-key]')?.getAttribute?.('data-turn-key') || lastUserMessage?.getAttribute?.('data-content-search-unit-key') || lastUserMessage?.getAttribute?.('data-chatgpt-search-unit-key'),
        visibilityState: document.visibilityState,
        focused: Boolean(document.hasFocus?.()),
      };
    })();
}
