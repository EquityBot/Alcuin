const readOracleEditorText = node => {
    if (!node) return '';
    if (node instanceof HTMLTextAreaElement) return (node.value ?? '').replace(/\r\n?/g, '\n');
    const read = current => {
      if (current instanceof Element && current.matches('[data-inline-selection-pill], [data-inline-selection-pill-cursor-target]')) return '';
      // October 2026: selecting a pill also inserts one space after it.
      if (current.nodeType === 3) return current.previousSibling instanceof Element && current.previousSibling.matches('[data-inline-selection-pill]') ?
        (current.nodeValue || '').replace(/^[ \u00a0]/, '') : current.nodeValue || '';
      if (current.nodeName === 'BR') return current.classList.contains('ProseMirror-trailingBreak') ? '' : '\n';
      const children = Array.from(current.childNodes || []);
      if (!children.length) return current.innerText ?? current.textContent ?? '';
      const block = value => /^(P|DIV|PRE|LI)$/.test(value?.nodeName || '');
      return children.map((child, index) =>
        (index > 0 && (block(child) || block(children[index - 1])) ? '\n' : '') + read(child)
      ).join('');
    };
    return read(node).replace(/\r\n?/g, '\n');
  };
