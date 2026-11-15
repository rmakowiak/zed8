import vue from '@vitejs/plugin-vue';
import { defineConfig, type UserConfig } from 'vite';
import { editorUiAliases } from './vite/aliases.mjs';
import { devServerPlugin } from './vite/dev-ports.mjs';
import { worktreeBadgePlugin } from './vite/vite-plugin-worktree-badge.mjs';

const plugins: UserConfig['plugins'] = [
	vue(),
	devServerPlugin(process.env),
	worktreeBadgePlugin(process.env),
	editorUiAliases(),
];

export default defineConfig({ plugins });
