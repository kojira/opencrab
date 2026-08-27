"use strict";

const fs = require("node:fs");
const net = require("node:net");
const { parseObject, isInt, intLexeme, JsonSyntaxError } = require("./json");
const { parseUuid } = require("./wire");

class ConfigError extends Error {
  constructor(msg) {
    super(msg);
    this.name = "ConfigError";
  }
}

function loadPlacement(path) {
  let bytes;
  try {
    bytes = fs.readFileSync(path);
  } catch (e) {
    throw new ConfigError(`read placement: ${e.message}`);
  }
  let obj;
  try {
    obj = parseObject(bytes);
  } catch (e) {
    if (e instanceof JsonSyntaxError) {
      throw new ConfigError("placement JSON");
    }
    throw e;
  }
  return validate(obj);
}

function validate(obj) {
  if (typeof obj.http_bind !== "string") {
    throw new ConfigError("http_bind must be a socket address");
  }
  const bind = parseHttpBind(obj.http_bind);
  if (!bind) {
    throw new ConfigError("http_bind must be a socket address");
  }
  if (!bind.loopback) {
    throw new ConfigError("http_bind must be loopback");
  }
  if (typeof obj.core_socket !== "string" || obj.core_socket.length === 0 || !obj.core_socket.startsWith("/")) {
    throw new ConfigError("core_socket must be an absolute path");
  }
  if (!Array.isArray(obj.instances) || obj.instances.length === 0) {
    throw new ConfigError("instances must be nonempty");
  }
  const seen = new Set();
  const instances = [];
  for (const inst of obj.instances) {
    if (!inst || typeof inst !== "object" || Array.isArray(inst)) {
      throw new ConfigError("instance");
    }
    const instanceId = parseUuid(inst.instance_id);
    if (!instanceId) {
      throw new ConfigError("instance_id must be canonical lowercase UUID");
    }
    if (seen.has(instanceId)) {
      throw new ConfigError("duplicate instance_id is a double live; refuse startup");
    }
    seen.add(instanceId);
    if (!isInt(inst.revision)) {
      throw new ConfigError("revision must be positive");
    }
    const revision = intLexeme(inst.revision);
    if (typeof inst.author_id !== "string" || inst.author_id.length === 0) {
      throw new ConfigError("author_id must be nonempty");
    }
    instances.push({
      instance_id: instanceId,
      revision,
      author_id: inst.author_id,
    });
  }
  return {
    http_bind: obj.http_bind,
    host: bind.host,
    port: bind.port,
    core_socket: obj.core_socket,
    instances,
  };
}

function parseHttpBind(s) {
  let host;
  let portStr;
  if (s.startsWith("[")) {
    const idx = s.lastIndexOf("]:");
    if (idx < 0) {
      return null;
    }
    host = s.slice(1, idx);
    portStr = s.slice(idx + 2);
  } else {
    const idx = s.lastIndexOf(":");
    if (idx < 0) {
      return null;
    }
    host = s.slice(0, idx);
    portStr = s.slice(idx + 1);
  }
  if (!/^[0-9]+$/.test(portStr)) {
    return null;
  }
  const port = Number(portStr);
  if (!Number.isInteger(port) || port < 0 || port > 65535) {
    return null;
  }
  const ver = net.isIP(host);
  if (ver === 0) {
    return null;
  }
  const loopback = ver === 4 ? ipv4Loopback(host) : host === "::1";
  return { host, port, loopback };
}

function ipv4Loopback(host) {
  const parts = host.split(".");
  if (parts.length !== 4) {
    return false;
  }
  return Number(parts[0]) === 127;
}

module.exports = { loadPlacement, ConfigError };
