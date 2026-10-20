/**
 * git-ai plugin for OpenCode
 *
 * This plugin integrates git-ai with OpenCode to track AI-generated code.
 * It uses the tool.execute.before and tool.execute.after events to create
 * checkpoints that mark code changes as human or AI-authored.
 *
 * Installation:
 *   - Automatically installed by `git-ai install-hooks`
 *   - Or manually copy to ~/.config/opencode/plugins/git-ai.ts (global)
 *   - Or to .opencode/plugins/git-ai.ts (project-local)
 *
 * Requirements:
 *   - git-ai must be installed (path is injected at install time)
 *
 * @see https://github.com/git-ai-project/git-ai
 * @see https://opencode.ai/docs/plugins/
 */

import type { Plugin } from "@opencode-ai/plugin"
import { dirname, isAbsolute, join } from "path"
import { spawn } from "node:child_process"

// Node desktop sidecars and Bun CLI runtimes both support child_process.
// Close stdin when no hook payload is needed; never inherit the host's stdin.
const runCommand = (binary: string, args: string[], input?: string): Promise<string> =>
  new Promise((resolve, reject) => {
    const child = spawn(binary, args, {
      stdio: [input === undefined ? "ignore" : "pipe", "pipe", "pipe"],
    })
    const stdout: Buffer[] = []
    child.stdout!.on("data", (chunk: Buffer) => stdout.push(chunk))
    if (input !== undefined) {
      child.stdin!.on("error", () => {})
      child.stdin!.end(input)
    }
    const stderr: Buffer[] = []
    const timer = setTimeout(() => {
      child.kill("SIGTERM")
      reject(new Error("OpenCode token report timed out after 30 seconds"))
    }, 30000)
    child.stderr!.on("data", (chunk: Buffer) => stderr.push(chunk))
    child.on("error", (error) => { clearTimeout(timer); reject(error) })
    child.on("close", (code) => {
      clearTimeout(timer)
      if (code === 0) resolve(Buffer.concat(stdout).toString())
      else reject(new Error(`OpenCode token report exited ${code}: ${Buffer.concat(stderr).toString().trim()}`))
    })
  })

// Absolute path to git-ai binary, replaced at install time by `git-ai install-hooks`
const GIT_AI_BIN = "__GIT_AI_BINARY_PATH__"

// Tools that modify files and should be tracked
const FILE_EDIT_TOOLS = new Set([
  "edit",
  "write",
  "patch",
  "multiedit",
  "apply_patch",
  "applypatch",
])

const APPLY_PATCH_FILE_PREFIXES = [
  "*** Update File: ",
  "*** Add File: ",
  "*** Delete File: ",
  "*** Move to: ",
]

const isEditTool = (toolName: string): boolean => FILE_EDIT_TOOLS.has(toolName.toLowerCase())

const isBashTool = (toolName: string): boolean => {
  const name = toolName.toLowerCase()
  return name === "bash" || name === "shell"
}

