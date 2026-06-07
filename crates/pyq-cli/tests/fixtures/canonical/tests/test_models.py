import pytest

from pkg.core import tested_public


@pytest.mark.django_db
class TestModels:
    def test_one(self):
        assert tested_public("x") == "x"

    def helper(self):
        # not a `test_*` method — not collected
        return 1
