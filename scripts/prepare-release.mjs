#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const root = process.cwd();
const args = process.argv.slice(2);

const releaseArg = args.find((arg) => !arg.startsWith('--'));
const dryRun = args.includes('--dry-run');

if (!releaseArg) {
  console.error('Usage: npm run release:prepare -- <patch|minor|major|x.y.z> [--dry-run]');
  process.exit(1);
}

const dateStr = new Date().toISOString().slice(0, 10);

const packagePath = path.join(root, 'package.json');
const packageLockPath = path.join(root, 'package-lock.json');
const tauriConfPath = path.join(root, 'src-tauri', 'tauri.conf.json');
const cargoTomlPath = path.join(root, 'src-tauri', 'Cargo.toml');
const cargoLockPath = path.join(root, 'src-tauri', 'Cargo.lock');
const i18nDir = path.join(root, 'src', 'i18n');
const i18nFiles = fs.readdirSync(i18nDir).filter((f) => f.endsWith('.json')).sort();
const changelogPath = path.join(root, 'CHANGELOG.md');
const changelogZhPath = path.join(root, 'CHANGELOG-zh.md');

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf8'));
}

function writeJson(filePath, value) {
  fs.writeFileSync(filePath, `${JSON.stringify(value, null, 2)}\n`);
}

function parseSemver(version) {
  const m = version.match(/^(\d+)\.(\d+)\.(\d+)$/);
  if (!m) return null;
  return { major: Number(m[1]), minor: Number(m[2]), patch: Number(m[3]) };
}

function bumpVersion(current, releaseType) {
  const parsed = parseSemver(current);
  if (!parsed) {
    throw new Error(`Current package version is not SemVer: ${current}`);
  }

  if (releaseType === 'patch') {
    return `${parsed.major}.${parsed.minor}.${parsed.patch + 1}`;
  }
  if (releaseType === 'minor') {
    return `${parsed.major}.${parsed.minor + 1}.0`;
  }
  if (releaseType === 'major') {
    return `${parsed.major + 1}.0.0`;
  }

  if (parseSemver(releaseType)) {
    return releaseType;
  }

  throw new Error(`Invalid release type/version: ${releaseType}`);
}

// A locale without settings.version falls back to en's, so only en must have it.
function updateSettingsVersion(i18nObj, nextVersion, fileLabel) {
  if (!i18nObj.settings || typeof i18nObj.settings.version !== 'string') {
    if (fileLabel.endsWith('/en.json')) throw new Error(`Missing settings.version in ${fileLabel}`);
    return;
  }
  i18nObj.settings.version = i18nObj.settings.version.replace(/\d+\.\d+\.\d+/, nextVersion);
}

function updateCargoPackageVersion(cargoToml, nextVersion) {
  const packageStart = cargoToml.indexOf('[package]');
  if (packageStart === -1) {
    throw new Error('Missing [package] in src-tauri/Cargo.toml');
  }
  const nextSection = cargoToml.indexOf('\n[', packageStart + '[package]'.length);
  const packageEnd = nextSection === -1 ? cargoToml.length : nextSection;
  const packageSection = cargoToml.slice(packageStart, packageEnd);
  if (!/^version = "[^"]+"$/m.test(packageSection)) {
    throw new Error('Missing package version in src-tauri/Cargo.toml');
  }
  const updatedSection = packageSection.replace(
    /^version = "[^"]+"$/m,
    `version = "${nextVersion}"`,
  );
  return `${cargoToml.slice(0, packageStart)}${updatedSection}${cargoToml.slice(packageEnd)}`;
}

