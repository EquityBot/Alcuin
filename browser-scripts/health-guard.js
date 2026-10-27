// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* alcuin-dom:health-guard */

return (() => { const throttle=(() => {
    const PHRASES = ["too many requests","making requests too quickly","temporarily limited access to your conversations"];
    const matches = (value) => {
      const text = (value || '').toLowerCase();
      return PHRASES.some((phrase) => text.includes(phrase));
    };
    // Dialogs first, because that is what this notice is; the body sweep is a
    // fallback for a variant that does not carry the role.
    const dialogs = Array.from(document.querySelectorAll('[role="dialog"], [role="alertdialog"]'));
    for (const dialog of dialogs) {
      const text = (dialog.innerText || '').trim();
      if (matches(text)) return { message: text.slice(0, 400) };
    }
    const body = (document.body && document.body.innerText) || '';
    if (matches(body)) {
      const line = body
        .split('\n')
        .map((entry) => entry.trim())
        .find((entry) => matches(entry));
      return { message: (line || 'ChatGPT is rate limiting this session.').slice(0, 400) };
    }
    return null;
  })(); const models=(() => {
    const PHRASES = ["could not load chatgpt models","couldn't load chatgpt models","could not load models","couldn't load models"];
    const BUTTON_SELECTOR = 'button[aria-label="Select ChatGPT model"], button[data-codex-intelligence-trigger="true"], [data-testid="model-switcher-dropdown-button"], button.__composer-pill[aria-haspopup="menu"]';
    const matches = (value) => {
      const text = (value || '').toLowerCase();
      return PHRASES.some((phrase) => text.includes(phrase));
    };
    const trimmed = (value) => (value || '').replace(/\s+/g, ' ').trim().slice(0, 200);
    // The model button first: that is where the failure was observed. It carries
    // aria-hidden measurement text, so innerText is preferred for the message.
    const button = document.querySelector(BUTTON_SELECTOR);
    if (button && matches(button.textContent)) {
      return { message: trimmed(button.innerText || button.textContent), where: 'model-button' };
    }
    // Then the picker's own open menu. The conversation is never searched: a
    // chat about this very failure would otherwise read as the failure itself.
    const menus = Array.from(document.querySelectorAll('[role="menu"], [role="dialog"], [role="listbox"], [data-radix-popper-content-wrapper]'));
    for (const menu of menus) {
      const text = menu.innerText || '';
      if (matches(text)) return { message: trimmed(text), where: 'menu' };
    }
    return null;
  })(); return {throttled:Boolean(throttle),modelsLoadFailed:Boolean(models),throttleMessage:throttle?.message ?? null,modelsMessage:models?.message ?? null,modelsWhere:models?.where ?? null}; })();
}
