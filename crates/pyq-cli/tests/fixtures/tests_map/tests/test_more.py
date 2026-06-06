from pkg.calc import helper


class TestThings:
    def test_via_helper(self):
        assert helper() == 3

    def not_collected(self):
        return helper()
