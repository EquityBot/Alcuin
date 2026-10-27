// Native exact-message copy-control adaptation; included by extract_browser.rs.
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
