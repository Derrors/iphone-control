// The control page as the program embeds it: apps/iphone-control/web/index.html with each `@include <path>` line
// replaced by that file, as build.rs does. The page tests read it from here.
import { readFileSync } from 'node:fs';

const web = new URL('../../apps/iphone-control/web/', import.meta.url);

export function viewerPage() {
  return readFileSync(new URL('index.html', web), 'utf8')
    .replace(/^@include (\S+)\n/gm, (_, rel) => readFileSync(new URL(rel, web), 'utf8'));
}
