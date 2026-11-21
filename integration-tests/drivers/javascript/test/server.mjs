// Starts one graphbuff-server (HTTP + Bolt, token auth, in-memory) and stops it again.

import { spawn } from 'node:child_process'
import fs from 'node:fs'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

export const TOKEN = 'smoke-secret'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..')

function binary () {
  if (process.env.GRAPHBUFF_SERVER) return process.env.GRAPHBUFF_SERVER
  const exe = process.platform === 'win32' ? 'graphbuff-server.exe' : 'graphbuff-server'
  // Whichever of debug and release was built last.
  const built = ['debug', 'release']
    .map(p => path.join(root, 'target', p, exe))
    .filter(p => fs.existsSync(p))
    .sort((a, b) => fs.statSync(b).mtimeMs - fs.statSync(a).mtimeMs)
  if (!built.length) {
    throw new Error('graphbuff-server not built: run `cargo build -p graphbuff-server` or set GRAPHBUFF_SERVER')
  }
  return built[0]
}

function freePort () {
  return new Promise((resolve, reject) => {
    const s = net.createServer()
    s.unref()
    s.on('error', reject)
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address()
      s.close(() => resolve(port))
    })
  })
}

export async function startServer () {
  const http = await freePort()
  const bolt = await freePort()
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'graphbuff-config-'))
  const config = path.join(dir, 'graphbuff.toml')
  fs.writeFileSync(config, [
    '[server]',
    `bind = "127.0.0.1:${http}"`,
    `bolt = "127.0.0.1:${bolt}"`,
    '[storage]',
    'mode = "memory"',
    '[security]',
    `token = "${TOKEN}"`,
    '[queries]',
    'transaction_timeout_seconds = 5',
    '[logs]',
    'dir = "-"',
    ''
  ].join('\n'))
  const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('GRAPHBUFF_')))
  const child = spawn(binary(), ['--config', config], { env, stdio: ['ignore', 'ignore', 'inherit'] })
  let exited = false
  child.on('exit', () => { exited = true })

  const deadline = Date.now() + 20_000
  for (;;) {
    try {
      const res = await fetch(`http://127.0.0.1:${http}/health`)
      if (res.ok) break
    } catch {
      // not up yet
    }
    if (exited || Date.now() > deadline) {
      child.kill()
      throw new Error('graphbuff-server did not start')
    }
    await new Promise(resolve => setTimeout(resolve, 100))
  }

  return {
    bolt: `bolt://127.0.0.1:${bolt}`,
    neo4j: `neo4j://127.0.0.1:${bolt}`,
    async stop () {
      if (!exited) {
        const done = new Promise(resolve => child.once('exit', resolve))
        child.kill()
        await done
      }
      fs.rmSync(dir, { recursive: true, force: true })
    }
  }
}