function updateCargoLockVersion(cargoLock, nextVersion) {
  const packagePattern = /(\[\[package\]\]\nname = "skills-manager"\nversion = ")[^"]+("\n)/;
  if (!packagePattern.test(cargoLock)) {
    throw new Error('Missing skills-manager package entry in src-tauri/Cargo.lock');
  }
  return cargoLock.replace(
    packagePattern,
    (_match, prefix, suffix) => `${prefix}${nextVersion}${suffix}`,
  );
}

function ensureChangelogEntry(changelog, nextVersion, { zh = false } = {}) {
  const heading = `## [${nextVersion}] - ${dateStr}`;
  if (changelog.includes(heading) || changelog.includes(`## [${nextVersion}] -`)) {
    return changelog;
  }

  const sections = zh
    ? ['### 发布概览', '- ', '', '### 用户可见更新', '- ', '', '### 开发者与治理更新', '- ']
    : ['### Release Overview', '- ', '', '### User-facing', '- ', '', '### Developer & Governance', '- '];

  const entry = [heading, '', ...sections, ''].join('\n');

  const firstReleaseHeading = changelog.search(/^## \[/m);
  if (firstReleaseHeading === -1) {
    return `${changelog.trimEnd()}\n\n${entry}\n`;
  }

  return `${changelog.slice(0, firstReleaseHeading)}${entry}${changelog.slice(firstReleaseHeading)}`;
}

// Refresh the README star-history snapshot. Best-effort: a failure here (no gh
// auth, no network, no python3) must never block the version bump / changelog.
function refreshStarHistory() {
  const script = path.join(root, 'scripts', 'gen-star-history.py');
  const res = spawnSync('python3', [script], { stdio: 'inherit' });
  return !res.error && res.status === 0;
}

function main() {
  const pkg = readJson(packagePath);
  const tauriConf = readJson(tauriConfPath);
  const cargoToml = fs.readFileSync(cargoTomlPath, 'utf8');
  const cargoLock = fs.readFileSync(cargoLockPath, 'utf8');
  const locales = i18nFiles.map((file) => ({ file, json: readJson(path.join(i18nDir, file)) }));
  const changelog = fs.readFileSync(changelogPath, 'utf8');
  const changelogZh = fs.readFileSync(changelogZhPath, 'utf8');

  const currentVersion = pkg.version;
  const nextVersion = bumpVersion(currentVersion, releaseArg);

  pkg.version = nextVersion;
  // npm rewrites both of these on any install, so leaving them behind means
  // every contributor's `npm install` produces a stray diff. They sat at
  // 1.22.1 while package.json was 1.38.0 until #432 noticed.
  const packageLock = readJson(packageLockPath);
  packageLock.version = nextVersion;
  if (packageLock.packages?.['']) {
    packageLock.packages[''].version = nextVersion;
  }
  tauriConf.version = nextVersion;
  const nextCargoToml = updateCargoPackageVersion(cargoToml, nextVersion);
  const nextCargoLock = updateCargoLockVersion(cargoLock, nextVersion);
  for (const { file, json } of locales) {
    updateSettingsVersion(json, nextVersion, `src/i18n/${file}`);
  }
  const nextChangelog = ensureChangelogEntry(changelog, nextVersion);
  const nextChangelogZh = ensureChangelogEntry(changelogZh, nextVersion, { zh: true });

  if (dryRun) {
    console.log(`[dry-run] ${currentVersion} -> ${nextVersion}`);
    return;
  }

  writeJson(packagePath, pkg);
  writeJson(packageLockPath, packageLock);
  writeJson(tauriConfPath, tauriConf);
  fs.writeFileSync(cargoTomlPath, nextCargoToml);
  fs.writeFileSync(cargoLockPath, nextCargoLock);
  for (const { file, json } of locales) {
    writeJson(path.join(i18nDir, file), json);
  }
  fs.writeFileSync(changelogPath, nextChangelog);
  fs.writeFileSync(changelogZhPath, nextChangelogZh);

  const starOk = refreshStarHistory();

  console.log(`Prepared release ${nextVersion}`);
  console.log('Updated:');
  console.log('- CHANGELOG.md');
  console.log('- CHANGELOG-zh.md');
  console.log('- package.json');
  console.log('- package-lock.json');
  console.log('- src-tauri/tauri.conf.json');
  console.log('- src-tauri/Cargo.toml');
  console.log('- src-tauri/Cargo.lock');
  for (const { file } of locales) {
    console.log(`- src/i18n/${file}`);
  }
  console.log(
    starOk
      ? '- assets/star-history.svg'
      : '- assets/star-history.svg (skipped: refresh failed — run `python3 scripts/gen-star-history.py` manually)',
  );
}

main();
