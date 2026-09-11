import type { Api, Model, Provider } from "@dum-e/ai";

function mergeModels(baseline: readonly Model<Api>[], dynamic: readonly Model<Api>[]): Model<Api>[] {
	const merged = [...baseline];
	for (const model of dynamic) {
		const index = merged.findIndex((entry) => entry.id === model.id);
		if (index >= 0) merged[index] = model;
		else merged.push(model);
	}
	return merged;
}

/** In standalone DUM-E, remote pi.dev catalog fetching is disabled. Static built-in models provide catalog. */
export function withRemoteCatalog(provider: Provider, _catalogBaseUrl?: string, _localGeneratedAt?: number): Provider {
	const dynamicModels: readonly Model<Api>[] = [];

	return {
		...provider,
		getModels: () => mergeModels(provider.getModels(), dynamicModels),
		refreshModels: async () => {},
	};
}
