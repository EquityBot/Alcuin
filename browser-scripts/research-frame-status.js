// Derived from maintained Deep Research status expressions (MIT).
(() => {
    const rawText = document.body?.innerText || '';
    const html = document.body?.innerHTML || '';
    const cleanText = (value) => String(value || '').replace(/\s+/g, ' ').trim();
    const sections = typeof document.querySelectorAll === 'function'
      ? Array.from(document.querySelectorAll('section'))
      : [];
    const planSection = sections.find((section) => {
      if (typeof section.querySelector !== 'function' ||
          typeof section.querySelectorAll !== 'function' ||
          !cleanText(section.querySelector('h2')?.textContent) ||
          section.querySelectorAll('ul li').length === 0) {
        return false;
      }
      const buttons = Array.from(section.querySelectorAll('button'));
      const hasPlanAction = buttons.some((button) =>
        /^(edit|update|编辑|更新)$/i.test(cleanText(button.textContent))
      );
      const hasResearchStatus = Boolean(section.querySelector('p.loading-shimmer')) ||
        buttons.some((button) =>
          /stop research|停止研究/i.test(cleanText(button.getAttribute?.('aria-label')))
        );
      return hasPlanAction || hasResearchStatus;
    });
    const planTitle = cleanText(planSection?.querySelector?.('h2')?.textContent);
    const planSteps = planSection && typeof planSection.querySelectorAll === 'function'
      ? Array.from(planSection.querySelectorAll('ul li'))
          .map((item) => cleanText(item.textContent))
          .filter(Boolean)
      : [];
    const planActionText = planSection && typeof planSection.querySelectorAll === 'function'
      ? Array.from(planSection.querySelectorAll('button'))
          .map((button) => cleanText(button.textContent))
          .find((text) => /^(edit|update|编辑|更新)$/i.test(text)) || ''
      : '';
    const hasResearchShimmer = Boolean(planSection?.querySelector?.('p.loading-shimmer'));
    const hasStopResearchControl = typeof planSection?.querySelectorAll === 'function' &&
      Array.from(planSection.querySelectorAll('button')).some((button) =>
        /stop research|停止研究/i.test(cleanText(button.getAttribute?.('aria-label')))
      );
    const researchStarted = planSteps.length > 0 && (hasResearchShimmer || hasStopResearchControl);
    const isPlaceholder = (line) => /^(called tool|used tool|użyto narzędzia|narzędzie wywołane)$/i.test(line);
    const isCompletionLine = (line) =>
      /^(research completed|badanie ukończone)\b/i.test(line);
    const isCounterLine = (line) =>
      /^(\d+\s+)?(citation|citations|source|sources|search|searches|cytat|cytaty|cytatów|źródło|źródła|wyszukiwanie|wyszukiwania|wyszukiwań)\b/i.test(line);
    const normalizeReport = (text) => {
      const lines = String(text || '')
        .split(/\n+/)
        .map((line) => line.trim())
        .filter(Boolean)
        .filter((line) => !/^\d+$/.test(line));
      const reportIndex = lines.findIndex((line) => /deep research report/i.test(line));
      const candidates = reportIndex >= 0 ? lines.slice(reportIndex + 1) : lines;
      let started = false;
      const reportLines = candidates.filter((line) => {
        if (!started) {
          if (
            /deep research report/i.test(line) ||
            isCompletionLine(line) ||
            isCounterLine(line) ||
            isPlaceholder(line)
          ) {
            return false;
          }
          started = true;
        }
        return true;
      });
      if (reportLines.length > 1 && reportLines[0] === reportLines[1]) {
        reportLines.shift();
      }
      return reportLines.join('\n').trim();
    };
    const reportText = normalizeReport(rawText);
    const completed = /research completed|badanie ukończone/i.test(rawText) &&
      reportText.length >= 40 &&
      !isPlaceholder(reportText);
    const inProgress = /researching|badanie|searching|searches|wyszukiwa|citation|cytat|source|źród|reading|completed|ukończone/i.test(rawText);
    return {
      completed,
      inProgress,
      researchStarted,
      planTitle: planTitle || undefined,
      planSteps: planSteps.length > 0 ? planSteps : undefined,
      planActionText: planActionText || undefined,
      textLength: reportText.length || rawText.trim().length,
      text: completed ? reportText : undefined,
      html: completed ? html : undefined,
    };
  })()
