from pkg.calc import add, helper


def test_add():
    assert add(1, 2) == 3


def test_helper():
    assert helper() == 3


def not_a_test():
    return add(0, 0)
