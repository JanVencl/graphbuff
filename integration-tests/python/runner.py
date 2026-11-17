"""behave runner that takes the hooks and step definitions from python/ instead of a
`steps/` folder next to the features, so cucumber/ and tck/ share one set of Python.
Selected by `runner = python.runner:Runner` in integration-tests/behave.ini."""

import behave.runner


class Runner(behave.runner.Runner):
    def setup_paths(self):
        self.config.steps_dir = "python/steps"
        self.config.environment_file = "python/environment.py"
        super().setup_paths()
