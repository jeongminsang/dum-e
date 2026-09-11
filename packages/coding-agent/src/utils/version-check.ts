import { compare, valid } from "semver";

export interface LatestPiRelease {
	version: string;
	packageName?: string;
	note?: string;
}

/** Include useful errno details hidden behind Node's generic "fetch failed" error. */
export function formatVersionCheckError(error: unknown): string {
	const rootMessage = error instanceof Error && error.message ? error.message : String(error);
	const cause = error instanceof Error ? error.cause : undefined;
	const causes = cause instanceof AggregateError ? cause.errors : cause === undefined ? [] : [cause];
	const codes = causes
		.map((value) =>
			typeof value === "object" && value !== null && "code" in value && typeof value.code === "string"
				? value.code
				: undefined,
		)
		.filter((code): code is string => code !== undefined);

	if (codes.length > 0) return `${rootMessage} (${[...new Set(codes)].join(", ")})`;
	const causeMessage = causes.find(
		(value): value is Error => value instanceof Error && Boolean(value.message),
	)?.message;
	return causeMessage ? `${rootMessage} (cause: ${causeMessage})` : rootMessage;
}

export function comparePackageVersions(leftVersion: string, rightVersion: string): number | undefined {
	const left = valid(leftVersion.trim());
	const right = valid(rightVersion.trim());
	if (!left || !right) {
		return undefined;
	}
	return compare(left, right);
}

export function isNewerPackageVersion(candidateVersion: string, currentVersion: string): boolean {
	const comparison = comparePackageVersions(candidateVersion, currentVersion);
	if (comparison !== undefined) {
		return comparison > 0;
	}
	return candidateVersion.trim() !== currentVersion.trim();
}

export async function getLatestPiRelease(
	_currentVersion: string,
	_options: { timeoutMs?: number; retry?: boolean } = {},
): Promise<LatestPiRelease | undefined> {
	if (process.env.DUME_OFFLINE || process.env.DUME_SKIP_VERSION_CHECK) return undefined;
	// External version check disabled for standalone DUM-E
	return undefined;
}

export async function getLatestPiVersion(
	currentVersion: string,
	options: { timeoutMs?: number; retry?: boolean } = {},
): Promise<string | undefined> {
	return (await getLatestPiRelease(currentVersion, options))?.version;
}

export async function checkForNewPiVersion(_currentVersion: string): Promise<LatestPiRelease | undefined> {
	if (process.env.DUME_SKIP_VERSION_CHECK || process.env.DUME_OFFLINE) return undefined;
	return undefined;
}
