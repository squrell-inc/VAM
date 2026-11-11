import { defineCollection } from 'astro:content';
import { z } from 'astro/zod';
import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';

// `status` says what the page describes; `proves` names the scenario ids or test paths that back
// its claims (see `vam-e2e check --docs`). `status` is optional until every page has one.
export const collections = {
	docs: defineCollection({
		loader: docsLoader(),
		schema: docsSchema({
			extend: z.object({
				status: z.enum(['shipped', 'planned', 'stale']).optional(),
				proves: z.array(z.string()).optional(),
			}),
		}),
	}),
};
