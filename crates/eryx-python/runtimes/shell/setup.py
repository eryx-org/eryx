"""Build a platform wheel: the runtime is native code for one target."""

from setuptools import setup
from setuptools.command.bdist_wheel import bdist_wheel


class PlatformWheel(bdist_wheel):
    """Tag the wheel py3-none-<platform>: no Python code is compiled."""

    def finalize_options(self):
        super().finalize_options()
        self.root_is_pure = False

    def get_tag(self):
        return ("py3", "none", super().get_tag()[2])


setup(cmdclass={"bdist_wheel": PlatformWheel})
