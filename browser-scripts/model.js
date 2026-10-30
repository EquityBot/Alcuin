// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* industrious-dom:model */
const assertOracleSubmission = () => { if (!oracleTemporaryChat()) throw Error('Verified Temporary Chat required'); if (window !== window.top) throw Error('Top renderer required');
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
    if ((cfg.token ?? null) &&
        !window.__oraclePickerInspections?.get((cfg.token ?? null))?.active)
      return { status: 'pending' };
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
    // Capture the selectors and matcher literals up front so the browser expression stays pure.
    const BUTTON_SELECTOR = 'button[aria-label="Select ChatGPT model"], button[data-codex-intelligence-trigger="true"], [data-testid="model-switcher-dropdown-button"], button.__composer-pill[aria-haspopup="menu"]';
    const COMPOSER_MODEL_SIGNAL_SELECTOR = '[data-testid="composer-footer-actions"]';
    const LABEL_TOKENS = cfg.labelTokens;
    const TEST_IDS = cfg.testIdTokens;
    const PRIMARY_LABEL = cfg.model;
    const MODEL_STRATEGY = cfg.strategy;
    const COMPOSER_SIGNAL_INCLUDES = [];
    const COMPOSER_SIGNAL_EXCLUDES = ['thinking','pro'];
    const COMPOSER_SIGNAL_ALLOW_BLANK = true;
    const INITIAL_WAIT_MS = 150;
    const REOPEN_INTERVAL_MS = 400;
    const MAX_WAIT_MS = 20000;
    const SETTLE_WAIT_MS = 1500;
    const normalizeText = (value) => {
      if (!value) {
        return '';
      }
      return value
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, ' ')
        .replace(/\s+/g, ' ')
        .trim();
    };
    const hasToken = (value, token) => normalizeText(value).split(' ').includes(token);
    // Normalize every candidate token to keep fuzzy matching deterministic.
    const normalizedTarget = normalizeText(PRIMARY_LABEL);
    // "Latest" (GPT-6 since 2026-09) is a radio in the advanced view whose composer pill reads
    // "6 Pro" / "6 High"…, while GPT-5.6 Sol's reads "5.6 Pro". Declared up front: getResolvedLabel
    // runs on the picker-less path before the selection helpers below are initialized.
    const targetIsLatest = normalizedTarget === 'latest' || normalizedTarget === 'gpt 6';
    // ChatGPT localizes the Latest radio itself (for example, Japanese "最新") but
    // keeps the GPT-6 composer pill numeric. Keep this allow-list exact so GPT-5.6
    // Sol or an arbitrary localized menu row can never satisfy a Latest request.
    const isLatestModelLabel = (value) => {
      const label = String(value ?? '').normalize('NFC').trim();
      return label === 'GPT-6' || label === 'Latest' || label === '最新' || label === '최신';
    };
    const normalizedTokens = Array.from(new Set([normalizedTarget, ...LABEL_TOKENS]))
      .map((token) => normalizeText(token))
      .filter(Boolean);
    const targetWords = normalizedTarget.split(' ').filter(Boolean);
    const desiredVersion = normalizedTarget.includes('5 6')
      ? '5-6'
      : normalizedTarget.includes('5 5')
        ? '5-5'
        : normalizedTarget.includes('5 4')
          ? '5-4'
        : normalizedTarget.includes('5 3')
          ? '5-3'
        : normalizedTarget.includes('5 2')
        ? '5-2'
        : normalizedTarget.includes('5 1')
          ? '5-1'
          : normalizedTarget.includes('5 0')
            ? '5-0'
          : null;
    const desiredModelVariant = normalizedTarget.includes(' sol') ? 'sol' : null;
    const wantsPro = normalizedTarget.includes(' pro') || normalizedTarget.endsWith(' pro') || normalizedTokens.includes('pro');
    const wantsInstant = normalizedTarget.includes('instant');
    const wantsThinking = normalizedTarget.includes('thinking');
    const targetUsesCurrentGpt55Alias =
      desiredVersion === '5-5' || normalizedTarget === 'pro' || normalizedTarget === 'chatgpt pro';
    const labelHasProWord = (label) => label === 'pro' || label.startsWith('pro ') || label.includes(' pro ') || label.endsWith(' pro');
    const legacyProVersionTokens = ['5 4', '5 2', '5 1', '5 0', 'gpt54', 'gpt52', 'gpt51', 'gpt50', 'gpt 5 pro'];
    const labelHasLegacyProVersion = (value) => {
      const label = normalizeText(value);
      return legacyProVersionTokens.some((token) => label.includes(token));
    };
    const isTargetGpt55VisibleAlias = (value) => {
      if (!targetUsesCurrentGpt55Alias) return false;
      const label = normalizeText(value);
      if (wantsPro) {
        // ChatGPT UI as of 2026-05: the picker shows just "Pro" (no longer "Pro Extended").
        // "Extended" is now a thinking-effort sub-setting, not part of the model label.
        // Accept bare "pro", legacy "pro extended", and reversed "extended pro" (composer pill).
        return (label === 'pro' || label === 'pro extended' || label === 'extended pro') && !label.includes('thinking');
      }
      if (wantsThinking) {
        // ChatGPT UI as of 2026-05: the picker shows "Thinking" or "Thinking · Extended"
        // (normalized to "thinking extended"). Accept both old "thinking heavy" and new labels.
        return (label === 'thinking' || label === 'thinking extended' || label === 'thinking heavy') && !label.includes('pro');
      }
      return false;
    };
    const hasProComposerPill = () => Boolean(
      Array.from(document.querySelectorAll('button.__composer-pill, button[aria-label]'))
        .filter((node) => {
          const label = normalizeText(node.getAttribute?.('aria-label') ?? '');
          return node.matches?.('button.__composer-pill') || label.includes('click to remove');
        })
        .some((node) => {
          const label = normalizeText(
            (node.getAttribute?.('aria-label') ?? '') + ' ' + (node.textContent ?? '')
          );
          return hasToken(label, 'pro') && !hasToken(label, 'thinking');
        })
    );

    const isVisibleElement = (node) => {
      if (typeof HTMLElement === 'undefined') return false;
      if (!(node instanceof HTMLElement)) return false;
      try {
        const rect = node.getBoundingClientRect?.();
        const style = window.getComputedStyle?.(node);
        if (!rect) return true;
        return (
          rect.width > 0 &&
          rect.height > 0 &&
          style?.display !== 'none' &&
          style?.visibility !== 'hidden'
        );
      } catch {
        // DOM test doubles and older Chromium nodes can omit one of these APIs.
        // Treat them as visible; real hidden picker rows still report a zero rect
        // or display:none and are filtered above.
        return true;
      }
    };
    const looksLikeModelPill = (node) => {
      if (!(node instanceof HTMLElement) || !node.matches('button.__composer-pill')) return false;
      if (!isVisibleElement(node)) return false;
      const label = normalizeText(
        (node.textContent ?? '') + ' ' + (node.getAttribute('aria-label') ?? '') + ' ' + (node.getAttribute('title') ?? '')
      );
      if (!label) return false;
      if (label.includes('click to remove')) return false;
      const modelTokens = [
        'chatgpt',
        'gpt',
        'instant',
        'thinking',
        'pro',
        'extended',
        'standard',
        'medium',
        'high',
        'heavy',
        'light',
      ];
      return modelTokens.some((token) => hasToken(label, token));
    };
    const findModelButton = () => {
      const explicit = document.querySelector(BUTTON_SELECTOR);
      if (explicit) return explicit;
      return Array.from(document.querySelectorAll('button.__composer-pill')).find(looksLikeModelPill) ?? null;
    };

    const closeMenu = () => {
      const dialogCloseButton = document.querySelector(
        '[role="dialog"] [data-testid="close-button"]',
      );
      if (dialogCloseButton) {
        try {
          if (dispatchClickSequence(dialogCloseButton)) return;
        } catch {}
      }
      const button = findModelButton();
      if (!button) return;
      try {
        if (dispatchClickSequence(button)) {
          lastPointerClick = performance.now();
          return;
        }
      } catch {}
      try {
        document.dispatchEvent(
          new KeyboardEvent('keydown', {
            key: 'Escape',
            code: 'Escape',
            keyCode: 27,
            which: 27,
            bubbles: true,
          }),
        );
      } catch {}
    };

    // The current composer trigger contains aria-hidden measurement text ("Thinking effort")
    // before its visible label. Reading textContent would turn a visible "6 Pro"/"Pro" into
    // "Thinking effortThinking effortPro" and obscure the actual selection.
    const getButtonLabel = () => {
      const button = findModelButton();
      return (button?.innerText ?? button?.textContent ?? '').trim();
    };
    // With the picker closed the only evidence for "Latest" is the composer pill, so a version-less
    // "latest" target must be decided on it: the blank composer signal would otherwise pass as
    // "already selected" while GPT-5.6 Sol is active. Defined here, before getResolvedLabel, because
    // the "current" strategy resolves the label before the selection helpers further down exist.
    const latestButtonSelected = () => {
      const label = normalizeText(getButtonLabel());
      return /^(chatgpt |gpt )?6(?![0-9 .]*[0-9])/.test(label) && !/(^| )5 6/.test(label);
    };
    const getComposerModelLabel = () =>
      (document.querySelector(COMPOSER_MODEL_SIGNAL_SELECTOR)?.textContent ?? '').trim();
    const readComposerModelSignal = () => normalizeText(getComposerModelLabel());
    const isEffortOnlyLabel = (label) =>
      label === 'medium' ||
      label === 'high' ||
      label === 'extra high' ||
      label === 'extended' ||
      label === 'standard' ||
      label === 'heavy' ||
      label === 'light';
    const formatModelOptionLabel = (label) => {
      const normalized = normalizeText(label ?? '');
      if (normalized === '5 6' || normalized === 'gpt 5 6') return 'GPT-5.6';
      if (normalized === '5 5' || normalized === 'gpt 5 5') return 'GPT-5.5';
      if (normalized === '5 4' || normalized === 'gpt 5 4') return 'GPT-5.4';
      if (normalized === '5 3' || normalized === 'gpt 5 3') return 'GPT-5.3';
      if (normalized === '5 2' || normalized === 'gpt 5 2') return 'GPT-5.2';
      if (normalized === '5 1' || normalized === 'gpt 5 1') return 'GPT-5.1';
      if (normalized === '5 0' || normalized === 'gpt 5 0') return 'GPT-5.0';
      return label || '';
    };
    const withProPillSignal = (label) => {
      const resolved = label || '';
      if (!wantsPro || !hasProComposerPill()) return resolved;
      const normalized = normalizeText(resolved);
      if (!normalized) return resolved;
      if (normalized.includes('thinking')) return 'Pro';
      if (normalized.includes('pro')) return resolved;
      return resolved + ' + Pro';
    };
    const isThinkingEffortLabel = (label) =>
      label === 'extended' ||
      label === 'standard' ||
      label === 'heavy' ||
      label === 'light' ||
      label === 'medium' ||
      label === 'high' ||
      label === 'extra high';
    const isNonProIntelligenceThinkingLabel = (label) =>
      label === 'medium' || label === 'high' || label === 'extra high';
    const scoreNonProGpt55ThinkingLabel = (label) => {
      if (label === 'extra high') return 1400;
      if (label === 'high') return 1200;
      return 1000;
    };
    const versionFromLabel = (label) => {
      const match = normalizeText(label).match(/(?:^| )(?:gpt )?(\d+) (\d+)(?: |$)/);
      return match ? match[1] + '-' + match[2] : null;
    };
    const versionFromTestId = (testid) => {
      const normalized = normalizeText(testid);
      if (normalized.includes('5 6') || normalized.includes('gpt56')) return '5-6';
      if (normalized.includes('5 5') || normalized.includes('gpt55')) return '5-5';
      if (normalized.includes('5 4') || normalized.includes('gpt54')) return '5-4';
      if (normalized.includes('5 3') || normalized.includes('gpt53')) return '5-3';
      if (normalized.includes('5 2') || normalized.includes('gpt52')) return '5-2';
      if (normalized.includes('5 1') || normalized.includes('gpt51')) return '5-1';
      if (normalized.includes('5 0') || normalized.includes('gpt50')) return '5-0';
      return null;
    };
    const getConfigurationDialog = () => document.querySelector('[role="dialog"]');
    const getConfiguredVersionLabel = () =>
      (getConfigurationDialog()
        ?.querySelector?.('[role="combobox"][aria-labelledby="model-selection-label"]')
        ?.textContent ?? '').trim();
    const getConfiguredVariantLabel = () => {
      const label =
        (getConfigurationDialog()
          ?.querySelector?.('[aria-label="Model options"] [role="radio"][aria-checked="true"]')
          ?.textContent ?? '').trim();
      const normalized = normalizeText(label);
      if (normalized.startsWith('instant')) return 'Instant';
      if (normalized.startsWith('thinking')) return 'Thinking';
      if (normalized.startsWith('pro')) return 'Pro';
      return label;
    };
    const configuredSelectionMatchesTarget = () => {
      const configuredVersionLabel = normalizeText(getConfiguredVersionLabel());
      const configuredVersion = versionFromLabel(configuredVersionLabel);
      if (!configuredVersion || configuredVersion !== desiredVersion) return false;
      const configuredVariant = normalizeText(getConfiguredVariantLabel());
      if (
        desiredModelVariant &&
        !configuredVersionLabel.includes(desiredModelVariant) &&
        !configuredVariant.includes(desiredModelVariant)
      ) return false;
      if (wantsPro) return labelHasProWord(configuredVariant);
      if (wantsInstant) return configuredVariant.includes('instant');
      if (wantsThinking) {
        return configuredVariant.includes('thinking') && !labelHasProWord(configuredVariant);
      }
      return true;
    };

    // ---------- Unified Intelligence picker: Advanced -> Model submenu ----------
    // The slider picker exposes only the current effort in the composer pill. Its
    // model versions live behind Advanced -> Model, next to an identically-shaped
    // Effort submenu. Identify the Model opener positively so a Pro effort label can
    // never be mistaken for a model target.
    const ADVANCED_VIEW_SELECTOR = '[data-model-picker-view="advanced"], [data-testid="composer-model-picker-slider-advanced-view"]';
    const INTELLIGENCE_PICKER_SELECTOR = '[data-testid="composer-intelligence-picker-content"]';
    const SUBMENU_OPENER_SELECTOR = '[role="menuitem"][aria-haspopup="menu"]';
    const ADVANCED_WORDS = ['advanced', 'erweitert', '高级', '고급', 'avanzado', 'avancado', 'avance'];
    const MODEL_WORDS = ['model', 'modell', '模型', '모델', 'modelo', 'modello', 'modele'];
    const EFFORT_WORDS = [
      'effort', 'aufwand', '强度', '努力', '추론 수준',
      'esfuerzo', 'esforco', 'sforzo', 'inspanning', 'wysilek',
    ];
    const pickerNodeLabel = (node) => {
      const value =
        (node?.getAttribute?.('aria-label') ?? '') + ' ' + (node?.textContent ?? '');
      try {
        return value
          .toLowerCase()
          .normalize('NFD')
          .replace(/[̀-ͯ]/g, '')
          .normalize('NFC')
          .replace(/\s+/g, ' ')
          .trim();
      } catch {
        return value.toLowerCase().replace(/\s+/g, ' ').trim();
      }
    };
    const containsPickerWord = (label, words) =>
      words.some((word) => label.includes(word));
    const isSubmenuOpener = (node) =>
      node?.getAttribute?.('role') === 'menuitem' &&
      node?.getAttribute?.('aria-haspopup') === 'menu';
    const isUnifiedPickerMenu = (menu) =>
      Boolean(
        menu?.getAttribute?.('data-testid') === 'composer-intelligence-picker-content' ||
          menu?.querySelector?.(INTELLIGENCE_PICKER_SELECTOR) ||
          menu?.querySelector?.(ADVANCED_VIEW_SELECTOR) ||
          (menu?.querySelector?.('[data-model-picker-view]') &&
            menu?.getAttribute?.('aria-labelledby') === findModelButton()?.id),
      );
    const findUnifiedPickerMenu = () =>
      Array.from(document.querySelectorAll('[role="menu"], [data-radix-collection-root], [role="listbox"], [role="dialog"]')).find(isUnifiedPickerMenu) ??
      null;
    const findAdvancedToggle = (menu) => {
      const semanticToggle = menu?.querySelector?.('[data-model-picker-view-toggle="true"]');
      if (semanticToggle && isVisibleElement(semanticToggle)) return semanticToggle;
      for (const item of (menu || document).querySelectorAll('[role="menuitem"]')) {
        if (!isVisibleElement(item)) continue;
        if (containsPickerWord(pickerNodeLabel(item), ADVANCED_WORDS)) return item;
      }
      return null;
    };
    const findModelSubmenuOpener = (menu) => {
      const scope = menu?.querySelector?.(ADVANCED_VIEW_SELECTOR) || menu || document;
      for (const item of scope.querySelectorAll(SUBMENU_OPENER_SELECTOR)) {
        if (!isVisibleElement(item) || !isSubmenuOpener(item)) continue;
        const label = pickerNodeLabel(item);
        if (
          containsPickerWord(label, MODEL_WORDS) &&
          !containsPickerWord(label, EFFORT_WORDS)
        ) {
          return item;
        }
      }
      return null;
    };
    const advancedModelSignalMatchesTarget = (menu = null) => {
      const parentMenu = menu || findUnifiedPickerMenu();
      const opener = findModelSubmenuOpener(parentMenu);
      if (!opener) return false;
      const label = normalizeText(pickerNodeLabel(opener));
      const version = versionFromLabel(label);
      if (desiredVersion && version !== desiredVersion) return false;
      if (desiredModelVariant && !label.split(' ').includes(desiredModelVariant)) return false;
      if (wantsPro) return labelHasProWord(label) && !labelHasLegacyProVersion(label);
      if (wantsInstant) return label.includes('instant');
      if (wantsThinking) return Boolean(desiredVersion) && !labelHasProWord(label);
      if (desiredVersion) return true;
      // A version-less target ("Latest") must match the radio that is actually checked in the
      // advanced view: the opener's text lists every radio label, so a substring test would
      // report "Latest" as selected while GPT-5.6 Sol is the checked model.
      const checkedAdvancedRadio = findCheckedAdvancedModelRadio(parentMenu);
      if (checkedAdvancedRadio) {
        const checkedLabel = checkedAdvancedRadio.textContent ?? '';
        return targetIsLatest
          ? isLatestModelLabel(checkedLabel)
          : normalizedTokens.some((token) => token && normalizeText(checkedLabel) === token);
      }
      return normalizedTokens.some((token) => token && label.includes(token));
    };
    const findCheckedAdvancedModelRadio = (menu = null) => {
      const scope = menu || findUnifiedPickerMenu() || document;
      return (
        scope?.querySelector?.(
          '[data-testid="composer-model-picker-slider-advanced-view"] [role="menuitemradio"][aria-checked="true"]',
        ) ??
        scope?.querySelector?.(
          '[data-model-picker-view] [role="menuitemradio"][aria-checked="true"]',
        ) ??
        null
      );
    };
    const getAdvancedModelLabel = () => {
      const opener = findModelSubmenuOpener(findUnifiedPickerMenu());
      if (!opener) return '';
      const raw = (opener.textContent ?? '').trim();
      const normalized = normalizeText(pickerNodeLabel(opener));
      const version = versionFromLabel(normalized);
      if (!version) {
        const checkedAdvancedRadio = findCheckedAdvancedModelRadio(findUnifiedPickerMenu());
        const checkedLabel = (checkedAdvancedRadio?.textContent ?? '').trim();
        return checkedLabel || raw;
      }
      const [major, minor] = version.split('-');
      const suffix = normalized.split(' ').includes('sol') ? ' Sol' : '';
      return 'GPT-' + major + '.' + minor + suffix;
    };
    const isLegacyFlatIntelligencePickerOpen = () => {
      const menu = findUnifiedPickerMenu();
      return Boolean(
        menu &&
          !menu.querySelector?.(ADVANCED_VIEW_SELECTOR) &&
          !findAdvancedToggle(menu),
      );
    };
    const getResolvedLabel = (observedOptionLabel = '') => {
      if (targetIsLatest) {
        const checkedAdvancedRadio = findCheckedAdvancedModelRadio();
        if (checkedAdvancedRadio) return (checkedAdvancedRadio.textContent ?? '').trim();
        // Picker closed: the pill ("6 Pro") is the evidence; report the radio's name so callers
        // can compare against the requested target instead of the tier-suffixed pill text.
        if (latestButtonSelected()) return PRIMARY_LABEL;
        const currentButtonLabel = getButtonLabel();
        if (currentButtonLabel) return currentButtonLabel;
        // No picker button at all (e.g. the "current" strategy on a page that hides it): fall back
        // to the generic composer/observed label resolution below.
      }
      if (configuredSelectionMatchesTarget()) {
        const variant = getConfiguredVariantLabel();
        const version = formatModelOptionLabel(getConfiguredVersionLabel());
        if (desiredModelVariant === 'sol' && labelHasProWord(normalizeText(variant))) return version;
        return [variant, version].filter(Boolean).join(' ');
      }
      if (advancedModelSignalMatchesTarget()) {
        return getAdvancedModelLabel();
      }
      const composerLabel = getComposerModelLabel();
      const normalizedComposerLabel = normalizeText(composerLabel);
      if (
        desiredVersion &&
        desiredModelVariant &&
        versionFromLabel(normalizedComposerLabel) === desiredVersion &&
        normalizedComposerLabel.split(' ').includes(desiredModelVariant)
      ) {
        if (desiredModelVariant === 'sol' && hasProComposerPill()) return PRIMARY_LABEL;
        return withProPillSignal(composerLabel);
      }
      const buttonLabel = getButtonLabel();
      const normalizedButton = normalizeText(buttonLabel);
      if (
        desiredVersion &&
        desiredModelVariant &&
        versionFromLabel(normalizedButton) === desiredVersion &&
        normalizedButton.split(' ').includes(desiredModelVariant)
      ) {
        if (desiredModelVariant === 'sol' && hasProComposerPill()) return PRIMARY_LABEL;
        return withProPillSignal(buttonLabel);
      }
      const observedLabel = (label) => {
        const formatted = formatModelOptionLabel(label);
        return formatted && !isEffortOnlyLabel(normalizeText(formatted))
          ? withProPillSignal(formatted)
          : '';
      };
      return (
        observedLabel(observedOptionLabel) ||
        observedLabel(composerLabel) ||
        observedLabel(buttonLabel) ||
        (wantsPro && hasProComposerPill() ? 'Pro' : '')
      );
    };
    if ((targetIsLatest || PRIMARY_LABEL === 'GPT-5.6 Sol') &&
        (MODEL_STRATEGY !== 'current' || cfg.inspectCurrent !== false)) {
      return (async () => {
      const triggers = () => [...new Set(Array.from(document.querySelectorAll(BUTTON_SELECTOR)))]
        .filter(node => isVisibleElement(node) && !node.closest?.('[role="menu"], [data-radix-collection-root]'));
      const resolveTrigger = () => { const nodes = triggers(); return nodes.length === 1 ? nodes[0] : null; };
      const trigger = resolveTrigger();
      const invalid = reason => ({ status: 'selection-invalid', reason });
      if (!trigger && triggers().length === 0) return { status: MODEL_STRATEGY === 'current' ? 'pending' : 'button-missing' };
      if (!trigger) return invalid('composer trigger is ambiguous');
      function createPickerInspection(trigger, resolveTrigger, candidates, visible, click, additionalCleanupComplete = () => true) {
    const token = cfg.token ?? null;
    const record = token ? window.__oraclePickerInspections?.get(token) : null;
    const active = () => !token || record?.active === true;
    const cleanupClick = target => {
      if (!record?.finalising) return click(target);
      const oracleSubmissionIsActive = () => {
        try {

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
          const gate = claim?.inspection;
          return gate?.token === token && gate.rendererId === window.__oracleOwnershipRendererId &&
            gate.parentReservationId === (record.parent?.reservationId ?? null) && gate.state === 'finalising' &&
            (record.parent ? matchesOracleSubmission(claim, record.parent) &&
              ['active', 'releasing', 'quarantined'].includes(claim.submission.state) : !claim.submission);
        } catch { return false; }
      };
      function dispatchOwnedCleanup(target, validate = () => true){
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
      return dispatchOwnedCleanup(target);
    };
    const timers = new Map();
    const sleep = (ms, finalising = false) => {
      if (!token) return new Promise(resolve => setTimeout(resolve, ms));
      return new Promise(resolve => {
        if (!(finalising ? record?.finalising === true : active())) return resolve();
        let handle;
        let fired = false;
        handle = setTimeout(() => { fired = true; timers.delete(handle); resolve(); }, ms);
        if (!fired) timers.set(handle, resolve);
      });
    };
    const stop = () => {
      for (const [handle, resolve] of timers) { clearTimeout(handle); resolve(); }
      timers.clear();
    };
    if (record) record.stop = stop;
    const ownerKey = trigger?.id || trigger?.getAttribute('aria-controls') || null;
    const initialId = trigger?.id || null;
    const initialControls = trigger?.getAttribute('aria-controls') || null;
    const before = new Set(candidates().filter(visible));
    const controlledNodes = controls => controls ? Array.from(document.querySelectorAll('[id]'))
      .filter(node => node.id === controls && visible(node)) : [];
    const opened = [];
    let rootNode = null;
    let rootId = null;
    let rootLabel = null;
    const bindRoot = node => {
      if (rootNode && (node !== rootNode || (node.id || null) !== rootId ||
          (node.getAttribute('aria-labelledby') || null) !== rootLabel)) {
        rootAmbiguous = ambiguous = true;
        return null;
      }
      rootNode = node;
      rootId = node.id || null;
      rootLabel = node.getAttribute('aria-labelledby') || null;
      return node;
    };
    let causalRoot = false;
    let rootClicked = false;
    let rootClicks = 0;
    let rootClickedAt = 0;
    let openedControls = null;
    let rootClosing = false;
    let ambiguous = !ownerKey;
    let rootAmbiguous = !ownerKey;
    const currentTrigger = () => {
      const current = resolveTrigger();
      if (!current || current !== trigger || (current.id || null) !== initialId) return null;
      const controls = current.getAttribute('aria-controls') || null;
      if (controls === initialControls) {
        if (openedControls && rootNode && visible(rootNode) && !rootClosing) return null;
        return current;
      }
      if (initialControls || !rootClicked || !initialId || current !== trigger || !controls) return null;
      const linked = controlledNodes(controls);
      if (linked.length !== 1 || linked[0].getAttribute('aria-labelledby') !== initialId || before.has(linked[0]) ||
          (rootNode && linked[0] !== rootNode) || (openedControls && controls !== openedControls)) return null;
      openedControls = controls;
      return current;
    };
    const root = () => {
      const current = currentTrigger();
      if (!current) rootAmbiguous = ambiguous = true;
      if (!current || rootAmbiguous) return null;
      const menus = candidates().filter(visible);
      const controls = openedControls || initialControls;
      if (controlledNodes(controls).length > 1) {
        rootAmbiguous = ambiguous = true;
        return null;
      }
      const linked = menus.filter(menu =>
        (initialControls && menu.id === initialControls) ||
        (initialId && menu.getAttribute('aria-labelledby') === initialId));
      if (linked.length > 1 || linked.some(menu =>
        (initialControls && menu.id !== initialControls) ||
        (menu.getAttribute('aria-labelledby') && menu.getAttribute('aria-labelledby') !== initialId))) {
        rootAmbiguous = ambiguous = true;
        return null;
      }
      if (linked.length === 1) {
        if (rootNode && linked[0] !== rootNode) {
          rootAmbiguous = ambiguous = true;
          return null;
        }
        return bindRoot(linked[0]);
      }
      if (initialControls) return null;
      if (causalRoot) {
        return menus.includes(rootNode) ? bindRoot(rootNode) : null;
      }
      if (!rootClicked) return null;
      const fresh = menus.filter(menu => !before.has(menu) &&
        (!menu.getAttribute('aria-labelledby') || menu.getAttribute('aria-labelledby') === initialId));
      if (fresh.length > 1) rootAmbiguous = ambiguous = true;
      if (fresh.length !== 1) return null;
      if (rootNode && fresh[0] !== rootNode) {
        rootAmbiguous = ambiguous = true;
        return null;
      }
      if (!bindRoot(fresh[0])) return null;
      causalRoot = true;
      return rootNode;
    };
    const openRoot = () => {
      if (!active()) return;
      if (root()) return;
      const current = currentTrigger();
      if (!current || rootAmbiguous) return;
      // ChatGPT can ignore a click that lands just after the composer mounts. Click again only
      // while no menu was bound and the trigger still reports closed, at most 3 clicks in all.
      if (rootClicked && (rootNode || rootClicks >= 3 || performance.now() - rootClickedAt < 600 ||
          current.getAttribute('aria-expanded') !== 'false' || current.getAttribute('aria-controls') !== initialControls)) return;
      const controlled = initialControls ? document.getElementById(initialControls) : null;
      const labelledBy = controlled?.getAttribute('aria-labelledby');
      if (labelledBy && labelledBy !== initialId) {
        rootAmbiguous = ambiguous = true;
        return;
      }
      if (current.getAttribute('aria-expanded') === 'true') {
        if (!initialControls) rootAmbiguous = ambiguous = true;
        return;
      }
      rootClicked = true;
      rootClicks += 1;
      rootClickedAt = performance.now();
      if (active()) click(current);
    };
    const selectable = node => ['aria-checked', 'aria-selected', 'aria-current', 'data-selected']
      .some(name => node.getAttribute(name) !== null) ||
      ['checked', 'unchecked', 'selected', 'unselected', 'on', 'off', 'true', 'false'].includes(node.getAttribute('data-state')) ||
      ['radio', 'menuitemradio', 'option', 'checkbox', 'menuitemcheckbox'].includes(node.getAttribute('role'));
    const disclosure = (node, kind) => {
      if (!node || !visible(node) || selectable(node) || node.disabled ||
          node.getAttribute('aria-disabled') === 'true') return false;
      const expanded = node.getAttribute('aria-expanded');
      if (kind === 'advanced') return node.getAttribute('data-model-picker-view-toggle') === 'true' ||
        expanded === 'true' || expanded === 'false';
      return node.getAttribute('role') === 'menuitem' && node.getAttribute('aria-haspopup') === 'menu' &&
        (Boolean(node.getAttribute('aria-controls')) || expanded === 'true' || expanded === 'false');
    };
    const openSurface = (opener, surfaces, kind, contained = false, qualifies = () => true, closedSemantically = () => false) => {
      if (!active()) return () => null;
      const parent = root();
      if (!parent || !parent.contains(opener) || !disclosure(opener, kind) || !qualifies(opener)) {
        ambiguous = true;
        return () => null;
      }
      const id = opener.id || null;
      const controls = opener.getAttribute('aria-controls') || null;
      const initial = new Set(surfaces().filter(visible));
      const resolveOpener = () => {
        const parent = root();
        if (!parent) return null;
        if (id) {
          const matches = Array.from(parent.querySelectorAll('[id]')).filter(node => node.id === id);
          if (matches.length !== 1 || matches[0] !== opener || matches[0].getAttribute('aria-controls') !== controls) {
            ambiguous = true;
            return null;
          }
          return matches[0];
        }
        if (!parent.contains(opener) || opener.getAttribute('aria-controls') !== controls) {
          ambiguous = true;
          return null;
        }
        return opener;
      };
      let surface = null;
      let surfaceId = null;
      let surfaceLabel = null;
      const bindSurface = node => {
        if (surface && (node !== surface || (surface.id || null) !== surfaceId ||
            (surface.getAttribute('aria-labelledby') || null) !== surfaceLabel)) {
          ambiguous = true;
          return null;
        }
        if (!surface) {
          surface = node;
          surfaceId = node.id || null;
          surfaceLabel = node.getAttribute('aria-labelledby') || null;
        }
        return surface;
      };
      const resolveSurface = () => {
        if (ambiguous) return null;
        if (!resolveOpener()) return null;
        const controlled = controlledNodes(controls);
        if (controlled.length > 1 || (surface && controlled.some(node => node !== surface))) {
          ambiguous = true;
          return null;
        }
        const all = surfaces().filter(node => visible(node) && (!contained || root()?.contains(node)));
        if (all.some(node => (controls && node.id === controls || contained && root()?.contains(node) || !initial.has(node)) &&
            node.getAttribute('aria-labelledby') && node.getAttribute('aria-labelledby') !== id)) {
          ambiguous = true;
          return null;
        }
        const linked = all.filter(node => (controls && node.id === controls) ||
          (id && node.getAttribute('aria-labelledby') === id));
        if (linked.length > 1 || linked.some(node =>
          (controls && node.id !== controls) ||
          (node.getAttribute('aria-labelledby') && node.getAttribute('aria-labelledby') !== id))) {
          ambiguous = true;
          return null;
        }
        if (linked.length === 1) return bindSurface(linked[0]);
        if (controls) return null;
        if (contained && all.length === 1) return bindSurface(all[0]);
        if (surface) return all.includes(surface) ? bindSurface(surface) : null;
        const fresh = all.filter(node => !initial.has(node));
        if (fresh.length > 1) ambiguous = true;
        return fresh.length === 1 ? bindSurface(fresh[0]) : null;
      };
      if (resolveSurface()) return resolveSurface;
      if (opener.getAttribute('aria-expanded') === 'true') return resolveSurface;
      if (contained && controls) {
        const controlled = document.getElementById(controls);
        if (controlled && !parent.contains(controlled)) {
          ambiguous = true;
          return () => null;
        }
      }
      opened.push({ resolveOpener, resolveSurface, surfaces, kind, qualifies, closedSemantically,
        getSurface: () => surface, getSurfaceId: () => surfaceId, getSurfaceLabel: () => surfaceLabel });
      if (active()) click(opener);
      return resolveSurface;
    };
    let cleanupRun = null;
    let cleaned = false;
    let rootClosed = false;
    const rootGone = () => {
      if (!rootNode) return false;
      const controls = openedControls || initialControls;
      const menus = candidates().filter(visible);
      const current = resolveTrigger();
      const currentControls = current?.getAttribute('aria-controls') || null;
      if (current !== trigger || (current?.id || null) !== initialId ||
          ((rootNode.id || null) !== rootId) || (rootNode.getAttribute('aria-labelledby') || null) !== rootLabel ||
          (currentControls !== (openedControls || initialControls) &&
            !(currentControls === initialControls || !currentControls && !visible(rootNode) && current?.getAttribute('aria-expanded') === 'false'))) {
        rootAmbiguous = ambiguous = true;
      }
      if (controlledNodes(controls).some(node => node !== rootNode) || menus.some(menu => menu !== rootNode &&
          ((controls && menu.id === controls) || (initialId && menu.getAttribute('aria-labelledby') === initialId))))
        rootAmbiguous = ambiguous = true;
      return !rootAmbiguous && (rootNode.isConnected === false || !visible(rootNode));
    };
    const observeAdditionalCleanup = async (finalising, until) => {
      const allowed = () => !token || (finalising ? record?.finalising === true : active());
      if (!allowed() || ambiguous) return false;
      if (additionalCleanupComplete() === true) return true;
      const deadline = until ?? performance.now() + 2000;
      while (allowed() && !ambiguous && performance.now() < deadline) {
        await sleep(25, finalising);
        if (!allowed() || ambiguous || performance.now() >= deadline) return false;
        if (additionalCleanupComplete() === true) return true;
      }
      return false;
    };
    const cleanupOwned = async (finalising) => {
      let complete = true;
      const until = performance.now() + 2000;
      const allowed = () => (!token || (finalising ? record?.finalising === true : active())) && performance.now() < until;
      const waitClosed = async test => {
        while (allowed() && !ambiguous && !test()) await sleep(25, finalising);
        return allowed() && !ambiguous && test();
      };
      for (const entry of opened.slice().reverse()) {
        if (entry.closed) continue;
        if (!allowed()) return false;
        const retained = entry.getSurface();
        if (rootGone()) {
          const id = entry.getSurfaceId();
          const label = entry.getSurfaceLabel();
          if (!retained || (retained.id || null) !== id || (retained.getAttribute('aria-labelledby') || null) !== label ||
              controlledNodes(id).some(node => node !== retained) ||
              entry.surfaces().some(node => visible(node) && (node === retained || id && node.id === id || label && node.getAttribute('aria-labelledby') === label))) return false;
          entry.closed = true;
          continue;
        }
        if (!root()) return false;
        const opener = entry.resolveOpener();
        if (!opener || !disclosure(opener, entry.kind) || !entry.qualifies(opener)) {
          complete = false;
          continue;
        }
        let original = entry.closingSurface || entry.getSurface();
        let current = entry.closingSurface || entry.resolveSurface();
        while (finalising && !original && allowed() && !ambiguous) {
          await sleep(25, true);
          current = entry.resolveSurface();
          original = entry.getSurface();
        }
        if (!original) { if (finalising) complete = false; continue; }
        const stillVisible = () => {
          const id = entry.getSurfaceId();
          const nodes = controlledNodes(id);
          if (nodes.some(node => node !== original) || (visible(original) &&
              ((original.id || null) !== id || (original.getAttribute('aria-labelledby') || null) !== entry.getSurfaceLabel()))) ambiguous = true;
          if (!ambiguous && entry.kind === 'advanced' && entry.closingSurface === original &&
              root()?.contains(original) && entry.closedSemantically(original) &&
              !entry.surfaces().some(node => node === original && visible(node))) {
            if (entry.surfaces().some(node => node !== original && visible(node))) ambiguous = true;
            else return false;
          }
          return original && ((original.isConnected !== false && visible(original)) || nodes.length > 0);
        };
        if (!stillVisible()) { entry.closed = true; continue; }
        if (current !== original) {
          complete = false;
          continue;
        }
        if (!allowed()) return false;
        if (!entry.closingSurface) {
          entry.closingSurface = original;
          cleanupClick(opener);
        }
        if (await waitClosed(() => !stillVisible())) entry.closed = true;
        else complete = false;
      }
      let picker = rootGone() ? null : root();
      while (finalising && rootClicked && !rootClosed && !rootNode && !picker && allowed() && !rootAmbiguous) {
        await sleep(25, true);
        picker = root();
      }
      if (finalising && rootClicked && !rootClosed && !rootNode && !picker) complete = false;
      if (rootClicked && !rootClosed && rootNode) {
        if (!rootGone()) {
          if (!rootClosing) {
            const current = currentTrigger();
            if (!current || picker !== rootNode || rootAmbiguous || !allowed()) return false;
            rootClosing = true;
            cleanupClick(current);
          }
          if (await waitClosed(rootGone)) rootClosed = true;
          else complete = false;
        } else rootClosed = true;
      }
      const structuralComplete = complete && !ambiguous;
      cleaned = structuralComplete && (!rootClicked || rootClosed) && opened.every(entry => entry.closed);
      return structuralComplete && await observeAdditionalCleanup(finalising, until);
    };
    // Names what an unconfirmed cleanup left behind, so its failure can be attributed.
    const cleanupState = () => {
      const facts = [];
      if (rootAmbiguous) facts.push('root-ambiguous');
      else if (ambiguous) facts.push('ambiguous');
      if (rootNode && rootNode.isConnected !== false && visible(rootNode)) facts.push('menu-visible');
      else if (rootClicked && !rootClosed) facts.push('menu-unconfirmed');
      for (const entry of opened) if (!entry.closed) facts.push(entry.kind + '-open');
      if (additionalCleanupComplete() !== true) facts.push('overlay-open');
      return facts.join(' ') || 'deadline';
    };
    const cleanup = async (finalising = false) => {
      if (cleanupRun) await cleanupRun;
      cleanupRun = cleaned ? observeAdditionalCleanup(finalising) : cleanupOwned(finalising);
      try {
        const complete = await cleanupRun;
        if (record) {
          try { record.cleanupState = complete ? null : cleanupState(); }
          catch { record.cleanupState = 'unreadable'; }
        }
        return complete;
      }
      finally { cleanupRun = null; }
    };
    if (record) record.cleanup = cleanup;
    return { ownerKey, root, rootGone, openRoot, disclosure, openSurface, cleanup, sleep,
      get active() { return active(); },
      get ambiguous() { return ambiguous; } };
  }
      const inspection = createPickerInspection(trigger, resolveTrigger,
        () => Array.from(document.querySelectorAll('[role="menu"], [data-radix-collection-root], [role="listbox"], [role="dialog"]')), isVisibleElement, dispatchClickSequence);
      let resolveAdvanced = null;
      let resolveModels = null;
      let clickedModel = false;
      let retainedModelScopes = null;
      const expectedSource = null;
      const sourceMatches = label => expectedSource === 'GPT-6'
        ? isLatestModelLabel(label) : normalizeText(label) === 'gpt 5 6 sol';
      const targetMatches = label => targetIsLatest ? isLatestModelLabel(label) : normalizeText(label) === 'gpt 5 6 sol';
      const modelDisclosure = node => containsPickerWord(pickerNodeLabel(node), MODEL_WORDS) && !containsPickerWord(pickerNodeLabel(node), EFFORT_WORDS) ||
        /^gpt [0-9]+(?: [0-9]+)?(?: sol)?$/.test(normalizeText(node.textContent || ''));
      const readModels = menu => {
        const explicit = resolveModels?.();
        const advanced = resolveAdvanced?.();
        const scopes = [...new Set([...Array.from(menu.querySelectorAll(
          '[data-model-picker-view="models"], [data-model-picker-view="simple"], ' + ADVANCED_VIEW_SELECTOR
        )).filter(scope => isVisibleElement(scope) &&
          (scope.getAttribute('data-model-picker-view') !== 'simple' ||
            Array.from(scope.querySelectorAll('[role="menuitemradio"]')).some(row =>
              isVisibleElement(row) && (targetMatches(row.textContent || '') || /^gpt [0-9]/.test(normalizeText(row.textContent || '')))))),
          ...[explicit, advanced].filter(scope => scope && isVisibleElement(scope))])];
        if (retainedModelScopes && (scopes.length !== retainedModelScopes.length || scopes.some((scope, index) => scope !== retainedModelScopes[index])))
          return { invalid: true, rows: [], checked: [] };
        const roots = scopes.length ? scopes : [menu];
        const rows = [...new Set(roots.flatMap(scope => Array.from(scope.querySelectorAll('[role="menuitemradio"]'))))]
          .filter(row => isVisibleElement(row) && (explicit || scopes.length || !isUnifiedPickerMenu(menu) && !isEffortOnlyLabel(normalizeText(row.textContent || ''))));
        if (scopes.length && rows.length) retainedModelScopes ??= scopes;
        return { rows, checked: rows.filter(row => row.getAttribute('aria-checked') === 'true') };
      };
      const run = async () => {
        inspection.openRoot();
        const deadline = performance.now() + 3000;
        while (inspection.active && performance.now() < deadline) {
          if (clickedModel && inspection.rootGone()) return { status: 'pending' };
          const menu = inspection.root();
          if (inspection.ambiguous) return invalid('picker ownership changed or is ambiguous');
          if (!menu) {
            if (clickedModel && inspection.rootGone()) return { status: 'pending' };
            if (!clickedModel) inspection.openRoot();
            await inspection.sleep(75);
            continue;
          }
          const observed = readModels(menu);
          if (observed.invalid) return invalid('model view was replaced');
          if (inspection.ambiguous) return invalid('model scope ownership changed');
          if (observed.checked.length > 1) return invalid('multiple visible checked models');
          if (expectedSource && !clickedModel &&
              (observed.checked.length !== 1 || !sourceMatches(observed.checked[0].textContent || '')) &&
              observed.rows.length) return invalid('quota transition source model changed');
          if (observed.checked.length === 1) {
            const label = (observed.checked[0].textContent || '').trim();
            if (MODEL_STRATEGY === 'current' || targetMatches(label)) {
              if (inspection.root() !== menu || inspection.ambiguous) return invalid('picker ownership changed');
              return { status: clickedModel ? 'switched' : 'already-selected', label,
                ...(cfg.inspectCurrent === 'evidence' ? { checkedCount: 1, ownerKey: inspection.ownerKey } : {}) };
            }
          }
          if (observed.rows.length) {
            if (MODEL_STRATEGY === 'current') return invalid('no uniquely checked model');
            const targets = observed.rows.filter(row => targetMatches(row.textContent || ''));
            if (targets.length !== 1) return invalid('model option is missing or ambiguous');
            if (inspection.root() !== menu || inspection.ambiguous || !inspection.active) return invalid('picker ownership changed');
            if (!clickedModel) {
              const source = observed.checked[0];
              const sourceLabel = source?.textContent || '';
              const target = targets[0];
              const targetLabel = target.textContent || '';
              const rowIdentity = row => ['id', 'role', 'aria-label', 'data-testid'].map(name => row.getAttribute(name));
              const sourceIdentity = expectedSource ? JSON.stringify(rowIdentity(source)) : null;
              const targetIdentity = JSON.stringify(rowIdentity(target));
              const transitionValid = () => {
                if (!expectedSource) return true;
                const current = readModels(menu);
                return inspection.active && !inspection.ambiguous && inspection.root() === menu &&
                  !current.invalid && current.checked.length === 1 && current.checked[0] === source &&
                  (source.textContent || '') === sourceLabel && sourceMatches(sourceLabel) &&
                  JSON.stringify(rowIdentity(source)) === sourceIdentity &&
                  current.rows.filter(row => targetMatches(row.textContent || '')).length === 1 &&
                  current.rows.includes(target) && (target.textContent || '') === targetLabel &&
                  JSON.stringify(rowIdentity(target)) === targetIdentity &&
                  isVisibleElement(target) && target.disabled !== true && target.getAttribute('aria-disabled') !== 'true' &&
                  !['', 'true'].includes(target.getAttribute('data-disabled'));
              };
              if (!transitionValid() || !dispatchClickSequence(target, transitionValid)) return invalid('model selection authority expired');
              clickedModel = true;
            }
          } else {
            const advanced = Array.from(menu.querySelectorAll('[data-model-picker-view-toggle="true"], [role="menuitem"]'))
              .filter(node => isVisibleElement(node) && inspection.disclosure(node, 'advanced') &&
                (node.getAttribute('data-model-picker-view-toggle') === 'true' || containsPickerWord(pickerNodeLabel(node), ADVANCED_WORDS)));
            if (advanced.length > 1) return invalid('model view is ambiguous');
            if (!resolveAdvanced && advanced.length === 1) resolveAdvanced = inspection.openSurface(advanced[0],
              () => Array.from(inspection.root()?.querySelectorAll('[data-model-picker-view="models"], ' + ADVANCED_VIEW_SELECTOR) || []),
              'advanced', true, () => true, node => node.getAttribute('data-model-picker-view') === 'simple');
            const parent = resolveAdvanced?.() || menu;
            const openers = Array.from(parent.querySelectorAll(SUBMENU_OPENER_SELECTOR)).filter(node =>
              isVisibleElement(node) && inspection.disclosure(node, 'submenu') &&
              modelDisclosure(node));
            if (openers.length > 1) return invalid('model submenu is ambiguous');
            if (!resolveModels && openers.length === 1) resolveModels = inspection.openSurface(openers[0],
              () => Array.from(document.querySelectorAll('[role="menu"], [data-radix-collection-root], [role="listbox"], [role="dialog"]')).filter(node => node !== inspection.root()),
              'submenu', false, modelDisclosure);
          }
          await inspection.sleep(75);
        }
        return invalid('owned model evidence is unavailable');
      };
      try {
        const result = await run();
        return await inspection.cleanup() ? result : invalid('picker cleanup is unconfirmed');
      } catch (error) {
        await inspection.cleanup().catch(() => false);
        throw error;
      }
      })();
    }
    if (MODEL_STRATEGY === 'current') {
      const currentLabel = getResolvedLabel('') || null;
      // An ambiguous pill still requires checked-radio evidence when an open
      // submenu supplies a richer model label. Never infer a model from effort.
      const ambiguousLabel = [currentLabel, getButtonLabel()].some((label) =>
        (PRIMARY_LABEL === 'GPT-5.6 Sol' && /^(?:gpt )?5 6 pro$/.test(normalizeText(label))) ||
        (targetIsLatest && ['extra high', 'pro'].includes(normalizeText(label)))
      );
      if (cfg.inspectCurrent === true || cfg.inspectCurrent === 'evidence' || (cfg.inspectCurrent === 'when-ambiguous' && ambiguousLabel)) {
        const trigger = findModelButton();
        if (!trigger) return { status: 'button-missing' };
        const structured = cfg.inspectCurrent === 'evidence';
        function createPickerInspection(trigger, resolveTrigger, candidates, visible, click, additionalCleanupComplete = () => true) {
    const token = cfg.token ?? null;
    const record = token ? window.__oraclePickerInspections?.get(token) : null;
    const active = () => !token || record?.active === true;
    const cleanupClick = target => {
      if (!record?.finalising) return click(target);
      const oracleSubmissionIsActive = () => {
        try {

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
          const gate = claim?.inspection;
          return gate?.token === token && gate.rendererId === window.__oracleOwnershipRendererId &&
            gate.parentReservationId === (record.parent?.reservationId ?? null) && gate.state === 'finalising' &&
            (record.parent ? matchesOracleSubmission(claim, record.parent) &&
              ['active', 'releasing', 'quarantined'].includes(claim.submission.state) : !claim.submission);
        } catch { return false; }
      };
      function dispatchOwnedCleanup(target, validate = () => true){
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
      return dispatchOwnedCleanup(target);
    };
    const timers = new Map();
    const sleep = (ms, finalising = false) => {
      if (!token) return new Promise(resolve => setTimeout(resolve, ms));
      return new Promise(resolve => {
        if (!(finalising ? record?.finalising === true : active())) return resolve();
        let handle;
        let fired = false;
        handle = setTimeout(() => { fired = true; timers.delete(handle); resolve(); }, ms);
        if (!fired) timers.set(handle, resolve);
      });
    };
    const stop = () => {
      for (const [handle, resolve] of timers) { clearTimeout(handle); resolve(); }
      timers.clear();
    };
    if (record) record.stop = stop;
    const ownerKey = trigger?.id || trigger?.getAttribute('aria-controls') || null;
    const initialId = trigger?.id || null;
    const initialControls = trigger?.getAttribute('aria-controls') || null;
    const before = new Set(candidates().filter(visible));
    const controlledNodes = controls => controls ? Array.from(document.querySelectorAll('[id]'))
      .filter(node => node.id === controls && visible(node)) : [];
    const opened = [];
    let rootNode = null;
    let rootId = null;
    let rootLabel = null;
    const bindRoot = node => {
      if (rootNode && (node !== rootNode || (node.id || null) !== rootId ||
          (node.getAttribute('aria-labelledby') || null) !== rootLabel)) {
        rootAmbiguous = ambiguous = true;
        return null;
      }
      rootNode = node;
      rootId = node.id || null;
      rootLabel = node.getAttribute('aria-labelledby') || null;
      return node;
    };
    let causalRoot = false;
    let rootClicked = false;
    let rootClicks = 0;
    let rootClickedAt = 0;
    let openedControls = null;
    let rootClosing = false;
    let ambiguous = !ownerKey;
    let rootAmbiguous = !ownerKey;
    const currentTrigger = () => {
      const current = resolveTrigger();
      if (!current || current !== trigger || (current.id || null) !== initialId) return null;
      const controls = current.getAttribute('aria-controls') || null;
      if (controls === initialControls) {
        if (openedControls && rootNode && visible(rootNode) && !rootClosing) return null;
        return current;
      }
      if (initialControls || !rootClicked || !initialId || current !== trigger || !controls) return null;
      const linked = controlledNodes(controls);
      if (linked.length !== 1 || linked[0].getAttribute('aria-labelledby') !== initialId || before.has(linked[0]) ||
          (rootNode && linked[0] !== rootNode) || (openedControls && controls !== openedControls)) return null;
      openedControls = controls;
      return current;
    };
    const root = () => {
      const current = currentTrigger();
      if (!current) rootAmbiguous = ambiguous = true;
      if (!current || rootAmbiguous) return null;
      const menus = candidates().filter(visible);
      const controls = openedControls || initialControls;
      if (controlledNodes(controls).length > 1) {
        rootAmbiguous = ambiguous = true;
        return null;
      }
      const linked = menus.filter(menu =>
        (initialControls && menu.id === initialControls) ||
        (initialId && menu.getAttribute('aria-labelledby') === initialId));
      if (linked.length > 1 || linked.some(menu =>
        (initialControls && menu.id !== initialControls) ||
        (menu.getAttribute('aria-labelledby') && menu.getAttribute('aria-labelledby') !== initialId))) {
        rootAmbiguous = ambiguous = true;
        return null;
      }
      if (linked.length === 1) {
        if (rootNode && linked[0] !== rootNode) {
          rootAmbiguous = ambiguous = true;
          return null;
        }
        return bindRoot(linked[0]);
      }
      if (initialControls) return null;
      if (causalRoot) {
        return menus.includes(rootNode) ? bindRoot(rootNode) : null;
      }
      if (!rootClicked) return null;
      const fresh = menus.filter(menu => !before.has(menu) &&
        (!menu.getAttribute('aria-labelledby') || menu.getAttribute('aria-labelledby') === initialId));
      if (fresh.length > 1) rootAmbiguous = ambiguous = true;
      if (fresh.length !== 1) return null;
      if (rootNode && fresh[0] !== rootNode) {
        rootAmbiguous = ambiguous = true;
        return null;
      }
      if (!bindRoot(fresh[0])) return null;
      causalRoot = true;
      return rootNode;
    };
    const openRoot = () => {
      if (!active()) return;
      if (root()) return;
      const current = currentTrigger();
      if (!current || rootAmbiguous) return;
      // ChatGPT can ignore a click that lands just after the composer mounts. Click again only
      // while no menu was bound and the trigger still reports closed, at most 3 clicks in all.
      if (rootClicked && (rootNode || rootClicks >= 3 || performance.now() - rootClickedAt < 600 ||
          current.getAttribute('aria-expanded') !== 'false' || current.getAttribute('aria-controls') !== initialControls)) return;
      const controlled = initialControls ? document.getElementById(initialControls) : null;
      const labelledBy = controlled?.getAttribute('aria-labelledby');
      if (labelledBy && labelledBy !== initialId) {
        rootAmbiguous = ambiguous = true;
        return;
      }
      if (current.getAttribute('aria-expanded') === 'true') {
        if (!initialControls) rootAmbiguous = ambiguous = true;
        return;
      }
      rootClicked = true;
      rootClicks += 1;
      rootClickedAt = performance.now();
      if (active()) click(current);
    };
    const selectable = node => ['aria-checked', 'aria-selected', 'aria-current', 'data-selected']
      .some(name => node.getAttribute(name) !== null) ||
      ['checked', 'unchecked', 'selected', 'unselected', 'on', 'off', 'true', 'false'].includes(node.getAttribute('data-state')) ||
      ['radio', 'menuitemradio', 'option', 'checkbox', 'menuitemcheckbox'].includes(node.getAttribute('role'));
    const disclosure = (node, kind) => {
      if (!node || !visible(node) || selectable(node) || node.disabled ||
          node.getAttribute('aria-disabled') === 'true') return false;
      const expanded = node.getAttribute('aria-expanded');
      if (kind === 'advanced') return node.getAttribute('data-model-picker-view-toggle') === 'true' ||
        expanded === 'true' || expanded === 'false';
      return node.getAttribute('role') === 'menuitem' && node.getAttribute('aria-haspopup') === 'menu' &&
        (Boolean(node.getAttribute('aria-controls')) || expanded === 'true' || expanded === 'false');
    };
    const openSurface = (opener, surfaces, kind, contained = false, qualifies = () => true, closedSemantically = () => false) => {
      if (!active()) return () => null;
      const parent = root();
      if (!parent || !parent.contains(opener) || !disclosure(opener, kind) || !qualifies(opener)) {
        ambiguous = true;
        return () => null;
      }
      const id = opener.id || null;
      const controls = opener.getAttribute('aria-controls') || null;
      const initial = new Set(surfaces().filter(visible));
      const resolveOpener = () => {
        const parent = root();
        if (!parent) return null;
        if (id) {
          const matches = Array.from(parent.querySelectorAll('[id]')).filter(node => node.id === id);
          if (matches.length !== 1 || matches[0] !== opener || matches[0].getAttribute('aria-controls') !== controls) {
            ambiguous = true;
            return null;
          }
          return matches[0];
        }
        if (!parent.contains(opener) || opener.getAttribute('aria-controls') !== controls) {
          ambiguous = true;
          return null;
        }
        return opener;
      };
      let surface = null;
      let surfaceId = null;
      let surfaceLabel = null;
      const bindSurface = node => {
        if (surface && (node !== surface || (surface.id || null) !== surfaceId ||
            (surface.getAttribute('aria-labelledby') || null) !== surfaceLabel)) {
          ambiguous = true;
          return null;
        }
        if (!surface) {
          surface = node;
          surfaceId = node.id || null;
          surfaceLabel = node.getAttribute('aria-labelledby') || null;
        }
        return surface;
      };
      const resolveSurface = () => {
        if (ambiguous) return null;
        if (!resolveOpener()) return null;
        const controlled = controlledNodes(controls);
        if (controlled.length > 1 || (surface && controlled.some(node => node !== surface))) {
          ambiguous = true;
          return null;
        }
        const all = surfaces().filter(node => visible(node) && (!contained || root()?.contains(node)));
        if (all.some(node => (controls && node.id === controls || contained && root()?.contains(node) || !initial.has(node)) &&
            node.getAttribute('aria-labelledby') && node.getAttribute('aria-labelledby') !== id)) {
          ambiguous = true;
          return null;
        }
        const linked = all.filter(node => (controls && node.id === controls) ||
          (id && node.getAttribute('aria-labelledby') === id));
        if (linked.length > 1 || linked.some(node =>
          (controls && node.id !== controls) ||
          (node.getAttribute('aria-labelledby') && node.getAttribute('aria-labelledby') !== id))) {
          ambiguous = true;
          return null;
        }
        if (linked.length === 1) return bindSurface(linked[0]);
        if (controls) return null;
        if (contained && all.length === 1) return bindSurface(all[0]);
        if (surface) return all.includes(surface) ? bindSurface(surface) : null;
        const fresh = all.filter(node => !initial.has(node));
        if (fresh.length > 1) ambiguous = true;
        return fresh.length === 1 ? bindSurface(fresh[0]) : null;
      };
      if (resolveSurface()) return resolveSurface;
      if (opener.getAttribute('aria-expanded') === 'true') return resolveSurface;
      if (contained && controls) {
        const controlled = document.getElementById(controls);
        if (controlled && !parent.contains(controlled)) {
          ambiguous = true;
          return () => null;
        }
      }
      opened.push({ resolveOpener, resolveSurface, surfaces, kind, qualifies, closedSemantically,
        getSurface: () => surface, getSurfaceId: () => surfaceId, getSurfaceLabel: () => surfaceLabel });
      if (active()) click(opener);
      return resolveSurface;
    };
    let cleanupRun = null;
    let cleaned = false;
    let rootClosed = false;
    const rootGone = () => {
      if (!rootNode) return false;
      const controls = openedControls || initialControls;
      const menus = candidates().filter(visible);
      const current = resolveTrigger();
      const currentControls = current?.getAttribute('aria-controls') || null;
      if (current !== trigger || (current?.id || null) !== initialId ||
          ((rootNode.id || null) !== rootId) || (rootNode.getAttribute('aria-labelledby') || null) !== rootLabel ||
          (currentControls !== (openedControls || initialControls) &&
            !(currentControls === initialControls || !currentControls && !visible(rootNode) && current?.getAttribute('aria-expanded') === 'false'))) {
        rootAmbiguous = ambiguous = true;
      }
      if (controlledNodes(controls).some(node => node !== rootNode) || menus.some(menu => menu !== rootNode &&
          ((controls && menu.id === controls) || (initialId && menu.getAttribute('aria-labelledby') === initialId))))
        rootAmbiguous = ambiguous = true;
      return !rootAmbiguous && (rootNode.isConnected === false || !visible(rootNode));
    };
    const observeAdditionalCleanup = async (finalising, until) => {
      const allowed = () => !token || (finalising ? record?.finalising === true : active());
      if (!allowed() || ambiguous) return false;
      if (additionalCleanupComplete() === true) return true;
      const deadline = until ?? performance.now() + 2000;
      while (allowed() && !ambiguous && performance.now() < deadline) {
        await sleep(25, finalising);
        if (!allowed() || ambiguous || performance.now() >= deadline) return false;
        if (additionalCleanupComplete() === true) return true;
      }
      return false;
    };
    const cleanupOwned = async (finalising) => {
      let complete = true;
      const until = performance.now() + 2000;
      const allowed = () => (!token || (finalising ? record?.finalising === true : active())) && performance.now() < until;
      const waitClosed = async test => {
        while (allowed() && !ambiguous && !test()) await sleep(25, finalising);
        return allowed() && !ambiguous && test();
      };
      for (const entry of opened.slice().reverse()) {
        if (entry.closed) continue;
        if (!allowed()) return false;
        const retained = entry.getSurface();
        if (rootGone()) {
          const id = entry.getSurfaceId();
          const label = entry.getSurfaceLabel();
          if (!retained || (retained.id || null) !== id || (retained.getAttribute('aria-labelledby') || null) !== label ||
              controlledNodes(id).some(node => node !== retained) ||
              entry.surfaces().some(node => visible(node) && (node === retained || id && node.id === id || label && node.getAttribute('aria-labelledby') === label))) return false;
          entry.closed = true;
          continue;
        }
        if (!root()) return false;
        const opener = entry.resolveOpener();
        if (!opener || !disclosure(opener, entry.kind) || !entry.qualifies(opener)) {
          complete = false;
          continue;
        }
        let original = entry.closingSurface || entry.getSurface();
        let current = entry.closingSurface || entry.resolveSurface();
        while (finalising && !original && allowed() && !ambiguous) {
          await sleep(25, true);
          current = entry.resolveSurface();
          original = entry.getSurface();
        }
        if (!original) { if (finalising) complete = false; continue; }
        const stillVisible = () => {
          const id = entry.getSurfaceId();
          const nodes = controlledNodes(id);
          if (nodes.some(node => node !== original) || (visible(original) &&
              ((original.id || null) !== id || (original.getAttribute('aria-labelledby') || null) !== entry.getSurfaceLabel()))) ambiguous = true;
          if (!ambiguous && entry.kind === 'advanced' && entry.closingSurface === original &&
              root()?.contains(original) && entry.closedSemantically(original) &&
              !entry.surfaces().some(node => node === original && visible(node))) {
            if (entry.surfaces().some(node => node !== original && visible(node))) ambiguous = true;
            else return false;
          }
          return original && ((original.isConnected !== false && visible(original)) || nodes.length > 0);
        };
        if (!stillVisible()) { entry.closed = true; continue; }
        if (current !== original) {
          complete = false;
          continue;
        }
        if (!allowed()) return false;
        if (!entry.closingSurface) {
          entry.closingSurface = original;
          cleanupClick(opener);
        }
        if (await waitClosed(() => !stillVisible())) entry.closed = true;
        else complete = false;
      }
      let picker = rootGone() ? null : root();
      while (finalising && rootClicked && !rootClosed && !rootNode && !picker && allowed() && !rootAmbiguous) {
        await sleep(25, true);
        picker = root();
      }
      if (finalising && rootClicked && !rootClosed && !rootNode && !picker) complete = false;
      if (rootClicked && !rootClosed && rootNode) {
        if (!rootGone()) {
          if (!rootClosing) {
            const current = currentTrigger();
            if (!current || picker !== rootNode || rootAmbiguous || !allowed()) return false;
            rootClosing = true;
            cleanupClick(current);
          }
          if (await waitClosed(rootGone)) rootClosed = true;
          else complete = false;
        } else rootClosed = true;
      }
      const structuralComplete = complete && !ambiguous;
      cleaned = structuralComplete && (!rootClicked || rootClosed) && opened.every(entry => entry.closed);
      return structuralComplete && await observeAdditionalCleanup(finalising, until);
    };
    // Names what an unconfirmed cleanup left behind, so its failure can be attributed.
    const cleanupState = () => {
      const facts = [];
      if (rootAmbiguous) facts.push('root-ambiguous');
      else if (ambiguous) facts.push('ambiguous');
      if (rootNode && rootNode.isConnected !== false && visible(rootNode)) facts.push('menu-visible');
      else if (rootClicked && !rootClosed) facts.push('menu-unconfirmed');
      for (const entry of opened) if (!entry.closed) facts.push(entry.kind + '-open');
      if (additionalCleanupComplete() !== true) facts.push('overlay-open');
      return facts.join(' ') || 'deadline';
    };
    const cleanup = async (finalising = false) => {
      if (cleanupRun) await cleanupRun;
      cleanupRun = cleaned ? observeAdditionalCleanup(finalising) : cleanupOwned(finalising);
      try {
        const complete = await cleanupRun;
        if (record) {
          try { record.cleanupState = complete ? null : cleanupState(); }
          catch { record.cleanupState = 'unreadable'; }
        }
        return complete;
      }
      finally { cleanupRun = null; }
    };
    if (record) record.cleanup = cleanup;
    return { ownerKey, root, rootGone, openRoot, disclosure, openSurface, cleanup, sleep,
      get active() { return active(); },
      get ambiguous() { return ambiguous; } };
  }
        const inspection = createPickerInspection(trigger, findModelButton,
          () => Array.from(document.querySelectorAll('[role="menu"], [data-radix-collection-root], [role="listbox"], [role="dialog"]')).filter(isUnifiedPickerMenu),
          isVisibleElement, dispatchClickSequence);
        let advancedScope = null;
        let modelScope = null;
        inspection.openRoot();
        const until = performance.now() + 3000;
        // Only menu and view openers are clicked. Never dispatch to a model radio.
        return new Promise((resolve) => {
          const finish = async (label = null, checkedCount = 0, pending = false) => {
            const ownerKey = inspection.active && inspection.root() ? inspection.ownerKey : null;
            const cleaned = await inspection.cleanup().catch(() => false);
            if (!cleaned) { label = null; pending = false; }
            const status = pending ? 'pending' : 'already-selected';
            resolve(structured ? { status, label, checkedCount, ownerKey } : { status, label });
          };
          const poll = () => {
            if (!inspection.active) return void finish();
            const menu = inspection.root();
            if (inspection.ambiguous) return void finish();
            const scope = modelScope?.() || menu?.querySelector?.(ADVANCED_VIEW_SELECTOR) || menu?.querySelector?.('[data-model-picker-view]');
            const rows = Array.from(scope?.querySelectorAll?.('[role="menuitemradio"]') || []).filter(isVisibleElement);
            const checked = rows.filter((row) => row.getAttribute?.('aria-checked') === 'true');
            if (checked.length > 1) return finish(null, checked.length);
            if (checked.length === 1) return finish((checked[0].textContent || '').trim(), 1);
            if (rows.length) return finish();
            if (performance.now() >= until) return finish(null, 0, true);
            if (!menu) inspection.openRoot();
            if (menu) {
              const advancedCandidates = Array.from(menu.querySelectorAll('[role="menuitem"], [data-model-picker-view-toggle="true"]'))
                .filter(node => isVisibleElement(node) && containsPickerWord(pickerNodeLabel(node), ADVANCED_WORDS));
              const advanced = advancedCandidates.filter(node => inspection.disclosure(node, 'advanced'));
              if (advancedCandidates.length && advanced.length !== 1) return finish();
              if (!advancedScope && advanced.length === 1) advancedScope = inspection.openSurface(advanced[0],
                () => Array.from(inspection.root()?.querySelectorAll(ADVANCED_VIEW_SELECTOR) || []), 'advanced', true,
                node => containsPickerWord(pickerNodeLabel(node), ADVANCED_WORDS));
              const parent = advancedScope?.() || menu.querySelector(ADVANCED_VIEW_SELECTOR) || menu;
              const openers = Array.from(parent.querySelectorAll(SUBMENU_OPENER_SELECTOR)).filter(node =>
                isVisibleElement(node) && inspection.disclosure(node, 'submenu') &&
                containsPickerWord(pickerNodeLabel(node), MODEL_WORDS) && !containsPickerWord(pickerNodeLabel(node), EFFORT_WORDS));
              if (openers.length > 1) return finish();
              if (!modelScope && openers.length === 1) modelScope = inspection.openSurface(openers[0],
                () => Array.from(document.querySelectorAll('[role="menu"], [data-radix-collection-root], [role="listbox"], [role="dialog"]')).filter(node => node !== inspection.root()), 'submenu', false,
                node => containsPickerWord(pickerNodeLabel(node), MODEL_WORDS) && !containsPickerWord(pickerNodeLabel(node), EFFORT_WORDS));
            }
            void inspection.sleep(75).then(poll);
          };
          poll();
        });
      }
      return {
        status: 'already-selected',
        label: currentLabel,
      };
    }

    const button = findModelButton();
    if (!button) {
      return { status: 'button-missing' };
    }
    const buttonMatchesTarget = () => {
      if (configuredSelectionMatchesTarget()) return true;
      const normalizedLabel = normalizeText(getButtonLabel());
      if (!normalizedLabel) return false;
      if (wantsThinking && !wantsPro && hasProComposerPill()) return false;
      if (isTargetGpt55VisibleAlias(normalizedLabel)) return true;
      if (
        wantsThinking &&
        desiredVersion === '5-5' &&
        !hasProComposerPill() &&
        isThinkingEffortLabel(normalizedLabel) &&
        (isTargetGpt55VisibleAlias(readComposerModelSignal()) ||
          (isNonProIntelligenceThinkingLabel(normalizedLabel) &&
            isLegacyFlatIntelligencePickerOpen()))
      ) {
        return true;
      }
      if (
        wantsPro &&
        hasProComposerPill() &&
        (normalizedLabel === 'chatgpt' ||
          normalizedLabel === 'extended' ||
          normalizedLabel === 'standard' ||
          normalizedLabel === 'heavy' ||
          normalizedLabel === 'light')
      ) {
        return true;
      }
      if (desiredVersion) {
        if (desiredVersion === '5-6' && !normalizedLabel.includes('5 6')) return false;
        if (desiredVersion === '5-5' && !normalizedLabel.includes('5 5')) return false;
        if (desiredVersion === '5-4' && !normalizedLabel.includes('5 4')) return false;
        if (desiredVersion === '5-3' && !normalizedLabel.includes('5 3')) return false;
        if (desiredVersion === '5-2' && !normalizedLabel.includes('5 2')) return false;
        if (desiredVersion === '5-1' && !normalizedLabel.includes('5 1')) return false;
        if (desiredVersion === '5-0' && !normalizedLabel.includes('5 0')) return false;
      }
      if (desiredModelVariant && !normalizedLabel.includes(desiredModelVariant)) return false;
      if (wantsPro && labelHasLegacyProVersion(normalizedLabel)) return false;
      if (wantsPro && !labelHasProWord(normalizedLabel)) return false;
      if (wantsInstant && !normalizedLabel.includes('instant')) return false;
      if (
        wantsThinking &&
        desiredVersion &&
        versionFromLabel(normalizedLabel) === desiredVersion &&
        !labelHasProWord(normalizedLabel)
      ) {
        return true;
      }
      if (wantsThinking && !normalizedLabel.includes('thinking')) return false;
      // Also reject if button has variants we DON'T want
      if (
        !wantsPro &&
        normalizedLabel.includes(' pro') &&
        !(desiredModelVariant === 'sol' && hasProComposerPill())
      ) return false;
      if (!wantsInstant && normalizedLabel.includes('instant')) return false;
      if (!wantsThinking && normalizedLabel.includes('thinking')) return false;
      return true;
    };
    const buttonHasGenericLabel = () => {
      const normalizedLabel = normalizeText(getButtonLabel());
      return !normalizedLabel || normalizedLabel === 'chatgpt';
    };
    const composerSignalMatchesTarget = () => {
      const signal = readComposerModelSignal();
      if (!signal) {
        return COMPOSER_SIGNAL_ALLOW_BLANK;
      }
      if (wantsPro && labelHasLegacyProVersion(signal)) {
        return false;
      }
      if (COMPOSER_SIGNAL_EXCLUDES.some((token) =>
        token &&
        signal.includes(token) &&
        !(token === 'pro' && desiredModelVariant === 'sol' && hasProComposerPill())
      )) {
        return false;
      }
      if (COMPOSER_SIGNAL_INCLUDES.length === 0) {
        return true;
      }
      return COMPOSER_SIGNAL_INCLUDES.some((token) => token && signal.includes(token));
    };
    const activeSelectionMatchesTarget = () => {
      if (targetIsLatest) {
        const checkedAdvancedRadio = findCheckedAdvancedModelRadio();
        if (checkedAdvancedRadio) {
          return isLatestModelLabel(checkedAdvancedRadio.textContent ?? '');
        }
        return latestButtonSelected();
      }
      if (advancedModelSignalMatchesTarget()) {
        return true;
      }
      if (buttonMatchesTarget()) {
        return true;
      }
      if (!buttonHasGenericLabel()) {
        return false;
      }
      return composerSignalMatchesTarget();
    };
    const selectionStateChanged = (previousButtonLabel, previousComposerSignal) => {
      const currentButtonLabel = normalizeText(getButtonLabel());
      const currentComposerSignal = readComposerModelSignal();
      if (
        currentButtonLabel &&
        currentButtonLabel !== previousButtonLabel &&
        !buttonHasGenericLabel()
      ) {
        return true;
      }
      return currentComposerSignal !== previousComposerSignal;
    };

    if (activeSelectionMatchesTarget()) {
      return { status: 'already-selected', label: getResolvedLabel() };
    }

    let lastPointerClick = 0;
    const pointerClick = () => {
      if (dispatchClickSequence(button)) {
        lastPointerClick = performance.now();
      }
    };

    const getOptionLabel = (node) => node?.textContent?.trim() ?? '';
    const isDetachedProEffortMenu = (menu) => {
      const text = normalizeText(menu?.textContent ?? '');
      return (
        menu?.getAttribute?.('data-testid') !== 'composer-intelligence-picker-content' &&
        text.includes('pro standard') &&
        text.includes('pro extended')
      );
    };
    const isNestedEffortControl = (node, menu) =>
      node instanceof HTMLElement &&
      (node.getAttribute('data-model-picker-thinking-effort-action') === 'true' ||
        node.getAttribute('data-composer-intelligence-pro-effort-action') === 'true' ||
        Boolean(node.closest('[data-model-picker-thinking-effort-action="true"]')) ||
        Boolean(node.closest('[data-composer-intelligence-pro-effort-action="true"]')) ||
        (isUnifiedPickerMenu(menu) && isSubmenuOpener(node) &&
          containsPickerWord(pickerNodeLabel(node), EFFORT_WORDS)) ||
        isDetachedProEffortMenu(menu));
    const optionIsSelected = (node) => {
      if (!(node instanceof HTMLElement)) {
        return false;
      }
      const ariaChecked = node.getAttribute('aria-checked');
      const ariaSelected = node.getAttribute('aria-selected');
      const ariaCurrent = node.getAttribute('aria-current');
      const dataSelected = node.getAttribute('data-selected');
      const dataState = (node.getAttribute('data-state') ?? '').toLowerCase();
      const selectedStates = ['checked', 'selected', 'on', 'true'];
      if (ariaChecked === 'true' || ariaSelected === 'true' || ariaCurrent === 'true') {
        return true;
      }
      if (dataSelected === 'true' || selectedStates.includes(dataState)) {
        return true;
      }
      return false;
    };

    const scoreOption = (normalizedText, testid, node) => {
      // Assign a score to every node so we can pick the most likely match without brittle equality checks.
      // Latest is localized in the advanced radio list. Match the documented labels
      // exactly instead of falling through to generic scoring, which could select Sol.
      if (targetIsLatest) {
        return isLatestModelLabel(node?.textContent ?? '') ? 2000 : 0;
      }
      if (!normalizedText && !testid) {
        return 0;
      }
      let score = 0;
      const normalizedTestId = (testid ?? '').toLowerCase();
      const candidateTextVersion = versionFromLabel(normalizedText);
      const candidateIsVersionSubmenuTrigger = Boolean(
        candidateTextVersion && node?.getAttribute?.('aria-haspopup') === 'menu'
      );
      const candidateIsVersionCombobox =
        node?.getAttribute?.('role') === 'combobox' &&
        node?.getAttribute?.('aria-labelledby') === 'model-selection-label';
      if (candidateIsVersionCombobox && candidateTextVersion === desiredVersion) {
        return 0;
      }
      const candidateOpensConfiguration =
        Boolean(desiredVersion) &&
        desiredVersion !== '5-5' &&
        (normalizedTestId === 'model-configure-modal' ||
          (candidateIsVersionCombobox && node?.getAttribute?.('aria-expanded') !== 'true'));
      const candidateIsConfiguredVersionNavigation =
        Boolean(getConfigurationDialog()) &&
        candidateTextVersion === desiredVersion &&
        (node?.getAttribute?.('role') === 'option' ||
          node?.getAttribute?.('role') === 'menuitemradio');
      if (
        desiredModelVariant &&
        !normalizedText.includes(desiredModelVariant) &&
        !candidateIsVersionSubmenuTrigger &&
        !candidateOpensConfiguration &&
        !candidateIsConfiguredVersionNavigation
      ) {
        return 0;
      }
      if (candidateOpensConfiguration) {
        return 2000;
      }
      let exactTestIdMatch = false;
      if (normalizedTestId) {
        if (desiredVersion) {
          // data-testid strings have been observed with both dotted and dashed versions (e.g. gpt-5.2-pro vs gpt-5-2-pro).
          const has56 =
            normalizedTestId.includes('5-6') ||
            normalizedTestId.includes('5.6') ||
            normalizedTestId.includes('gpt-5-6') ||
            normalizedTestId.includes('gpt-5.6') ||
            normalizedTestId.includes('gpt56');
          const has52 =
            normalizedTestId.includes('5-2') ||
            normalizedTestId.includes('5.2') ||
            normalizedTestId.includes('gpt-5-2') ||
            normalizedTestId.includes('gpt-5.2') ||
            normalizedTestId.includes('gpt52');
          const has55 =
            normalizedTestId.includes('5-5') ||
            normalizedTestId.includes('5.5') ||
            normalizedTestId.includes('gpt-5-5') ||
            normalizedTestId.includes('gpt-5.5') ||
            normalizedTestId.includes('gpt55');
          const has54 =
            normalizedTestId.includes('5-4') ||
            normalizedTestId.includes('5.4') ||
            normalizedTestId.includes('gpt-5-4') ||
            normalizedTestId.includes('gpt-5.4') ||
            normalizedTestId.includes('gpt54');
          const has53 =
            normalizedTestId.includes('5-3') ||
            normalizedTestId.includes('5.3') ||
            normalizedTestId.includes('gpt-5-3') ||
            normalizedTestId.includes('gpt-5.3') ||
            normalizedTestId.includes('gpt53');
          const has51 =
            normalizedTestId.includes('5-1') ||
            normalizedTestId.includes('5.1') ||
            normalizedTestId.includes('gpt-5-1') ||
            normalizedTestId.includes('gpt-5.1') ||
            normalizedTestId.includes('gpt51');
          const has50 =
            normalizedTestId.includes('5-0') ||
            normalizedTestId.includes('5.0') ||
            normalizedTestId.includes('gpt-5-0') ||
            normalizedTestId.includes('gpt-5.0') ||
            normalizedTestId.includes('gpt50');
          const candidateVersion = has56
            ? '5-6'
            : has55
              ? '5-5'
            : has54
              ? '5-4'
              : has53
                ? '5-3'
                : has52
                  ? '5-2'
                  : has51
                    ? '5-1'
                    : has50
                      ? '5-0'
                      : null;
          // If a candidate advertises a different version, ignore it entirely.
          if (candidateVersion && candidateVersion !== desiredVersion) {
            return 0;
          }
          // When targeting an explicit version, avoid selecting submenu wrappers that can contain legacy models.
          if (normalizedTestId.includes('submenu') && candidateVersion === null) {
            return 0;
          }
        }
        // Exact testid matches take priority over substring matches
        const exactMatch = TEST_IDS.find((id) => id && normalizedTestId === id);
        if (exactMatch) {
          exactTestIdMatch = true;
          score += 1500;
          if (exactMatch.startsWith('model-switcher-')) score += 200;
        } else {
          const matches = TEST_IDS.filter((id) => id && normalizedTestId.includes(id));
          if (matches.length > 0) {
            // Prefer the most specific match (longest token) instead of treating any hit as equal.
            // This prevents generic tokens (e.g. "pro") from outweighing version-specific targets.
            const best = matches.reduce((acc, token) => (token.length > acc.length ? token : acc), '');
            score += 200 + Math.min(900, best.length * 25);
            if (best.startsWith('model-switcher-')) score += 120;
            if (best.includes('gpt-')) score += 60;
          }
        }
      }
      const candidateGpt55VisibleAlias = isTargetGpt55VisibleAlias(normalizedText);
      const candidateIsNonProThinkingEffort =
        isNonProIntelligenceThinkingLabel(normalizedText) && !normalizedTestId.includes('pro');
      const hasActiveProPill = hasProComposerPill();
      const candidateIsNonProGpt55Thinking =
        wantsThinking && desiredVersion === '5-5' && candidateIsNonProThinkingEffort;
      const candidateClearsProForThinking =
        wantsThinking && !wantsPro && hasActiveProPill && candidateIsNonProThinkingEffort;
      const candidateOpensVersionSubmenu =
        (desiredVersion === '5-6' && candidateIsVersionSubmenuTrigger) ||
        (wantsThinking &&
          desiredVersion !== '5-5' &&
          normalizedText === 'gpt 5 5' &&
          !normalizedTestId.includes('pro'));
      const candidateSelectsDesiredVersion =
        Boolean(desiredVersion) && candidateTextVersion === desiredVersion;
      if (
        desiredVersion &&
        candidateTextVersion &&
        candidateTextVersion !== desiredVersion &&
        !candidateOpensVersionSubmenu
      ) {
        return 0;
      }
      const candidateIsGpt55ThinkingFamily =
        wantsThinking &&
        desiredVersion === '5-5' &&
        (normalizedText === 'gpt 5 5' ||
          normalizedText === '5 5' ||
          normalizedTestId.includes('gpt-5-5') ||
          normalizedTestId.includes('gpt-5.5') ||
          normalizedTestId.includes('gpt55'));
      const candidateHasThinking =
        normalizedText.includes('thinking') ||
        normalizedTestId.includes('thinking') ||
        candidateIsNonProGpt55Thinking ||
        candidateClearsProForThinking ||
        candidateOpensVersionSubmenu ||
        candidateIsGpt55ThinkingFamily ||
        (wantsThinking && desiredVersion && (exactTestIdMatch || candidateSelectsDesiredVersion));
      const candidateHasLegacyProVersion = labelHasLegacyProVersion(normalizedText);
      const candidateHasPro =
        labelHasProWord(normalizedText) ||
        normalizedText.includes('proresearch') ||
        normalizedTestId.includes('pro');
      const candidateHasInstant =
        normalizedText.includes('instant') || normalizedTestId.includes('instant');
      const candidateSelectsConfiguredVersion =
        Boolean(getConfigurationDialog()) &&
        candidateSelectsDesiredVersion &&
        (node?.getAttribute?.('role') === 'option' ||
          node?.getAttribute?.('role') === 'menuitemradio');
      const candidateOpensInstantSubmenu =
        wantsInstant &&
        candidateSelectsDesiredVersion &&
        !candidateHasInstant &&
        (normalizedTestId.includes('submenu') ||
          node?.getAttribute?.('aria-haspopup') === 'menu' ||
          node?.getAttribute?.('data-has-submenu') !== null);
      if (wantsPro && candidateHasThinking) return 0;
      if (desiredModelVariant === 'sol' && candidateHasPro) return 0;
      if (wantsPro && candidateHasLegacyProVersion && !candidateSelectsDesiredVersion) return 0;
      if (wantsPro && !candidateHasPro && !candidateSelectsDesiredVersion) return 0;
      if (
        wantsInstant &&
        !candidateHasInstant &&
        !candidateOpensInstantSubmenu &&
        !candidateSelectsConfiguredVersion
      ) return 0;
      if (wantsThinking && candidateHasPro) return 0;
      if (wantsThinking && !candidateHasThinking && !candidateSelectsDesiredVersion) return 0;
      if (desiredVersion === '5-5' && normalizedText && !candidateGpt55VisibleAlias) {
        const candidateHasVersion =
          normalizedText.includes('5 5') ||
          normalizedText.includes('gpt55') ||
          normalizedText.includes('gpt 5 5');
        const versionLikeLabel = /(?:^|\s)5\s+[0-9](?:\s|$)/.test(normalizedText) || normalizedText.includes('gpt');
        if (versionLikeLabel && !candidateHasVersion) {
          return 0;
        }
      }
      if (candidateGpt55VisibleAlias) {
        score += 900;
      }
      if (wantsPro && candidateHasPro && !candidateSelectsDesiredVersion) {
        score += 900;
      }
      if (wantsInstant && candidateHasInstant && !candidateSelectsDesiredVersion) {
        score += 900;
      }
      if (
        wantsThinking &&
        candidateHasThinking &&
        !candidateSelectsDesiredVersion &&
        !candidateOpensVersionSubmenu
      ) {
        score += 900;
      }
      if (candidateIsNonProGpt55Thinking) {
        score += scoreNonProGpt55ThinkingLabel(normalizedText);
      }
      if (candidateClearsProForThinking) {
        score += scoreNonProGpt55ThinkingLabel(normalizedText) + 600;
      }
      if (
        desiredVersion &&
        candidateTextVersion === desiredVersion &&
        !candidateOpensInstantSubmenu &&
        !(wantsThinking && desiredVersion === '5-5' && normalizedText === 'gpt 5 5')
      ) {
        score += 1200;
      }
      if (candidateOpensInstantSubmenu) {
        score += 300;
      }
      if (candidateOpensVersionSubmenu) {
        score += 500;
      }
      if (candidateIsGpt55ThinkingFamily) {
        score += 260;
      }
      if (normalizedText && normalizedTarget) {
        if (normalizedText === normalizedTarget) {
          score += 500;
        } else if (normalizedText.startsWith(normalizedTarget)) {
          score += 420;
        } else if (normalizedText.includes(normalizedTarget)) {
          score += 380;
        }
      }
      for (const token of normalizedTokens) {
        // Reward partial matches to the expanded label/token set.
        if (token && normalizedText.includes(token)) {
          const tokenWeight = Math.min(120, Math.max(10, token.length * 4));
          score += tokenWeight;
        }
      }
      if (targetWords.length > 1) {
        let missing = 0;
        for (const word of targetWords) {
          if (!normalizedText.includes(word)) {
            missing += 1;
          }
        }
        score -= missing * 12;
      }
      // If the caller didn't explicitly ask for Pro, prefer non-Pro options when both exist.
      if (wantsPro) {
        if (!labelHasProWord(normalizedText)) {
          score -= 80;
        }
      } else if (labelHasProWord(normalizedText)) {
        score -= 40;
      }
      // Similarly for Thinking variant
      if (wantsThinking) {
        if (
          !candidateIsGpt55ThinkingFamily &&
          !candidateIsNonProGpt55Thinking &&
          !candidateClearsProForThinking &&
          !candidateOpensVersionSubmenu &&
          !normalizedText.includes('thinking') &&
          !normalizedTestId.includes('thinking')
        ) {
          score -= 80;
        }
      } else if (normalizedText.includes('thinking') || normalizedTestId.includes('thinking')) {
        score -= 40;
      }
      // Similarly for Instant variant
      if (wantsInstant) {
        if (!normalizedText.includes('instant') && !normalizedTestId.includes('instant')) {
          score -= 80;
        }
      } else if (normalizedText.includes('instant') || normalizedTestId.includes('instant')) {
        score -= 40;
      }
      return Math.max(score, 0);
    };

    const hasModelSwitcherItem = (node) =>
      Boolean(node?.querySelector?.('[data-testid^="model-switcher-"]'));
    const hasModelLikeMenuText = (node) => {
      const text = normalizeText(node?.textContent ?? '');
      return (
        text.includes('instant') ||
        text.includes('thinking') ||
        labelHasProWord(text) ||
        text.includes('5 5') ||
        text.includes('5 4') ||
        text.includes('5 2') ||
        text.includes('gpt 5') ||
        text.includes('gpt5')
      );
    };
    const queryPickerMenus = () => {
      const menus = Array.from(document.querySelectorAll('[role="menu"], [data-radix-collection-root], [role="listbox"], [role="dialog"]'));
      const pickerMenus = menus.filter(hasModelSwitcherItem);
      if (pickerMenus.length === 0) return menus;
      const textFallbackMenus = menus.filter(
        (menu) => !pickerMenus.includes(menu) && hasModelLikeMenuText(menu),
      );
      return pickerMenus.concat(textFallbackMenus);
    };
    const isSubmenuOption = (node, testid) =>
      (testid ?? '').toLowerCase().includes('submenu') ||
      (testid ?? '').toLowerCase() === 'model-configure-modal' ||
      (node?.getAttribute?.('role') === 'combobox' &&
        node?.getAttribute?.('aria-labelledby') === 'model-selection-label') ||
      node?.getAttribute?.('aria-haspopup') === 'menu' ||
      node?.getAttribute?.('data-has-submenu') !== null;
    const canTrustSelectedOption = (node, normalizedText, testid) => {
      if (!optionIsSelected(node)) return false;
      if (getConfigurationDialog() && !configuredSelectionMatchesTarget()) return false;
      const optionVersion = versionFromLabel(normalizedText) ?? versionFromTestId(testid);
      if (desiredVersion && optionVersion !== desiredVersion) return false;
      if (desiredModelVariant && !normalizedText.includes(desiredModelVariant)) return false;
      if (
        desiredModelVariant === 'sol' &&
        (labelHasProWord(normalizedText) || normalizeText(testid ?? '').includes('pro'))
      ) return false;
      if (desiredVersion === '5-6') return true;
      const currentButtonLabel = normalizeText(getButtonLabel());
      return !labelHasProWord(currentButtonLabel) && !hasProComposerPill();
    };
    const openedSubmenuKeys = new Set();
    let clickedTargetOption = false;
    const submenuKey = (normalizedText, testid) =>
      normalizeText(testid ?? '') + '|' + normalizedText;

    const findBestOption = () => {
      // Walk through every menu item and keep whichever earns the highest score.
      let bestMatch = null;
      const menus = queryPickerMenus();
      for (const menu of menus) {
        const buttons = Array.from(menu.querySelectorAll('button, [role="menuitem"], [role="menuitemradio"], [data-testid*="model-switcher-"], [role="option"], [role="radio"], [role="combobox"]'));
        for (const option of buttons) {
          if (!isVisibleElement(option)) {
            continue;
          }
          if (isNestedEffortControl(option, menu)) {
            continue;
          }
          const text = option.textContent ?? '';
          const normalizedText = normalizeText(text);
          const testid = option.getAttribute('data-testid') ?? '';
          const optionSubmenuKey = submenuKey(normalizedText, testid);
          if (isSubmenuOption(option, testid) && openedSubmenuKeys.has(optionSubmenuKey)) {
            continue;
          }
          let score = scoreOption(normalizedText, testid, option);
          if (score <= 0) {
            continue;
          }
          if (canTrustSelectedOption(option, normalizedText, testid)) {
            score += 1000;
          }
          const label = getOptionLabel(option);
          if (!bestMatch || score > bestMatch.score) {
            bestMatch = {
              node: option,
              label,
              score,
              testid,
              normalizedText,
              submenuKey: optionSubmenuKey,
            };
          }
        }
      }
      return bestMatch;
    };
    const waitForTargetSelection = (previousButtonLabel, previousComposerSignal) => new Promise((resolve) => {
      const waitStart = performance.now();
      const check = () => {
        if (typeof oracleSubmissionIsActive === 'function' && !oracleSubmissionIsActive()) { resolve('cancelled'); return; }
        if (activeSelectionMatchesTarget()) {
          resolve('target');
          return;
        }
        const currentButtonLabel = normalizeText(getButtonLabel());
        if (
          wantsInstant &&
          desiredVersion === '5-5' &&
          currentButtonLabel === 'instant' &&
          currentButtonLabel !== previousButtonLabel
        ) {
          resolve('target');
          return;
        }
        if (selectionStateChanged(previousButtonLabel, previousComposerSignal)) {
          resolve('changed');
          return;
        }
        if (performance.now() - waitStart > SETTLE_WAIT_MS) {
          resolve('timeout');
          return;
        }
        setTimeout(check, 100);
      };
      check();
    });
    const dispatchHoverSequence = (target) => {
      if (typeof oracleSubmissionIsActive === 'function' && !oracleSubmissionIsActive()) return false;
      if (!target || !(target instanceof EventTarget)) return false;
      const rect = target.getBoundingClientRect?.();
      const clientX = rect ? rect.x + rect.width / 2 : 0;
      const clientY = rect ? rect.y + rect.height / 2 : 0;
      const types = ['pointerover', 'pointerenter', 'mouseover', 'mouseenter', 'pointermove', 'mousemove'];
      for (const type of types) {
        if (typeof oracleSubmissionIsActive === 'function' && !oracleSubmissionIsActive()) return false;
        try {
          const common = { bubbles: true, cancelable: true, view: window, clientX, clientY };
          const event =
            type.startsWith('pointer') && 'PointerEvent' in window
              ? new PointerEvent(type, { ...common, pointerId: 1, pointerType: 'mouse' })
              : new MouseEvent(type, common);
          target.dispatchEvent(event);
        } catch {}
      }
      try {
        if (typeof oracleSubmissionIsActive === 'function' && !oracleSubmissionIsActive()) return false;
        target.focus?.();
      } catch {}
      return true;
    };
    const openSubmenuOption = (node) => {
      dispatchHoverSequence(node);
      dispatchClickSequence(node);
    };
    const descendIntoAdvancedModelPicker = () => {
      const parentMenu = findUnifiedPickerMenu();
      if (!parentMenu) return 'unavailable';

      const opener = findModelSubmenuOpener(parentMenu);
      if (opener) {
        if (advancedModelSignalMatchesTarget(parentMenu)) {
          return 'already-selected';
        }
        const key = submenuKey(
          normalizeText(opener.textContent ?? ''),
          opener.getAttribute?.('data-testid') ?? '',
        );
        openedSubmenuKeys.add(key);
        if (opener.getAttribute?.('aria-expanded') !== 'true') {
          openSubmenuOption(opener);
        }
        return 'opened';
      }

      const advancedToggle = findAdvancedToggle(parentMenu);
      if (!advancedToggle) return 'unavailable';
      if (advancedToggle.getAttribute?.('aria-expanded') !== 'true') {
        dispatchClickSequence(advancedToggle);
      }
      return 'expanded';
    };

    return new Promise((resolve) => {
      const start = performance.now();
      const detectTemporaryChat = () => {
        try {
          const url = new URL(window.location.href);
          const flag = (url.searchParams.get('temporary-chat') ?? '').toLowerCase();
          if (flag === 'true' || flag === '1' || flag === 'yes') return true;
        } catch {}
        const title = (document.title || '').toLowerCase();
        if (title.includes('temporary chat')) return true;
        const body = (document.body?.innerText || '').toLowerCase();
        return body.includes('temporary chat');
      };
      const collectAvailableOptions = () => {
        const menuRoots = queryPickerMenus();
        const nodes = menuRoots.flatMap((root) => Array.from(root.querySelectorAll('button, [role="menuitem"], [role="menuitemradio"], [data-testid*="model-switcher-"], [role="option"], [role="radio"], [role="combobox"]')));
        const labels = nodes
          .map((node) => (node?.textContent ?? '').trim())
          .filter(Boolean)
          .filter((label, index, arr) => arr.indexOf(label) === index);
        return labels.slice(0, 12);
      };
      const ensureMenuOpen = () => {
        const menuOpen = queryPickerMenus().length > 0;
        if (!menuOpen && performance.now() - lastPointerClick > REOPEN_INTERVAL_MS) {
          pointerClick();
        }
      };

      // Open once and wait a tick before first scan.
      pointerClick();
      const openDelay = () => new Promise((r) => setTimeout(r, INITIAL_WAIT_MS));
      let initialized = false;
      const attempt = async () => {
        if (typeof oracleSubmissionIsActive === 'function' && !oracleSubmissionIsActive()) { resolve({ status: 'ownership-inactive' }); return; }
        if (performance.now() - start > MAX_WAIT_MS) {
          resolve({
            status: 'option-not-found',
            hint: { temporaryChat: detectTemporaryChat(), availableOptions: collectAvailableOptions() },
          });
          return;
        }
        if (!initialized) {
          initialized = true;
          await openDelay();
        }
        if (typeof oracleSubmissionIsActive === 'function' && !oracleSubmissionIsActive()) { resolve({ status: 'ownership-inactive' }); return; }
        ensureMenuOpen();
        const match = findBestOption();
        if (match) {
          if (
            activeSelectionMatchesTarget() ||
            canTrustSelectedOption(match.node, match.normalizedText, match.testid)
          ) {
            const resolvedLabel = getResolvedLabel(match.label);
            closeMenu();
            resolve({
              status: clickedTargetOption ? 'switched' : 'already-selected',
              label: resolvedLabel,
            });
            return;
          }
          const previousButtonLabel = normalizeText(getButtonLabel());
          const previousComposerSignal = readComposerModelSignal();
          // Submenus (e.g. "Legacy models") need a second pass to pick the actual model option.
          // Keep scanning once the submenu opens instead of treating the submenu click as a final switch.
          const isSubmenu = isSubmenuOption(match.node, match.testid);
          if (isSubmenu) {
            openedSubmenuKeys.add(match.submenuKey);
            openSubmenuOption(match.node);
            setTimeout(attempt, REOPEN_INTERVAL_MS / 2);
            return;
          }
          clickedTargetOption = true;
          dispatchClickSequence(match.node);
          // Wait for the selected model signal to settle before reopening the picker.
          waitForTargetSelection(previousButtonLabel, previousComposerSignal).then((selectionSettled) => {
            if (selectionSettled === 'target') {
              const resolvedLabel = getResolvedLabel(match.label);
              closeMenu();
              resolve({ status: 'switched', label: resolvedLabel });
              return;
            }
            attempt();
          });
          return;
        }
        const advancedState = descendIntoAdvancedModelPicker();
        if (advancedState === 'already-selected') {
          const resolvedLabel = getResolvedLabel();
          closeMenu();
          resolve({ status: 'already-selected', label: resolvedLabel });
          return;
        }
        if (advancedState === 'expanded' || advancedState === 'opened') {
          setTimeout(attempt, REOPEN_INTERVAL_MS / 2);
          return;
        }
        if (performance.now() - start > MAX_WAIT_MS) {
          resolve({
            status: 'option-not-found',
            hint: { temporaryChat: detectTemporaryChat(), availableOptions: collectAvailableOptions() },
          });
          return;
        }
        setTimeout(attempt, REOPEN_INTERVAL_MS / 2);
      };
      attempt();
    });
  })();
}
