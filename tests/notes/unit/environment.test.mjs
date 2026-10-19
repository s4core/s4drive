import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const application = readFileSync(new URL('../../../src-tauri/dist/js/main.js', import.meta.url), 'utf8');

for (const partialTauri of [false, true]) {
  test(`application explains unavailable desktop IPC (${partialTauri ? 'partial' : 'missing'} Tauri)`, () => {
    const controls = [{ disabled: false }, { disabled: false }];
    const content = { children: [], replaceChildren(...children) { this.children = children; } };
    const badge = { textContent: 'Idle' };
    const classes = new Set();
    const document = {
      readyState: 'complete',
      body: { classList: { add: (name) => classes.add(name) } },
      createElement: () => ({ setAttribute(name, value) { this[name] = value; } }),
      getElementById: (id) => id === 'appContent' ? content : badge,
      querySelectorAll: () => controls,
    };
    const window = partialTauri ? { __TAURI__: { core: { invoke() { assert.fail('unsupported IPC must not be invoked'); } } } } : {};
    vm.runInNewContext(application, { document, window });
    assert.equal(content.children.length, 1);
    assert.equal(content.children[0].role, 'alert');
    assert.equal(content.children[0].textContent, 'Open the S4Drive desktop app to use your files.');
    assert.equal(badge.textContent, 'Unavailable');
    assert.ok(controls.every((control) => control.disabled));
    assert.ok(classes.has('ready'));
  });
}
