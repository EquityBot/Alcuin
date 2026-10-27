// Ported from the reference implementation's DOM expressions.
(cfg) => {
/* industrious-dom:blocked */

return (() => {
    const title = String(document.title || '').toLowerCase();
    const titleSaysChallenge =
      title.includes("just a moment") ||
      (title.includes('attention required') && title.includes('cloudflare'));
    const hasAppShell = Boolean(document.querySelector(
      'form[data-chatgpt-composer], [data-turn-key], #prompt-textarea, [data-testid="prompt-textarea"], [data-testid^="conversation-turn"], [data-testid="profile-button"], main form[data-type], nav a[href*="/c/"]'
    ));
    const bodyText = String((document.body && document.body.innerText) || '')
      .toLowerCase().replace(/\s+/g, ' ').trim();
    const isShortPage = bodyText.length < 600;
    const hasChallengeWidget = Boolean(document.querySelector(
      '#challenge-form, #challenge-running, #cf-challenge-running, [class*="cf-challenge"], iframe[src*="challenges.cloudflare.com"], iframe[src*="/cdn-cgi/challenge-platform/"]'
    ));
    const saysVerifying = /verify(ing)? you are human|checking your browser|needs to review the security of your connection|just a moment/.test(bodyText);
    const hasChallengeScript = Boolean(document.querySelector("script[src*=\"/challenge-platform/\"]"));
    const securityText = String(document.body?.innerText || '').toLowerCase().replace(/\s+/g, ' ');
    return {
      strong:
        !hasAppShell &&
        (titleSaysChallenge || hasChallengeWidget || (isShortPage && saysVerifying)),
      shell: hasAppShell,
      // A genuine interstitial is a SHORT page; the content-rich app never is. So the script
      // only counts on a short page with no app shell.
      weak: !hasAppShell && hasChallengeScript && isShortPage,
      securityBlock: securityText.includes('suspicious activity detected') &&
        securityText.includes('secure your account') &&
        securityText.includes('regain access'),
    };
  })();
}
