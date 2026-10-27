// Derived from maintained Web Search and composer controls (MIT).
(() => {
        const button = window.__oracleAttachmentPlusNodes?.[cfg.plusGuardId];
        const check = () => {
          assertOracleSubmission();
          const navigation = (() => {
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
        modeUnverified: false,
      };
    })();
    const expected = cfg.navigation;
    let contextMatches = false;
    try {
      const current = new URL(snapshot.currentUrl);
      const id = current.pathname.match(/(?:^|\/)c\/([^/]+)/)?.[1] ?? null;
      contextMatches = Boolean(expected) && !snapshot.workSelected && !snapshot.modeUnverified &&
        current.origin.toLowerCase() === expected.origin &&
        (expected.conversationId !== null ? id === expected.conversationId :
          id === null && (current.pathname.replace(/\/+$/, '') || '/') === expected.landingPath);
    } catch {}
    return { ...snapshot, contextMatches };
  })();
          const binding = searchBinding();
          const rect = button?.getBoundingClientRect();
          return { ...navigation, focused: Boolean(binding && binding.plus === button) && button instanceof HTMLElement && button.isConnected &&
            document.activeElement === button && binding.form.querySelectorAll(([
  "#composer-plus-btn",
  'button[data-testid="composer-plus-btn"]',
  'form button[aria-label="Add files and more"]',
]).join(', ')).length === 1 && binding.form.querySelector(([
  "#composer-plus-btn",
  'button[data-testid="composer-plus-btn"]',
  'form button[aria-label="Add files and more"]',
]).join(', ')) === button &&
            !button.hasAttribute('disabled') && button.getAttribute('aria-disabled') !== 'true' &&
            rect.width > 0 && rect.height > 0 };
        };
        const safeCheck = () => { try { return check(); } catch { return { currentUrl: location.href, contextMatches: false, focused: false }; } };
        const guard = { sawKeyDown: false, clicked: false, blocked: null };
        const cancel = (event, state) => { event.preventDefault(); event.stopImmediatePropagation(); guard.blocked = state; };
        const onKey = event => {
          if (event.key !== 'Enter' || !event.isTrusted) return;
          if (event.type === 'keyup') { if (guard.blocked || !oracleSubmissionIsActive()) cancel(event, guard.blocked ?? { focused: false }); return; }
          guard.sawKeyDown = true;
          const state = safeCheck();
          if (guard.blocked || !state.contextMatches || !state.focused) cancel(event, guard.blocked ?? state);
        };
        const onClick = event => {
          if (!guard.sawKeyDown && true) return;
          const state = safeCheck();
          if (guard.blocked || !state.contextMatches || !state.focused || !(event.target instanceof Node) || !button.contains(event.target)) cancel(event, guard.blocked ?? state);
          else guard.clicked = true;
        };
        guard.cleanup = () => {
          window.removeEventListener('keydown', onKey, true);
          window.removeEventListener('keyup', onKey, true);
          window.removeEventListener('click', onClick, true);
        };
        const guards = window.__oracleAttachmentPlusGuards ??= Object.create(null);
        guards[cfg.plusGuardId] = guard;
        const state = safeCheck();
        if (!state.contextMatches || !state.focused) { guard.blocked = state; return state; }
        window.addEventListener('keydown', onKey, true);
        window.addEventListener('keyup', onKey, true);
        window.addEventListener('click', onClick, true);
        if (!true) button.click();
        return state;
      })()
