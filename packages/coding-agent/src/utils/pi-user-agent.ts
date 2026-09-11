export function getPiUserAgent(version: string): string {
	const runtime = process.versions.bun ? `bun/${process.versions.bun}` : `node/${process.version}`;
	return `dume/${version} (${process.platform}; ${runtime}; ${process.arch})`;
}
