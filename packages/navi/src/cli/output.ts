/** JSON framing keeps page content as data, not terminal commands or forged log lines. */
export function formatOutput(value: unknown): string {
	const json = JSON.stringify(value) ?? "null";
	return json.replace(
		/[\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g,
		(character) => `\\u${character.charCodeAt(0).toString(16).padStart(4, "0")}`,
	);
}
