// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* alcuin-dom:ui-warnings */

return (() => {
        const warningPattern = /too many requests|sending too many requests|too quickly|temporarily limited access|please wait a few minutes|rate limit|rate limited|slow down|try again later|temporarily unavailable|something went wrong|failed to generate|verify you are human|unusual activity|cloudflare|challenge|login required|sign in/i;
        const selectors = [
          '[role="alert"]',
          '[role="status"]',
          '[role="dialog"]',
          '[aria-live]',
          '[data-testid*="toast" i]',
          '[data-testid*="banner" i]',
          '[data-testid*="error" i]',
          '[class*="toast" i]',
          '[class*="banner" i]'
        ];
        const isVisible = (element) => {
          if (!(element instanceof HTMLElement)) return false;
          let current = element;
          while (current) {
            const currentStyle = window.getComputedStyle(current);
            if (
              !currentStyle ||
              currentStyle.display === 'none' ||
              currentStyle.visibility === 'hidden' ||
              currentStyle.visibility === 'collapse' ||
              Number.parseFloat(currentStyle.opacity || '1') === 0
            ) {
              return false;
            }
            current = current.parentElement;
          }
          const rect = element.getBoundingClientRect();
          return rect.width > 0 && rect.height > 0;
        };
        const describe = (element, source, selector = null) => ({
          text: (element.innerText || element.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 1000),
          source,
          selector,
          role: element.getAttribute('role'),
          ariaLive: element.getAttribute('aria-live')
        });
        const out = [];
        const seen = new Set();
        const warningContainers = [];
        const overlapsWarningContainer = (element) => warningContainers.some((container) => (
          container !== element && (container.contains(element) || element.contains(container))
        ));
        const add = (element, entry) => {
          if (!entry.text || !warningPattern.test(entry.text)) return;
          const key = entry.text + '|' + (entry.role || '') + '|' + (entry.ariaLive || '');
          if (seen.has(key)) return;
          seen.add(key);
          warningContainers.push(element);
          out.push(entry);
        };
        for (const selector of selectors) {
          if (out.length >= 5) break;
          let elements = [];
          try {
            elements = Array.from(document.querySelectorAll(selector));
          } catch {
            elements = [];
          }
          for (const element of elements) {
            if (out.length >= 5) break;
            if (overlapsWarningContainer(element)) continue;
            if (isVisible(element)) add(element, describe(element, 'selector', selector));
          }
        }
        return out.slice(0, 5);
      })();
}
