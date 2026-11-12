// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightSidebarTopics from 'starlight-sidebar-topics';
import { readFileSync } from 'node:fs';

// docs/redirects.json is the one source of redirects (old URL -> { to, kind }); the docs lint in
// `vam-e2e check` reads the same file and checks that each target is a page.
const redirects = Object.fromEntries(
	Object.entries(
		JSON.parse(readFileSync(new URL('./redirects.json', import.meta.url), 'utf8')),
	).map(([from, { to }]) => [from, to]),
);

// https://astro.build/config
export default defineConfig({
	redirects,
	integrations: [
		starlight({
			title: 'VAM',
			components: {
				// Renders the `status` and `proves` frontmatter (planned/out-of-date banner, proofs).
				PageTitle: './src/components/PageTitle.astro',
			},
			description:
				'Documentation for the Virtual Agentic Machine — a Rust workspace for microVM-based agent workloads.',
			plugins: [
				starlightSidebarTopics([
					// ==========================================================
					// TOPIC 1: DEVELOPER DOCS (Default Topic)
					// ==========================================================
					{
						label: 'Developer Docs',
						link: '/getting-started/introduction/',
						icon: 'rocket',
						items: [
							{
								label: 'Getting Started',
								items: [
									{ label: 'Introduction', slug: 'getting-started/introduction' },
									{ label: 'Quickstart (5 Min)', slug: 'getting-started/quickstart' },
									{ label: 'Installation & Setup', slug: 'getting-started/installation' },
									{ label: 'macOS + Lima', slug: 'getting-started/macos-lima' },
									{ label: 'Windows + WSL', slug: 'getting-started/windows-wsl' },
									{ label: 'Users and roles', slug: 'getting-started/users-and-roles' },
									{ label: 'CLI Reference', slug: 'getting-started/cli' },
									{ label: 'Platform Tour', slug: 'getting-started/platform-tour' },
									{ label: 'Project Status', slug: 'getting-started/project-status' },
								],
							},
							{
								label: 'Agent & Spec Authoring',
								items: [
									{ label: 'Authoring model', slug: 'authoring/model' },
									{ label: 'Agent Package Layout', slug: 'authoring/agent-package' },
									{ label: 'Agent Specs (agent.json)', slug: 'authoring/agent-spec' },
									{ label: 'Skills & Frontmatter (SKILL.md)', slug: 'authoring/skill-manifest' },
									{ label: 'Publish a Skill Package', slug: 'authoring/publish-skill' },
									{ label: 'Run Specs (run.json)', slug: 'authoring/run-spec' },
									{ label: 'Playbooks (playbook.json)', slug: 'authoring/playbook-spec' },
									{ label: 'Playbook Tutorial', slug: 'authoring/playbook-tutorial' },
								],
							},
							{
								label: 'In-VM Tool Development',
								items: [
									{ label: 'Writing In-VM Tools', slug: 'tools/writing-tools' },
									{ label: 'Tool Runtime Templates', slug: 'tools/tool-runtimes' },
								],
							},
							{
								label: 'Zero-Trust Integrations',
								items: [
									{ label: 'Egress Proxy Architecture', slug: 'integrations/egress-proxy-deepdive' },
									{ label: 'Jira Integration Walkthrough', slug: 'integrations/jira-assistant-walkthrough' },
								],
							},
							{
								label: 'Security & Governance',
								items: [
									{ label: 'Cedar Policies & HITL', slug: 'security/cedar-and-hitl' },
									{ label: 'HITL Approval Lifecycle', slug: 'security/hitl-lifecycle' },
									{ label: 'Secrets Vault Management', slug: 'security/secret-management' },
								],
							},
							{
								label: 'Tutorials & Recipes',
								items: [
									{ label: 'Jira Assistant (HITL Demo)', slug: 'tutorials/jira-assistant' },
									{ label: 'Bun Assistant (Workspace)', slug: 'tutorials/bun-assistant' },
								],
							},
						],
					},

					// ==========================================================
					// TOPIC 2: ARCHITECTURE & SUBSYSTEM RFCs
					// ==========================================================
					{
						label: 'Architecture & RFCs',
						link: '/architecture/overview/',
						icon: 'puzzle',
						items: [
							{
								label: 'System Architecture',
								items: [
									{ label: 'Overview', slug: 'architecture/overview' },
									{ label: 'Isolation', slug: 'architecture/isolation' },
									{ label: 'Cold-start & Golden Snapshot', slug: 'architecture/cold-start' },
									{ label: 'Testing', slug: 'architecture/testing' },
								],
							},
							{
								label: 'Spec',
								items: [
									{ label: 'Overview', slug: 'spec/overview' },
									{ label: 'Glossary', slug: 'spec/glossary' },
								],
							},
							{
								label: 'Images & Build (planned)',
								items: [
									{ label: 'RootFS Assembly', slug: 'build/rootfs' },
									{ label: 'Overview', slug: 'build/overview' },
									{ label: 'Build Architecture', slug: 'build/architecture' },
									{ label: 'Vamfile Language', slug: 'build/vamfile' },
									{ label: 'Directive Reference', slug: 'build/directives' },
									{ label: '`vam build` Command', slug: 'build/cli' },
									{ label: 'Sources & Build Context', slug: 'build/sources' },
									{ label: 'Builder Backends', slug: 'build/backends' },
									{ label: 'Runtime Injection', slug: 'build/runtime-injection' },
									{ label: 'Guest Compatibility Contract', slug: 'build/compatibility' },
									{ label: 'Template Catalog', slug: 'build/templates' },
								],
							},
							{
								label: 'Core VMM',
								items: [
									{ label: 'Overview', slug: 'core/overview' },
									{ label: 'Prerequisites', slug: 'core/prerequisites' },
								],
							},
							{
								label: 'Orchestrator',
								items: [
									{ label: 'Host daemon', slug: 'orchestrator/overview' },
									{ label: 'Host API', slug: 'orchestrator/api' },
									{ label: 'Workspace Files API', slug: 'orchestrator/workspace-files' },
									{ label: 'Workspace Files Upload', slug: 'orchestrator/workspace-files-upload' },
									{ label: 'Sessions and runs', slug: 'orchestrator/session' },
									{ label: 'Catalog', slug: 'orchestrator/catalog' },
									{ label: 'Users, keys and ownership', slug: 'orchestrator/user-isolation' },
									{ label: 'Notifications', slug: 'orchestrator/notifications' },
								],
							},
							{
								label: 'Hosted Services (HSP)',
								items: [
									{ label: 'Overview', slug: 'hosted/overview' },
									{ label: 'Web Broker', slug: 'hosted/web' },
									{ label: 'Broker-Triggered Runs', slug: 'hosted/triggered-runs' },
									{ label: 'Schedule Broker', slug: 'hosted/schedule' },
									{ label: 'Notify Broker', slug: 'hosted/notify' },
								],
							},
							{
								label: 'Network',
								items: [
									{ label: 'Overview', slug: 'network/overview' },
								],
							},
							{
								label: 'Runtime',
								items: [
									{ label: 'Guest runtime', slug: 'runtime/overview' },
									{ label: 'Agent loop', slug: 'runtime/agent' },
									{ label: 'Inference', slug: 'runtime/inference' },
									{ label: 'Tools', slug: 'runtime/tools' },
									{ label: 'Playbooks, gates and approvals', slug: 'runtime/workflow' },
									{ label: 'Jig', slug: 'runtime/jig' },
									{ label: 'Inbox and Outbox', slug: 'runtime/inbox-outbox' },
									{ label: 'Benchmark Results', slug: 'runtime/benchmark-results' },
								],
							},
							{
								label: 'Security',
								items: [
									{ label: 'Overview', slug: 'security/overview' },
									{ label: 'Run Receipt', slug: 'security/run-receipt' },
									{ label: 'Permissions', slug: 'security/permissions' },
									{ label: 'Secret Injection Proxy', slug: 'security/secrets-proxy' },
								],
							},
							{
								label: 'Storage',
								items: [
									{ label: 'Overview', slug: 'storage/overview' },
									{ label: 'Turn Commits & Thread Log', slug: 'storage/turn-commits' },
								],
							},
							{
								label: 'Telemetry',
								items: [
									{ label: 'Overview', slug: 'telemetry/overview' },
									{ label: 'Event Catalog', slug: 'telemetry/event-catalog' },
								],
							},
						],
					},

					// ==========================================================
					// TOPIC 3: AGENT SPEC & LLM REFERENCE
					// ==========================================================
					{
						label: 'Agent Spec & LLMs',
						link: '/reference/agent-cheat-sheet/',
						icon: 'laptop',
						items: [
							{
								label: 'AI Agent Reference',
								items: [
									{ label: 'Agent Cheatsheet', slug: 'reference/agent-cheat-sheet' },
								],
							},
						],
					},
				]),
			],
		}),
	],
});
