"use strict";

function invariant(cond, msg) {
  if (!cond) {
    console.error("invariant violation:", msg);
    process.exit(1);
  }
}

module.exports = { invariant };
