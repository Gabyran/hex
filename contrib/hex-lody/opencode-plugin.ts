import { Plugin } from "@opencode/plugin"
import { spawn } from "node:child_process"
import { existsSync, mkdirSync, openSync } from "node:fs"
import { homedir } from "node:os"
import { basename, join } from "node:path"

const HEX_LODY = process.env.HEX_LODY_BIN ?? join(homedir(), ".local/bin/hex-lody")
const STATE_DIR = join(homedir(), ".local/state/hex-lody")
const RECORDED_AT = /^\[scrawl [^\n]*\n[\s\S]*?\nRecorded at (.+\.scrawl)\.?\n/

export default Plugin.define({
  id: "hex-lody",
  async setup(ctx) {
    await ctx.session.hook("prompt", (event) => {
      const folder = RECORDED_AT.exec(event.prompt.text)?.[1]
      if (!folder || !existsSync(join(folder, "scrawl.json"))) return

      mkdirSync(STATE_DIR, { recursive: true })
      const log = openSync(join(STATE_DIR, "opencode.log"), "a")
      spawn(HEX_LODY, ["send", folder], {
        detached: true,
        stdio: ["ignore", log, log],
        env: { ...process.env, PATH: `${process.env.PATH}:/opt/homebrew/bin:/usr/local/bin:/usr/bin` },
      }).unref()

      const header = event.prompt.text.split("\n", 1)[0]
      event.prompt.text = `${header}\n\nHanded to a new Lody session (${basename(folder)}). Nothing to do here; reply "handed off".`
      event.prompt.files = []
    })
  },
})
