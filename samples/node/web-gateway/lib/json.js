"use strict";

const { invariant } = require("./fail");

const KNOWN_INT = new Set(["protocol", "revision", "seq"]);
const U64_MAX = (1n << 64n) - 1n;
const I64_MAX = (1n << 63n) - 1n;

class JsonSyntaxError extends Error {
  constructor() {
    super("bad_request");
    this.name = "JsonSyntaxError";
  }
}

function fatalDecode(bytes) {
  invariant(Buffer.isBuffer(bytes) || bytes instanceof Uint8Array, "fatalDecode wants bytes");
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    throw new JsonSyntaxError();
  }
}

function parseObject(bytes) {
  const text = fatalDecode(bytes);
  const p = new Parser(text);
  p.skipWs();
  if (p.peek() !== "{") {
    throw new JsonSyntaxError();
  }
  const value = p.parseValue(null);
  p.skipWs();
  if (!p.eof()) {
    throw new JsonSyntaxError();
  }
  invariant(value !== null && typeof value === "object" && !Array.isArray(value) && !value.__int, "top object");
  return value;
}

function isInt(v) {
  return Boolean(v && typeof v === "object" && v.__int === true && typeof v.lexeme === "string");
}

function intLexeme(v) {
  invariant(isInt(v), "intLexeme");
  return v.lexeme;
}

function dump(v) {
  if (v === null) {
    return "null";
  }
  if (isInt(v)) {
    return v.lexeme;
  }
  if (typeof v === "string") {
    return JSON.stringify(v);
  }
  if (typeof v === "boolean") {
    return v ? "true" : "false";
  }
  if (typeof v === "number") {
    invariant(Number.isFinite(v), "non-finite number");
    return JSON.stringify(v);
  }
  if (Array.isArray(v)) {
    return `[${v.map(dump).join(",")}]`;
  }
  if (typeof v === "object") {
    const keys = Object.keys(v);
    return `{${keys.map((k) => `${JSON.stringify(k)}:${dump(v[k])}`).join(",")}}`;
  }
  invariant(false, `dump type ${typeof v}`);
}

class Parser {
  constructor(s) {
    this.s = s;
    this.i = 0;
  }

  eof() {
    return this.i >= this.s.length;
  }

  peek() {
    return this.s[this.i];
  }

  skipWs() {
    while (this.i < this.s.length) {
      const c = this.s[this.i];
      if (c === " " || c === "\t" || c === "\n" || c === "\r") {
        this.i += 1;
        continue;
      }
      return;
    }
  }

  parseValue(keyHint) {
    this.skipWs();
    if (this.eof()) {
      throw new JsonSyntaxError();
    }
    const c = this.peek();
    if (c === "{") {
      return this.parseObject();
    }
    if (c === "[") {
      return this.parseArray();
    }
    if (c === '"') {
      return this.parseString();
    }
    if (c === "t") {
      return this.consumeWord("true", true);
    }
    if (c === "f") {
      return this.consumeWord("false", false);
    }
    if (c === "n") {
      return this.consumeWord("null", null);
    }
    if (c === "-" || (c >= "0" && c <= "9")) {
      return this.parseNumber(keyHint);
    }
    throw new JsonSyntaxError();
  }

  consumeWord(word, value) {
    if (this.s.slice(this.i, this.i + word.length) !== word) {
      throw new JsonSyntaxError();
    }
    this.i += word.length;
    return value;
  }

  parseObject() {
    this.i += 1;
    const obj = Object.create(null);
    const seen = new Set();
    this.skipWs();
    if (this.peek() === "}") {
      this.i += 1;
      return obj;
    }
    for (;;) {
      this.skipWs();
      if (this.peek() !== '"') {
        throw new JsonSyntaxError();
      }
      const key = this.parseString();
      if (seen.has(key)) {
        throw new JsonSyntaxError();
      }
      seen.add(key);
      this.skipWs();
      if (this.peek() !== ":") {
        throw new JsonSyntaxError();
      }
      this.i += 1;
      obj[key] = this.parseValue(key);
      this.skipWs();
      const sep = this.peek();
      if (sep === ",") {
        this.i += 1;
        continue;
      }
      if (sep === "}") {
        this.i += 1;
        return obj;
      }
      throw new JsonSyntaxError();
    }
  }

