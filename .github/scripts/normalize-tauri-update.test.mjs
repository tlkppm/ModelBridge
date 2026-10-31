import assert from "node:assert/strict";
import test from "node:test";
import { normalizeTauriUpdate } from "./normalize-tauri-update.mjs";

function fixture() {
  const repository = "owner/repository";
  const version = "1.2.3";
  const assets = [];
  const platforms = {};
  for (const platform of ["windows-x86_64", "linux-x86_64", "darwin-aarch64", "darwin-x86_64"]) {
    const id = assets.length + 1;
    const name = `${platform}.zip`;
    assets.push({ id, name, size: 128, state: "uploaded" });
    assets.push({ id: id + 1, name: `${name}.sig`, size: 64, state: "uploaded" });
    platforms[platform] = {
      signature: "signed-payload",
      url: `https://api.github.com/repos/${repository}/releases/assets/${id}`,
    };
  }
  return {
    manifest: { version, platforms },
    release: { tag_name: `v${version}`, assets },
    repository,
    version,
  };
}

function normalize(input, platform) {
  return normalizeTauriUpdate(input.manifest, input.release, input.repository, input.version, platform);
}

test("normalizes four verified platform assets to public download URLs", () => {
  const input = fixture();
  const result = normalize(input);
  for (const [platform, entry] of Object.entries(result.platforms)) {
    assert.equal(entry.url, `https://github.com/owner/repository/releases/download/v1.2.3/${platform}.zip`);
  }
});

test("rejects an empty draft instead of accepting an upload log", () => {
  const input = fixture();
  input.release.assets = [];
  assert.throws(() => normalize(input), /no uploaded assets/);
});

test("requires all four platforms before final publication", () => {
  const input = fixture();
  delete input.manifest.platforms["darwin-x86_64"];
  assert.throws(() => normalize(input), /missing platform darwin-x86_64/);
});

test("validates only the current platform during parallel builds", () => {
  const input = fixture();
  input.manifest.platforms = { "darwin-aarch64": input.manifest.platforms["darwin-aarch64"] };
  assert.equal(Object.keys(normalize(input, "macos-aarch64").platforms).length, 1);
});

test("rejects assets that are not uploaded", () => {
  const input = fixture();
  input.release.assets[0].state = "new";
  assert.throws(() => normalize(input), /not uploaded successfully/);
});

test("rejects empty uploaded assets", () => {
  const input = fixture();
  input.release.assets[0].size = 0;
  assert.throws(() => normalize(input), /not uploaded successfully/);
});

test("requires the uploaded signature file", () => {
  const input = fixture();
  input.release.assets.splice(1, 1);
  assert.throws(() => normalize(input), /missing its uploaded signature file/);
});

test("rejects a download URL that is not a release asset", () => {
  const input = fixture();
  input.manifest.platforms["windows-x86_64"].url = "https://example.invalid/update.zip";
  assert.throws(() => normalize(input), /unknown release asset/);
});

test("ignores another platform's in-flight upload during a platform check", () => {
  const input = fixture();
  input.release.assets[0].state = "new";
  input.release.assets[0].size = 0;
  assert.ok(normalize(input, "macos-aarch64").platforms["darwin-aarch64"]);
  assert.throws(() => normalize(input), /not uploaded successfully/);
});

test("rejects an unknown build platform", () => {
  assert.throws(() => normalize(fixture(), "unknown"), /unknown release platform/);
});
