#!/usr/bin/env node
// Extension host child process, ported from the TypeScript
// `src/extensions/host-process.ts` with an identical stdio JSONL protocol.
// Runs inside Node (extensions are JS/TS modules by design; .ts loads with
// Node's --experimental-strip-types when available).

import readline from "node:readline/promises";
import process from "node:process";

const send = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);

const tools = new Map();
const commands = new Map();
const providers = new Map();
const handlers = new Map();

const api = {
  registerTool(tool) {
    tools.set(tool.name, tool);
    send({ type: "tool", tool });
  },
  registerCommand(command) {
    commands.set(command.name, command);
    send({ type: "command", command });
  },
  registerProvider(provider) {
    providers.set(provider.id, provider);
    send({ type: "provider", provider });
  },
  on(event, handler) {
    const list = handlers.get(event) ?? [];
    list.push(handler);
    handlers.set(event, list);
  },
};

const context = { cwd: process.cwd() };

for (const file of process.argv.slice(2)) {
  try {
    const module = await import(`file://${file}`);
    if (typeof module.default !== "function") throw new Error("extension must export a default factory");
    await module.default(api);
  } catch (error) {
    send({ type: "extension_error", error: `${file}: ${error instanceof Error ? error.message : String(error)}` });
  }
}
send({ type: "ready" });

const lines = readline.createInterface({ input: process.stdin });
for await (const line of lines) {
  if (!line.trim()) continue;
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    continue;
  }
  try {
    if (message.type === "tool_call") {
      const tool = tools.get(message.name ?? "");
      if (!tool) throw new Error(`Unknown extension tool: ${message.name}`);
      const output = await tool.execute(message.args ?? {}, context);
      send({ type: "tool_result", id: message.id ?? "", output: typeof output === "string" ? output : String(output) });
    } else if (message.type === "command_call") {
      const command = commands.get(message.name ?? "");
      if (!command) throw new Error(`Unknown extension command: ${message.name}`);
      const output = await command.handler(message.text ?? "", context);
      send({ type: "command_result", id: message.id ?? "", output: typeof output === "string" ? output : String(output) });
    } else if (message.type === "event") {
      for (const handler of handlers.get(message.event ?? "") ?? []) await handler(message.payload);
    } else if (message.type === "hook") {
      let args = message.args ?? {};
      let block = false;
      let reason;
      let output;
      for (const handler of handlers.get(message.name ?? "") ?? []) {
        const result = await handler({ args, output: message.text });
        if (!result || typeof result !== "object") {
          if (typeof result === "string") output = result;
          continue;
        }
        if (result.args && typeof result.args === "object" && !Array.isArray(result.args)) args = result.args;
        if (result.block === true) {
          block = true;
          reason = typeof result.reason === "string" ? result.reason : reason;
        }
        if (typeof result.output === "string") output = result.output;
      }
      send({ type: "hook_result", id: message.id ?? "", payload: { args, block, reason, output } });
    } else if (message.type === "shutdown") {
      lines.close();
    }
  } catch (error) {
    send({ type: message.type === "command_call" ? "command_result" : "tool_result", id: message.id ?? "", error: error instanceof Error ? error.message : String(error) });
  }
}
