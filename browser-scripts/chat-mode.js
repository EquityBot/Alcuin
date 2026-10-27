// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* alcuin-dom:chat-mode */

return (() => {
    const normalize = (value) => String(value || '').toLowerCase().replace(/\s+/g, ' ').trim();
    const isVisible = (node) => {
      if (!(node instanceof HTMLElement)) return false;
      const rect = node.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return false;
      const style = window.getComputedStyle(node);
      return style.display !== 'none' && style.visibility !== 'hidden' && style.opacity !== '0';
    };
    const isSelected = (node) =>
      node?.getAttribute?.('aria-pressed') === 'true' ||
      node?.getAttribute?.('aria-checked') === 'true' ||
      node?.getAttribute?.('data-state') === 'on';
    const conversationIdFromPath = (value) => {
      const match = String(value || '').match(/(?:^|\/)c\/([^/]+)(?:\/|$)/);
      return match?.[1] || null;
    };
    const isStructuredWorkBadge = (node) =>
      node instanceof HTMLElement &&
      node.tagName === 'SPAN' &&
      normalize(node.textContent) === 'work' &&
      node.childElementCount === 0 &&
      !node.hasAttribute('dir') &&
      node.classList.contains('shrink-0') &&
      node.parentElement?.matches('span.flex.items-center');
    // September 2026 rows keep the title in [data-thread-title], so any other leaf reading
    // "Work" in the row is a mode label, whatever its classes.
    const hasWorkLabelOutsideTitle = (link) => {
      const title = link.querySelector?.('[data-thread-title]');
      if (!title) return false;
      return Array.from(link.querySelectorAll('*')).some(
        (node) =>
          node.childElementCount === 0 && normalize(node.textContent) === 'work' && !title.contains(node),
      );
    };
    // Reference ensurePromptReady precedes the mode probe: wait for an enabled composer.
    const inputSelectors = ["form[data-chatgpt-composer] [contenteditable=\"true\"][role=\"textbox\"]","textarea[data-id=\"prompt-textarea\"]","textarea[placeholder*=\"Send a message\"]","textarea[aria-label=\"Chat with ChatGPT\"]","textarea[aria-label=\"Message ChatGPT\"]","textarea:not([disabled]):not(#pending-home-input)","textarea[name=\"prompt-textarea\"]","#prompt-textarea",".ProseMirror","[contenteditable=\"true\"][role=\"textbox\"]","[contenteditable=\"true\"][data-virtualkeyboard=\"true\"]"];
    const promptReady = inputSelectors.some((selector) =>
      Array.from(document.querySelectorAll(selector)).some((node) => !node.hasAttribute('disabled') && !node.matches('#pending-home-input')));
    if (!promptReady) return { status: 'prompt-pending' };

    const pathname = typeof location?.pathname === 'string' ? location.pathname : '';
    const conversationId = conversationIdFromPath(pathname);
    const expectedUrl = cfg.expectedConversationUrl ?? null;
    if (expectedUrl) {
      try {
        const expected = new URL(expectedUrl);
        const expectedId = conversationIdFromPath(expected.pathname);
        if (!expectedId || expected.origin !== location.origin || expectedId !== conversationId) {
          return { status: 'conversation-unresolved', reason: 'conversation-identity-mismatch' };
        }
      } catch {
        return { status: 'conversation-unresolved', reason: 'conversation-identity-mismatch' };
      }
    }
    if (conversationId) {
      // Conversation messages can contain same-origin links to the current thread. Only sidebar
      // history rows count; messages never render inside the sidebar nav.
      const activeHistoryLinks = Array.from(
        document.querySelectorAll('a.__menu-item[href*="/c/"], nav a[href*="/c/"]'),
      ).filter((node) => {
        try {
          const candidateUrl = new URL(node.getAttribute('href') || '', location.origin);
          return candidateUrl.origin === location.origin && conversationIdFromPath(candidateUrl.pathname) === conversationId;
        } catch {
          return false;
        }
      });
      if (activeHistoryLinks.length > 0) {
        const hasWorkBadge = activeHistoryLinks.some(
          (link) =>
            Array.from(link.querySelectorAll('span')).some(isStructuredWorkBadge) ||
            hasWorkLabelOutsideTitle(link),
        );
        if (hasWorkBadge) return { status: 'work-conversation' };

        const ariaLabels = activeHistoryLinks
          .map((link) => normalize(link.getAttribute('aria-label')))
          .filter(Boolean);
        if ((ariaLabels.length === 0 && !activeHistoryLinks.some(link => link.querySelector('[data-thread-title]'))) || ariaLabels.some((aria) => /,\s*work\s*$/.test(aria))) {
          return { status: 'conversation-unresolved' };
        }
        return { status: 'chat-conversation' };
      }
      return { status: 'conversation-unresolved', reason: 'history-row-absent' };
    }

    const modeGroup = document.querySelector('[role="group"][aria-label="Composer mode"]');
    const modeButtons = modeGroup ? Array.from(modeGroup.querySelectorAll('button')).filter(isVisible) : [];
    const selectedMode = modeButtons.find(isSelected);
    if (normalize(selectedMode?.textContent) === 'chat') return { status: 'chat-selected' };
    if (normalize(selectedMode?.textContent) === 'work') {
      const chatButton = modeButtons.find(node => normalize(node.textContent) === 'chat');
      if (chatButton) {
        const rect = chatButton.getBoundingClientRect();
        return { status: 'work-selected', chatPoint: { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 } };
      }
    }
    const radios = Array.from(document.querySelectorAll('button[role="radio"]')).filter(isVisible);
    const chat = radios.find((node) => normalize(node.textContent) === 'chat');
    const work = radios.find((node) => normalize(node.textContent) === 'work');
    if (!chat && !work) return { status: 'controls-absent' };
    if (!chat || !work) return { status: 'mode-unresolved' };
    if (isSelected(chat)) return { status: 'chat-selected' };
    if (isSelected(work)) {
      const rect = chat.getBoundingClientRect();
      return {
        status: 'work-selected',
        chatPoint: { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 },
      };
    }
    return { status: 'mode-unresolved' };
  })();
}
