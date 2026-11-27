import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import fs from 'node:fs';

const noevGrammar = JSON.parse(
  fs.readFileSync(new URL('./src/grammars/noev.tmLanguage.json', import.meta.url), 'utf8'),
);
const ebnfGrammar = JSON.parse(
  fs.readFileSync(new URL('./src/grammars/ebnf.tmLanguage.json', import.meta.url), 'utf8'),
);

const site = process.env.SITE_URL || 'https://aethar01.github.io';
const base = process.env.SITE_BASE ?? '/noev';

export default defineConfig({
  site,
  base,
  integrations: [
    starlight({
      title: 'Noev',
      description:
        'A statically typed hybrid classical-quantum programming language with linear qubit ownership and automatic circuit extraction.',
      logo: {
        src: './src/assets/logo.svg',
      },
      favicon: '/favicon.svg',
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/Aethar01/noev',
        },
      ],
      editLink: {
        baseUrl: 'https://github.com/Aethar01/noev/edit/main/website/',
      },
      customCss: ['./src/styles/custom.css'],
      expressiveCode: {
        themes: ['github-dark-default', 'github-light-default'],
        shiki: {
          langs: [noevGrammar, ebnfGrammar],
        },
      },
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Installation', slug: 'getting-started/installation' },
            { label: 'Quickstart', slug: 'getting-started/quickstart' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Language Tour', slug: 'guides/language-tour' },
            { label: 'Ownership and Borrowing', slug: 'guides/ownership' },
            { label: 'Circuits and Execution', slug: 'guides/circuits' },
            { label: 'Interop (Rust, Python, C)', slug: 'guides/interop' },
            { label: 'Backends and Targets', slug: 'guides/backends' },
            { label: 'Testing and Tooling', slug: 'guides/tooling' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Syntax and Features', slug: 'reference/syntax' },
            { label: 'Quantum API', slug: 'reference/quantum' },
            { label: 'Standard Library', slug: 'reference/stdlib' },
            { label: 'Error Index (N001–N024)', slug: 'reference/errors' },
            { label: 'CLI Reference', slug: 'reference/cli' },
          ],
        },
        {
          label: 'Specification',
          collapsed: false,
          items: [
            { label: 'Overview', slug: 'spec' },
            { label: 'Grammar', slug: 'spec/grammar' },
            { label: 'Types', slug: 'spec/types' },
            { label: 'Ownership', slug: 'spec/ownership' },
            { label: 'Core Intrinsics', slug: 'spec/core' },
            { label: 'Standard Library', slug: 'spec/std' },
            { label: 'Execution Model', slug: 'spec/execution' },
            { label: 'Runtime and Backends', slug: 'spec/runtime' },
            { label: 'Modules and Packages', slug: 'spec/modules' },
            { label: 'Interop', slug: 'spec/interop' },
            { label: 'Testing', slug: 'spec/testing' },
          ],
        },
      ],
    }),
  ],
});
