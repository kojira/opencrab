"use strict";

const net = require("node:net");
const { invariant } = require("./fail");
const {
  MAX_FRAME,
  helloFrame,
  saidFrame,
  okFrame,
  errFrame,
  sayText,
  encodeFrame,
  parseFrameBytes,
} = require("./wire");

const LIVE_QUEUE_CAP = 32;
const SAID_TIMEOUT_MS = 10_000;
const RECONNECT_MIN_MS = 200;
const RECONNECT_MAX_MS = 8_000;

function blankInner() {
  return {
    acknowledged: new Map(),
    remembered: new Map(),
    pendingSaid: new Map(),
    pendingTurn: new Map(),
    live: new Map(),
    closed: true,
    generation: 0,
  };
}

function createClient(instanceId, authorId) {
  return {
    instanceId,
    authorId,
    inner: blankInner(),
    write: null,
    closedWaiters: [],
    reqSeq: 1,
  };
}

function notifyClosed(client) {
  const waiters = client.closedWaiters;
  client.closedWaiters = [];
  for (const w of waiters) {
    w();
  }
}

function waitClosed(client) {
  return new Promise((resolve) => {
    client.closedWaiters.push(resolve);
  });
}

function connectionLive(client) {
  return !client.inner.closed;
}

function rememberedBinding(client, address) {
  return client.inner.remembered.get(address) || null;
}

function bindingForAddress(client, address) {
  if (client.inner.closed) {
    return null;
  }
  return client.inner.acknowledged.get(address) || null;
}

function nextId(client) {
  const n = client.reqSeq;
  client.reqSeq += 1;
  return `said:${n}`;
}

function sendFrame(client, value) {
  const q = client.write;
  if (!q || q.dead) {
    return false;
  }
  return q.enqueue(value);
}

function liveQueue(inner, address) {
  let q = inner.live.get(address);
  if (!q) {
    q = { events: [], waiters: [] };
    inner.live.set(address, q);
  }
  return q;
}

function tryPush(q, ev) {
  while (q.waiters.length > 0) {
    const w = q.waiters.shift();
    if (!w.done) {
      w.done = true;
      w.resolve(ev);
      return true;
    }
  }
  if (q.events.length >= LIVE_QUEUE_CAP) {
    return false;
  }
  q.events.push(ev);
  return true;
}

function closeAll(client, code, generation) {
  const inner = client.inner;
  if (inner.closed || inner.generation !== generation) {
    return;
  }
  inner.closed = true;
  for (const [address, bindingId] of inner.acknowledged) {
    inner.remembered.set(address, bindingId);
  }
  inner.acknowledged.clear();
  for (const pending of inner.pendingSaid.values()) {
    pending.resolve({ kind: "disconnected" });
  }
  inner.pendingSaid.clear();
  inner.pendingTurn.clear();
  const ev = { kind: "error", code, detail: null };
  for (const q of inner.live.values()) {
    tryPush(q, ev);
    for (const w of q.waiters) {
      if (!w.done) {
        w.done = true;
        w.resolve(null);
      }
    }
    q.waiters = [];
  }
  if (client.write) {
    client.write.dead = true;
    if (client.write.socket) {
      client.write.socket.destroy();
    }
  }
  console.error(`close instance=${client.instanceId} code=${code}`);
  notifyClosed(client);
}

function postSaid(client, address, origin, text, attachments) {
  const inner = client.inner;
  if (inner.closed) {
    return Promise.resolve({ refuse: "not_ready" });
  }
  const bindingId = inner.acknowledged.get(address);
  if (!bindingId) {
    return Promise.resolve({ refuse: "not_ready" });
  }
  const turn = inner.pendingTurn.get(bindingId);
  if (turn) {
    if (turn.origin !== origin) {
      return Promise.resolve({ refuse: "busy" });
    }
  } else {
    inner.pendingTurn.set(bindingId, { sawSay: false, origin });
  }
  const id = nextId(client);
  const outcome = new Promise((resolve) => {
    inner.pendingSaid.set(id, { kind: "said", resolve });
  });
  const generation = inner.generation;
  console.error(`said instance=${client.instanceId} binding=${bindingId} origin=${origin}`);
  if (!sendFrame(client, saidFrame(id, bindingId, origin, client.authorId, text, attachments))) {
    inner.pendingTurn.delete(bindingId);
    inner.pendingSaid.delete(id);
    return Promise.resolve({ kind: "disconnected" });
  }
  let timer;
  const timeout = new Promise((resolve) => {
    timer = setTimeout(() => resolve("timeout"), SAID_TIMEOUT_MS);
  });
  return Promise.race([outcome, timeout]).then((result) => {
    clearTimeout(timer);
    if (result === "timeout") {
      closeAll(client, "disconnect", generation);
      return { kind: "disconnected" };
    }
    if (result.kind !== "accepted") {
      client.inner.pendingTurn.delete(bindingId);
    }
    console.error(`said ack instance=${client.instanceId} binding=${bindingId}`);
    return result;
  });
}

