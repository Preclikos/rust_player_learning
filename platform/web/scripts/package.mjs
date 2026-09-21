#!/usr/bin/env node
// Turn a wasm-pack output directory into the publishable npm package.
//
// wasm-pack names the package after the crate (`bridge-web`) and takes the
// version from Cargo.toml. The published artefact is `@preclikos/rustplayer`
// with the version from the release TAG (web-vX.Y.Z), the same rule the
// Android AAR and the iOS XCFramework follow — no file bump per release.
//
//   node scripts/package.mjs <pkg-dir> <version>
//
// Used by .github/workflows/publish-web.yml; runnable locally for a dry run:
//   node scripts/package.mjs www/pkg 0.0.0-local && (cd www/pkg && npm pack --dry-run)

import { readFileSync, writeFileSync, copyFileSync, existsSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const [pkgDir, version] = process.argv.slice(2);
if (!pkgDir || !version) {
  console.error('usage: package.mjs <pkg-dir> <version>');
  process.exit(2);
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error(`version "${version}" is not semver (expected X.Y.Z[-pre])`);
  process.exit(2);
}

const here = dirname(fileURLToPath(import.meta.url));
const crateDir = resolve(here, '..');
const pkgJsonPath = join(pkgDir, 'package.json');
const pkg = JSON.parse(readFileSync(pkgJsonPath, 'utf8'));

// The README wasm-pack copies is the crate's; keep it (it documents embedding).
if (!existsSync(join(pkgDir, 'README.md')) && existsSync(join(crateDir, 'README.md'))) {
  copyFileSync(join(crateDir, 'README.md'), join(pkgDir, 'README.md'));
}

const out = {
  name: '@preclikos/rustplayer',
  version,
  description:
    'DASH player engine (WebCodecs HEVC, WebGPU, Web Audio, ClearKey) — the same Rust core as the Android AAR and iOS XCFramework, for the browser.',
  license: 'UNLICENSED',
  repository: { type: 'git', url: 'git+https://github.com/Preclikos/rust_player_learning.git' },
  homepage: 'https://github.com/Preclikos/rust_player_learning/tree/master/platform/web',
  publishConfig: { registry: 'https://npm.pkg.github.com', access: 'restricted' },
  type: pkg.type ?? 'module',
  main: pkg.main,
  types: pkg.types,
  files: [...new Set([...(pkg.files ?? []), 'README.md'])],
  sideEffects: pkg.sideEffects,
  keywords: ['dash', 'hevc', 'webcodecs', 'webgpu', 'player', 'wasm'],
};

writeFileSync(pkgJsonPath, JSON.stringify(out, null, 2) + '\n');
console.log(`${out.name}@${out.version} ready in ${pkgDir}`);
