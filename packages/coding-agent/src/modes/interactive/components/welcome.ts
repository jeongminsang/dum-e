import { type Component, truncateToWidth, visibleWidth } from "@dum-e/tui";
import { APP_NAME } from "../../../config.ts";
import { theme } from "../theme/theme.ts";
import { keyText } from "./keybinding-hints.ts";

export interface RecentSession {
	name: string;
	timeAgo: string;
}

export interface WelcomeComponentOptions {
	changelogMarkdown?: string;
	reducedMotion?: boolean;
}

// biome-ignore format: preserve ASCII art layout
const DUME_ROBOT_LOGO = [
	"   .------.   ",
	"  /  [o][o]\\  ",
	" |    __    | ",
	"  \\  '=='  /  ",
	"   '--||--'   ",
	"    //||\\\\    ",
	"   // || \\\\   ",
	"  //  ||  \\\\  ",
	"  ==  ==  ==  ",
];

const GRADIENT_STOPS: ReadonlyArray<readonly [number, number, number]> = [
	[255, 179, 0], // Cyber Amber
	[255, 110, 0], // Industrial Orange
	[0, 215, 255], // Electric Cyan
	[95, 135, 255], // Steel Blue
	[212, 212, 212], // Titanium White
];

const INTRO_MS = 900;
const INTRO_TICK_MS = 40;

function interpolateColor(
	c1: readonly [number, number, number],
	c2: readonly [number, number, number],
	factor: number,
): [number, number, number] {
	return [
		Math.round(c1[0] + (c2[0] - c1[0]) * factor),
		Math.round(c1[1] + (c2[1] - c1[1]) * factor),
		Math.round(c1[2] + (c2[2] - c1[2]) * factor),
	];
}

function getGradientColor(t: number): [number, number, number] {
	const clamped = Math.max(0, Math.min(1, t));
	const segmentCount = GRADIENT_STOPS.length - 1;
	const scaled = clamped * segmentCount;
	const index = Math.min(Math.floor(scaled), segmentCount - 1);
	const factor = scaled - index;
	return interpolateColor(GRADIENT_STOPS[index], GRADIENT_STOPS[index + 1], factor);
}

function applyGradient(lines: readonly string[], phase = 0): string[] {
	const reset = "\x1b[0m";
	const rows = lines.length;
	const cols = Math.max(...lines.map((l) => l.length));
	const span = Math.max(1, cols + rows - 1);

	return lines.map((line, r) => {
		let out = "";
		for (let c = 0; c < line.length; c++) {
			const char = line[c];
			if (char === " ") {
				out += " ";
				continue;
			}
			const diag = (r + c) / span;
			const t = (diag + phase) % 1;
			const [red, green, blue] = getGradientColor(t);
			out += `\x1b[38;2;${red};${green};${blue}m${char}`;
		}
		return out + reset;
	});
}

export class WelcomeComponent implements Component {
	private animStart: number | null = null;
	private animTimer: NodeJS.Timeout | null = null;
	private readonly version: string;
	private modelName: string;
	private providerName: string;
	private recentSessions: RecentSession[];
	private readonly options: WelcomeComponentOptions;

	constructor(
		version: string,
		modelName: string,
		providerName: string,
		recentSessions: RecentSession[] = [],
		options: WelcomeComponentOptions = {},
	) {
		this.version = version;
		this.modelName = modelName;
		this.providerName = providerName;
		this.recentSessions = recentSessions;
		this.options = options;
	}

	invalidate(): void {}

	setModel(modelName: string, providerName: string): void {
		this.modelName = modelName;
		this.providerName = providerName;
	}

	setRecentSessions(sessions: RecentSession[]): void {
		this.recentSessions = sessions;
	}

	playIntro(requestRender: () => void): void {
		this.stopAnimation();
		if (this.options.reducedMotion) {
			requestRender();
			return;
		}
		this.animStart = performance.now();
		requestRender();
		this.animTimer = setInterval(() => {
			const elapsed = performance.now() - (this.animStart ?? 0);
			if (elapsed >= INTRO_MS) {
				this.stopAnimation();
			}
			requestRender();
		}, INTRO_TICK_MS);
		this.animTimer.unref?.();
	}

	dispose(): void {
		this.stopAnimation();
	}

	private stopAnimation(): void {
		if (this.animTimer != null) {
			clearInterval(this.animTimer);
			this.animTimer = null;
		}
		this.animStart = null;
	}

	private currentLogoFrame(): string[] {
		if (this.animStart == null) {
			return applyGradient(DUME_ROBOT_LOGO, 0);
		}
		const elapsed = performance.now() - this.animStart;
		const phase = (elapsed / INTRO_MS) % 1;
		return applyGradient(DUME_ROBOT_LOGO, phase);
	}

	private centerText(text: string, width: number): string {
		const visible = visibleWidth(text);
		if (visible >= width) return text;
		const leftPad = Math.floor((width - visible) / 2);
		return " ".repeat(leftPad) + text;
	}

