"""Check generator definition errors before they can emit misleading parsers."""

import copy
import json
import unittest

import generate
import import_i3


class GrammarValidation(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.schema = json.loads(generate.SCHEMA.read_text())

    def test_ambiguous_spellings_and_duplicate_productions_are_rejected(self):
        schema = copy.deepcopy(self.schema)
        schema['enums']['Direction']['Up'].append('left')
        with self.assertRaisesRegex(ValueError, 'duplicate spelling'):
            generate.generate(schema)
        schema = copy.deepcopy(self.schema)
        rules = schema['productions']['Border']['rules']
        rules.append(rules[0])
        with self.assertRaisesRegex(ValueError, 'duplicate production'):
            generate.generate(schema)

    def test_unknown_capture_types_and_nonterminal_tails_are_rejected(self):
        schema = copy.deepcopy(self.schema)
        rule = schema['productions']['Border']['rules'][0]
        rule['pattern'] = ['$width:UnregisteredType']
        with self.assertRaisesRegex(ValueError, 'invalid capture'):
            generate.generate(schema)
        rule['pattern'] = ['$width:BorderWidth*', 'trailing']
        with self.assertRaisesRegex(ValueError, 'tail captures must be last'):
            generate.generate(schema)

    def test_native_key_mapping_must_cover_the_entire_declared_vocabulary(self):
        schema = copy.deepcopy(self.schema)
        del schema['physical_keys']['Return']
        with self.assertRaisesRegex(ValueError, 'exactly one physical identity'):
            generate.mappings(schema)

    def test_upstream_references_resolve_and_inventory_accounts_for_every_state(self):
        imported = import_i3.import_states()
        report = generate.coverage(self.schema)
        self.assertEqual(set(imported['states']), {entry['state'] for entry in report['states']})
        self.assertTrue(any(entry['disposition'] == 'reference-only' for entry in report['states']))
        schema = copy.deepcopy(self.schema)
        schema['references']['ModeHeader'].append('config.spec:NOT_A_STATE')
        with self.assertRaisesRegex(ValueError, 'unknown upstream state'):
            generate.coverage(schema)


if __name__ == '__main__':
    unittest.main()
