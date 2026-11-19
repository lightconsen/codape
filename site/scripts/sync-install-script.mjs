#!/usr/bin/env node
/**
 * Copies the repository's installer to site/public/install.sh.
 *
 * The landing page advertises `curl -fsSL https://codape.com/install.sh | sh`,
 * so the script has to exist at that path in the deployed artifact. Copying it
 * at build time rather than committing a second copy means the site can never
 * serve an installer that has drifted from scripts/install.sh.
 *
 * Runs from package.json's predev/prebuild; paths are resolved from this file,
 * so it works from any working directory.
 */

import { copyFileSync, readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const source = resolve(here, '../../scripts/install.sh')
const target = resolve(here, '../public/install.sh')

const script = readFileSync(source, 'utf8')
if (!script.startsWith('#!/bin/sh')) {
  throw new Error(`${source} does not look like the installer; refusing to copy it`)
}

copyFileSync(source, target)
console.log(`install.sh: ${source} -> ${target}`)
