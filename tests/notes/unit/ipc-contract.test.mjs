import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const rust = readFileSync(new URL('../../../src-tauri/src/notes.rs', import.meta.url), 'utf8');
const controller = readFileSync(
  new URL('../../../src-tauri/dist/js/notes/notes-controller.js', import.meta.url),
  'utf8',
);

// Arguments Tauri injects itself; they never come from the frontend payload.
const INJECTED = new Set(['app', '_app', 'state']);

/** Split Rust parameters on commas outside generic brackets. */
function splitParams(params) {
  const parts = [];
  let depth = 0;
  let current = '';
  for (const ch of params) {
    if (ch === '<') depth += 1;
    if (ch === '>') depth -= 1;
    if (ch === ',' && depth === 0) {
      parts.push(current);
      current = '';
    } else {
      current += ch;
    }
  }
  parts.push(current);
  return parts;
}

/** Notes commands with their attribute and IPC argument names. */
function rustCommands() {
  const commands = new Map();
  const pattern =
    /#\[tauri::command([^\]]*)\]\s*pub async fn (notes_\w+)\s*(?:<[^>]*>)?\s*\(([\s\S]*?)\)\s*->/g;
  for (const [, policy, name, params] of rust.matchAll(pattern)) {
    const args = splitParams(params)
      .map((param) => param.trim())
      .filter(Boolean)
      .map((param) => {
        const [argName, type] = param.split(/:(.*)/s).map((part) => part.trim());
        return { name: argName, optional: /^Option</.test(type) };
      })
      .filter((arg) => !INJECTED.has(arg.name));
    commands.set(name, { policy, args });
  }
  return commands;
}

/** Top-level keys of the object literal starting at `start` ('{'). */
function topLevelKeys(source, start) {
  const keys = [];
  let depth = 0;
  let token = '';
  for (let i = start; i < source.length; i += 1) {
    const ch = source[i];
    if (ch === '{') {
      depth += 1;
      if (depth > 1) continue;
      token = '';
      continue;
    }
    if (ch === '}') {
      depth -= 1;
      if (depth === 0) {
        if (token.trim()) keys.push(token.trim());
        return keys.map((key) => key.split(':')[0].trim()).filter(Boolean);
      }
      continue;
    }
    if (depth !== 1) continue;
    if (ch === ',') {
      if (token.trim()) keys.push(token.trim());
      token = '';
    } else {
      token += ch;
    }
  }
  throw new Error('unterminated payload object');
}

/** Every `invoke('notes_…', {…})` in the controller with its payload keys. */
function controllerCalls() {
  const calls = [];
  for (const match of controller.matchAll(/invoke\(\s*'(notes_\w+)'\s*(,\s*\{)?/g)) {
    const keys = match[2] ? topLevelKeys(controller, match.index + match[0].length - 1) : [];
    calls.push({ name: match[1], keys });
  }
  return calls;
}

test('N01: every Notes wrapper declares snake_case arguments', () => {
  const commands = rustCommands();
  assert.ok(commands.size >= 20, 'must inspect all existing Notes commands');
  for (const [name, { policy }] of commands) {
    assert.match(policy, /rename_all\s*=\s*"snake_case"/, name);
  }
});

test('N01: IPC argument names carry no leading underscore', () => {
  for (const [name, { args }] of rustCommands()) {
    for (const arg of args) {
      assert.ok(!arg.name.startsWith('_'), `${name}: ${arg.name}`);
    }
  }
});

test('N01: controller payloads match the Rust command arguments', () => {
  const commands = rustCommands();
  const calls = controllerCalls();
  assert.ok(calls.length >= 20, 'must inspect the controller calls');
  for (const { name, keys } of calls) {
    const command = commands.get(name);
    assert.ok(command, `${name} is not a Notes command`);
    const known = new Set(command.args.map((arg) => arg.name));
    for (const key of keys) {
      assert.ok(known.has(key), `${name}: unknown argument ${key}`);
    }
    for (const arg of command.args.filter((a) => !a.optional)) {
      assert.ok(keys.includes(arg.name), `${name}: missing argument ${arg.name}`);
    }
  }
});

test('N01: notes_save keeps one top-level request with snake_case fields', () => {
  const save = controllerCalls().find((call) => call.name === 'notes_save');
  assert.deepEqual(save.keys, ['request']);
  const start = controller.indexOf('{', controller.indexOf('request:', controller.indexOf("'notes_save'")));
  const fields = topLevelKeys(controller, start);
  for (const field of ['workspace_id', 'request_id', 'note_id', 'expected', 'body']) {
    assert.ok(fields.includes(field), field);
  }
});