function nextLive(client, address) {
  const inner = client.inner;
  if (inner.closed) {
    const q = inner.live.get(address);
    if (q && q.events.length > 0) {
      return Promise.resolve(q.events.shift());
    }
    return Promise.resolve({ kind: "error", code: "disconnect", detail: null });
  }
  const q = liveQueue(inner, address);
  if (q.events.length > 0) {
    return Promise.resolve(q.events.shift());
  }
  return new Promise((resolve) => {
    q.waiters.push({ done: false, resolve });
  });
}

function handleBind(client, msg, generation) {
  console.error(`bind instance=${client.instanceId} binding=${msg.bindingId} address=${msg.address}`);
  const inner = client.inner;
  if (inner.closed) {
    return;
  }
  const existing = inner.acknowledged.get(msg.address);
  if (existing && existing !== msg.bindingId) {
    closeAll(client, "binding_conflict", generation);
    return;
  }
  inner.remembered.set(msg.address, msg.bindingId);
  inner.acknowledged.set(msg.address, msg.bindingId);
  liveQueue(inner, msg.address);
  sendFrame(client, okFrame(msg.id));
}

function handleSay(client, msg, generation) {
  console.error(`say instance=${client.instanceId} binding=${msg.bindingId}`);
  const text = sayText(msg.payload);
  if (text === null) {
    sendFrame(client, errFrame(msg.id, "external_rejected", null));
    return false;
  }
  const inner = client.inner;
  if (inner.closed) {
    return true;
  }
  let address = null;
  for (const [addr, bid] of inner.acknowledged) {
    if (bid === msg.bindingId) {
      address = addr;
      break;
    }
  }
  if (address === null) {
    sendFrame(client, errFrame(msg.id, "external_rejected", null));
    return false;
  }
  const q = liveQueue(inner, address);
  if (!tryPush(q, { kind: "message", text })) {
    sendFrame(client, errFrame(msg.id, "external_rejected", null));
    return false;
  }
  const turn = inner.pendingTurn.get(msg.bindingId);
  if (turn) {
    turn.sawSay = true;
  }
  if (!sendFrame(client, okFrame(msg.id))) {
    closeAll(client, "disconnect", generation);
    return true;
  }
  return false;
}

function handleActivity(client, msg) {
  const inner = client.inner;
  if (inner.closed) {
    return;
  }
  let address = null;
  for (const [addr, bid] of inner.acknowledged) {
    if (bid === msg.bindingId) {
      address = addr;
      break;
    }
  }
  if (address === null) {
    return;
  }
  const q = liveQueue(inner, address);
  tryPush(q, { kind: "activity", activityId: msg.activityId, state: msg.state });
  if (msg.state === "ended") {
    const turn = inner.pendingTurn.get(msg.bindingId);
    if (turn) {
      inner.pendingTurn.delete(msg.bindingId);
      if (!turn.sawSay) {
        tryPush(q, { kind: "completed_no_reply" });
      }
    }
  }
}

function handleResponse(client, msg, generation) {
  const inner = client.inner;
  const pending = inner.pendingSaid.get(msg.id);
  if (!pending) {
    closeAll(client, "response_invalid", generation);
    return;
  }
  inner.pendingSaid.delete(msg.id);
  let outcome;
  if (pending.kind === "hello") {
    if (msg.ok && !msg.seq.present) {
      outcome = { kind: "accepted", seq: "0" };
    } else if (!msg.ok) {
      outcome = { kind: "wire_err", code: msg.code || "bad_request", detail: msg.detail };
    } else {
      closeAll(client, "response_invalid", generation);
      return;
    }
  } else {
    invariant(pending.kind === "said", "pending kind");
    if (msg.ok) {
      if (!msg.seq.present) {
        closeAll(client, "response_invalid", generation);
        return;
      }
      if (msg.seq.value === null) {
        outcome = { kind: "not_admitted" };
      } else {
        outcome = { kind: "accepted", seq: msg.seq.value };
      }
    } else {
      outcome = { kind: "wire_err", code: msg.code || "bad_request", detail: msg.detail };
    }
  }
  pending.resolve(outcome);
}

function handleMsg(client, msg, generation) {
  switch (msg.type) {
    case "bind":
      handleBind(client, msg, generation);
      return false;
    case "say":
      return handleSay(client, msg, generation);
    case "activity":
      handleActivity(client, msg);
      return false;
    case "response":
      handleResponse(client, msg, generation);
      return false;
    case "reverse":
    case "unknown":
      if (msg.id) {
        sendFrame(client, errFrame(msg.id, "unknown_message", null));
      }
      return false;
    case "invalid":
      if (msg.id) {
        sendFrame(client, errFrame(msg.id, msg.code, null));
      }
      if (msg.code === "response_invalid") {
        closeAll(client, "response_invalid", generation);
        return true;
      }
      return false;
    default:
      invariant(false, `msg type ${msg.type}`);
  }
}

