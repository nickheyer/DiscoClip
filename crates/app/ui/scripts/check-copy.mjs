import { readFileSync, readdirSync } from 'node:fs';
import { dirname, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parse } from 'svelte/compiler';
import ts from 'typescript';

const ui = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repo = resolve(ui, '../../..');
const failures = [];
const filler = /\b(?:in force|amounts to|wider scope|at once|seamlessly|delve|leverage|it(?:'s| is) worth noting|as a matter of course)\b/i;

function check(file, source, offset, value) {
	const text = value.replace(/`[^`]*`/g, '').replace(/https?:\/\/\S+/g, '').replace(/\s+/g, ' ').trim();
	if (!text || /^<(?:path|rect|line|circle|polygon|polyline|svg|reference)\b/.test(text)) return;
	const reasons = [];
	if (text.includes('\u2014')) reasons.push('Remove the em dash.');
	if (/;\s+[A-Za-z]/.test(text) && !/^[\w-]+=[^\s]+;/.test(text) && !/^(?:application|text)\//.test(text)) {
		reasons.push('Split the sentence at the semicolon.');
	}
	if (filler.test(text)) reasons.push('Use direct wording.');
	if (text.split(/(?<=[.!?])\s+/).some((sentence) => sentence.split(/\s+/).length > 30)) {
		reasons.push('Keep sentences to 30 words.');
	}
	if (reasons.length) {
		const line = source.slice(0, offset).split('\n').length;
		failures.push(`${relative(repo, file)}:${line}: ${reasons.join(' ')}\n  ${text.slice(0, 220)}`);
	}
}

function comments(file, source, text = source, offset = 0) {
	const scanner = ts.createScanner(ts.ScriptTarget.Latest, false, ts.LanguageVariant.Standard, text);
	let token;
	while ((token = scanner.scan()) !== ts.SyntaxKind.EndOfFileToken) {
		if (token === ts.SyntaxKind.SingleLineCommentTrivia || token === ts.SyntaxKind.MultiLineCommentTrivia) {
			const comment = scanner.getTokenText().replace(/^\/\/\/?|^\/\*\*?|\*\/$/g, '').replace(/^\s*\* ?/gm, '');
			check(file, source, offset + scanner.getTokenPos(), comment);
		}
	}
}

function svelte(file, source) {
	const ast = parse(source, { modern: true });
	const seen = new Set();
	function visit(node) {
		if (!node || typeof node !== 'object' || seen.has(node)) return;
		seen.add(node);
		if (node.type === 'Text' || node.type === 'Comment') check(file, source, node.start, node.data);
		if (node.type === 'Literal' && typeof node.value === 'string') check(file, source, node.start, node.value);
		if (node.type === 'TemplateElement') check(file, source, node.start, node.value.raw);
		for (const [key, value] of Object.entries(node)) {
			if (['loc', 'css', 'parent'].includes(key)) continue;
			if (Array.isArray(value)) value.forEach(visit);
			else if (value && typeof value === 'object') visit(value);
		}
	}
	visit(ast);
	for (const script of [ast.instance, ast.module]) {
		if (script) comments(file, source, source.slice(script.content.start, script.content.end), script.content.start);
	}
	if (ast.css) comments(file, source, source.slice(ast.css.start, ast.css.end), ast.css.start);
}

function typescript(file, source) {
	const ast = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true);
	function visit(node) {
		if (ts.isStringLiteralLike(node) || ts.isTemplateHead(node) || ts.isTemplateMiddle(node) || ts.isTemplateTail(node)) {
			check(file, source, node.getStart(ast), node.text);
		}
		ts.forEachChild(node, visit);
	}
	visit(ast);
	comments(file, source);
}

function rust(file, source) {
	let i = 0;
	while (i < source.length) {
		const start = i;
		if (source.startsWith('//', i)) {
			const end = source.indexOf('\n', i);
			i = end < 0 ? source.length : end;
			check(file, source, start, source.slice(start + 2, i).replace(/^[/!]\s?/, ''));
		} else if (source.startsWith('/*', i)) {
			let depth = 1;
			i += 2;
			while (i < source.length && depth) {
				if (source.startsWith('/*', i)) { depth++; i += 2; }
				else if (source.startsWith('*/', i)) { depth--; i += 2; }
				else i++;
			}
			check(file, source, start, source.slice(start + 2, i - 2).replace(/^\s*\* ?/gm, ''));
		} else if (source[i] === 'r' && /^r#*"/.test(source.slice(i, i + 258))) {
			const prefix = source.slice(i).match(/^r(#{0,255})"/)[0];
			const suffix = `"${prefix.slice(1, -1)}`;
			const end = source.indexOf(suffix, i + prefix.length);
			i = end < 0 ? source.length : end + suffix.length;
		} else if (source[i] === '"') {
			i++;
			while (i < source.length) {
				if (source[i] === '\\') i += 2;
				else if (source[i++] === '"') break;
			}
		} else if (source[i] === "'") {
			const character = source.slice(i).match(/^'(?:\\(?:u\{[0-9a-fA-F]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'/);
			i += character ? character[0].length : 1;
		} else i++;
	}
}

function rustComments(directory) {
	for (const entry of readdirSync(directory, { withFileTypes: true })) {
		if (['target', 'node_modules', '.svelte-kit', 'cache', 'build'].includes(entry.name)) continue;
		const file = resolve(directory, entry.name);
		if (entry.isDirectory()) rustComments(file);
		else if (file.endsWith('.rs')) rust(file, readFileSync(file, 'utf8'));
	}
}

function markdown(file, source) {
	let fenced = false;
	let offset = 0;
	let paragraph = '';
	let start = 0;
	function flush() { check(file, source, start, paragraph); paragraph = ''; }
	for (const line of source.split('\n')) {
		if (/^\s*```/.test(line)) { flush(); fenced = !fenced; }
		else if (!fenced) {
			if (!line.trim()) flush();
			else if (/^\s*\|/.test(line)) {
				flush();
				for (const cell of line.replace(/`[^`]*`/g, '').split('|')) check(file, source, offset, cell);
			} else {
				if (/^\s*(?:[-*]|\d+\.|#)\s/.test(line)) flush();
				if (!paragraph) start = offset;
				paragraph += ` ${line}`;
			}
		}
		offset += line.length + 1;
	}
	flush();
}

function walk(directory) {
	for (const entry of readdirSync(directory, { withFileTypes: true })) {
		const file = resolve(directory, entry.name);
		if (entry.isDirectory()) walk(file);
		else if (/\.(?:ts|svelte|css)$/.test(file)) {
			const source = readFileSync(file, 'utf8');
			if (file.endsWith('.svelte')) svelte(file, source);
			else if (file.endsWith('.ts')) typescript(file, source);
			else comments(file, source);
		}
	}
}

walk(resolve(ui, 'src'));
rustComments(resolve(repo, 'crates'));
const example = resolve(repo, 'discoclip.example.toml');
const config = readFileSync(example, 'utf8');
let configOffset = 0;
for (const line of config.split('\n')) {
	const comment = line.indexOf('#');
	if (comment >= 0) check(example, config, configOffset + comment, line.slice(comment + 1));
	configOffset += line.length + 1;
}
for (const name of ['README.md', 'API.md', 'TODOS.md', 'crates/app/ui/DESIGN.md']) {
	const file = resolve(repo, name);
	markdown(file, readFileSync(file, 'utf8'));
}
if (failures.length) {
	console.error(failures.join('\n'));
	console.error(`\n${failures.length} copy issue(s). See crates/app/ui/DESIGN.md.`);
	process.exitCode = 1;
} else {
	console.log('Copy checks passed.');
}
