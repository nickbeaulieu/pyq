import pytest

from pkg.core import tested_public


@pytest.mark.slow
def test_tested_public():
    assert tested_public(" Ada ") == "ada"


@pytest.mark.parametrize("v", ["a", "b"])
def test_param(v):
    assert tested_public(v) == v


def not_a_test():
    # a non-`test_*` function in a test file — not collected
    return 1
