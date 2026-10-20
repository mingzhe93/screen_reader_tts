#!/usr/bin/env node

// Checks the Markdown parser the Assistant page uses (src/assistant-markdown.ts): that each
// supported construct parses, that hostile input (raw HTML, javascript: links, images) can
// only come out as plain text or an inert link, and that huge input stays fast.
// Only the pure parser is tested here; the DOM renderer builds elements from these nodes.
//
// Run with: npm run test:assistant

import assert from 'node:assert/strict';
import os from 'os';
import path from 'path';
import { fileURLToPath, pathToFileURL } from 'url';
import { build } from 'esbuild';

const here = path.dirname(fileURLToPath(import.meta.url));
const bundle = path.join(os.tmpdir(), `voicereader-assistant-markdown-${process.pid}.mjs`);
await build({
  entryPoints: [path.join(here, '..', 'src', 'assistant-markdown.ts')],
  bundle: true,
  format: 'esm',
  platform: 'node',
  outfile: bundle,
  logLevel: 'silent',
});
const { parseMarkdown, markdownToPlainText } = await import(pathToFileURL(bundle).href);

let passed = 0;
function check(name, fn) {
  try {
    fn();
    passed += 1;
  } catch (error) {
    console.error(`FAIL ${name}`);
    throw error;
  }
}

const text = (value) => ({ type: 'text', text: value });

/** Every inline node in a block tree, depth first. */
function inlines(blocks) {
  const found = [];
  const walk = (nodes) => {
    for (const node of nodes) {
      found.push(node);
      if (node.children) walk(node.children);
    }
  };
  const visit = (block) => {
    if (block.inline) walk(block.inline);
    if (block.items) {
      for (const item of block.items) {
        walk(item.inline);
        if (item.sublist) visit(item.sublist);
      }
    }
    if (block.blocks) block.blocks.forEach(visit);
    if (block.header) for (const cell of [...block.header, ...block.rows.flat()]) walk(cell);
  };
  blocks.forEach(visit);
  return found;
}

check('paragraphs split on blank lines and keep soft line breaks', () => {
  const blocks = parseMarkdown('first line\nsecond line\n\nnext');
  assert.equal(blocks.length, 2);
  assert.deepEqual(blocks[0], { type: 'paragraph', inline: [text('first line\nsecond line')] });
  assert.deepEqual(blocks[1], { type: 'paragraph', inline: [text('next')] });
});

check('ATX headings', () => {
  const [h1, h3, notHeading] = parseMarkdown('# Title\n\n### Sub ###\n\n#hashtag');
  assert.deepEqual(h1, { type: 'heading', level: 1, inline: [text('Title')] });
  assert.deepEqual(h3, { type: 'heading', level: 3, inline: [text('Sub')] });
  assert.equal(notHeading.type, 'paragraph');
});

check('emphasis, bold and inline code', () => {
  const [p] = parseMarkdown('a *em* and **bold** and `code` and _also_ __strong__ end');
  assert.deepEqual(p.inline, [
    text('a '),
    { type: 'em', children: [text('em')] },
    text(' and '),
    { type: 'strong', children: [text('bold')] },
    text(' and '),
    { type: 'code', text: 'code' },
    text(' and '),
    { type: 'em', children: [text('also')] },
    text(' '),
    { type: 'strong', children: [text('strong')] },
    text(' end'),
  ]);
});

check('snake_case words and spaced asterisks are not emphasis', () => {
  const [p] = parseMarkdown('use my_var_name and 2 * 3 * 4');
  assert.deepEqual(p.inline, [text('use my_var_name and 2 * 3 * 4')]);
});

check('nested emphasis', () => {
  const [p] = parseMarkdown('**bold with *em* inside**');
  assert.deepEqual(p.inline, [
    { type: 'strong', children: [text('bold with '), { type: 'em', children: [text('em')] }, text(' inside')] },
  ]);
});

check('fenced code block keeps its text and language', () => {
  const [code] = parseMarkdown('```python\nprint("<b>hi</b>")\n\n  x = 1\n```');
  assert.deepEqual(code, { type: 'code', lang: 'python', text: 'print("<b>hi</b>")\n\n  x = 1', closed: true });
  const [tilde] = parseMarkdown('~~~\nraw **not bold**\n~~~');
  assert.deepEqual(tilde, { type: 'code', lang: '', text: 'raw **not bold**', closed: true });
});

check('an unterminated code fence is kept as an open code block', () => {
  const blocks = parseMarkdown('Here:\n\n```js\nconst a = 1;\nconst b = ');
  assert.equal(blocks.length, 2);
  assert.deepEqual(blocks[1], { type: 'code', lang: 'js', text: 'const a = 1;\nconst b = ', closed: false });
});

