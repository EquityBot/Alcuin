// Derived from maintained Deep Research status expressions (MIT).
(() => {
    const MIN_TURN_INDEX = cfg.minTurnIndex;
    const stopVisible = Boolean(document.querySelector('[data-testid="stop-button"]'));
    const scopedToNewTurns = MIN_TURN_INDEX >= 0;
    const pageText = String(document.body?.innerText || '').toLowerCase().replace(/\s+/g, ' ');
    const accountBlocked = pageText.includes('suspicious activity detected') &&
      pageText.includes('secure your account') &&
      pageText.includes('regain access');
    const isAssistantTurn = (node) => {
      const attr = String(node.getAttribute('data-message-author-role') || (node.getAttribute?.('data-content-search-unit-key') || node.getAttribute?.('data-chatgpt-search-unit-key'))?.split(':').at(-1) || node.getAttribute('data-turn') || node.dataset?.turn || '').toLowerCase();
      return attr === 'assistant' ||
        Boolean(node.querySelector(':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]')) ||
        String(node.getAttribute('data-testid') || '').toLowerCase().includes('conversation-turn') &&
          /chatgpt\s+said/i.test(node.innerText || node.textContent || '');
    };
    const conversationTurns = readTurns();
    const allAssistantTurns = Array.from(document.querySelectorAll(':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"]), [data-turn="assistant"]'));
    const scopedTurns = scopedToNewTurns
      ? conversationTurns.slice(MIN_TURN_INDEX).filter(isAssistantTurn)
      : allAssistantTurns;
    const lastTurn = scopedTurns[scopedTurns.length - 1] || (scopedToNewTurns ? null : allAssistantTurns[allAssistantTurns.length - 1]);
    const text = (lastTurn?.textContent || '').trim();
    const normalized = text.toLowerCase().replace(/\s+/g, ' ').trim();
    const textLength = text.length;
    const lines = text.split(/\n+/).map(line => line.trim()).filter(Boolean);
    const tailIsPlanningPanel = text.length <= 1500 &&
      lines.length >= 4 &&
      lines.length <= 20 &&
      /^update$/i.test(lines[1] || '') &&
      /^stop research$/i.test(lines[lines.length - 1] || '') &&
      /^determining steps for creating a report(?:\.\.\.)?$/i.test(lines[lines.length - 2] || '');
    const isToolStub = normalized === 'called tool' ||
      normalized === 'used tool' ||
      normalized === 'użyto narzędzia' ||
      normalized === 'narzędzie wywołane';
    const incompleteResult = isToolStub ||
      normalized === 'planning' ||
      normalized === 'researching' ||
      normalized === 'searching the web' ||
      (text.trimStart().startsWith('<system-reminder>') &&
        /<system-reminder>[\s\S]*#\s*plan mode\b/i.test(text)) ||
      tailIsPlanningPanel;
    const finished = Boolean(lastTurn?.querySelector('.turn-action-controls button[aria-label="Copy"], .turn-action-controls button[aria-label="Copied"], .turn-action-controls button[aria-label="Rate response"], .turn-action-controls button[aria-label="Regenerate response"], button[data-testid="copy-turn-action-button"], button[data-testid="good-response-turn-action-button"], button[data-testid="bad-response-turn-action-button"], button[aria-label="Share"]')) &&
      textLength >= 40 &&
      !incompleteResult;
    const hasIframe = Array.from(document.querySelectorAll('iframe')).some(f => {
      const rect = f.getBoundingClientRect();
      return rect.width > 200 && rect.height > 200;
    });
    const hasScopedDeepResearchIframe = Array.from(lastTurn?.querySelectorAll?.('iframe') || []).some(f => {
      const rect = f.getBoundingClientRect();
      const descriptor = String(f.getAttribute('src') || '') + ' ' + String(f.getAttribute('name') || '');
      return rect.width > 200 && rect.height > 200 &&
        /connector_openai_deep_research|deep-research/i.test(descriptor);
    });
    const hasActiveScopedResearch = scopedToNewTurns && Boolean(lastTurn) &&
      hasScopedDeepResearchIframe &&
      (textLength < 40 || isToolStub || tailIsPlanningPanel || /chatgpt\s+said:?$/i.test(text));
    return { finished, stopVisible, textLength, hasIframe, isToolStub, incompleteResult, researchActivity: tailIsPlanningPanel || (isToolStub && hasScopedDeepResearchIframe), hasActiveScopedResearch, accountBlocked };
  })()
