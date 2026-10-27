const isGeneratedImage = (img) => {
      const url = new URL(img?.src || '', location.origin || 'https://chatgpt.com');
      const host = url.hostname.toLowerCase();
      if (url.protocol !== 'https:' || url.port) return false;
      if (host !== 'chatgpt.com' && host !== 'chat.openai.com') return false;
      if (url.pathname !== '/backend-api/estuary/content') return false;
      if (!String(url.searchParams.get('id') || '').startsWith('file_')) return false;
      const alt = String(img.alt || '').toLowerCase();
      if (alt.includes('generated image')) return true;
      let node = img;
      while (node instanceof HTMLElement) {
        if (String(node.id || '').startsWith('image-')) return true;
        if (String(node.className || '').includes('imagegen-image')) return true;
        node = node.parentElement;
      }
      return false;
    };
    const serializeImages = (root) =>
      Array.from(root.querySelectorAll('img')).filter(isGeneratedImage).map((img) => ({
        url: img.src || '',
        alt: img.alt || '',
        width: img.naturalWidth || 0,
        height: img.naturalHeight || 0,
      }));

const isChatGptDownloadUrl = (value) => {
      const raw = String(value || '').trim();
      if (!raw || raw.startsWith('sandbox:') || raw.startsWith('blob:')) return false;
      const isSafeSandboxPath = (path) => {
        const value = String(path || '');
        return value.startsWith('/mnt/data/') &&
          !value.includes('\\') &&
          !value.includes('\0') &&
          !value.split('/').includes('..');
      };
      try {
        const url = new URL(raw, location.origin || 'https://chatgpt.com');
        const host = url.hostname.toLowerCase();
        const allowedHost = host === 'chatgpt.com' || host === 'chat.openai.com';
        const pathName = url.pathname.toLowerCase();
        const isKnownFileDownload =
          (pathName === '/backend-api/sandbox/download' && isSafeSandboxPath(url.searchParams.get('path') || '')) ||
          /^\/backend-api\/files\/[^/]+\/(?:download|content)\/?$/.test(pathName) ||
          (pathName === '/backend-api/estuary/content' && String(url.searchParams.get('id') || '').startsWith('file_'));
        return allowedHost && url.protocol === 'https:' && !url.port && isKnownFileDownload;
      } catch {
        return false;
      }
    };
    const isSandboxUrl = (value) => {
      const raw = String(value || '').trim();
      if (!raw.startsWith('sandbox:/mnt/data/')) return false;
      try {
        return decodeURI(new URL(raw).pathname).startsWith('/mnt/data/') &&
          !decodeURI(new URL(raw).pathname).includes('\\') &&
          !decodeURI(new URL(raw).pathname).includes('\0') &&
          !decodeURI(new URL(raw).pathname).split('/').includes('..');
      } catch {
        return false;
      }
    };
    const basename = (value) => {
      const raw = String(value || '').split(/[?#]/)[0].replace(/\/+$/g, '');
      const part = raw.slice(raw.lastIndexOf('/') + 1);
      try {
        return decodeURIComponent(part);
      } catch {
        return part;
      }
    };
    const hrefKind = (value) => {
      if (!value) return '';
      if (isSandboxUrl(value)) return 'sandbox';
      if (isChatGptDownloadUrl(value)) return 'chatgpt-file-endpoint';
      return '';
    };
    const collectValues = (node) => {
      const values = [];
      if (String(node.tagName || '').toLowerCase() === 'a') {
        values.push(node.getAttribute('href') || '', node.href || '', node.getAttribute('download') || '');
      }
      for (const attribute of Array.from(node.attributes || [])) {
        values.push(String(attribute.value || ''));
      }
      for (const anchor of Array.from(node.querySelectorAll?.('a[href], a[download]') || [])) {
        values.push(anchor.getAttribute('href') || '', anchor.href || '', anchor.getAttribute('download') || '');
        for (const attribute of Array.from(anchor.attributes || [])) {
          values.push(String(attribute.value || ''));
        }
      }
      return values;
    };
    const serializeCandidate = (node, candidateId, allowGenericDownload = false) => {
      if (!(node instanceof HTMLElement)) return null;
      const values = collectValues(node);
      const downloadUrl = values.find(isChatGptDownloadUrl) || '';
      const sandboxUrl = values.find(isSandboxUrl) || '';
      const downloadLabel = (node.getAttribute('aria-label') || '').trim();
      // A file card's 'Download file' button names its file only through the card's
      // 'Open preview of <name>' button (observed 27 October 2026); the nearest ancestor within the
      // same answer holding a preview button is the card, and it must hold exactly one preview and
      // one Download file button, or the name is ambiguous.
      const answer = node.closest(':is([data-message-author-role="assistant"], [data-content-search-unit-key$=":assistant"], [data-chatgpt-search-unit-key$=":assistant"])');
      let card = downloadLabel === 'Download file' && node.matches('button') && answer ? node.parentElement : null;
      while (card && answer.contains(card) && !card.querySelector('button[aria-label^="Open preview of "]')) card = card.parentElement;
      if (card && !answer.contains(card)) card = null;
      const previews = card ? card.querySelectorAll('button[aria-label^="Open preview of "]') : [];
      const downloads = card ? card.querySelectorAll('button[aria-label="Download file"]') : [];
      const cardLabel = previews.length === 1 && downloads.length === 1 ? (previews[0].getAttribute('aria-label') || '').trim() : '';
      const labelFilename = (/^Download\s+(.+\.[a-z0-9]{1,10})$/i.exec(downloadLabel) ||
        /^Open preview of\s+(.+\.[a-z0-9]{1,10})$/i.exec(cardLabel))?.[1]?.trim() || '';
      const safeLabelFilename =
        labelFilename && labelFilename.length <= 255 &&
        !labelFilename.includes('/') && !labelFilename.includes('\\') &&
        labelFilename !== '.' && labelFilename !== '..';
      const genericDownload = allowGenericDownload &&
        /download/i.test([node.textContent || '', ...values].join(' ')) &&
        (node.matches('button,[role="button"]') ||
          node.matches('a') && (downloadUrl || sandboxUrl));
      if (!downloadUrl && !sandboxUrl && !safeLabelFilename && !genericDownload) return null;
      const label = (node.textContent || node.getAttribute('aria-label') || node.getAttribute('title') || '').trim();
      const downloadAttr = String(node.tagName || '').toLowerCase() === 'a' ? node.getAttribute('download') || '' : '';
      const filename = downloadAttr || basename(sandboxUrl) || basename(downloadUrl) || labelFilename || label || '';
      return {
        url: downloadUrl || sandboxUrl || values.find(hrefKind) || 'browser-download',
        candidateId: !downloadUrl && !sandboxUrl ? candidateId : '',
        downloadUrl,
        sandboxUrl,
        filename,
        label,
        mimeType: node.getAttribute('type') || '',
      };
    };
    const serializeFiles = (root, turnIndex) =>
      Array.from(root.querySelectorAll([
        'a[href]',
        'a[download]',
        'button',
        '[role="button"]',
        '[data-testid]',
        '[aria-label]',
        '[title]',
      ].join(',')))
        .map((node, controlIndex) => serializeCandidate(node, turnIndex + ':' + controlIndex))
        .filter(Boolean);
    const serializeImageButtons = (root, turnIndex) =>
      Array.from(root.querySelectorAll('a[href],a[download],button,[role="button"],[data-testid],[aria-label],[title]'))
        .map((node, controlIndex) => {
          if (!(node instanceof HTMLElement) || node.disabled || node.getAttribute('aria-disabled') === 'true') return null;
          const box = node.getBoundingClientRect();
          const style = getComputedStyle(node);
          if (!box.width || !box.height || style.display === 'none' || style.visibility === 'hidden') return null;
          const candidate = serializeCandidate(node, turnIndex + ':' + controlIndex, true);
          return candidate && /download/i.test([node.textContent || '', ...collectValues(node)].join(' '))
            ? {...candidate, candidateId: turnIndex + ':' + controlIndex, imageButton: true} : null;
        }).filter(Boolean);