	render(termWidth: number): string[] {
		const boxWidth = Math.max(0, termWidth - 2);
		if (boxWidth < 10) return [];

		const dualContentWidth = boxWidth - 3;
		const minLeftCol = 22;
		const minRightCol = 28;
		const showRightColumn = dualContentWidth >= minLeftCol + minRightCol;

		const leftCol = showRightColumn ? Math.min(34, Math.floor(dualContentWidth * 0.45)) : boxWidth - 2;
		const rightCol = showRightColumn ? dualContentWidth - leftCol : 0;

		const logoLines = this.currentLogoFrame();
		const modelPill = `${theme.fg("accent", "model:")} ${theme.bold(this.modelName)}`;
		const providerPill = `${theme.fg("dim", "via:")} ${theme.fg("muted", this.providerName)}`;

		const leftLines = [
			"",
			this.centerText(theme.bold(theme.fg("accent", "DUM-E AUTOMATON")), leftCol),
			this.centerText(theme.fg("dim", "autonomous · adaptive · precise"), leftCol),
			"",
			...logoLines.map((line) => this.centerText(line, leftCol)),
			"",
			this.centerText(modelPill, leftCol),
			this.centerText(providerPill, leftCol),
			"",
		];

		const flowKeys = [
			{ key: "/", label: "commands" },
			{ key: "!", label: "shell execution" },
			{ key: keyText("app.model.select") || "ctrl+l", label: "model selector" },
			{ key: keyText("app.thinking.cycle") || "shift+tab", label: "reasoning effort" },
			{ key: keyText("app.tools.expand") || "ctrl+o", label: "expand tools" },
			{ key: keyText("app.clear") || "ctrl+c", label: "clear / interrupt" },
		];

		const rightLines: string[] = [];
		if (showRightColumn) {
			const sep = ` ${theme.fg("dim", "─".repeat(Math.max(0, rightCol - 4)))}`;
			rightLines.push("");
			rightLines.push(` ${theme.bold(theme.fg("accent", "FLOW KEYS"))}`);
			for (const item of flowKeys) {
				const keyStyled = theme.bold(theme.fg("dim", item.key.padEnd(12)));
				const labelStyled = theme.fg("muted", item.label);
				rightLines.push(truncateToWidth(`   ${keyStyled} ${labelStyled}`, rightCol));
			}

			rightLines.push(sep);
			rightLines.push(` ${theme.bold(theme.fg("accent", "RECENT SESSIONS"))}`);
			if (this.recentSessions.length === 0) {
				rightLines.push(`   ${theme.fg("dim", "No previous sessions found")}`);
			} else {
				for (const s of this.recentSessions.slice(0, 3)) {
					const nameStr = theme.fg("muted", s.name);
					const timeStr = theme.fg("dim", `(${s.timeAgo})`);
					rightLines.push(truncateToWidth(`   ${nameStr} ${timeStr}`, rightCol));
				}
			}

			if (this.options.changelogMarkdown) {
				rightLines.push(sep);
				rightLines.push(` ${theme.bold(theme.fg("accent", "WHAT'S NEW"))}`);
				const firstChangelogLine = this.options.changelogMarkdown.split("\n")[0] || "";
				rightLines.push(truncateToWidth(`   ${theme.fg("dim", firstChangelogLine)}`, rightCol));
			}
			rightLines.push("");
		}

		// Frame border
		const hChar = "─";
		const tl = theme.fg("dim", "╭");
		const tr = theme.fg("dim", "╮");
		const bl = theme.fg("dim", "╰");
		const br = theme.fg("dim", "╯");
		const v = theme.fg("dim", "│");

		const title = ` ${APP_NAME} v${this.version} · Automaton Forge `;
		const titlePrefix = hChar.repeat(3);
		const titleStyled = theme.fg("dim", titlePrefix) + theme.fg("muted", title);
		const titleVis = visibleWidth(titlePrefix) + visibleWidth(title);
		const headerSpace = boxWidth - 2;
		const topBorder =
			titleVis >= headerSpace
				? tl + truncateToWidth(titleStyled, headerSpace) + tr
				: tl + titleStyled + theme.fg("dim", hChar.repeat(headerSpace - titleVis)) + tr;

		const bottomBorder = bl + theme.fg("dim", hChar.repeat(boxWidth - 2)) + br;

		const lines: string[] = [topBorder];
		const maxRows = showRightColumn ? Math.max(leftLines.length, rightLines.length) : leftLines.length;

		for (let i = 0; i < maxRows; i++) {
			const left = leftLines[i] ?? "";
			const leftPadded = truncateToWidth(left, leftCol) + " ".repeat(Math.max(0, leftCol - visibleWidth(left)));

			if (showRightColumn) {
				const right = rightLines[i] ?? "";
				const rightPadded =
					truncateToWidth(right, rightCol) + " ".repeat(Math.max(0, rightCol - visibleWidth(right)));
				lines.push(`${v} ${leftPadded} ${v} ${rightPadded} ${v}`);
			} else {
				lines.push(`${v} ${leftPadded} ${v}`);
			}
		}

		lines.push(bottomBorder);
		return lines;
	}
}