check('a half-written **bold stays literal', () => {
  const [p] = parseMarkdown('this is **bold');
  assert.deepEqual(p.inline, [text('this is **bold')]);
  const [q] = parseMarkdown('a `half code and *half em');
  assert.deepEqual(q.inline, [text('a `half code and *half em')]);
});

check('unordered and ordered lists with one nesting level', () => {
  const [ul, ol] = parseMarkdown('- one\n- two\n  - nested a\n  - nested b\n- three\n\n3. third\n4. fourth');
  assert.equal(ul.type, 'list');
  assert.equal(ul.ordered, false);
  assert.equal(ul.items.length, 3);
  assert.deepEqual(ul.items[0].inline, [text('one')]);
  assert.equal(ul.items[1].sublist.items.length, 2);
  assert.deepEqual(ul.items[1].sublist.items[1].inline, [text('nested b')]);
  assert.equal(ol.ordered, true);
  assert.equal(ol.start, 3);
  assert.equal(ol.items.length, 2);
});

check('list items that continue on an indented line', () => {
  const [list] = parseMarkdown('1. first\n   continues here\n2. second');
  assert.equal(list.items.length, 2);
  assert.deepEqual(list.items[0].inline, [text('first\ncontinues here')]);
});

check('block quote and horizontal rule', () => {
  const [quote, hr, after] = parseMarkdown('> quoted **text**\n> more\n\n---\n\nafter');
  assert.equal(quote.type, 'quote');
  assert.equal(quote.blocks.length, 1);
  assert.equal(quote.blocks[0].type, 'paragraph');
  assert.deepEqual(hr, { type: 'hr' });
  assert.equal(after.type, 'paragraph');
  assert.equal(parseMarkdown('* * *')[0].type, 'hr');
});

check('links and bare URLs', () => {
  const [p] = parseMarkdown('see [the docs](https://example.com/a_b) or https://example.org/x. Done');
  const links = p.inline.filter((n) => n.type === 'link');
  assert.equal(links.length, 2);
  assert.deepEqual(links[0], { type: 'link', children: [text('the docs')], url: 'https://example.com/a_b', openable: true });
  assert.equal(links[1].url, 'https://example.org/x');
  assert.equal(links[1].openable, true);
  assert.deepEqual(p.inline[p.inline.length - 1], text('. Done'));
});

check('a bare URL in parentheses does not swallow the closing parenthesis', () => {
  const [p] = parseMarkdown('(see https://example.com/page)');
  const link = p.inline.find((n) => n.type === 'link');
  assert.equal(link.url, 'https://example.com/page');
});

check('links with parentheses in the URL', () => {
  const [p] = parseMarkdown('[wiki](https://en.wikipedia.org/wiki/Function_(mathematics)) ok');
  const link = p.inline.find((n) => n.type === 'link');
  assert.equal(link.url, 'https://en.wikipedia.org/wiki/Function_(mathematics)');
  assert.equal(link.openable, true);
});

check('raw HTML stays plain text', () => {
  for (const html of [
    '<img src=x onerror=alert(1)>',
    '<script>alert(1)</script>',
    '<a href="javascript:alert(1)">click</a>',
    '<iframe src="https://evil.example"></iframe>',
    '<b onclick="x()">bold</b>',
  ]) {
    const blocks = parseMarkdown(html);
    assert.equal(blocks.length, 1, html);
    assert.equal(blocks[0].type, 'paragraph', html);
    const nodes = blocks[0].inline;
    for (const node of nodes) {
      assert.ok(node.type === 'text' || node.type === 'link', `${html}: unexpected ${node.type}`);
    }
    const shown = nodes.map((n) => (n.type === 'text' ? n.text : n.children.map((c) => c.text).join(''))).join('');
    assert.equal(shown, html);
  }
  // The pure-text case is exactly one text node.
  assert.deepEqual(parseMarkdown('<img src=x onerror=alert(1)>')[0].inline, [text('<img src=x onerror=alert(1)>')]);
  assert.deepEqual(parseMarkdown('<script>alert(1)</script>')[0].inline, [text('<script>alert(1)</script>')]);
  assert.deepEqual(parseMarkdown('<a href="javascript:alert(1)">click</a>')[0].inline, [
    text('<a href="javascript:alert(1)">click</a>'),
  ]);
});

check('HTML inside a fenced code block is code text, not markup', () => {
  const [code] = parseMarkdown('```html\n<script>alert(1)</script>\n```');
  assert.equal(code.type, 'code');
  assert.equal(code.text, '<script>alert(1)</script>');
});

