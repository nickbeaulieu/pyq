# A test module. Its env reads are not part of the app surface and are excluded
# from the default view.
import os


def test_uses_env():
    return os.getenv("TEST_ONLY_VAR")
