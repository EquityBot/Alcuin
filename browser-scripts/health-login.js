// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* alcuin-dom:health-login */

return (async () => {
    // /api/auth/session remains cookie-authenticated and exposes user presence without
    // requiring the bearer token used by /backend-api/*. Never return or log its token.
    const pageUrl = typeof location === 'object' && location?.href ? location.href : null;
    const onAuthPage =
      typeof location === 'object' &&
      typeof location.pathname === 'string' &&
      /^\/(auth|login|signin)/i.test(location.pathname);

    const hasLoginCta = () => {
      const candidates = Array.from(
        document.querySelectorAll(
          [
            'a[href*="/auth/login"]',
            'a[href*="/auth/signin"]',
            'button[type="submit"]',
            'button[data-testid*="login"]',
            'button[data-testid*="log-in"]',
            'button[data-testid*="sign-in"]',
            'button[data-testid*="signin"]',
            'button',
            'a',
          ].join(','),
        ),
      );
      const textMatches = (text) => {
        if (!text) return false;
        const normalized = text.toLowerCase().trim();
        return (
          ['log in', 'login', 'sign in', 'signin', 'sign up for free'].includes(normalized) ||
          normalized.startsWith('continue with') ||
          normalized.includes('get responses tailored to you') ||
          normalized.includes('log in to get answers')
        );
      };
      for (const node of candidates) {
        if (!(node instanceof HTMLElement)) continue;
        const rect = node.getBoundingClientRect();
        const style = window.getComputedStyle(node);
        if (
          rect.width <= 0 ||
          rect.height <= 0 ||
          style.display === 'none' ||
          style.visibility === 'hidden'
        ) {
          continue;
        }
        const label =
          node.textContent?.trim() ||
          node.getAttribute('aria-label') ||
          node.getAttribute('title') ||
          '';
        if (textMatches(label)) {
          return true;
        }
      }
      return false;
    };

    // Learned 2026-05-16: ChatGPT's /backend-api/* endpoints now sit behind Cloudflare bot
    // mitigation. Programmatic fetch from the page can return 403 with cf-mitigated:challenge
    // even when the user is logged in via cookies and the SPA renders normally. Detect that
    // case via the response body shape (Cloudflare interstitial HTML) and fall back to a
    // DOM-based logged-in signal instead of looping waitForLogin until the 20-min timeout.
    const isCloudflareBody = (body) => {
      if (typeof body !== 'string' || body.length === 0) return false;
      const head = body.slice(0, 2000).toLowerCase();
      return (
        head.includes('cf-mitigated') ||
        head.includes('cloudflare') ||
        (head.includes('<style global>') && head.includes('scale-appear'))
      );
    };
    const readSessionDetail = async () => {
      try {
        if (typeof fetch !== 'function') {
          return { status: 0, resolved: false, authenticated: false, cfBlocked: false, error: null };
        }
        const controller = new AbortController();
        const timeout = setTimeout(() => controller.abort(), cfg.timeoutMs);
        try {
          const response = await fetch('/api/auth/session', {
            cache: 'no-store',
            credentials: 'include',
            signal: controller.signal,
          });
          let cfBlocked = false;
          if (response.status === 403 || response.status === 503 || response.status === 429) {
            try {
              const text = await response.clone().text();
              cfBlocked = isCloudflareBody(text);
            } catch {}
          }
          if (response.status !== 200) {
            return {
              status: response.status || 0,
              resolved: false,
              authenticated: false,
              cfBlocked,
              error: null,
            };
          }
          try {
            const body = await response.json();
            const resolved =
              Boolean(body) && typeof body === 'object' && !Array.isArray(body);
            return {
              status: response.status || 0,
              resolved,
              authenticated: resolved && Boolean(body.user),
              cfBlocked,
              error: null,
            };
          } catch {
            return {
              status: response.status || 0,
              resolved: false,
              authenticated: false,
              cfBlocked,
              error: null,
            };
          }
        } finally {
          clearTimeout(timeout);
        }
      } catch (err) {
        return {
          status: 0,
          resolved: false,
          authenticated: false,
          cfBlocked: false,
          error: err ? String(err) : 'unknown',
        };
      }
    };
    const readBackendDetail = async () => {
      try {
        if (typeof fetch !== 'function') return { status: 0, cfBlocked: false, error: null };
        const controller = new AbortController();
        const timeout = setTimeout(() => controller.abort(), cfg.timeoutMs);
        try {
          const response = await fetch('/backend-api/me', {
            cache: 'no-store',
            credentials: 'include',
            signal: controller.signal,
          });
          let cfBlocked = false;
          if (response.status === 403 || response.status === 503 || response.status === 429) {
            try {
              const text = await response.clone().text();
              cfBlocked = isCloudflareBody(text);
            } catch {}
          }
          return { status: response.status || 0, cfBlocked, error: null };
        } finally {
          clearTimeout(timeout);
        }
      } catch (err) {
        return { status: 0, cfBlocked: false, error: err ? String(err) : 'unknown' };
      }
    };
    const readAuthDetail = async () => {
      const session = await readSessionDetail();
      const sessionDenied =
        !session.cfBlocked && (session.status === 401 || session.status === 403);
      if (session.resolved || sessionDenied) {
        return { session, backend: null };
      }
      return { session, backend: await readBackendDetail() };
    };

    const hasAppAuthSignal = () => {
      // Composer must be present and visible — the auth/login page never renders one.
      if (typeof document.querySelector !== 'function') return false;
      const composerSelectors = [
        '#prompt-textarea',
        '.ProseMirror',
        'textarea[data-id="prompt-textarea"]',
        'textarea[name="prompt-textarea"]',
        '[contenteditable="true"][role="textbox"]',
      ];
      const composer = composerSelectors.map((s) => document.querySelector(s)).find(Boolean);
      if (!composer) return false;
      const rect = composer.getBoundingClientRect && composer.getBoundingClientRect();
      const style = window.getComputedStyle(composer);
      if (!rect || rect.width <= 0 || rect.height <= 0 || style.display === 'none' || style.visibility === 'hidden') {
        return false;
      }
      // Logged-in users should have an account affordance or prior chat history. Generic
      // composer/model pills also appear in guest sessions, so they are not auth proof.
      const profileButton = document.querySelector('[data-testid="accounts-profile-button"]');
      const historyItem = document.querySelector('[data-testid^="history-item-"]');
      return Boolean(profileButton || historyItem);
    };

    const classifyAuth = (auth, appSignal) => {
      const sessionResolved = auth.session.status === 200 && auth.session.resolved;
      const sessionDenied =
        !auth.session.cfBlocked &&
        (auth.session.status === 401 || auth.session.status === 403);
      const sessionUnavailable = !sessionResolved && !sessionDenied;
      const backendStatus = auth.backend ? auth.backend.status : null;
      const backendUnavailable =
        Boolean(auth.backend) &&
        (auth.backend.cfBlocked ||
          backendStatus === 0 ||
          backendStatus === 401 ||
          backendStatus === 403 ||
          backendStatus === 429 ||
          backendStatus === 503);
      return {
        authenticated:
          auth.session.authenticated ||
          (sessionUnavailable &&
            (backendStatus === 200 || (backendUnavailable && appSignal))),
        sessionResolved,
        sessionUnavailable,
      };
    };

    let auth = await readAuthDetail();
    let domLoginCta = hasLoginCta();
    let appAuthenticated = hasAppAuthSignal();
    let classification = classifyAuth(auth, appAuthenticated);
    const settleDeadline = Date.now() + Math.min(cfg.timeoutMs, 2500);
    while (
      !domLoginCta &&
      !classification.authenticated &&
      classification.sessionUnavailable &&
      Date.now() < settleDeadline
    ) {
      await new Promise((resolve) => setTimeout(resolve, 100));
      domLoginCta = hasLoginCta();
      appAuthenticated = hasAppAuthSignal();
      auth = await readAuthDetail();
      classification = classifyAuth(auth, appAuthenticated);
    }

    const loginSignals = domLoginCta || onAuthPage;
    const backendStatus = auth.backend ? auth.backend.status : null;
    const cfBlocked = auth.session.cfBlocked || Boolean(auth.backend?.cfBlocked);
    const error = auth.session.error || auth.backend?.error || null;
    const ok = !loginSignals && classification.authenticated;
    return {
      ok,
      status: auth.session.status,
      backendStatus,
      redirected: false,
      url: pageUrl,
      pageUrl,
      domLoginCta,
      onAuthPage,
      appAuthenticated,
      cfBlocked,
      sessionAuthenticated: auth.session.authenticated,
      sessionResolved: classification.sessionResolved,
      error,
    };
  })();
}
