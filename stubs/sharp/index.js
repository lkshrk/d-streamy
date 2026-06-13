"use strict";

// No-op stub for `sharp`, injected via package.json `overrides`.
//
// Why: dstreamy's only sharp consumer is @dank074/discord-video-stream's
// stream-preview path, which dstreamy never enables (streamPreview defaults
// to false and we never call setStreamPreview). Real sharp resolves a libvips
// prebuilt compiled without OpenJPEG, and sharp's module-load code derefs the
// absent `format.jp2k.output` and throws — crashing the daemon on import.
// Stubbing removes both the crash and the ~10MB native libvips dylib from the
// notarized bundle. If stream previews are ever wanted, drop this override and
// generate the JPEG via the already-bundled node-av/ffmpeg instead.

function chain() {
  const self = {
    resize: () => self,
    extract: () => self,
    jpeg: () => self,
    png: () => self,
    webp: () => self,
    raw: () => self,
    toBuffer: () => Promise.reject(new Error("sharp is stubbed in dstreamy")),
  };
  return self;
}

function sharp() {
  return chain();
}

sharp.format = {};
sharp.versions = {};
sharp.cache = () => {};
sharp.concurrency = () => 0;

module.exports = sharp;
module.exports.default = sharp;