  parseArray() {
    this.i += 1;
    const items = [];
    this.skipWs();
    if (this.peek() === "]") {
      this.i += 1;
      return items;
    }
    for (;;) {
      items.push(this.parseValue(null));
      this.skipWs();
      const sep = this.peek();
      if (sep === ",") {
        this.i += 1;
        continue;
      }
      if (sep === "]") {
        this.i += 1;
        return items;
      }
      throw new JsonSyntaxError();
    }
  }

  parseString() {
    if (this.peek() !== '"') {
      throw new JsonSyntaxError();
    }
    this.i += 1;
    let out = "";
    while (!this.eof()) {
      const c = this.s[this.i];
      if (c === '"') {
        this.i += 1;
        return out;
      }
      if (c === "\\") {
        this.i += 1;
        out += this.parseEscape();
        continue;
      }
      const code = c.charCodeAt(0);
      if (code < 0x20) {
        throw new JsonSyntaxError();
      }
      out += c;
      this.i += 1;
    }
    throw new JsonSyntaxError();
  }

  parseEscape() {
    if (this.eof()) {
      throw new JsonSyntaxError();
    }
    const c = this.s[this.i];
    this.i += 1;
    switch (c) {
      case '"':
      case "\\":
      case "/":
        return c;
      case "b":
        return "\b";
      case "f":
        return "\f";
      case "n":
        return "\n";
      case "r":
        return "\r";
      case "t":
        return "\t";
      case "u": {
        const hex = this.s.slice(this.i, this.i + 4);
        if (!/^[0-9a-fA-F]{4}$/.test(hex)) {
          throw new JsonSyntaxError();
        }
        this.i += 4;
        return String.fromCharCode(parseInt(hex, 16));
      }
      default:
        throw new JsonSyntaxError();
    }
  }

  parseNumber(keyHint) {
    const start = this.i;
    if (this.peek() === "-") {
      this.i += 1;
    }
    if (this.eof()) {
      throw new JsonSyntaxError();
    }
    if (this.peek() === "0") {
      this.i += 1;
    } else if (this.peek() >= "1" && this.peek() <= "9") {
      while (this.peek() >= "0" && this.peek() <= "9") {
        this.i += 1;
      }
    } else {
      throw new JsonSyntaxError();
    }
    if (this.peek() === ".") {
      this.i += 1;
      if (!(this.peek() >= "0" && this.peek() <= "9")) {
        throw new JsonSyntaxError();
      }
      while (this.peek() >= "0" && this.peek() <= "9") {
        this.i += 1;
      }
    }
    if (this.peek() === "e" || this.peek() === "E") {
      this.i += 1;
      if (this.peek() === "+" || this.peek() === "-") {
        this.i += 1;
      }
      if (!(this.peek() >= "0" && this.peek() <= "9")) {
        throw new JsonSyntaxError();
      }
      while (this.peek() >= "0" && this.peek() <= "9") {
        this.i += 1;
      }
    }
    const lexeme = this.s.slice(start, this.i);
    if (KNOWN_INT.has(keyHint)) {
      return this.knownInt(keyHint, lexeme);
    }
    const n = Number(lexeme);
    if (!Number.isFinite(n)) {
      throw new JsonSyntaxError();
    }
    return n;
  }

  knownInt(key, lexeme) {
    if (!/^-?(0|[1-9][0-9]*)$/.test(lexeme)) {
      throw new JsonSyntaxError();
    }
    const n = BigInt(lexeme);
    if (key === "seq") {
      if (n < 1n || n > I64_MAX) {
        throw new JsonSyntaxError();
      }
    } else if (key === "revision") {
      if (n < 1n || n > U64_MAX) {
        throw new JsonSyntaxError();
      }
    } else if (key === "protocol") {
      if (n < 0n || n > U64_MAX) {
        throw new JsonSyntaxError();
      }
    } else {
      invariant(false, `unknown known-int ${key}`);
    }
    return { __int: true, lexeme };
  }
}

module.exports = {
  JsonSyntaxError,
  fatalDecode,
  parseObject,
  isInt,
  intLexeme,
  dump,
  KNOWN_INT,
};
