import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, '../..');
const specSrcDir = path.join(repoRoot, 'spec');
const specOutDir = path.join(repoRoot, 'website/src/content/docs/spec');

const specMeta = {
  'README.md': {
    out: 'index.md',
    title: 'Specification Overview',
    description: 'Overview and design principles of the Noev 0.5 language specification.',
  },
  'grammar.md': {
    out: 'grammar.md',
    title: 'Grammar',
    description: 'Lexical structure, keywords, literals, and syntax of Noev.',
  },
  'types.md': {
    out: 'types.md',
    title: 'Types',
    description: 'Classical and linear quantum types, generics, traits, literals, and conversions.',
  },
  'ownership.md': {
    out: 'ownership.md',
    title: 'Ownership',
    description: 'Moves, borrows, linearity, reversible contexts, uncomputation, and error codes N001–N024.',
  },
  'core.md': {
    out: 'core.md',
    title: 'Core Intrinsics',
    description: 'Built-in gates, measurements, observables, and maths intrinsics.',
  },
  'std.md': {
    out: 'std.md',
    title: 'Standard Library',
    description: 'Standard library types, collections, I/O, and reference definitions in Noev.',
  },
  'execution.md': {
    out: 'execution.md',
    title: 'Execution Model',
    description: 'Single-run semantics, sampling, classical control, and circuit extraction.',
  },
  'runtime.md': {
    out: 'runtime.md',
    title: 'Runtime and Backends',
    description: 'Code generators, runtime C ABI, target capabilities, and plug-in backends.',
  },
  'modules.md': {
    out: 'modules.md',
    title: 'Modules and Packages',
    description: 'Files as modules, visibility, imports, prelude, and noev.toml packages.',
  },
  'interop.md': {
    out: 'interop.md',
    title: 'Interop',
    description: 'Embedding Noev in Rust, Python, and C/C++, plus extern C imports and inline OpenQASM.',
  },
  'testing.md': {
    out: 'testing.md',
    title: 'Testing',
    description: 'Built-in test fn syntax, expected failures, discovery, and noev test options.',
  },
};

fs.rmSync(specOutDir, { recursive: true, force: true });
fs.mkdirSync(specOutDir, { recursive: true });

function rewriteLinks(markdown, isIndex) {
  const prefix = isIndex ? './' : '../';
  // Rewrite markdown links to sibling spec/*.md files, preserving optional anchors.
  return markdown.replace(
    /\]\(([a-z0-9_-]+)\.md(#[^)]+)?\)/gi,
    (match, stem, hash = '') => {
      if (stem.toLowerCase() === 'readme') {
        return `](${isIndex ? './' : '../'}${hash})`;
      }
      return `](${prefix}${stem}/${hash})`;
    },
  );
}

for (const [filename, meta] of Object.entries(specMeta)) {
  const srcPath = path.join(specSrcDir, filename);
  if (!fs.existsSync(srcPath)) {
    throw new Error(`Missing spec file: ${srcPath}`);
  }

  let body = fs.readFileSync(srcPath, 'utf8');
  // Remove leading H1 because Starlight renders frontmatter title as H1.
  body = body.replace(/^#\s+[^\n]+\r?\n+/, '');
  body = rewriteLinks(body, filename === 'README.md');

  const frontmatter = [
    '---',
    `title: ${JSON.stringify(meta.title)}`,
    `description: ${JSON.stringify(meta.description)}`,
    `editUrl: "https://github.com/Aethar01/noev/edit/main/spec/${filename}"`,
    '---',
    '',
  ].join('\n');

  fs.writeFileSync(path.join(specOutDir, meta.out), frontmatter + body, 'utf8');
}

console.log(`Synced ${Object.keys(specMeta).length} spec files into website/src/content/docs/spec/`);
