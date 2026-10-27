// Provider fields qualified with a public Auto Private turn on 21 October 2026.
// Only one exact outgoing text request is admitted. No answer chunks accumulate.
// The retained wrapper drops payload/guard references even if another wrapper
// still delegates to it after cleanup.
(cfg, privateChat) => {
  if (Object.hasOwn(window, '__oracleGrokWitness')) throw Error('Earlier Grok witness needs exact cleanup');
  if (!privateChat()) throw Error('Grok Private Chat is required before witness installation');
  let originalDocument = document;
  const NativeSocket = window.WebSocket;
  const prototype = NativeSocket?.prototype;
  const descriptor = prototype && Object.getOwnPropertyDescriptor(prototype, 'send');
  if (!descriptor || typeof descriptor.value !== 'function' || !descriptor.writable)
    throw Error('Grok WebSocket observation is unavailable');
  const original = descriptor.value;
  const id = value => typeof value === 'string' && /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(value);
  if (!id(cfg.dispatchId) || !id(cfg.owner?.claimId) || !id(cfg.owner?.reservationId) ||
      typeof cfg.prompt !== 'string' || !cfg.prompt || cfg.prompt.length > 524288 ||
      !/^[a-f0-9]{64}$/.test(cfg.promptSha256) || !['Auto','Fast'].includes(cfg.mode))
    throw Error('Invalid Grok witness intent');
  let expected = cfg.prompt;
  let socket = null, clientId = null, userId = null, responseId = null, assistantId = null;
  let responseCreated = false, itemDone = false, complete = false, failure = null, closed = false, sealed = null;
  let expiryTimer = null;
  let sendArmed = false;
  let clickAuthorized = false;
  let lateSendBlocked = false;
  const deadline = Date.now() + Math.min(cfg.lifetimeMs, 2400000);
  if (!Number.isSafeInteger(cfg.lifetimeMs) || cfg.lifetimeMs < 1) throw Error('Invalid Grok witness deadline');
  const intent = Object.freeze({version:1,provider:'grok',kind:'grok-websocket-v1',
    claimId:cfg.owner.claimId,reservationId:cfg.owner.reservationId,dispatchId:cfg.dispatchId,
    promptSha256:cfg.promptSha256,mode:cfg.mode,privateChat:true});
  const current = () => {
    if (document !== originalDocument || window !== window.top || !privateChat() || Date.now() > deadline)
      throw Error('Grok witness privacy, document or deadline changed');
    const modes = Array.from(document.querySelectorAll('button[aria-haspopup="menu"], button[aria-haspopup="listbox"]'))
      .filter(el => el instanceof HTMLElement && el.getClientRects().length > 0 &&
        !el.closest('[id^="response-"], [data-message-author-role], [data-role]') &&
        /^(Auto|Fast|Expert|Heavy)(?:\s|$)/.test(el.innerText.trim()));
    if (modes.length !== 1 || !new RegExp('^' + intent.mode + '(?:\\s|$)').test(modes[0].innerText.trim()))
      throw Error('Grok selected mode changed');
    const claim = JSON.parse(sessionStorage.getItem('oracle:target-claim') ?? 'null');
    if (!claim || claim.id !== intent.claimId || claim.retiring || claim.inspection ||
        claim.submission?.id !== intent.reservationId || claim.submission.state !== 'active' ||
        !['started','acknowledged'].includes(claim.submission.dispatch))
      throw Error('Grok witness owner changed');
  };
  const invalidate = reason => { failure ??= reason; };
  const parse = raw => {
    if (typeof raw !== 'string' || raw.length > 1048576) throw Error('Grok frame is unsupported or oversized');
    const value = JSON.parse(raw);
    if (!value || typeof value !== 'object' || Array.isArray(value)) throw Error('Invalid Grok frame');
    return value;
  };
  const uuid = value => id(value?.uuid) ? value.uuid : null;
  const detach = () => {
    if (expiryTimer !== null) clearTimeout(expiryTimer);
    expiryTimer = null;
    // A click handler can await before its first WebSocket write. If that work
    // is unsettled, retain only a payload-free refusal wrapper in this realm.
    // Restoring the native method here would permit a late unobserved request.
    lateSendBlocked = clickAuthorized && (!complete || failure !== null);
    if (!lateSendBlocked && prototype.send === observedSend) Object.defineProperty(prototype, 'send', descriptor);
    if (socket) {
      socket.removeEventListener('message', received);
      socket.removeEventListener('close', disconnected);
      socket.removeEventListener('error', disconnected);
    }
    socket = null;
    // Release all prompt-bearing references and lexical mutation authority.
    expected = cfg = privateChat = originalDocument = null;
    closed = true;
  };
  const result = () => failure ? {state:'invalid',reason:failure} :
    {state:complete ? 'completed' : userId ? 'committed' : 'pending',...intent,
      clientMessageId:clientId,userMessageId:userId ? 'response-' + userId : null,
      responseId,assistantMessageId:assistantId ? 'response-' + assistantId : null};
  const read = () => {
    if (closed) return sealed;
    try { current(); } catch { invalidate('grok-context-changed'); }
    return result();
  };
  const stop = () => {
    if (closed) return sealed;
    try { sealed = read(); } finally { detach(); }
    return sealed;
  };
  const acknowledge = () => {
    if (closed) throw Error('Stopped Grok witness has no live acknowledgement authority');
    return read();
  };
  const authorizeSend = () => {
    if (closed || failure || clientId || sendArmed) throw Error('Grok send witness is unavailable or consumed');
    current();
    clickAuthorized = true;
    sendArmed = true;
    return {...intent};
  };
  const disconnected = () => { invalidate('grok-socket-disconnected'); };
  const received = event => {
    if (closed || failure) return;
    try {
      current();
      const value = parse(event.data), type = value.type;
      if (type === 'conversation.item.added' && value.item?.role === 'user') {
        const client = uuid(value.item.x_grok?.client_message_id), user = uuid(value.item.id);
        const chunks = value.item.x_grok?.input_chunks;
        if (!user || client !== clientId || userId || !Array.isArray(chunks) || chunks.length !== 1 ||
            chunks[0]?.text?.text !== expected) throw Error('Grok user acknowledgement differs');
        userId = user;
      } else if (type === 'response.created') {
        const response = uuid(value.response?.id);
        if (!userId || !response || responseCreated) throw Error('Grok response creation differs');
        responseCreated = true;
        responseId = response;
      } else if (type === 'response.output_item.added') {
        const assistant = uuid(value.item?.id), response = uuid(value.response_id);
        if (!responseCreated || response !== responseId || !assistant || assistantId || value.output_index !== 0 ||
            value.item?.role !== 'assistant' || uuid(value.item.x_grok?.parent_response_id) !== userId)
          throw Error('Grok assistant parent differs');
        assistantId = assistant;
      } else if (type === 'response.output_item.done') {
        if (!assistantId || uuid(value.response_id) !== responseId || uuid(value.item?.id) !== assistantId ||
            value.item.role !== 'assistant' || value.item.status !== 'completed' || value.output_index !== 0 || itemDone)
          throw Error('Grok output completion differs');
        itemDone = true;
      } else if (['response.chunk','response.content_part.added','response.content_part.done','response.output_text.done'].includes(type)) {
        if (!assistantId || complete || uuid(value.response_id) !== responseId || uuid(value.item_id) !== assistantId ||
            (type !== 'response.chunk' && (value.output_index !== 0 || value.content_index !== 0)))
          throw Error('Grok content identity differs');
      } else if (type === 'response.done') {
        if (!itemDone || uuid(value.response?.id) !== responseId || value.response.status !== 'completed' || complete)
          throw Error('Grok response completion differs');
        complete = true;
      } else if (type === 'error' || type === 'response.failed' || type === 'response.cancelled') {
        throw Error('Grok response failed');
      }
    } catch { invalidate('grok-wire-conflict'); }
  };
  function observedSend(raw) {
    if (closed) {
      if (lateSendBlocked && parse(raw).type === 'response.create')
        throw Error('Expired or stopped Grok Send cannot dispatch late; preserve target without replay');
      return original.call(this, raw);
    }
    let value;
    try { value = parse(raw); } catch {
      invalidate('grok-outgoing-frame-unreadable');
      throw Error('Grok outgoing request cannot be verified');
    }
    if (value.type !== 'response.create') return original.call(this, raw);
    try {
      if (failure || !sendArmed) throw Error('Grok witness is invalid or Send was not authorised');
      sendArmed = false;
      current();
      const destination = new URL(this.url);
      const client = uuid(value.item?.x_grok?.client_message_id), chunks = value.item?.x_grok?.input_chunks;
      if (!(this instanceof NativeSocket) || destination.protocol !== 'wss:' ||
          destination.hostname !== 'grok.com' || destination.port || destination.username || destination.password ||
          socket || !client || value.item?.role !== 'user' || !Array.isArray(chunks) || chunks.length !== 1 ||
          Object.keys(chunks[0]).length !== 1 || Object.keys(chunks[0].text ?? {}).length !== 1 ||
          chunks[0]?.text?.text !== expected)
        throw Error('Grok outgoing request differs');
      socket = this;
      clientId = client;
      socket.addEventListener('message', received);
      socket.addEventListener('close', disconnected);
      socket.addEventListener('error', disconnected);
      return original.call(this, raw);
    } catch {
      invalidate('grok-outgoing-request-conflict');
      throw Error('Grok outgoing request cannot be verified; preserve target without replay');
    }
  }
  const api = Object.freeze({owner:Object.freeze({...cfg.owner}),dispatchId:cfg.dispatchId,read,acknowledge,authorizeSend,stop});
  try {
    Object.defineProperty(prototype, 'send', {...descriptor,value:observedSend});
    Object.defineProperty(window, '__oracleGrokWitness', {value:api,writable:false,configurable:true});
    expiryTimer = setTimeout(() => { invalidate('grok-witness-expired'); stop(); }, Math.max(1, deadline - Date.now()));
  } catch (error) {
    detach();
    throw error;
  }
  return true;
}
