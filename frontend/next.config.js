const path = require('path');
const fs = require('fs');
const crypto = require('crypto');
const tiptapPmResolveBase = path.dirname(require.resolve('@tiptap/pm/model'));
const resolveFromTiptapPm = (pkg) =>
  require.resolve(pkg, { paths: [tiptapPmResolveBase] });

const hashBuildInput = (hash, absolutePath, relativePath) => {
  if (!fs.existsSync(absolutePath)) return;
  const stat = fs.statSync(absolutePath);
  if (stat.isDirectory()) {
    for (const entry of fs.readdirSync(absolutePath).sort()) {
      hashBuildInput(hash, path.join(absolutePath, entry), path.join(relativePath, entry));
    }
    return;
  }
  if (!stat.isFile()) return;
  hash.update(relativePath);
  hash.update('\0');
  hash.update(fs.readFileSync(absolutePath));
  hash.update('\0');
};

const deterministicBuildId = () => {
  const explicit = process.env.MEETILY_BUILD_ID;
  if (explicit) {
    if (!/^[A-Za-z0-9._-]{1,64}$/.test(explicit)) {
      throw new Error('MEETILY_BUILD_ID contains unsupported characters');
    }
    return explicit;
  }

  const hash = crypto.createHash('sha256');
  for (const input of ['src', 'public', 'package.json', 'pnpm-lock.yaml', 'tsconfig.json']) {
    hashBuildInput(hash, path.join(__dirname, input), input);
  }
  hashBuildInput(hash, __filename, 'next.config.js');
  return `meetily-${hash.digest('hex').slice(0, 20)}`;
};

/** @type {import('next').NextConfig} */
const nextConfig = {
  outputFileTracingRoot: __dirname,
  generateBuildId: async () => deterministicBuildId(),
  // Serial compilation removes worker-order-dependent chunk IDs so identical
  // release inputs produce byte-identical exported assets.
  experimental: {
    cpus: 1,
  },
  reactStrictMode: false, // Disabled for BlockNote compatibility
  output: 'export',
  images: {
    unoptimized: true,
  },
  // Next 15's embedded legacy linter cannot resolve plugins with strict pnpm.
  // Lint is run explicitly through the ESLint 9 flat config instead.
  eslint: {
    ignoreDuringBuilds: true,
  },
  // Add basePath configuration
  basePath: '',
  assetPrefix: '/',

  // Add webpack configuration for Tauri
  webpack: (config, { isServer }) => {
    // Webpack's persistent cache alternates module/chunk IDs across otherwise
    // identical sequential release builds in Next 15. Disable it so packaged
    // static assets are derived only from the reviewed source inputs.
    config.cache = false;
    config.parallelism = 1;
    config.optimization.moduleIds = 'deterministic';
    config.optimization.chunkIds = 'deterministic';
    config.optimization.mangleExports = 'deterministic';
    // Module concatenation consumes module-discovery order, which can differ
    // across otherwise identical builds even with one Next worker.
    config.optimization.concatenateModules = false;

    if (!isServer) {
      config.resolve.fallback = {
        ...config.resolve.fallback,
        fs: false,
        path: false,
        os: false,
      };

      // Keep ProseMirror single-instanced for BlockNote/Tiptap.
      config.resolve.alias = {
        ...config.resolve.alias,
        '@blocknote/core$': require.resolve('@blocknote/core'),
        '@blocknote/react$': require.resolve('@blocknote/react'),
        '@blocknote/shadcn$': require.resolve('@blocknote/shadcn'),
        'prosemirror-model': resolveFromTiptapPm('prosemirror-model'),
        'prosemirror-state': resolveFromTiptapPm('prosemirror-state'),
        'prosemirror-view': resolveFromTiptapPm('prosemirror-view'),
        'prosemirror-transform': resolveFromTiptapPm('prosemirror-transform'),
        'prosemirror-tables': resolveFromTiptapPm('prosemirror-tables'),
        'prosemirror-schema-list': resolveFromTiptapPm('prosemirror-schema-list'),
        'prosemirror-keymap': resolveFromTiptapPm('prosemirror-keymap'),
        'prosemirror-commands': resolveFromTiptapPm('prosemirror-commands'),
        'prosemirror-history': resolveFromTiptapPm('prosemirror-history'),
        'prosemirror-inputrules': resolveFromTiptapPm('prosemirror-inputrules'),
        'prosemirror-gapcursor': resolveFromTiptapPm('prosemirror-gapcursor'),
        'prosemirror-dropcursor': resolveFromTiptapPm('prosemirror-dropcursor'),
      };
    }
    return config;
  },
}

module.exports = nextConfig