const normalizePath = (rawPath: string, cwd?: string): string | null => {
  const trimmed = rawPath.trim().replace(/^['"]|['"]$/g, "")
  if (!trimmed) {
    return null
  }

  const withoutScheme = trimmed
    .replace(/^file:\/\/localhost/, "")
    .replace(/^file:\/\//, "")

  const isWindowsAbs = /^[a-zA-Z]:[\\/]/.test(withoutScheme)
  if (isAbsolute(withoutScheme) || isWindowsAbs) {
    return withoutScheme
  }

  // Use provided cwd, or fall back to process.cwd() for relative paths
  const resolvedCwd = cwd || process.cwd()
  return join(resolvedCwd, withoutScheme)
}

const collectApplyPatchPaths = (raw: string, out: Set<string>): void => {
  for (const line of raw.split("\n")) {
    const trimmed = line.trim()
    for (const prefix of APPLY_PATCH_FILE_PREFIXES) {
      if (trimmed.startsWith(prefix)) {
        const path = trimmed.slice(prefix.length).trim().replace(/^['"]|['"]$/g, "")
        if (path) {
          out.add(path)
        }
      }
    }
  }
}

const collectToolPaths = (value: unknown, out: Set<string>): void => {
  if (typeof value === "string") {
    if (value.startsWith("file://")) {
      out.add(value)
    }
    collectApplyPatchPaths(value, out)
    return
  }

  if (Array.isArray(value)) {
    for (const item of value) {
      collectToolPaths(item, out)
    }
    return
  }

  if (!value || typeof value !== "object") {
    return
  }

  for (const [key, val] of Object.entries(value)) {
    const keyLower = key.toLowerCase()
    const isSinglePathKey = keyLower === "file_path" || keyLower === "filepath" || keyLower === "path" || keyLower === "fspath"
    const isMultiPathKey = keyLower === "files" || keyLower === "filepaths" || keyLower === "file_paths"

    if (isSinglePathKey && typeof val === "string") {
      out.add(val)
    } else if (isMultiPathKey) {
      if (typeof val === "string") {
        out.add(val)
      } else if (Array.isArray(val)) {
        for (const item of val) {
          if (typeof item === "string") {
            out.add(item)
          }
        }
      }
    }

    collectToolPaths(val, out)
  }
}

const extractFilePaths = (args: unknown, cwd?: string): string[] => {
  const rawPaths = new Set<string>()
  collectToolPaths(args, rawPaths)

  const normalizedPaths = new Set<string>()
  for (const rawPath of rawPaths) {
    const normalized = normalizePath(rawPath, cwd)
    if (normalized) {
      normalizedPaths.add(normalized)
    }
  }

  return [...normalizedPaths]
}

/**
 * Tool call metadata, normalized across OpenCode versions.
 *
 * V1 passes `{tool, callID, sessionID, cwd}` plus a separate args object.
 * V2 passes `{tool, id, sessionID, input}` through `ctx.tool.hook`. Both are
 * funnelled into this shape so the checkpoint logic stays version-agnostic.
 */
type ToolCallInfo = {
  tool: string
  callID: string
  sessionID: string
  cwd?: string
  workdir?: string
}

/**
 * Shared setup for both entrypoints (OpenCode V1 and V2).
 *
 * Keeping the work here means the V1 and V2 entrypoints cannot drift apart:
 * they only adapt their own hook signature and delegate. Returns `null` when
 * git-ai is not installed, which makes both entrypoints no-ops.
 */
const createGitAiHandlers = async () => {
  // Check if git-ai is installed
  let gitAiInstalled = false
  try {
    await runCommand(GIT_AI_BIN, ["--version"])
    gitAiInstalled = true
  } catch {
    // git-ai not installed, plugin will be a no-op
  }

  if (!gitAiInstalled) {
    return null
  }

  // Track pending calls by callID so we can reference them in the after hook
  const pendingCalls = new Map<string, { repoDir: string; sessionID: string; toolInput: unknown }>()

  // Helper to find git repo root from a file path
  const findGitRepo = async (pathHint: string): Promise<string | null> => {
    const candidateDirs = [pathHint, dirname(pathHint)]

    for (const dir of candidateDirs) {
      try {
        const result = await runCommand("git", ["-C", dir, "rev-parse", "--show-toplevel"])
        const repoRoot = result.trim()
        if (repoRoot) {
          return repoRoot
        }
      } catch {
        // try next candidate
      }
    }

    return null
  }

  const resolveRepoDir = async (filePaths: string[], cwd?: string): Promise<string | null> => {
    if (cwd) {
      const fromCwd = await findGitRepo(cwd)
      if (fromCwd) {
        return fromCwd
      }
    }

    const fromProcessCwd = await findGitRepo(process.cwd())
    if (fromProcessCwd) {
      return fromProcessCwd
    }

    for (const filePath of filePaths) {
      const repo = await findGitRepo(filePath)
      if (repo) {
        return repo
      }
    }

    return null
  }

  const extractToolCwd = (inputCwd: unknown, outputArgs: Record<string, unknown> | undefined): string | undefined => {
    if (typeof outputArgs?.workdir === "string") return outputArgs.workdir
    if (typeof outputArgs?.cwd === "string") return outputArgs.cwd
    if (typeof inputCwd === "string") return inputCwd
    return undefined
  }

  /**
   * Shared pre-tool checkpoint. `cwd`/`workdir` come from the raw tool args so
   * this works for both hook shapes.
   */
  const handleToolBefore = async (call: ToolCallInfo, toolInput: unknown): Promise<void> => {
    const toolCwd = extractToolCwd(call.cwd ?? call.workdir, toolInput as Record<string, unknown> | undefined)

    if (!isEditTool(call.tool) && !isBashTool(call.tool)) {
      return
    }

    // File-edit tools carry file paths; bash tools do not, so resolve from cwd.
    const filePaths = isEditTool(call.tool) ? extractFilePaths(toolInput, toolCwd) : []
    const repoDir = await resolveRepoDir(filePaths, toolCwd)
    if (!repoDir) {
      return
    }

    pendingCalls.set(call.callID, { repoDir, sessionID: call.sessionID, toolInput })

    try {
      const hookInput = JSON.stringify({
        hook_event_name: "PreToolUse",
        session_id: call.sessionID,
        tool_use_id: call.callID,
        cwd: repoDir,
        tool_name: call.tool,
        tool_input: toolInput,
      })
      await runCommand(GIT_AI_BIN, ["checkpoint", "opencode", "--hook-input", "stdin"], hookInput)
    } catch (error) {
      console.error("[git-ai] Failed to create human checkpoint:", String(error))
    }
  }

  /** Shared post-tool checkpoint, paired with `handleToolBefore` by call id. */
  const handleToolAfter = async (call: ToolCallInfo): Promise<void> => {
    if (!isEditTool(call.tool) && !isBashTool(call.tool)) {
      return
    }

    const callInfo = pendingCalls.get(call.callID)
    pendingCalls.delete(call.callID)

    if (!callInfo) {
      return
    }

    try {
      const hookInput = JSON.stringify({
        hook_event_name: "PostToolUse",
        session_id: callInfo.sessionID,
        tool_use_id: call.callID,
        cwd: callInfo.repoDir,
        tool_name: call.tool,
        tool_input: callInfo.toolInput,
      })
      await runCommand(GIT_AI_BIN, ["checkpoint", "opencode", "--hook-input", "stdin"], hookInput)
    } catch (error) {
      console.error("[git-ai] Failed to create AI checkpoint:", String(error))
    }
  }

  /**
   * Report token usage + prompts for the finished turn.
   *
   * A prompt loop settling is OpenCode's equivalent of the Claude Code / Codex
   * `Stop` hook. The plugin must pass the session id explicitly: unlike Claude
   * (stdin transcript_path) and Codex (filename ordering), OpenCode exposes no
   * reliable way for the CLI to infer which session just finished.
   */
  const reportTokenUsage = async (sessionID: string | undefined): Promise<void> => {
    if (!sessionID) {
      console.warn("[git-ai] session event did not include sessionID")
      return
    }

    // OpenCode publishes the settling event while the final message/part may
    // still be flushing to storage, so a failed attempt is retried. The first
    // attempt must not wait: `opencode run` exits as soon as the session goes
    // idle, which tears down the plugin's pending work. Waiting first loses the
    // report entirely; trying immediately usually lands before teardown, and
    // subsequent attempts cover the flush race when it does not.
    const wait = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms))
    const delays = [0, 1000, 1500]
    for (let attempt = 0; attempt < delays.length; attempt += 1) {
      if (delays[attempt] > 0) {
        await wait(delays[attempt])
      }
      try {
        await runCommand(GIT_AI_BIN, ["report-token-usage", "opencode", "--session-id", sessionID])
        console.info(`[git-ai] Reported OpenCode token usage for ${sessionID}`)
        return
      } catch (error) {
        console.warn(
          `[git-ai] OpenCode token report attempt ${attempt + 1}/${delays.length} failed for ${sessionID}: ${String(error)}`,
        )
      }
    }

    console.error(`[git-ai] Failed to report OpenCode token usage after retries: ${sessionID}`)
  }

  return { handleToolBefore, handleToolAfter, reportTokenUsage }
}

/**
 * OpenCode V1 entrypoint.
 *
 * V1 loads a named export whose factory returns an object of hooks. The
 * `event` hook receives `{type, properties}`.
 */

export const GitAiPlugin: Plugin = async (_ctx) => {
  const handlers = await createGitAiHandlers()
  if (!handlers) {
    return {}
  }

  return {
    "tool.execute.before": async (input, output) => {
      await handlers.handleToolBefore(
        {
          tool: input.tool,
          callID: input.callID,
          sessionID: input.sessionID,
          cwd: (input as { cwd?: string }).cwd ?? (input as { workdir?: string }).workdir,
        },
        output.args,
      )
    },

    "tool.execute.after": async (input, _output) => {
      await handlers.handleToolAfter({
        tool: input.tool,
        callID: input.callID,
        sessionID: input.sessionID,
      })
    },

    event: async ({ event }) => {
      if (event.type !== "session.idle") {
        return
      }

      await handlers.reportTokenUsage(event.properties?.sessionID)
    },
  }
}

/**
 * OpenCode V2 entrypoint.
 *
 * V2 requires a default export with an id and a setup function. It ignores the
 * V1 named export, so both entrypoints can live in this one file. Hook inputs
 * use `id`/`input` instead of `callID`/args, and the event stream moves from a
 * callback to an async iterable where the session id lives on `data`.
 */
