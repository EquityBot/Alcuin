(owner, ids, expectedPrompt, webSearch = false) => {
  const previousIds = ids === null ? undefined : ids;
  return (() => {
    {
    if (window !== window.top) throw new Error('Industrious submission requires the top renderer.');

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

    const claim = readOracleTargetClaim();
    if (!matchesOracleSubmission(claim, owner) ||
        !(previousIds === undefined ? ['active'] : ['active', 'releasing', 'quarantined']).includes(claim.submission.state))
      throw new Error('Industrious submission reservation is absent or inactive.');
  }
    const navigation = (() => {
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
    const turns = (() => {
    const root = document;
    const containers = Array.from(root.querySelectorAll("[data-turn-key], [data-testid^=\"conversation-turn\"], [data-content-search-unit-key], [data-chatgpt-search-unit-key]")).filter(
      node => !node.parentElement?.closest("[data-turn-key], [data-testid^=\"conversation-turn\"], [data-content-search-unit-key], [data-chatgpt-search-unit-key]"),
    );
    return containers.length > 0
      ? containers.filter((node) => !containers.some((other) => other !== node && other.contains?.(node)))
      : Array.from(root.querySelectorAll("[data-turn-key], article[data-testid^=\"conversation-turn\"], div[data-testid^=\"conversation-turn\"], section[data-testid^=\"conversation-turn\"], article:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), div:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), section:is([data-message-author-role], [data-content-search-unit-key], [data-chatgpt-search-unit-key]), article[data-turn], div[data-turn], section[data-turn]"));
  })();
    const selector = ':is([data-message-author-role="user"], [data-content-search-unit-key$=":user"], [data-chatgpt-search-unit-key$=":user"])';
    const canonical = value => String(value ?? '').normalize('NFC').replace(/\r\n?/g, '\n');
    const markdownSourceText = value => {
    const punctuation = /[!-\/:-@\[-\x60{-~]/;
    const unescape = text => text.replace(/\\([!-\/:-@\[-\x60{-~])/g, '$1');
    const link = /\[(https?:(?:\\\S|[^\\\[\]\s])*)\]\((https?:(?:\\\S|[^\\()\[\]\s])*)\)/y;
    const autolinkEnd = index => {
      while (index < value.length) {
        if (/[\s)\]}"']/.test(value[index])) return true;
        if (value[index] === '\\' && punctuation.test(value[index + 1] ?? '')) index++;
        else if (/[.,;:!?]/.test(value[index])) index++;
        else return false;
      }
      return true;
    };
    const completeLiteralLink = text => /(^|[^!])\[[^\[\]\r\n]+\]\([^()\s]+\)/.test(text);
    const hasHost = text => { try { return /^https?:\/\//.test(text) && Boolean(new URL(text).host); } catch { return false; } };
    let text = '';
    let evidence = 0;
    let escapedLinkOpener = false;
    for (let index = 0; index < value.length;) {
      link.lastIndex = index;
      const match = value[index] === '[' ? link.exec(value) : null;
      if (match && unescape(match[1]) === unescape(match[2]) &&
          (index === 0 || /[\s([{\"']/.test(value[index - 1])) &&
          autolinkEnd(link.lastIndex) && hasHost(unescape(match[2]))) {
        text += unescape(match[2]);
        evidence++;
        index = link.lastIndex;
      } else if (value[index] === '\\' && punctuation.test(value[index + 1] ?? '')) {
        if (value[index + 1] === '(' && value[index - 1] === ']') escapedLinkOpener = true;
        text += value[index + 1];
        index += 2;
      } else if (/[\x60*<]/.test(value[index]) || (value[index] === ']' && value[index + 1] === '(')) {
        return null;
      } else text += value[index++];
    }
    return evidence || (escapedLinkOpener && completeLiteralLink(text)) ? text : null;
  };
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
    const expected = (previousIds === undefined ? null : canonical(expectedPrompt));
    const previous = new Set((previousIds ?? []));
    const stableId = value => typeof value === 'string' && value.length > 0 && value.length <= 256;
    const turnIds = turns.map(turn => turn.getAttribute('data-turn-key') || turn.getAttribute('data-testid') ||
      turn.getAttribute('data-content-search-unit-key') || turn.getAttribute('data-chatgpt-search-unit-key') || turn.getAttribute('data-message-id'));
    let invalid = turnIds.some(id => !stableId(id)) || new Set(turnIds).size !== turnIds.length;
    const messages = new Map();
    const texts = new Map();
    turns.forEach((turn, index) => {
      const users = [...(turn.matches(selector) ? [turn] : []), ...turn.querySelectorAll(selector)]
        .filter(user => !user.querySelector(selector));
      for (const user of users) {
        const searchIds = user.getAttribute('data-chatgpt-search-message-ids')?.trim().split(/\s+/);
        if (searchIds && searchIds.length !== 1) invalid = true;
        const id = userMessageId(user, turn);
        if (!stableId(id)) { invalid = true; continue; }
        const contentNodes = Array.from(user.querySelectorAll('[class~="whitespace-pre-wrap"]'))
          .filter(node => !node.parentElement?.closest('[class~="whitespace-pre-wrap"]'));
        if (contentNodes.length > 1) invalid = true;
        const original = expected !== null && !previous.has(id)
          ? readUserOriginalSource(user, turn, id) : { status: 'unavailable' };
        if (original.status === 'ambiguous') invalid = true;
        const text = original.status === 'present' ? canonical(original.text) : undefined;
        const old = messages.get(id);
        if (old && (old.turnId !== turnIds[index] || texts.get(id) !== text)) invalid = true;
        texts.set(id, text);
        const matches = expected !== null && !previous.has(id) && text !== undefined &&
          (text === expected || markdownSourceText(text) === expected ||
            webSearch === true && (text === expected + '@Web search' || text === '@Web search ' + expected));
        messages.set(id, { id, turnId: turnIds[index], index,
          ...(expected === null || previous.has(id) ? {} : { matches,
            ...(matches ? { fingerprintText: text } : {}) }) });
      }
    });
    return { ...navigation, invalid, turnIds, messages: Array.from(messages.values()), canonicalPrompt: canonical(expectedPrompt) };
  })();
}