check('javascript: and file: link targets are flagged not openable', () => {
  const [p] = parseMarkdown('[x](javascript:alert(1)) and [y](file:///etc/passwd) and [z](data:text/html,hi)');
  const links = p.inline.filter((n) => n.type === 'link');
  assert.equal(links.length, 3);
  assert.deepEqual(links[0], { type: 'link', children: [text('x')], url: 'javascript:alert(1)', openable: false });
  assert.deepEqual(links[1], { type: 'link', children: [text('y')], url: 'file:///etc/passwd', openable: false });
  assert.equal(links[2].openable, false);
});

check('only http and https are openable, any case', () => {
  const [p] = parseMarkdown('[a](HTTPS://EXAMPLE.COM) [b](http://example.com) [c](ftp://example.com) [d](//example.com) [e](/relative)');
  const flags = p.inline.filter((n) => n.type === 'link').map((n) => n.openable);
  assert.deepEqual(flags, [true, true, false, false, false]);
});

check('markdown images are never image nodes and keep alt and URL as text', () => {
  const blocks = parseMarkdown('![a cat](https://example.com/cat.png) and ![x](javascript:alert(1))');
  const nodes = inlines(blocks);
  assert.ok(nodes.every((n) => ['text', 'link'].includes(n.type)));
  assert.equal(nodes.filter((n) => n.type === 'link').length, 0);
  assert.deepEqual(blocks[0].inline, [text('![a cat](https://example.com/cat.png) and ![x](javascript:alert(1))')]);
});

check('no node type can carry markup', () => {
  const src = [
    '# <h1 onload=x>',
    '- <li onclick=x>',
    '> <svg onload=alert(1)>',
    '**<b>**',
    '[<img src=x onerror=y>](https://example.com)',
    '`<script>`',
  ].join('\n\n');
  const allowed = new Set(['text', 'code', 'strong', 'em', 'link']);
  for (const node of inlines(parseMarkdown(src))) assert.ok(allowed.has(node.type), node.type);
});

check('plain text for Speak and Copy', () => {
  const src = '# Title\n\nSome **bold**, *em*, `code` and [a link](https://example.com).\n\n- one\n- two\n\n1. first\n2. second\n\n> quoted\n\n---\n\n```js\nlet a = 1;\n```';
  assert.equal(
    markdownToPlainText(src),
    'Title\n\nSome bold, em, code and a link.\n\none\ntwo\n\n1. first\n2. second\n\nquoted\n\nlet a = 1;',
  );
  assert.equal(markdownToPlainText('half **bold'), 'half **bold');
  assert.equal(markdownToPlainText('<b>x</b>'), '<b>x</b>');
  assert.equal(markdownToPlainText(''), '');
});

check('pipe tables parse with header, alignment and rows', () => {
  const src = '| Endpoint | Price | Note |\n|:---|---:|:-:|\n| Search | $5 | **fast** |\n| Answer | $4 | |';
  const [table] = parseMarkdown(src);
  assert.equal(table.type, 'table');
  assert.deepEqual(table.align, ['left', 'right', 'center']);
  assert.deepEqual(table.header, [[text('Endpoint')], [text('Price')], [text('Note')]]);
  assert.equal(table.rows.length, 2);
  assert.equal(table.rows[0][2][0].type, 'strong');
  assert.deepEqual(table.rows[1][2], []);
});

check('tables without outer pipes, and with an escaped pipe in a cell', () => {
  const [table] = parseMarkdown('a | b\n--- | ---\nx \\| y | z');
  assert.equal(table.type, 'table');
  assert.deepEqual(table.align, [null, null]);
  assert.deepEqual(table.rows[0][0], [text('x | y')]);
  assert.deepEqual(table.rows[0][1], [text('z')]);
});

check('a row still being streamed is padded, and extra cells are cut', () => {
  const [table] = parseMarkdown('| a | b |\n|---|---|\n| 1 |\n| 1 | 2 | 3 |');
  assert.equal(table.rows.length, 2);
  assert.deepEqual(table.rows[0], [[text('1')], []]);
  assert.equal(table.rows[1].length, 2);
});

check('a header with its delimiter is a table before any row arrives; a lone header line is text', () => {
  const [table] = parseMarkdown('| a | b |\n|---|---|');
  assert.equal(table.type, 'table');
  assert.equal(table.rows.length, 0);
  assert.equal(parseMarkdown('| a | b |')[0].type, 'paragraph');
});

check('a table ends at a blank line and can interrupt a paragraph', () => {
  const blocks = parseMarkdown('Intro line\n| a | b |\n|---|---|\n| 1 | 2 |\n\nAfter');
  assert.deepEqual(blocks.map((b) => b.type), ['paragraph', 'table', 'paragraph']);
  assert.equal(blocks[1].rows.length, 1);
});

