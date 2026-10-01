#!/usr/bin/env python3
"""Check the application/engine dependency boundaries without Cargo changes.

This is a source dependency lint, not Rust name resolution: it checks identifiers
and qualified paths (including imports/aliases), ignoring comments, literals and
items explicitly gated with #[cfg(test)]. Rust visibility and workspace builds
remain the enforcement for resolved types. Platform-gated production code is
checked on every host. Run with Python 3.11+ from any directory.
"""
from __future__ import annotations

import argparse
from collections import deque
from dataclasses import dataclass
import json
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class Token:
    value: str
    line: int


# Preserve token boundaries so a comment/string never creates a false path.
_LITERAL = re.compile(
    r'(?P<raw>(?:br|cr|r)(?P<hashes>\#*)")'
    r'|(?P<string>(?:b|c)?"(?:\\.|[^"\\])*")'
    r"|(?P<char>b?'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])')",
    re.DOTALL,
)
_TOKEN = re.compile(r'(?:r\#)?[A-Za-z_][A-Za-z_0-9]*|::|[^\s]')


def rust_tokens(source: str) -> list[Token]:
    result: list[Token] = []
    index, line = 0, 1
    while index < len(source):
        end = index
        if source.startswith('//', index):
            end = source.find('\n', index)
            if end < 0:
                end = len(source)
        elif source.startswith('/*', index):
            end, depth = index + 2, 1
            while end < len(source) and depth:
                if source.startswith('/*', end):
                    depth += 1
                    end += 2
                elif source.startswith('*/', end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            if depth:
                raise ValueError('unterminated Rust block comment')
        elif match := _LITERAL.match(source, index):
            end = match.end()
            if match.group('raw'):
                closing = '"' + match.group('hashes')
                closing_at = source.find(closing, end)
                if closing_at < 0:
                    raise ValueError('unterminated Rust raw string')
                end = closing_at + len(closing)
            result.append(Token('<literal>', line))
        elif source[index].isspace():
            end = index + 1
        else:
            match = _TOKEN.match(source, index)
            assert match is not None
            result.append(Token(match.group().removeprefix('r#'), line))
            end = match.end()
        line += source[index:end].count('\n')
        index = end
    return result


def production_tokens(tokens: list[Token]) -> list[Token]:
    result: list[Token] = []
    marker = ['#', '[', 'cfg', '(', 'test', ')', ']']
    index = 0
    while index < len(tokens):
        if [t.value for t in tokens[index:index + len(marker)]] != marker:
            result.append(tokens[index])
            index += 1
            continue
        # Skip the attributed item/statement, including any additional attributes.
        index += len(marker)
        nesting = []
        while index < len(tokens):
            value = tokens[index].value
            index += 1
            # A comma in Result<T, E> is part of a test function signature,
            # not the end of an attributed field. Comparisons inside its body
            # are not generic brackets and must not change block nesting.
            if value == '<' and '{' not in nesting:
                nesting.append(value)
            elif value == '>' and nesting and nesting[-1] == '<':
                nesting.pop()
            elif value in ('(', '[', '{'):
                nesting.append(value)
            elif value in (')', ']', '}'):
                if not nesting:
                    break  # cfg(test) field/argument at the end of its owner
                opening = nesting.pop()
                if opening == '{' and not nesting:
                    break
            elif value in (';', ',') and not nesting:
                break
    return result


def is_test_file(path: Path) -> bool:
    return ('tests' in path.parts or path.stem == 'tests'
            or path.stem.endswith('_tests') or path.stem in ('test_support', 'test_fixture'))


PURE_GLOBS = (
    'src/media.rs', 'src/media/**/*.rs',
    'src/home/model.rs', 'src/home/model/**/*.rs',
    'src/home/controller.rs', 'src/home/controller/**/*.rs',
    'src/home/feed/controller.rs', 'src/home/feed/model.rs', 'src/home/feed/selectors.rs',
    'src/home/search/controller.rs', 'src/home/search/model.rs',
    'src/home/library/controller.rs', 'src/home/library/model.rs',
    'src/home/sidebar/controller.rs', 'src/home/sidebar/controller/**/*.rs',
    'src/home/favorites/controller.rs', 'src/home/favorites/actions.rs',
    'src/home/detail/controller.rs', 'src/home/detail/model.rs', 'src/home/detail/playback.rs',
    'src/home/played/controller.rs', 'src/home/played/model.rs',
    'src/home/resume_actions/controller.rs',
    'src/server/feature/catalog.rs', 'src/server/feature/controller.rs',
    'src/server/feature/form.rs', 'src/server/feature/icons.rs',
    'src/server/feature/model.rs', 'src/server/feature/selectors.rs',
    'src/player/model.rs', 'src/player/model/**/*.rs',
    'src/player/session.rs', 'src/player/session/**/*.rs',
    'src/player/queue/controller.rs', 'src/player/queue/model.rs',
    'src/player/reporting/controller.rs', 'src/player/reporting/completion.rs',
    'src/player/reporting/gateway.rs', 'src/home/gateway.rs', 'src/home/ports.rs',
    'src/settings.rs', 'src/settings/controller.rs', 'src/settings/model.rs',
    'src/settings/values.rs', 'src/settings/memory_budget.rs',
    'src/app/shell.rs', 'src/app/shell/**/*.rs',
    'src/persistence/model.rs', 'src/images/controller.rs', 'src/effects.rs',
    'src/player/ports.rs',
)
# A pure module remains pure when implementation moves into child modules.
PURE_GLOBS += tuple(pattern.removesuffix('.rs') + '/**/*.rs'
                    for pattern in PURE_GLOBS if pattern.endswith('.rs') and '*' not in pattern
                    and pattern != 'src/settings.rs')  # Its view/binding children are GPUI adapters.
ADAPTER_TYPES = {
    'EmbyClient', 'FileImageRepository', 'EmbyImageRepository', 'EmbyHomeGateway',
    'EmbyPlaybackGateway', 'PlaybackBackendAdapter', 'PersistenceService',
}
IO_IDENTIFIERS = {'reqwest', 'tokio', 'TcpStream', 'TcpListener', 'UdpSocket'}
PURE_FORBIDDEN = ADAPTER_TYPES | IO_IDENTIFIERS | {'gpui', 'gpui_platform'}
UI_FORBIDDEN = ADAPTER_TYPES | IO_IDENTIFIERS | {
    'CachedServer', 'ServerCache', 'PlaybackPage', 'TinyApp', 'ImageRepository',
    'ServerGateway', 'HomeGateway', 'PlaybackSourceGateway', 'PlaybackReportGateway',
    'AppPersistence', 'ServerFormController', 'SettingsController', 'HomeController',
    'PlaybackSessionController',
    'HomePorts', 'PlaybackPorts',
}
# These function identifiers are direct blocking entry points, not domain data.
IO_PATHS = (('std', '::', 'fs'), ('std', '::', 'net'), ('std', '::', 'thread'))


def identifier_violations(path: str, tokens: list[Token], forbidden: set[str], rule: str) -> list[str]:
    return [f'{path}:{token.line}: {rule}: {token.value}'
            for token in tokens if token.value in forbidden]


def source_violations(path: str, source: str, *, pure: bool = False) -> list[str]:
    tokens = production_tokens(rust_tokens(source))
    values = [token.value for token in tokens]
    errors: list[str] = []
    if pure:
        errors += identifier_violations(path, tokens, PURE_FORBIDDEN, 'pure model/controller imports UI or concrete IO')
    if path.startswith('src/ui/'):
        errors += identifier_violations(path, tokens, UI_FORBIDDEN, 'generic UI imports business state/adapter')
    if path.startswith('src/home/') and path != 'src/home/adapter.rs':
        errors += identifier_violations(path, tokens, {'EmbyHomeGateway', 'EmbyPlaybackGateway'},
                                        'Home binding must receive gateways from the composition root')
    if path.startswith('src/player/page/'):
        errors += identifier_violations(path, tokens, {'EmbyPlaybackGateway'},
                                        'Playback binding must receive gateways from the composition root')
    if path == 'src/server/view/icon_picker.rs':
        errors += identifier_violations(path, tokens,
                                        ADAPTER_TYPES | IO_IDENTIFIERS | {'TinyApp', 'ServerController', 'CachedServer'},
                                        'icon picker view receives props and callbacks, not Shell/IO owners')
    if pure or path.startswith('src/ui/'):
        for parts in IO_PATHS:
            for index in range(len(values) - len(parts) + 1):
                if tuple(values[index:index + len(parts)]) == parts:
                    errors.append(f'{path}:{tokens[index].line}: blocking IO/executor in model or generic UI: {"".join(parts)}')
        # Grouped std imports must not bypass the qualified-path guard.
        for index, value in enumerate(values):
            if value == 'use':
                end = values.index(';', index) if ';' in values[index:] else len(values)
                imported = values[index:end]
                if 'std' in imported and any(name in imported for name in ('fs', 'net', 'thread')):
                    errors.append(f'{path}:{tokens[index].line}: blocking std IO/executor import')
    if path.startswith('crates/tiny-playback/src/'):
        errors += identifier_violations(path, tokens, {'gpui', 'gpui_platform', 'tiny_player', 'TinyApp', 'PlaybackPage'}, 'engine depends on UI/application')
    elif path.startswith('src/') and path != 'src/effects.rs':
        errors += identifier_violations(path, tokens, {'generation'}, 'request generation belongs to effects::RequestSlot')
    return list(dict.fromkeys(errors))


def engine_dependency_violations(workspace: dict, engine: dict) -> list[str]:
    workspace_deps = workspace.get('workspace', {}).get('dependencies', {})
    tables = [('', engine)] + [(f'target.{name}.', target) for name, target in engine.get('target', {}).items()]
    errors = []
    for prefix, table in tables:
        for category in ('dependencies', 'dev-dependencies', 'build-dependencies'):
            for alias, value in table.get(category, {}).items():
                if isinstance(value, dict) and value.get('workspace'):
                    value = workspace_deps.get(alias, value)
                package = value.get('package', alias) if isinstance(value, dict) else alias
                if package in ('gpui', 'gpui_platform', 'tiny-player'):
                    errors.append(f'crates/tiny-playback/Cargo.toml: {prefix}{category}.{alias}: forbidden dependency {package}')
    return errors


def engine_graph_violations(metadata: dict) -> list[str]:
    packages = {package['id']: package for package in metadata['packages']}
    members = set(metadata['workspace_members'])
    engines = [package['id'] for package in packages.values()
               if package['name'] == 'tiny-playback' and package['id'] in members]
    if len(engines) != 1 or metadata.get('resolve') is None:
        return ['Cargo metadata must contain the workspace engine and resolved dependency graph']
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    pending = deque([(engines[0], ('tiny-playback',))])
    visited = set()
    errors = []
    while pending:
        package_id, chain = pending.popleft()
        if package_id in visited:
            continue
        visited.add(package_id)
        if packages[package_id]['name'] in ('gpui', 'gpui_platform', 'tiny-player'):
            errors.append('engine dependency graph: ' + ' -> '.join(chain))
        if package_id not in nodes:
            errors.append('Cargo metadata is missing dependency node: ' + package_id)
            continue
        # Do not filter dep_kinds: include build/dev and every target condition.
        for dependency in nodes[package_id]['deps']:
            target = dependency['pkg']
            pending.append((target, chain + (packages[target]['name'],)))
    return errors


def check(root: Path) -> tuple[list[str], int]:
    pure_paths = {path for pattern in PURE_GLOBS for path in root.glob(pattern)}
    paths = sorted(set(root.glob('src/**/*.rs')) | set(root.glob('crates/tiny-playback/src/**/*.rs')))
    errors: list[str] = []
    count = 0
    # Integration tests and examples are outside src/, but use the same module style.
    module_roots = ('src', 'tests', 'examples', 'crates/tiny-playback/src',
                    'crates/tiny-playback/tests', 'crates/tiny-playback/examples')
    for directory in module_roots:
        for path in sorted((root / directory).rglob('mod.rs')):
            relative = path.relative_to(root).as_posix()
            errors.append(f'{relative}: use <module>.rs with a child directory instead of mod.rs')
    for path in paths:
        relative = path.relative_to(root)
        if is_test_file(relative):
            continue
        count += 1
        errors += source_violations(relative.as_posix(), path.read_text(), pure=path in pure_paths)
    with (root / 'Cargo.toml').open('rb') as file:
        workspace = tomllib.load(file)
    with (root / 'crates/tiny-playback/Cargo.toml').open('rb') as file:
        engine = tomllib.load(file)
    errors += engine_dependency_violations(workspace, engine)
    return errors, count


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=ROOT)
    parser.add_argument('--metadata', type=Path,
                        help='read cargo metadata --locked --format-version 1 output instead of running Cargo offline')
    args = parser.parse_args()
    root = args.root.resolve()
    errors, count = check(root)
    if args.metadata:
        metadata = json.loads(args.metadata.read_text())
    else:
        result = subprocess.run(['cargo', 'metadata', '--locked', '--offline', '--format-version', '1'],
                                cwd=root, text=True, capture_output=True)
        if result.returncode:
            print('Cannot inspect the locked dependency graph. Run cargo metadata --locked --format-version 1 '
                  'to populate missing cached dependencies, then retry.')
            print(result.stderr.strip())
            return 1
        metadata = json.loads(result.stdout)
    if Path(metadata['workspace_root']).resolve() != root:
        errors.append('Cargo metadata belongs to a different workspace')
    else:
        errors += engine_graph_violations(metadata)
    if errors:
        print('\n'.join(errors))
        return 1
    print(f'UI dependency boundaries passed ({count} Rust source files; engine manifest and resolved graph).')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
