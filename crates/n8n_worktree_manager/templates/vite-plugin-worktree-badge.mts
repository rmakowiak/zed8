import type { Plugin } from 'vite';

// LOCAL DEV HACK (n8n-wt) — never meant to reach a commit. Marks which
// n8n-wt worktree's editor tab is open (favicon + a faint top-right corner
// label), read from N8N_WT_LABEL. No-ops when unset, so a plain checkout is
// unaffected.
// Runs in both `vite` dev-serve and `vite build` (no `apply` restriction) so
// the badge also shows on the backend port, which serves the built dist/.

const hashHue = (value: string): number => {
	let hash = 0;
	for (const char of value) hash = (hash * 31 + char.charCodeAt(0)) | 0;
	return Math.abs(hash) % 360;
};

const faviconDataUri = (label: string, hue: number): string => {
	const letter = label.charAt(0).toUpperCase();
	const svg =
		'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">' +
		`<circle cx="32" cy="32" r="30" fill="hsl(${hue},70%,45%)"/>` +
		'<text x="32" y="43" font-family="-apple-system,sans-serif" font-size="34" ' +
		`font-weight="700" fill="#fff" text-anchor="middle">${letter}</text></svg>`;
	return `data:image/svg+xml,${encodeURIComponent(svg)}`;
};

export const worktreeBadgePlugin = (env: NodeJS.ProcessEnv): Plugin => {
	const label = env.N8N_WT_LABEL;
	return {
		name: 'n8n-wt-worktree-badge',
		transformIndexHtml(html) {
			if (!label) return html;
			const hue = hashHue(label);
			const icon = faviconDataUri(label, hue);
			const banner =
				`<div style="position:fixed;top:8px;right:8px;z-index:2147483647;` +
				`pointer-events:none;padding:2px 8px;border-radius:6px;` +
				`background:hsla(${hue},70%,42%,0.55);color:#fff;` +
				`font:600 11px/1.4 -apple-system,BlinkMacSystemFont,sans-serif;` +
				`letter-spacing:.3px;text-transform:uppercase">${label}</div>`;
			return html
				.replace('</head>', `<link rel="icon" href="${icon}" /></head>`)
				.replace('</body>', `${banner}</body>`);
		},
	};
};
