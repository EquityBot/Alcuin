// Derived from maintained Web Search and composer controls (MIT).
(() => {
        const guards = window.__oracleAttachmentPlusGuards;
        const guard = guards?.[cfg.plusGuardId];
        const nodes = window.__oracleAttachmentPlusNodes;
        if (nodes) delete nodes[cfg.plusGuardId];
        if (!guard) return null;
        const summary = { cleaned: true, sawKeyDown: guard.sawKeyDown, clicked: guard.clicked, blocked: guard.blocked };
        guard.cleanup(); delete guards[cfg.plusGuardId]; return summary;
      })()
