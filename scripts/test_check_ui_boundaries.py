"""Regression tests for the source lint, including deliberate boundary violations."""
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from check_ui_boundaries import (
    check,
    engine_dependency_violations,
    engine_graph_violations,
    production_tokens,
    rust_tokens,
    source_violations,
)


class BoundaryChecks(unittest.TestCase):
    def check_sources(self, sources):
        """Exercise the actual directory globs and scanner, not a pure=True override."""
        with TemporaryDirectory() as directory:
            root = Path(directory)
            for path, source in sources.items():
                target = root / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(source)
            (root / 'Cargo.toml').write_text('[workspace]\n')
            engine = root / 'crates/tiny-playback/Cargo.toml'
            engine.parent.mkdir(parents=True, exist_ok=True)
            engine.write_text('[package]\nname = "tiny-playback"\n')
            return check(root)

    def test_actual_shared_and_feature_directories_receive_pure_rules(self):
        paths = (
            'src/media.rs', 'src/media/language.rs', 'src/media/new/rules.rs',
            'src/home/detail/model.rs', 'src/home/search/model.rs',
            'src/home/library/model.rs', 'src/home/sidebar/controller.rs',
            'src/player/reporting/gateway.rs', 'src/home/ports.rs',
            'src/settings/controller.rs', 'src/settings/model.rs',
            'src/home/detail/model/selection.rs', 'src/home/search/model/query.rs',
            'src/settings/model/validation.rs', 'src/settings/controller/editing.rs',
            'src/home/library/controller/paging.rs', 'src/player/ports.rs',
        )
        errors, count = self.check_sources({path: 'use gpui::Entity;' for path in paths})
        self.assertEqual(count, len(paths))
        self.assertEqual(len(errors), len(paths), errors)
        for path in paths:
            self.assertTrue(any(error.startswith(path + ':') and 'pure model/controller' in error
                                for error in errors), path)

    def test_actual_runtime_binding_and_view_paths_allow_gpui(self):
        paths = (
            'src/settings/binding.rs', 'src/settings/binding/track_preferences.rs',
            'src/settings/view.rs', 'src/settings/view/user.rs',
            'src/server/view/form.rs', 'src/home/detail/binding.rs',
        )
        errors, count = self.check_sources({path: 'use gpui::Entity;' for path in paths})
        self.assertEqual(count, len(paths))
        self.assertEqual(errors, [])

    def test_actual_ui_paths_reject_both_playback_ports_and_business_controllers(self):
        errors, count = self.check_sources({'src/ui/dropdown.rs': '''
use crate::media::gateway::PlaybackSourceGateway as Source;
use crate::player::reporting::gateway::PlaybackReportGateway as Reporter;
use crate::settings::SettingsController;
use crate::server::feature::form::ServerFormController;
'''})
        self.assertEqual(count, 1)
        self.assertEqual(len(errors), 4, errors)

    def test_home_bindings_cannot_reintroduce_concrete_gateway_assembly(self):
        errors, count = self.check_sources({
            'src/home/feed/binding.rs': 'use crate::home::adapter::EmbyHomeGateway;',
            'src/home/detail/launch.rs': 'use crate::player::adapter::EmbyPlaybackGateway;',
            'src/home/content.rs': '#[cfg(test)] fn fixture() { EmbyHomeGateway {} }',
            'src/home/adapter.rs': 'struct EmbyHomeGateway;',
        })
        self.assertEqual(count, 4)
        self.assertEqual(len(errors), 2, errors)
        self.assertTrue(all('composition root' in error for error in errors))

    def test_playback_bindings_receive_gateways_from_composition_root(self):
        errors, count = self.check_sources({
            'src/player/page/queue.rs': 'use crate::player::adapter::EmbyPlaybackGateway;',
            'src/player/page/reporting.rs': 'let gateway = EmbyPlaybackGateway { client, server };',
            'src/player/adapter.rs': 'struct EmbyPlaybackGateway;',
        })
        self.assertEqual(count, 3)
        self.assertEqual(len(errors), 2, errors)
        self.assertTrue(all('composition root' in error for error in errors))

    def test_mod_rs_is_rejected_in_application_engine_and_tests(self):
        paths = ('src/home/new/mod.rs', 'src/home/tests/mod.rs',
                 'crates/tiny-playback/src/new/mod.rs', 'tests/helpers/mod.rs',
                 'crates/tiny-playback/examples/helpers/mod.rs')
        errors, _ = self.check_sources({path: '' for path in paths})
        self.assertEqual(len(errors), len(paths), errors)
        self.assertTrue(all('instead of mod.rs' in error for error in errors))

    def test_icon_picker_view_cannot_take_shell_or_io_ownership(self):
        errors, count = self.check_sources({
            'src/server/view/icon_picker.rs': 'use crate::app::TinyApp; use crate::images::FileImageRepository;',
            'src/app/server_icon_picker.rs': 'use crate::images::FileImageRepository;',
        })
        self.assertEqual(count, 2)
        self.assertEqual(len(errors), 2, errors)
        self.assertTrue(all('props and callbacks' in error for error in errors))

    def test_ui_rejects_qualified_and_aliased_business_dependencies(self):
        for source in (
            'use crate::emby::EmbyClient as Client;',
            'use crate::{server::CachedServer, player::PlaybackPage};',
            'fn load() { crate::images::FileImageRepository.load(&request); }',
            'type Store = crate::persistence::PersistenceService;',
        ):
            with self.subTest(source=source):
                self.assertTrue(source_violations('src/ui/new_widget.rs', source))

    def test_pure_controller_rejects_ui_and_io_on_any_platform(self):
        for source in (
            'use gpui::{Context, Entity};',
            '#[cfg(target_os = "windows")] use gpui_platform as platform;',
            'fn read() { std::fs::read(path); }',
            'use std::{fs, path::Path};',
            'use reqwest as client;',
            'fn work() { std::thread::spawn(run); }',
        ):
            with self.subTest(source=source):
                self.assertTrue(source_violations('src/home/model/new.rs', source, pure=True))

    def test_comments_literals_lifetimes_and_test_items_do_not_hide_live_code(self):
        source = '''
// gpui and EmbyClient are forbidden in code, but fine in explanations.
/* outer /* FileImageRepository */ gpui */
const NOTES: &str = r###"gpui " EmbyClient // raw"###;
const ESCAPED: &str = "quoted \\\" gpui";
fn borrowed<'a>(value: &'a str) -> &'a str { value }
#[cfg(test)] mod tests { use gpui::Context; fn fixture() { let generation = 2; } }
#[cfg(test)] fn test_only() { crate::emby::EmbyClient::new(); }
#[cfg(not(test))] fn production() { let generation = 4; }
'''
        errors = source_violations('src/home/model/new.rs', source, pure=True)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn('generation', errors[0])
        self.assertNotIn('EmbyClient', errors[0])

    def test_test_only_fields_do_not_swallow_the_following_production_field(self):
        source = '''
struct Model {
    #[cfg(test)] pub(crate) fixture: gpui::Entity<Fixture>,
    current: gpui::Entity<Current>,
}
'''
        errors = source_violations('src/home/model/new.rs', source, pure=True)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn(':4:', errors[0])

    def test_test_function_generic_return_and_body_comparisons_are_skipped(self):
        source = '''
#[cfg(test)]
fn fixture() -> Result<Vec<(Item, Source)>, Error> {
    if position < limit { EmbyHomeGateway {} }
}
use gpui::Entity;
'''
        errors = source_violations('src/home/detail/model.rs', source, pure=True)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn('gpui', errors[0])

    def test_moving_generation_to_another_application_file_is_rejected(self):
        source = 'struct Request { generation: u64 }'
        self.assertTrue(source_violations('src/home/data.rs', source))
        self.assertEqual(source_violations('src/effects.rs', source, pure=True), [])

    def test_engine_rejects_ui_reference_but_allows_native_backend_io(self):
        self.assertTrue(source_violations('crates/tiny-playback/src/new.rs', 'use gpui::RenderImage;'))
        self.assertEqual(source_violations('crates/tiny-playback/src/new.rs', 'use reqwest; use std::fs;'), [])

    def test_engine_manifest_resolves_aliases_workspace_and_target_dependencies(self):
        workspace = {'workspace': {'dependencies': {'ui': {'package': 'gpui', 'version': '1'}}}}
        engine = {
            'dependencies': {'ui': {'workspace': True}},
            'target': {'cfg(windows)': {
                'build-dependencies': {'app': {'package': 'tiny-player', 'path': '../..'}},
                'dev-dependencies': {'native_ui': {'package': 'gpui_platform', 'version': '1'}},
            }},
        }
        errors = engine_dependency_violations(workspace, engine)
        self.assertEqual(len(errors), 3, errors)
        self.assertEqual(engine_dependency_violations({}, {'dependencies': {'reqwest': '0.12'}}), [])

    def test_tokens_preserve_line_numbers_and_raw_identifier_spelling(self):
        tokens = production_tokens(rust_tokens('/* first\n second */\nr#gpui::Entity;'))
        self.assertEqual((tokens[0].value, tokens[0].line), ('gpui', 3))
        self.assertTrue(source_violations('src/home/model/new.rs', 'use r#gpui::Entity;', pure=True))

    def test_resolved_graph_rejects_transitive_ui_even_under_an_aliased_target_edge(self):
        metadata = {
            'workspace_members': ['engine', 'app'],
            'packages': [{'id': identifier, 'name': name} for identifier, name in
                         [('engine', 'tiny-playback'), ('bridge', 'bridge'), ('ui', 'gpui'), ('app', 'tiny-player')]],
            'resolve': {'nodes': [
                {'id': 'engine', 'deps': [{'name': 'renamed', 'pkg': 'bridge',
                                          'dep_kinds': [{'kind': 'build', 'target': 'cfg(windows)'}]}]},
                {'id': 'bridge', 'deps': [{'name': 'native_ui', 'pkg': 'ui'}]},
                {'id': 'ui', 'deps': []},
                {'id': 'app', 'deps': [{'name': 'gpui', 'pkg': 'ui'}]},
            ]},
        }
        self.assertEqual(engine_graph_violations(metadata), ['engine dependency graph: tiny-playback -> bridge -> gpui'])
        metadata['resolve']['nodes'][1]['deps'] = []
        self.assertEqual(engine_graph_violations(metadata), [])
        metadata['resolve'] = None
        self.assertTrue(engine_graph_violations(metadata))


if __name__ == '__main__':
    unittest.main()
