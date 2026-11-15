"""Build a platform wheel: the runtime is native code for one target."""

from setuptools import Distribution, setup
from setuptools.command.bdist_wheel import bdist_wheel


class PlatformDistribution(Distribution):
    """Platform-specific, so the package installs from the wheel's root
    (platlib) rather than from a `.data/purelib` directory."""

    def has_ext_modules(self):
        return True


class PlatformWheel(bdist_wheel):
    """Tag the wheel py3-none-<platform>: no Python code is compiled."""

    def get_tag(self):
        return ("py3", "none", super().get_tag()[2])


setup(distclass=PlatformDistribution, cmdclass={"bdist_wheel": PlatformWheel})