check('text that only looks like a table stays text', () => {
  assert.equal(parseMarkdown('a | b')[0].type, 'paragraph');
  assert.deepEqual(parseMarkdown('text\n---').map((b) => b.type), ['paragraph', 'hr']);
  // The delimiter row must have as many columns as the header.
  assert.equal(parseMarkdown('| a | b |\n| - |')[0].type, 'paragraph');
  // Too many columns is not worth a table.
  const wide = Array.from({ length: 30 }, (_, i) => `c${i}`);
  assert.equal(parseMarkdown(`| ${wide.join(' | ')} |\n|${wide.map(() => '---').join('|')}|`)[0].type, 'paragraph');
  assert.equal(parseMarkdown('| a |\n|---|')[0].type, 'table');
});

check('table cells are inline text only: HTML, javascript links and images stay inert', () => {
  const [table] = parseMarkdown('| <img src=x onerror=y> | [x](javascript:alert(1)) |\n|---|---|\n| ![a](http://x/y.png) | `<script>` |');
  assert.equal(table.type, 'table');
  const cells = [...table.header, ...table.rows[0]].flat();
  const allowed = new Set(['text', 'code', 'strong', 'em', 'link']);
  for (const node of cells) assert.ok(allowed.has(node.type), node.type);
  assert.equal(cells.filter((n) => n.type === 'link' && n.openable).length, 0);
  assert.deepEqual(table.header[0], [text('<img src=x onerror=y>')]);
});

check('plain text for tables', () => {
  assert.equal(markdownToPlainText('| a | b |\n|---|---|\n| 1 | **2** |'), 'a | b\n1 | 2');
});

check('a 200 KB table parses fast and stops at the row limit', () => {
  const rows = '| cell one | cell two | cell three |\n'.repeat(6000);
  const started = performance.now();
  const blocks = parseMarkdown('| a | b | c |\n|---|---|---|\n' + rows);
  const elapsed = performance.now() - started;
  assert.equal(blocks[0].type, 'table');
  assert.equal(blocks[0].rows.length, 500);
  assert.ok(elapsed < 500, `took ${elapsed.toFixed(0)} ms`);
  // A line of nothing but pipes and dashes is not a catastrophic case for the delimiter check.
  const started2 = performance.now();
  parseMarkdown(('| - '.repeat(2000) + '|\n').repeat(100));
  assert.ok(performance.now() - started2 < 1000);
});

check('windows line endings parse like unix ones', () => {
  assert.deepEqual(parseMarkdown('# A\r\n\r\ntext\r\nmore'), parseMarkdown('# A\n\ntext\nmore'));
});

check('empty input and whitespace', () => {
  assert.deepEqual(parseMarkdown(''), []);
  assert.deepEqual(parseMarkdown('   \n\n  '), []);
});

check('a 200 KB document parses in under 200 ms', () => {
  const unit =
    '## Heading with **bold** and `code`\n\nA paragraph with *emphasis*, a [link](https://example.com/x), https://example.org/y and <b>html</b>.\n\n- item one\n- item two\n  - nested\n\n> quote line\n\n```js\nconst x = 1;\n```\n\n';
  let doc = '';
  while (doc.length < 200 * 1024) doc += unit;
  const started = performance.now();
  const blocks = parseMarkdown(doc);
  const plain = markdownToPlainText(doc);
  const elapsed = performance.now() - started;
  assert.ok(blocks.length > 1000);
  assert.ok(plain.length > 1000);
  if (process.env.VERBOSE) console.log(`200 KB document: ${elapsed.toFixed(0)} ms`);
  assert.ok(elapsed < 200, `took ${elapsed.toFixed(0)} ms`);
});

check('pathological 200 KB inputs stay fast', () => {
  const cases = {
    'unmatched asterisks': '*a '.repeat(70000),
    'unmatched underscores': '_a '.repeat(70000),
    'unmatched brackets': '[a](b '.repeat(35000),
    'unmatched backticks': '`a '.repeat(70000),
    'one long line of bold': '**b** '.repeat(35000),
    'nested quotes': '> '.repeat(100000) + 'x',
    'list lines': '- x\n'.repeat(50000),
  };
  for (const [name, doc] of Object.entries(cases)) {
    const started = performance.now();
    parseMarkdown(doc);
    const elapsed = performance.now() - started;
    if (process.env.VERBOSE) console.log(`${name}: ${elapsed.toFixed(0)} ms`);
    assert.ok(elapsed < 1000, `${name} took ${elapsed.toFixed(0)} ms`);
  }
});

console.log(`assistant markdown tests passed (${passed} checks)`);
