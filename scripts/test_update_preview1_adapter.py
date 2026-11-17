"""Tests for update_preview1_adapter.py: `python3 -m unittest discover -s scripts -p 'test_update*'`."""

import unittest

import update_preview1_adapter as u

MANUAL = '''[[component]]
name = "Something else"
version = "1.0.0"
url = "https://example.test/"
crate_version = "1.0.0"

[[component]]
name = "wasi_snapshot_preview1 command adapter (Wasmtime)"
version = "50.0.1"
license = "Apache-2.0 WITH LLVM-exception"
url = "https://github.com/bytecodealliance/wasmtime/releases/tag/v50.0.1"
crate = "wasmtime"
crate_version = "50.0.1"
covers = [{ path = "crates/eryx/adapters/wasi_snapshot_preview1.command.wasm", sha256 = "aa11" }]

[[component]]
name = "After"
version = "2.0.0"
'''


class UpdateEntry(unittest.TestCase):
    def test_updates_only_the_adapter_entry(self):
        got = u.update_entry(MANUAL, "51.0.0", "bb22")
        self.assertIn('url = "https://github.com/bytecodealliance/wasmtime/releases/tag/v51.0.0"', got)
        self.assertIn('crate_version = "51.0.0"', got)
        self.assertIn('sha256 = "bb22"', got)
        # The other entries keep their versions.
        self.assertIn('name = "Something else"\nversion = "1.0.0"', got)
        self.assertIn('name = "After"\nversion = "2.0.0"', got)

    def test_in_step_is_a_no_op(self):
        self.assertEqual(u.update_entry(MANUAL, "50.0.1", "aa11"), MANUAL)

    def test_a_missing_entry_fails(self):
        with self.assertRaises(u.Error):
            u.update_entry(MANUAL.replace("command adapter", "other"), "51.0.0", "bb22")


class Pins(unittest.TestCase):
    CARGO = {"workspace": {"dependencies": {
        "wit-component-wasmtime": {"package": "wit-component", "version": "=0.261.0"},
        "wasmtime-environ": "=50.0.1",
    }}}

    def test_matching_pins(self):
        upstream = {"workspace": {"dependencies": {"wit-component": "0.261.0"}}}
        self.assertEqual(u.check_pins(self.CARGO, "50.0.1", upstream), [])

    def test_stale_pins_are_reported(self):
        upstream = {"workspace": {"dependencies": {"wit-component": "0.262.0"}}}
        problems = u.check_pins(self.CARGO, "51.0.0", upstream)
        self.assertEqual(len(problems), 2, problems)
        self.assertIn("=0.262.0", problems[0])
        self.assertIn("=51.0.0", problems[1])


if __name__ == "__main__":
    unittest.main()
