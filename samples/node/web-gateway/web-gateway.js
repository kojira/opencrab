#!/usr/bin/env node
"use strict";

const { loadPlacement, ConfigError } = require("./lib/placement");
const { configDigest } = require("./lib/wire");
const { spawn } = require("./lib/client");
const { listenHttp } = require("./lib/http");

process.on("uncaughtException", (err) => {
  console.error(err);
  process.exit(1);
});
process.on("unhandledRejection", (err) => {
  console.error(err);
  process.exit(1);
});

const placementPath = process.argv[2];
if (!placementPath) {
  console.error("usage: web-gateway <placement.json>");
  process.exit(1);
}

let place;
try {
  place = loadPlacement(placementPath);
} catch (e) {
  if (e instanceof ConfigError) {
    console.error(e.message);
    process.exit(1);
  }
  throw e;
}

const instances = [];
for (const inst of place.instances) {
  const digest = configDigest(inst.author_id);
  instances.push(
    spawn(place.core_socket, inst.instance_id, inst.revision, inst.author_id, digest),
  );
}

listenHttp({ host: place.host, port: place.port }, instances).catch((err) => {
  console.error(err);
  process.exit(1);
});
