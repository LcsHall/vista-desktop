// Fail fast (before the slow Rust build) when the version stamps or the git
// tag disagree. Usage: node scripts/check-versions.mjs [vX.Y.Z]
import { readFileSync } from 'node:fs'

const tag = process.argv[2]
const conf = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8')).version
const pkg = JSON.parse(readFileSync('package.json', 'utf8')).version
const cargo = /^version\s*=\s*"([^"]+)"/m.exec(readFileSync('src-tauri/Cargo.toml', 'utf8'))[1]
// The app's own entry in the committed Cargo.lock. The test build runs
// `--locked`, which fails on a stale one (it did once: v0.3.0).
const lock = /\[\[package\]\]\r?\nname = "vista-platform"\r?\nversion = "([^"]+)"/.exec(
  readFileSync('src-tauri/Cargo.lock', 'utf8'),
)?.[1]

const die = (m) => { console.error(m); process.exit(1) }
if (conf !== pkg || conf !== cargo || conf !== lock) {
  die(`version mismatch: tauri.conf.json=${conf} package.json=${pkg} Cargo.toml=${cargo} Cargo.lock=${lock}`)
}
if (tag && tag !== `v${conf}`) die(`tag ${tag} != v${conf}`)
console.log(`versions aligned: ${conf}`)
