import test from 'node:test';
import assert from 'node:assert/strict';
import {
  applyMarkdownAction,
  renderMarkdown,
} from '../../../src-tauri/dist/js/notes/notes-markdown.js';

test('applyMarkdownAction: bold wraps selected text (6.1)', () => {
  const result = applyMarkdownAction('Hello world', 6, 11, 'bold');
  assert.equal(result.body, 'Hello **world**');
  assert.equal(result.start, 8);
  assert.equal(result.end, 13);
});

test('applyMarkdownAction: bold inserts delimiters and places caret inside when empty selection', () => {
  const result = applyMarkdownAction('Hello world', 5, 5, 'bold');
  assert.equal(result.body, 'Hello**** world');
  assert.equal(result.start, 7);
  assert.equal(result.end, 7);
});

test('applyMarkdownAction: italic wraps selected text with Unicode', () => {
  const text = 'Привет мир!';
  const result = applyMarkdownAction(text, 7, 10, 'italic');
  assert.equal(result.body, 'Привет *мир*!');
  assert.equal(result.start, 8);
  assert.equal(result.end, 11);
});

test('applyMarkdownAction: heading cycles heading levels on line', () => {
  // 1. None -> #
  const res1 = applyMarkdownAction('Title of note', 3, 3, 'heading');
  assert.equal(res1.body, '# Title of note');

  // 2. # -> ##
  const res2 = applyMarkdownAction(res1.body, 5, 5, 'heading');
  assert.equal(res2.body, '## Title of note');

  // 3. ## -> ###
  const res3 = applyMarkdownAction(res2.body, 6, 6, 'heading');
  assert.equal(res3.body, '### Title of note');
});

test('applyMarkdownAction: multiline list prefixes lines with - ', () => {
  const body = 'Item one\nItem two\nItem three';
  const result = applyMarkdownAction(body, 0, body.length, 'list');
  assert.equal(result.body, '- Item one\n- Item two\n- Item three');

  // Toggle off if all lines have -
  const toggled = applyMarkdownAction(result.body, 0, result.body.length, 'list');
  assert.equal(toggled.body, 'Item one\nItem two\nItem three');
});

test('applyMarkdownAction: multiline task prefixes lines with - [ ] ', () => {
  const body = 'Task A\nTask B';
  const result = applyMarkdownAction(body, 0, body.length, 'task');
  assert.equal(result.body, '- [ ] Task A\n- [ ] Task B');
});

test('applyMarkdownAction: code handles inline and multiline code block', () => {
  // Inline
  const inlineRes = applyMarkdownAction('const x = 1;', 6, 11, 'code');
  assert.equal(inlineRes.body, 'const `x = 1`;');

  // Multiline
  const multi = 'function test() {\n  return 42;\n}';
  const multiRes = applyMarkdownAction(multi, 0, multi.length, 'code');
  assert.equal(multiRes.body, '```\nfunction test() {\n  return 42;\n}\n```');
});

test('applyMarkdownAction: link wraps selection in markdown link', () => {
  const result = applyMarkdownAction('Visit website today', 6, 13, 'link');
  assert.equal(result.body, 'Visit [website](url) today');
});