function attachReader(socket, client, generation) {
  let buf = Buffer.alloc(0);
  let finished = false;
  const finish = (code) => {
    if (finished) {
      return;
    }
    finished = true;
    closeAll(client, code, generation);
  };
  socket.on("data", (chunk) => {
    if (finished || client.inner.generation !== generation) {
      return;
    }
    buf = Buffer.concat([buf, chunk]);
    for (;;) {
      const nl = buf.indexOf(0x0a);
      if (nl === -1) {
        if (buf.length > MAX_FRAME) {
          finish("too_large");
        }
        return;
      }
      const frameLen = nl + 1;
      if (frameLen > MAX_FRAME) {
        finish("too_large");
        return;
      }
      const frame = Buffer.from(buf.subarray(0, frameLen));
      buf = Buffer.from(buf.subarray(frameLen));
      const parsed = parseFrameBytes(frame);
      if (parsed.type === "frame_error") {
        finish(parsed.code);
        return;
      }
      if (handleMsg(client, parsed, generation)) {
        finished = true;
        return;
      }
    }
  });
  socket.on("error", () => finish("disconnect"));
  socket.on("close", () => finish("disconnect"));
}

function createWriteQueue(socket) {
  const q = {
    socket,
    items: [],
    writing: false,
    dead: false,
    enqueue(value) {
      if (q.dead) {
        return false;
      }
      q.items.push(value);
      q.kick();
      return true;
    },
    kick() {
      if (q.writing || q.dead) {
        return;
      }
      const value = q.items.shift();
      if (!value) {
        return;
      }
      const bytes = encodeFrame(value);
      if (bytes === null) {
        q.dead = true;
        q.socket.destroy();
        return;
      }
      q.writing = true;
      q.socket.write(bytes, (err) => {
        q.writing = false;
        if (err) {
          q.dead = true;
          return;
        }
        q.kick();
      });
    },
  };
  return q;
}

function connectUds(path) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ path });
    const onErr = (err) => {
      socket.destroy();
      reject(err);
    };
    socket.once("error", onErr);
    socket.once("connect", () => {
      socket.removeListener("error", onErr);
      resolve(socket);
    });
  });
}

function attach(client, socketPath, revision, digest) {
  return connectUds(socketPath).then((socket) => {
    const inner = client.inner;
    inner.generation += 1;
    const generation = inner.generation;
    inner.closed = false;
    inner.acknowledged.clear();
    inner.pendingSaid.clear();
    inner.pendingTurn.clear();
    inner.live.clear();
    client.write = createWriteQueue(socket);
    const helloId = `hello:${client.instanceId}`;
    const helloOutcome = new Promise((resolve) => {
      inner.pendingSaid.set(helloId, { kind: "hello", resolve });
    });
    console.error(`hello instance=${client.instanceId}`);
    if (!sendFrame(client, helloFrame(helloId, client.instanceId, revision, digest))) {
      inner.closed = true;
      socket.destroy();
      return Promise.reject(new Error("hello write"));
    }
    attachReader(socket, client, generation);
    return helloOutcome.then((outcome) => {
      if (outcome.kind === "accepted" || outcome.kind === "not_admitted") {
        console.error(`hello ok instance=${client.instanceId}`);
        return;
      }
      console.error(`hello failed instance=${client.instanceId}`);
      socket.destroy();
      throw new Error("hello failed");
    });
  });
}

function spawn(socketPath, instanceId, revision, authorId, digest) {
  const client = createClient(instanceId, authorId);
  const loop = async () => {
    let backoff = RECONNECT_MIN_MS;
    for (;;) {
      try {
        await attach(client, socketPath, revision, digest);
        console.error(`uds connected instance=${client.instanceId}`);
        backoff = RECONNECT_MIN_MS;
        if (client.inner.closed) {
          console.error(`uds closed during hello instance=${client.instanceId}`);
        } else {
          await waitClosed(client);
          console.error(`uds closed; reconnecting instance=${client.instanceId}`);
        }
      } catch {
        console.error(`uds connect/hello failed instance=${client.instanceId}`);
      }
      await new Promise((r) => setTimeout(r, backoff));
      backoff = Math.min(backoff * 2, RECONNECT_MAX_MS);
    }
  };
  loop().catch((err) => {
    console.error(err);
    process.exit(1);
  });
  return client;
}

module.exports = {
  spawn,
  connectionLive,
  rememberedBinding,
  bindingForAddress,
  postSaid,
  nextLive,
};
