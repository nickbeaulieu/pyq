from unittest import TestCase

from pkg.calc import add


class AdditionTests(TestCase):
    """A *TestCase subclass with a non-`Test*` name — collected by inheritance."""

    def test_add_is_correct(self):
        assert add(2, 2) == 4

    def _helper(self):
        return add(0, 0)
