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

    const assistantRoot = (() => { const root = lastAssistantTurn; const selector = ':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]'; const messages = Array.from(root.querySelectorAll(selector)).filter(node => { const owner = node.parentElement?.closest(selector); return !owner || owner === root; }); return messages.at(-1) ?? root.querySelector(selector) ?? (root.matches?.(selector) ? root : null); })();
    // ChatGPT's October 2026 renderer can show the finished-turn controls before it renders the
    // streamed text; a text block still holding a raw text node has not been rendered (observed
    // 27 October 2026, when a capture held only '[Download PUBLIC-L').
    if (assistantRoot && Array.from(assistantRoot.querySelectorAll('[data-d-text-flow=""]')).some((node) =>
      Array.from(node.childNodes).some((child) => child.nodeType === 3 && child.textContent.trim()))) return false;
    if (allowPageStatus) return true;
    const finished = '.turn-action-controls button[aria-label="Copy"], .turn-action-controls button[aria-label="Copied"], .turn-action-controls button[aria-label="Rate response"], .turn-action-controls button[aria-label="Regenerate response"], button[data-testid="copy-turn-action-button"], button[data-testid="good-response-turn-action-button"], button[data-testid="bad-response-turn-action-button"], button[aria-label="Share"]';
    if (findTurnAction(lastAssistantTurn, finished, finished)) return true;
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
