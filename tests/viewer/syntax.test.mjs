import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Script } from 'node:vm';
import { viewerPage } from './page.mjs';

const html = viewerPage();

// The other tests take single functions out of the page; a mistake elsewhere in its script (a name declared twice
// in one function) stopped the whole page at "connecting" while they all passed
test("the page's script compiles", () => {
  const scripts = [...html.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/g)].map(m => m[1]);
  assert.ok(scripts.length > 0, 'the page has an inline script');
  for (const s of scripts) {
    assert.doesNotThrow(() => new Script(s, { filename: 'viewer.html' }));
  }
});
